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

fn spawn_command(
    invocation: &CommandInvocation,
    instance_dir: &Path,
    log_name: &str,
) -> Result<Child, Box<dyn std::error::Error>> {
    let stdout = append_log(instance_dir.join(log_name))?;
    let stderr = stdout.try_clone()?;
    Ok(Command::new(&invocation.program)
        .args(&invocation.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()?)
}

fn append_log(path: PathBuf) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn write_shutdown_marker(instance_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let path = instance_dir.join("run/shutdown.requested");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    use std::io::Write;
    writeln!(file, "requested")?;
    file.sync_all()?;
    Ok(())
}

fn write_hibernate_marker(instance_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    write_private_marker(&instance_dir.join("run/hibernate.requested"), "requested")
}

fn write_private_marker(path: &Path, value: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    use std::io::Write;
    writeln!(file, "{value}")?;
    file.sync_all()?;
    Ok(())
}

fn shutdown_was_requested(store: &StateStore, name: &str) -> bool {
    let Ok(path) = store
        .instance_dir(name)
        .map(|directory| directory.join("run/shutdown.requested"))
    else {
        return false;
    };
    matches!(
        fs::symlink_metadata(path),
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink()
    )
}

fn hibernate_was_requested(store: &StateStore, name: &str) -> bool {
    requested_marker_exists(store, name, "hibernate.requested")
}

fn requested_marker_exists(store: &StateStore, name: &str, file: &str) -> bool {
    let Ok(path) = store
        .instance_dir(name)
        .map(|directory| directory.join("run").join(file))
    else {
        return false;
    };
    matches!(
        fs::symlink_metadata(path),
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink()
    )
}

fn state_after_qemu_exit(
    shutdown_requested: bool,
    hibernate_requested: bool,
    exit_success: bool,
) -> InstanceState {
    if hibernate_requested {
        InstanceState::Hibernated
    } else if shutdown_requested || exit_success {
        InstanceState::Stopped
    } else {
        InstanceState::Failed
    }
}

fn remove_shutdown_marker(store: &StateStore, name: &str) {
    remove_requested_marker(store, name, "shutdown.requested");
}

fn remove_hibernate_marker(store: &StateStore, name: &str) {
    remove_requested_marker(store, name, "hibernate.requested");
}

fn remove_requested_marker(store: &StateStore, name: &str, file: &str) {
    let Ok(path) = store
        .instance_dir(name)
        .map(|directory| directory.join("run").join(file))
    else {
        return;
    };
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            let _ = fs::remove_file(path);
        }
        _ => {}
    }
}

fn read_activity(store: &StateStore, name: &str) -> Option<u64> {
    let path = store.instance_dir(name).ok()?.join("run/last-activity");
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn host_memory_pressure() -> bool {
    let Ok(meminfo) = fs::read_to_string("/proc/meminfo") else {
        return false;
    };
    let value = |name: &str| -> Option<u64> {
        meminfo.lines().find_map(|line| {
            let value = line.strip_prefix(name)?.trim();
            value.split_ascii_whitespace().next()?.parse().ok()
        })
    };
    match (value("MemAvailable:"), value("MemTotal:")) {
        (Some(available), Some(total)) if total > 0 => available.saturating_mul(100) < total * 10,
        _ => false,
    }
}

fn request_guest_hibernate(
    manifest: &InstanceManifest,
    token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, manifest.control_port);
    let mut stream = TcpStream::connect_timeout(&address.into(), AGENT_CONTROL_TIMEOUT)?;
    stream.set_read_timeout(Some(AGENT_CONTROL_TIMEOUT))?;
    stream.set_write_timeout(Some(AGENT_CONTROL_TIMEOUT))?;
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token: token.to_owned(),
    };
    write_frame(&mut stream, &Frame::new(FrameKind::Hello, hello.encode()?))?;
    let response = read_frame(&mut stream)?;
    if response.kind != FrameKind::HelloOk {
        return Err("guest agent rejected the hibernate control connection".into());
    }
    let hello = ServerHello::decode(&response.payload)?;
    if !hello
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY_POWER_HIBERNATE_V1)
    {
        return Err("guest agent does not support Windows hibernation".into());
    }
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::PowerHibernate, Vec::new()),
    )?;
    let response = read_frame(&mut stream)?;
    match response.kind {
        FrameKind::Pong if response.payload.is_empty() => Ok(()),
        FrameKind::Error => Err(format!(
            "guest agent refused hibernation: {}",
            String::from_utf8_lossy(&response.payload)
        )
        .into()),
        _ => Err("guest agent returned an invalid hibernate response".into()),
    }
}

fn cleanup_stopped_runtime_artifacts(store: &StateStore, name: &str) {
    let Ok(instance_dir) = store.instance_dir(name) else {
        return;
    };
    for path in [
        instance_dir.join("run/qemu.pid"),
        instance_dir.join("run/installation-viewer.vv"),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                let _ = fs::remove_file(path);
            }
            _ => {}
        }
    }
    let _ = cleanup_runtime_sockets(&instance_dir);
}

fn wait_for_socket(
    socket: &Path,
    helpers: &mut [Child],
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if fs::symlink_metadata(socket)
            .map(|metadata| metadata.file_type().is_socket())
            .unwrap_or(false)
        {
            return Ok(());
        }
        for helper in helpers.iter_mut() {
            if let Some(status) = helper.try_wait()? {
                return Err(format!("helper process exited during startup with {status}").into());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {}", socket.display()).into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn cleanup_runtime_sockets(instance_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for path in [
        instance_dir.join("run/swtpm.sock"),
        instance_dir.join("run/qmp.sock"),
        instance_dir.join("run/recovery-vnc.sock"),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)?,
            Ok(_) => {
                return Err(
                    format!("refusing to replace non-socket path {}", path.display()).into(),
                )
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn wait_or_kill(child: &mut Child, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
    if wait_for_child_exit(child, timeout)?.is_some() {
        return Ok(());
    }
    child.kill()?;
    child.wait()?;
    Ok(())
}

fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(None)
}

fn wait_for_qmp_disconnect(
    socket: &Path,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if QmpClient::connect(socket).is_err() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "timed out waiting for the externally managed QEMU at {} to stop",
        socket.display()
    )
    .into())
}

fn refuse_unproven_external_force_stop(
    force: bool,
    requested_qmp_quit: bool,
    owns_process: bool,
    state: InstanceState,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if force
        && !requested_qmp_quit
        && !owns_process
        && matches!(
            state,
            InstanceState::Running | InstanceState::Installing | InstanceState::Suspended
        )
    {
        return Err(format!(
            "cannot prove active instance {name} stopped: QMP is unavailable and this daemon does not own its QEMU process; verify or terminate QEMU before changing the manifest"
        )
        .into());
    }
    Ok(())
}

fn stop_helpers(helpers: &mut [Child]) {
    for helper in helpers {
        if helper.try_wait().ok().flatten().is_none() {
            let _ = helper.kill();
        }
        let _ = helper.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_socket_cleanup_refuses_regular_files() {
        let root = std::env::temp_dir().join(format!("lsw-supervisor-test-{}", std::process::id()));
        let run = root.join("run");
        fs::create_dir_all(&run).expect("fixture should be created");
        fs::write(run.join("qmp.sock"), b"do not remove").expect("fixture file should be written");
        assert!(cleanup_runtime_sockets(&root).is_err());
        assert_eq!(
            fs::read(run.join("qmp.sock")).expect("fixture should remain"),
            b"do not remove"
        );
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn external_force_stop_requires_proof_that_an_active_vm_stopped() {
        for state in [
            InstanceState::Running,
            InstanceState::Installing,
            InstanceState::Suspended,
        ] {
            assert!(refuse_unproven_external_force_stop(true, false, false, state, "win").is_err());
            assert!(refuse_unproven_external_force_stop(true, true, false, state, "win").is_ok());
            assert!(refuse_unproven_external_force_stop(true, false, true, state, "win").is_ok());
        }
        assert!(refuse_unproven_external_force_stop(
            true,
            false,
            false,
            InstanceState::Stopped,
            "win"
        )
        .is_ok());
    }

    #[test]
    fn requested_shutdown_wins_qemu_exit_status() {
        assert_eq!(
            state_after_qemu_exit(true, false, false),
            InstanceState::Stopped
        );
        assert_eq!(
            state_after_qemu_exit(false, false, true),
            InstanceState::Stopped
        );
        assert_eq!(
            state_after_qemu_exit(false, false, false),
            InstanceState::Failed
        );
        assert_eq!(
            state_after_qemu_exit(false, true, false),
            InstanceState::Hibernated
        );
    }
}
