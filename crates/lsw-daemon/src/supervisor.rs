// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    CommandInvocation, HostCapabilities, InstanceState, LaunchPhase, Provisioner, QemuPlanner,
    StateStore, WindowsProfile,
};

use crate::qmp::QmpClient;

const START_TIMEOUT: Duration = Duration::from_secs(8);
const HELPER_TIMEOUT: Duration = Duration::from_secs(4);
const FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(4);

struct ManagedVm {
    qemu: Child,
    helpers: Vec<Child>,
}

pub struct Supervisor {
    store: StateStore,
    capabilities: HostCapabilities,
    processes: BTreeMap<String, ManagedVm>,
}

impl Supervisor {
    pub fn new(store: StateStore, capabilities: HostCapabilities) -> Self {
        Self {
            store,
            capabilities,
            processes: BTreeMap::new(),
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
                if let Some(mut managed) = self.processes.remove(&name) {
                    stop_helpers(&mut managed.helpers);
                    remove_shutdown_marker(&self.store, &name);
                    self.remove_ephemeral_overlay(&name);
                    let next = if status.success() {
                        InstanceState::Stopped
                    } else {
                        InstanceState::Failed
                    };
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
                if let Some(mut managed) = self.processes.remove(&name) {
                    let _ = managed.qemu.kill();
                    let _ = managed.qemu.wait();
                    stop_helpers(&mut managed.helpers);
                }
                remove_shutdown_marker(&self.store, &name);
                self.remove_ephemeral_overlay(&name);
                if let Err(error) = self.set_state(&name, InstanceState::Failed) {
                    eprintln!("lswd: could not mark {name:?} failed after helper exit: {error}");
                }
            }
        }
    }

    pub fn start(
        &mut self,
        name: &str,
        phase: LaunchPhase,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.poll();
        if self.qmp_status(name).is_ok() {
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
        cleanup_runtime_sockets(&instance_dir)?;
        remove_shutdown_marker(&self.store, name);
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
        self.store.load(name)?;
        let qmp_path = instance_dir.join("run/qmp.sock");

        let mut requested_qmp_quit = false;
        match QmpClient::connect(&qmp_path) {
            Ok(mut qmp) if force => {
                qmp.quit()?;
                requested_qmp_quit = true;
            }
            Ok(mut qmp) => {
                qmp.system_powerdown()?;
                write_shutdown_marker(&instance_dir)?;
                return Ok(vec![format!("graceful shutdown requested for {name}")]);
            }
            Err(error) if !force => {
                return Err(format!("cannot request a graceful shutdown: {error}").into())
            }
            Err(_) => {}
        }

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
        self.remove_ephemeral_overlay(name);
        Ok(vec![format!("instance {name} stopped")])
    }

    pub fn status(&mut self, name: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.poll();
        let manifest = self.store.load(name)?;
        match self.qmp_status(name) {
            Ok(qmp) => Ok(vec![
                format!("STATE={}", manifest.state),
                format!("QMP={qmp}"),
                "ACTIVE=true".to_owned(),
            ]),
            Err(_) => {
                let state = if matches!(
                    manifest.state,
                    InstanceState::Running | InstanceState::Installing
                ) {
                    let requested = self
                        .store
                        .instance_dir(name)?
                        .join("run/shutdown.requested")
                        .is_file();
                    let state = if requested {
                        InstanceState::Stopped
                    } else {
                        InstanceState::Failed
                    };
                    self.set_state(name, state)?;
                    remove_shutdown_marker(&self.store, name);
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
        self.store.update(&manifest)?;
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

fn remove_shutdown_marker(store: &StateStore, name: &str) {
    let Ok(path) = store
        .instance_dir(name)
        .map(|directory| directory.join("run/shutdown.requested"))
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
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    child.kill()?;
    child.wait()?;
    Ok(())
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
}
