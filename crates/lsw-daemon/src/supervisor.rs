// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lsw_core::{
    prepare_live_share_runtime, read_frame, write_frame, ClientHello, CommandInvocation, Frame,
    FrameKind, HostCapabilities, IdlePolicy, InstanceManifest, InstanceState, LaunchPhase,
    Provisioner, QemuPlanner, ServerHello, StateStore, WindowsProfile, AGENT_PROTOCOL_VERSION,
    CAPABILITY_POWER_HIBERNATE_V1,
};

use crate::qmp::QmpClient;

const START_TIMEOUT: Duration = Duration::from_secs(8);
const HELPER_TIMEOUT: Duration = Duration::from_secs(4);
const FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(4);
const POLICY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const AGENT_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

struct ManagedVm {
    qemu: Child,
    helpers: Vec<Child>,
}

pub struct Supervisor {
    store: StateStore,
    capabilities: HostCapabilities,
    processes: BTreeMap<String, ManagedVm>,
    last_policy_poll: Instant,
}

impl Supervisor {
    pub fn new(store: StateStore, capabilities: HostCapabilities) -> Self {
        Self {
            store,
            capabilities,
            processes: BTreeMap::new(),
            last_policy_poll: Instant::now(),
        }
    }

    pub fn store(&self) -> &StateStore {
        &self.store
    }

    pub fn poll(&mut self) {
        let names = self.processes.keys().cloned().collect::<Vec<_>>();
        for name in names {
            let exit = self
                .processes
                .get_mut(&name)
                .and_then(|managed| managed.qemu.try_wait().ok().flatten());
            if let Some(status) = exit {
                let shutdown_requested = shutdown_was_requested(&self.store, &name);
                let hibernate_requested = hibernate_was_requested(&self.store, &name);
                if let Some(mut managed) = self.processes.remove(&name) {
                    stop_helpers(&mut managed.helpers);
                    remove_shutdown_marker(&self.store, &name);
                    remove_hibernate_marker(&self.store, &name);
                    cleanup_stopped_runtime_artifacts(&self.store, &name);
                    self.remove_ephemeral_overlay(&name);
                    let next = state_after_qemu_exit(
                        shutdown_requested,
                        hibernate_requested,
                        status.success(),
                    );
                    if let Err(error) = self.set_state(&name, next) {
                        eprintln!("lswd: could not update {name:?} after QEMU exit: {error}");
                    }
                }
                continue;
            }

            let helper_exited = self.processes.get_mut(&name).is_some_and(|managed| {
                managed
                    .helpers
                    .iter_mut()
                    .any(|helper| helper.try_wait().ok().flatten().is_some())
            });
            if helper_exited {
                let shutdown_requested = shutdown_was_requested(&self.store, &name);
                let hibernate_requested = hibernate_was_requested(&self.store, &name);
                if let Some(mut managed) = self.processes.remove(&name) {
                    let natural_qemu_exit = if hibernate_requested {
                        wait_for_child_exit(&mut managed.qemu, FORCE_STOP_TIMEOUT)
                            .ok()
                            .flatten()
                    } else {
                        None
                    };
                    if natural_qemu_exit.is_none() {
                        let _ = managed.qemu.kill();
                        let _ = managed.qemu.wait();
                    }
                    stop_helpers(&mut managed.helpers);
                    remove_shutdown_marker(&self.store, &name);
                    remove_hibernate_marker(&self.store, &name);
                    cleanup_stopped_runtime_artifacts(&self.store, &name);
                    self.remove_ephemeral_overlay(&name);
                    let next = state_after_qemu_exit(
                        shutdown_requested,
                        hibernate_requested && natural_qemu_exit.is_some(),
                        natural_qemu_exit.is_some_and(|status| status.success()),
                    );
                    if let Err(error) = self.set_state(&name, next) {
                        eprintln!("lswd: could not update {name:?} after helper exit: {error}");
                    }
                }
            }
        }
        if self.last_policy_poll.elapsed() >= POLICY_POLL_INTERVAL {
            self.last_policy_poll = Instant::now();
            if let Err(error) = self.apply_idle_policies() {
                eprintln!("lswd: idle policy failed: {error}");
            }
        }
    }

    pub fn has_active_work(&self) -> bool {
        !self.processes.is_empty()
            || self.store.list().is_ok_and(|manifests| {
                manifests.into_iter().any(|manifest| {
                    matches!(
                        manifest.state,
                        InstanceState::Installing
                            | InstanceState::Running
                            | InstanceState::Suspended
                    ) && self.qmp_status(&manifest.spec.name).is_ok()
                })
            })
    }

    pub fn start(
        &mut self,
        name: &str,
        phase: LaunchPhase,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.poll();
        if let Ok(qmp_state) = self.qmp_status(name) {
            let manifest = self.store.load(name)?;
            if phase == LaunchPhase::Run
                && manifest.state == InstanceState::Suspended
                && qmp_state == "paused"
            {
                return self.resume(name);
            }
            return Ok(vec![format!("instance {name} is already running")]);
        }

        let manifest = self.store.load(name)?;
        match (manifest.state, phase) {
            (InstanceState::Configured, LaunchPhase::Run) => {
                return Err("instance is not installed; run the install phase first".into())
            }
            (InstanceState::Running | InstanceState::Installing, _) => {
                return Err("manifest says the instance is active but QMP is unavailable; inspect its logs before retrying".into())
            }
            (_, LaunchPhase::Install) if manifest.state != InstanceState::Configured => {
                return Err("refusing to reinstall a non-configured instance because its disk may contain data".into())
            }
            _ => {}
        }

        let instance_dir = self.store.instance_dir(name)?;
        let provisioner = Provisioner::new(self.capabilities.clone());
        let preparation = provisioner.plan(&manifest, &instance_dir)?;
        provisioner.apply(&preparation)?;
        cleanup_stopped_runtime_artifacts(&self.store, name);
        cleanup_runtime_sockets(&instance_dir)?;
        remove_shutdown_marker(&self.store, name);
        remove_hibernate_marker(&self.store, name);
        if phase == LaunchPhase::Run {
            let token = self.store.read_agent_token(name)?;
            let credential = if manifest.scoped_live_share_credential {
                lsw_core::derive_scoped_credential(&token, lsw_core::LIVE_SHARE_CREDENTIAL_SCOPE)?
            } else {
                token
            };
            prepare_live_share_runtime(&manifest, &instance_dir, &credential)?;
        }
        if phase == LaunchPhase::Run && manifest.spec.profile == WindowsProfile::Ephemeral {
            self.prepare_ephemeral_overlay(&instance_dir, manifest.state)?;
        }

        let plan =
            QemuPlanner::new(self.capabilities.clone()).plan(&manifest, &instance_dir, phase)?;
        if !plan.missing_capabilities.is_empty() {
            return Err(format!(
                "cannot start instance; missing {}",
                plan.missing_capabilities.join(", ")
            )
            .into());
        }

        let mut helpers = Vec::new();
        for helper in &plan.helper_commands {
            match spawn_command(helper, &instance_dir, "helper.log") {
                Ok(child) => helpers.push(child),
                Err(error) => {
                    stop_helpers(&mut helpers);
                    self.remove_ephemeral_overlay(name);
                    return Err(error);
                }
            }
        }
        if let Err(error) = wait_for_socket(
            &instance_dir.join("run/swtpm.sock"),
            &mut helpers,
            HELPER_TIMEOUT,
        ) {
            stop_helpers(&mut helpers);
            self.remove_ephemeral_overlay(name);
            return Err(error);
        }

        let qemu_invocation = CommandInvocation {
            program: plan.program,
            arguments: plan.arguments,
        };
        let qemu = match spawn_command(&qemu_invocation, &instance_dir, "qemu.log") {
            Ok(child) => child,
            Err(error) => {
                stop_helpers(&mut helpers);
                self.remove_ephemeral_overlay(name);
                return Err(error);
            }
        };
        self.processes
            .insert(name.to_owned(), ManagedVm { qemu, helpers });

        if let Err(error) = self.wait_for_qmp(name, START_TIMEOUT) {
            if let Some(mut managed) = self.processes.remove(name) {
                let _ = managed.qemu.kill();
                let _ = managed.qemu.wait();
                stop_helpers(&mut managed.helpers);
            }
            self.remove_ephemeral_overlay(name);
            let _ = self.set_state(name, InstanceState::Failed);
            return Err(error);
        }

        self.set_state(
            name,
            match phase {
                LaunchPhase::Install => InstanceState::Installing,
                LaunchPhase::Run => InstanceState::Running,
            },
        )?;
        self.record_activity(name)?;
        let _ = self.set_balloon_target(name, manifest.spec.memory_mib);
        Ok(vec![
            format!("instance {name} started in {phase} phase"),
            format!("QMP={}", instance_dir.join("run/qmp.sock").display()),
            format!("LOG={}", instance_dir.join("qemu.log").display()),
        ])
    }

    pub fn stop(
        &mut self,
        name: &str,
        force: bool,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.poll();
        let instance_dir = self.store.instance_dir(name)?;
        let manifest = self.store.load(name)?;
        if manifest.state == InstanceState::Hibernated {
            return Ok(vec![format!("instance {name} is hibernated")]);
        }
        let qmp_path = instance_dir.join("run/qmp.sock");

        let mut requested_qmp_quit = false;
        match QmpClient::connect(&qmp_path) {
            Ok(mut qmp) if force => {
                qmp.quit()?;
                requested_qmp_quit = true;
            }
            Ok(mut qmp) => {
                let status = qmp.status()?;
                if status == "paused" {
                    qmp.resume()?;
                    let resumed_status = qmp.status()?;
                    if resumed_status != "running" {
                        return Err(format!(
                            "QEMU accepted resume before shutdown for {name} but reported unexpected state {resumed_status:?}"
                        )
                        .into());
                    }
                    self.set_state(name, InstanceState::Running)?;
                }
                qmp.system_powerdown()?;
                write_shutdown_marker(&instance_dir)?;
                return Ok(vec![format!("graceful shutdown requested for {name}")]);
            }
            Err(error) if !force => {
                return Err(format!("cannot request a graceful shutdown: {error}").into())
            }
            Err(_) => {}
        }

        refuse_unproven_external_force_stop(
            force,
            requested_qmp_quit,
            self.processes.contains_key(name),
            manifest.state,
            name,
        )?;

        if let Some(managed) = self.processes.get_mut(name) {
            wait_or_kill(&mut managed.qemu, FORCE_STOP_TIMEOUT)?;
        } else if requested_qmp_quit {
            wait_for_qmp_disconnect(&qmp_path, FORCE_STOP_TIMEOUT)?;
        }
        if let Some(mut managed) = self.processes.remove(name) {
            stop_helpers(&mut managed.helpers);
        }
        self.set_state(name, InstanceState::Stopped)?;
        remove_shutdown_marker(&self.store, name);
        remove_hibernate_marker(&self.store, name);
        cleanup_stopped_runtime_artifacts(&self.store, name);
        self.remove_ephemeral_overlay(name);
        Ok(vec![format!("instance {name} stopped")])
    }

    pub fn status(&mut self, name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.poll();
        let manifest = self.store.load(name)?;
        match self.qmp_status(name) {
            Ok(qmp) => {
                let reconciled = match (manifest.state, qmp.as_str()) {
                    (InstanceState::Running, "paused") => InstanceState::Suspended,
                    (InstanceState::Suspended, "running") => InstanceState::Running,
                    (state, _) => state,
                };
                if reconciled != manifest.state {
                    self.set_state(name, reconciled)?;
                }
                Ok(vec![
                    format!("STATE={reconciled}"),
                    format!("QMP={qmp}"),
                    "ACTIVE=true".to_owned(),
                ])
            }
            Err(_) => {
                let active_manifest = matches!(
                    manifest.state,
                    InstanceState::Running | InstanceState::Installing | InstanceState::Suspended
                );
                if active_manifest {
                    let instance_dir = self.store.instance_dir(name)?;
                    let evidence = qemu_process_evidence(
                        &instance_dir,
                        name,
                        self.processes.contains_key(name),
                    );
                    if evidence != QemuProcessEvidence::Gone {
                        // A QMP endpoint accepts one active control client. A
                        // concurrent diagnostic can therefore make a healthy
                        // owned or externally inherited QEMU temporarily
                        // unreachable. Never unlink its live sockets or make a
                        // second launch eligible from one failed connection.
                        return Ok(vec![
                            format!("STATE={}", manifest.state),
                            "QMP=unavailable".to_owned(),
                            match evidence {
                                QemuProcessEvidence::Live => "ACTIVE=true".to_owned(),
                                QemuProcessEvidence::Unknown => "ACTIVE=unknown".to_owned(),
                                QemuProcessEvidence::Gone => unreachable!(),
                            },
                        ]);
                    }
                }
                let state = if matches!(
                    manifest.state,
                    InstanceState::Running | InstanceState::Installing | InstanceState::Suspended
                ) {
                    let requested = shutdown_was_requested(&self.store, name);
                    let hibernated = hibernate_was_requested(&self.store, name);
                    let state = if hibernated {
                        InstanceState::Hibernated
                    } else if requested {
                        InstanceState::Stopped
                    } else {
                        InstanceState::Failed
                    };
                    self.set_state(name, state)?;
                    remove_shutdown_marker(&self.store, name);
                    remove_hibernate_marker(&self.store, name);
                    cleanup_stopped_runtime_artifacts(&self.store, name);
                    self.remove_ephemeral_overlay(name);
                    state
                } else {
                    manifest.state
                };
                Ok(vec![
                    format!("STATE={state}"),
                    "QMP=unavailable".to_owned(),
                    "ACTIVE=false".to_owned(),
                ])
            }
        }
    }

    pub fn suspend(&mut self, name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.poll();
        let manifest = self.store.load(name)?;
        if manifest.state == InstanceState::Suspended {
            if matches!(self.qmp_status(name).as_deref(), Ok("paused")) {
                return Ok(vec![format!("instance {name} is already suspended")]);
            }
            return Err("manifest says the instance is suspended but QMP is not paused".into());
        }
        if manifest.state != InstanceState::Running {
            return Err(format!(
                "instance {name} cannot be suspended from state {}",
                manifest.state
            )
            .into());
        }

        let qmp_path = self.store.instance_dir(name)?.join("run/qmp.sock");
        let mut qmp = QmpClient::connect(&qmp_path)?;
        let status = qmp.status()?;
        if status != "running" {
            return Err(format!(
                "instance {name} cannot be suspended while QMP reports {status:?}"
            )
            .into());
        }
        qmp.pause()?;
        let status = qmp.status()?;
        if status != "paused" {
            return Err(format!(
                "QEMU accepted suspend for {name} but reported unexpected state {status:?}"
            )
            .into());
        }
        self.set_state(name, InstanceState::Suspended)?;
        Ok(vec![format!("instance {name} suspended in memory")])
    }

    pub fn resume(&mut self, name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.poll();
        let manifest = self.store.load(name)?;
        if manifest.state == InstanceState::Hibernated {
            return self.start(name, LaunchPhase::Run);
        }
        let qmp_path = self.store.instance_dir(name)?.join("run/qmp.sock");
        let mut qmp = QmpClient::connect(&qmp_path)?;
        let status = qmp.status()?;

        if manifest.state == InstanceState::Running && status == "running" {
            return Ok(vec![format!("instance {name} is already running")]);
        }
        if manifest.state != InstanceState::Suspended || status != "paused" {
            return Err(format!(
                "instance {name} cannot be resumed from manifest state {} and QMP state {status:?}",
                manifest.state
            )
            .into());
        }
        qmp.resume()?;
        let status = qmp.status()?;
        if status != "running" {
            return Err(format!(
                "QEMU accepted resume for {name} but reported unexpected state {status:?}"
            )
            .into());
        }
        self.set_state(name, InstanceState::Running)?;
        self.record_activity(name)?;
        let _ = self.set_balloon_target(name, manifest.spec.memory_mib);
        Ok(vec![format!("instance {name} resumed")])
    }

    pub fn hibernate(&mut self, name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.poll();
        let mut manifest = self.store.load(name)?;
        if manifest.state == InstanceState::Hibernated {
            return Ok(vec![format!("instance {name} is already hibernated")]);
        }
        if manifest.state == InstanceState::Suspended {
            self.resume(name)?;
            manifest = self.store.load(name)?;
        }
        if manifest.state != InstanceState::Running {
            return Err(format!(
                "instance {name} cannot hibernate from state {}",
                manifest.state
            )
            .into());
        }
        let instance_dir = self.store.instance_dir(name)?;
        write_hibernate_marker(&instance_dir)?;
        let token = self.store.read_agent_token(name)?;
        if let Err(error) = request_guest_hibernate(&manifest, &token) {
            remove_hibernate_marker(&self.store, name);
            return Err(error);
        }
        Ok(vec![format!("Windows hibernation requested for {name}")])
    }

    pub fn activity(&mut self, name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let manifest = self.store.load(name)?;
        self.record_activity(name)?;
        if manifest.state == InstanceState::Running {
            let _ = self.set_balloon_target(name, manifest.spec.memory_mib);
        }
        Ok(vec![format!("activity recorded for {name}")])
    }

    pub fn balloon(
        &mut self,
        name: &str,
        memory_mib: u32,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let manifest = self.store.load(name)?;
        if !(manifest.memory_min_mib..=manifest.spec.memory_mib).contains(&memory_mib) {
            return Err(format!(
                "balloon target must be between {} and {} MiB",
                manifest.memory_min_mib, manifest.spec.memory_mib
            )
            .into());
        }
        self.set_balloon_target(name, memory_mib)?;
        Ok(vec![format!(
            "instance {name} balloon target is {memory_mib} MiB"
        )])
    }

    pub fn qmp_status(&self, name: &str) -> Result<String, Box<dyn std::error::Error>> {
        let path = self.store.instance_dir(name)?.join("run/qmp.sock");
        QmpClient::connect(&path)?.status()
    }

    fn wait_for_qmp(
        &mut self,
        name: &str,
        timeout: Duration,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.qmp_status(name).is_ok() {
                return Ok(());
            }
            let Some(managed) = self.processes.get_mut(name) else {
                return Err("QEMU process disappeared while starting".into());
            };
            if let Some(status) = managed.qemu.try_wait()? {
                return Err(format!("QEMU exited during startup with {status}").into());
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for the QMP socket".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn set_state(
        &self,
        name: &str,
        state: InstanceState,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut manifest = self.store.load(name)?;
        manifest.state = state;
        manifest.state_changed_unix_seconds = unix_seconds();
        self.store.update(&manifest)?;
        Ok(())
    }

    fn record_activity(&self, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.store.instance_dir(name)?.join("run/last-activity");
        write_private_marker(&path, &unix_seconds().to_string())
    }

    fn set_balloon_target(
        &self,
        name: &str,
        memory_mib: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let instance_dir = self.store.instance_dir(name)?;
        let marker = instance_dir.join("run/balloon.target");
        if fs::read_to_string(&marker)
            .map(|value| value.trim() == memory_mib.to_string())
            .unwrap_or(false)
        {
            return Ok(());
        }
        let bytes = u64::from(memory_mib)
            .checked_mul(1024 * 1024)
            .ok_or("balloon target overflowed")?;
        QmpClient::connect(&instance_dir.join("run/qmp.sock"))?.balloon(bytes)?;
        write_private_marker(&marker, &memory_mib.to_string())
    }

    fn apply_idle_policies(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let now = unix_seconds();
        let memory_pressure = host_memory_pressure();
        for manifest in self.store.list()? {
            if manifest.idle_policy == IdlePolicy::Off {
                continue;
            }
            let activity = read_activity(&self.store, &manifest.spec.name)
                .unwrap_or(manifest.state_changed_unix_seconds);
            let idle_seconds = now.saturating_sub(activity);
            match manifest.state {
                InstanceState::Running => {
                    if memory_pressure
                        || (manifest.idle_timeout_seconds > 0
                            && idle_seconds >= manifest.idle_timeout_seconds / 2)
                    {
                        let _ =
                            self.set_balloon_target(&manifest.spec.name, manifest.memory_min_mib);
                    }
                    if manifest.idle_timeout_seconds > 0
                        && idle_seconds >= manifest.idle_timeout_seconds
                    {
                        let _ = self.suspend(&manifest.spec.name);
                    }
                }
                InstanceState::Suspended
                    if manifest.hibernate_timeout_seconds > 0
                        && now.saturating_sub(manifest.state_changed_unix_seconds)
                            >= manifest.hibernate_timeout_seconds =>
                {
                    let _ = self.hibernate(&manifest.spec.name);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn prepare_ephemeral_overlay(
        &self,
        instance_dir: &Path,
        previous_state: InstanceState,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let overlay = instance_dir.join("run/ephemeral.qcow2");
        match fs::symlink_metadata(&overlay) {
            Ok(metadata)
                if metadata.file_type().is_file()
                    && matches!(
                        previous_state,
                        InstanceState::Stopped | InstanceState::Failed
                    ) =>
            {
                fs::remove_file(&overlay)?;
            }
            Ok(_) => {
                return Err(format!(
                    "refusing to replace unexpected ephemeral overlay {}",
                    overlay.display()
                )
                .into())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let base = fs::canonicalize(instance_dir.join("disk.qcow2"))?;
        let qemu_img = self
            .capabilities
            .qemu_img
            .as_ref()
            .ok_or("qemu-img is required for an ephemeral overlay")?;
        let status = Command::new(qemu_img)
            .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
            .arg(&base)
            .arg(&overlay)
            .status()?;
        if !status.success() {
            let _ = fs::remove_file(&overlay);
            return Err(
                format!("qemu-img could not create the ephemeral overlay: {status}").into(),
            );
        }
        fs::set_permissions(&overlay, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    fn remove_ephemeral_overlay(&self, name: &str) {
        let Ok(manifest) = self.store.load(name) else {
            return;
        };
        if manifest.spec.profile != WindowsProfile::Ephemeral {
            return;
        }
        let Ok(path) = self
            .store
            .instance_dir(name)
            .map(|directory| directory.join("run/ephemeral.qcow2"))
        else {
            return;
        };
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                let _ = fs::remove_file(path);
            }
            _ => {}
        }
    }
}

mod runtime;
use runtime::*;

#[cfg(test)]
mod tests;
