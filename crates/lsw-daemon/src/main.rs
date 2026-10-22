// SPDX-License-Identifier: GPL-3.0-or-later

#![deny(unsafe_code)]

#[cfg(not(unix))]
compile_error!("lswd 1.0 beta currently requires a Unix host");

mod qmp;
mod runtime_config;
#[allow(unsafe_code)]
mod socket_activation;
mod supervisor;

use std::env;
use std::fs;
use std::io::{self, BufReader, BufWriter};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use lsw_core::{
    read_daemon_request, write_daemon_reply, DaemonInfo, DaemonReply, DaemonRequest,
    DaemonResponse, GuestCpuTuning, HostCapabilities, InstanceSummary, LaunchPhase,
    LaunchPlanSummary, StateStore, DAEMON_PROTOCOL_VERSION,
};
use runtime_config::*;
use supervisor::Supervisor;

const DAEMON_FEATURES: &[&str] = &["suspend", "resume", "hibernate", "balloon", "idle-exit"];
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_IDLE_EXIT: Duration = Duration::from_secs(30);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lswd: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let store = StateStore::new(state_root()?);
    store.initialize()?;
    let socket_path = socket_path(&store);
    let listener = if let Some(listener) = socket_activation::inherited_listener(&socket_path)? {
        validate_socket_path(&socket_path)?;
        listener
    } else {
        prepare_socket_path(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        listener
    };
    listener.set_nonblocking(true)?;
    println!("lswd listening on {}", socket_path.display());

    let mut supervisor = Supervisor::new(store, HostCapabilities::detect(), guest_cpu_tuning()?);
    let idle_exit = daemon_idle_exit()?;
    let mut last_request = std::time::Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                last_request = std::time::Instant::now();
                stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
                stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
                if let Err(error) = handle_connection(stream, &mut supervisor) {
                    eprintln!("lswd: request failed: {error}");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                supervisor.poll();
                if !supervisor.has_active_work() && last_request.elapsed() >= idle_exit {
                    println!("lswd exiting after {} idle seconds", idle_exit.as_secs());
                    return Ok(());
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => eprintln!("lswd: accept failed: {error}"),
        }
    }
}

fn validate_socket_path(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let parent = path
        .parent()
        .ok_or("daemon socket path does not have a parent directory")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    if !parent_metadata.file_type().is_dir() || parent_metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "socket directory {} must be a private directory",
            parent.display()
        )
        .into());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "activated socket {} must be a private Unix socket",
            path.display()
        )
        .into());
    }
    Ok(())
}

fn prepare_socket_path(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let parent = path
        .parent()
        .ok_or("daemon socket path does not have a parent directory")?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    } else if !fs::symlink_metadata(parent)?.file_type().is_dir() {
        return Err(format!("socket parent {} is not a directory", parent.display()).into());
    } else if fs::metadata(parent)?.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "socket directory {} must not be accessible by group or other users",
            parent.display()
        )
        .into());
    }

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if UnixStream::connect(path).is_ok() {
        return Err(format!("another lswd is already listening at {}", path.display()).into());
    }
    if !metadata.file_type().is_socket() {
        return Err(format!("refusing to replace non-socket path {}", path.display()).into());
    }
    fs::remove_file(path)?;
    Ok(())
}

fn handle_connection(
    stream: UnixStream,
    supervisor: &mut Supervisor,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let reply = match read_daemon_request(&mut reader) {
        Ok(Some(request)) => match dispatch(request, supervisor) {
            Ok(response) => DaemonReply::Ok(response),
            Err(error) => DaemonReply::Error(error.to_string()),
        },
        Ok(None) => DaemonReply::Error("empty request".to_owned()),
        Err(error) => DaemonReply::Error(error.to_string()),
    };
    let mut writer = BufWriter::new(stream);
    write_daemon_reply(&mut writer, &reply)?;
    Ok(())
}

fn dispatch(
    request: DaemonRequest,
    supervisor: &mut Supervisor,
) -> Result<DaemonResponse, Box<dyn std::error::Error>> {
    let notice = |lines: Vec<String>| DaemonResponse::Notice { lines };
    match request {
        DaemonRequest::Ping => Ok(DaemonResponse::Pong(DaemonInfo {
            protocol: DAEMON_PROTOCOL_VERSION,
            features: DAEMON_FEATURES
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
            pid: std::process::id(),
            rss_kib: current_rss_kib(),
        })),
        DaemonRequest::List => Ok(DaemonResponse::Instances {
            instances: supervisor
                .store()
                .list()?
                .into_iter()
                .map(|manifest| InstanceSummary {
                    name: manifest.spec.name,
                    state: manifest.state,
                    profile: manifest.spec.profile,
                })
                .collect(),
        }),
        DaemonRequest::Show { name } => Ok(DaemonResponse::Manifest {
            encoded: supervisor.store().load(&name)?.encode()?,
        }),
        DaemonRequest::Plan { name, phase } => plan(supervisor, &name, phase),
        DaemonRequest::Start { name, phase } => supervisor.start(&name, phase),
        DaemonRequest::Status { name } => supervisor.status(&name).map(DaemonResponse::Status),
        DaemonRequest::Suspend { name } => supervisor.suspend(&name).map(notice),
        DaemonRequest::Resume { name } => supervisor.resume(&name),
        DaemonRequest::Hibernate { name } => supervisor.hibernate(&name).map(notice),
        DaemonRequest::Activity { name } => supervisor.activity(&name).map(notice),
        DaemonRequest::Balloon { name, memory_mib } => {
            supervisor.balloon(&name, memory_mib).map(notice)
        }
        DaemonRequest::Stop { name, force } => supervisor.stop(&name, force).map(notice),
    }
}

fn plan(
    supervisor: &Supervisor,
    name: &str,
    phase: LaunchPhase,
) -> Result<DaemonResponse, Box<dyn std::error::Error>> {
    let manifest = supervisor.store().load(name)?;
    let instance_dir = supervisor.store().instance_dir(name)?;
    let plan = supervisor.planner().plan(&manifest, &instance_dir, phase)?;
    Ok(DaemonResponse::Plan(LaunchPlanSummary {
        helper_commands: plan
            .helper_commands
            .iter()
            .map(|helper| helper.display_command())
            .collect(),
        command: plan.display_command(),
        notes: plan.notes,
        missing_capabilities: plan
            .missing_capabilities
            .into_iter()
            .map(str::to_owned)
            .collect(),
    }))
}

#[cfg(test)]
mod tests;
