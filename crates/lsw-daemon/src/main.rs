// SPDX-License-Identifier: GPL-3.0-or-later

#![deny(unsafe_code)]

#[cfg(not(unix))]
compile_error!("lswd 1.0 beta currently requires a Unix host");

mod qmp;
#[allow(unsafe_code)]
mod socket_activation;
mod supervisor;

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use lsw_core::{HostCapabilities, LaunchPhase, QemuPlanner, StateStore, DAEMON_PROTOCOL_VERSION};
use supervisor::Supervisor;

const MAX_REQUEST_BYTES: u64 = 4096;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

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

    let mut supervisor = Supervisor::new(store, HostCapabilities::detect());
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
                stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
                if let Err(error) = handle_connection(stream, &mut supervisor) {
                    eprintln!("lswd: request failed: {error}");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                supervisor.poll();
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
    let mut request = String::new();
    let reader = BufReader::new(stream.try_clone()?);
    let bytes_read = reader.take(MAX_REQUEST_BYTES + 1).read_line(&mut request)?;

    let response = if bytes_read == 0 {
        Err("empty request".to_owned())
    } else if bytes_read as u64 > MAX_REQUEST_BYTES || !request.ends_with('\n') {
        Err("request is too large or is not newline-terminated".to_owned())
    } else {
        dispatch(request.trim_end_matches(&['\r', '\n'][..]), supervisor)
            .map_err(|error| error.to_string())
    };

    let mut writer = BufWriter::new(stream);
    match response {
        Ok(lines) => {
            writer.write_all(b"OK\n")?;
            for line in lines {
                writeln!(writer, "{}", escape_line(&line))?;
            }
        }
        Err(error) => writeln!(writer, "ERR {}", escape_line(&error))?,
    }
    writer.write_all(b".\n")?;
    writer.flush()?;
    Ok(())
}

fn dispatch(
    request: &str,
    supervisor: &mut Supervisor,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let parts = request.split_ascii_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["PING"] => Ok(vec![
            "PONG".to_owned(),
            format!("PROTOCOL={DAEMON_PROTOCOL_VERSION}"),
            "FEATURES=suspend,resume".to_owned(),
        ]),
        ["LIST"] => Ok(supervisor
            .store()
            .list()?
            .into_iter()
            .map(|manifest| {
                format!(
                    "INSTANCE\t{}\t{}\t{}",
                    manifest.spec.name, manifest.state, manifest.spec.profile
                )
            })
            .collect()),
        ["SHOW", name] => Ok(supervisor
            .store()
            .load(name)?
            .encode()?
            .lines()
            .map(str::to_owned)
            .collect()),
        ["PLAN", name, phase] => plan(supervisor, name, phase),
        ["START", name, phase] => supervisor.start(name, parse_phase(phase)?),
        ["STATUS", name] => supervisor.status(name),
        ["SUSPEND", name] => supervisor.suspend(name),
        ["RESUME", name] => supervisor.resume(name),
        ["STOP", name, "graceful"] => supervisor.stop(name, false),
        ["STOP", name, "force"] => supervisor.stop(name, true),
        [] => Err("empty request".into()),
        _ => Err(
            "unknown request; expected PING, LIST, SHOW, PLAN, START, STATUS, SUSPEND, RESUME, or STOP"
                .into(),
        ),
    }
}

fn plan(
    supervisor: &Supervisor,
    name: &str,
    phase: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let phase = parse_phase(phase)?;
    let manifest = supervisor.store().load(name)?;
    let instance_dir = supervisor.store().instance_dir(name)?;
    let plan =
        QemuPlanner::new(HostCapabilities::detect()).plan(&manifest, &instance_dir, phase)?;
    let mut response = plan
        .helper_commands
        .iter()
        .map(|helper| format!("HELPER\t{}", helper.display_command()))
        .collect::<Vec<_>>();
    response.push(format!("COMMAND\t{}", plan.display_command()));
    response.extend(plan.notes.into_iter().map(|note| format!("NOTE\t{note}")));
    response.extend(
        plan.missing_capabilities
            .into_iter()
            .map(|missing| format!("MISSING\t{missing}")),
    );
    Ok(response)
}

fn parse_phase(phase: &str) -> Result<LaunchPhase, Box<dyn std::error::Error>> {
    match phase {
        "install" => Ok(LaunchPhase::Install),
        "run" => Ok(LaunchPhase::Run),
        _ => Err("phase must be install or run".into()),
    }
}

fn escape_line(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\t', "%09")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn state_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(configured) = env::var_os("LSW_STATE_DIR") {
        return Ok(PathBuf::from(configured));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set; configure LSW_STATE_DIR")?;
    Ok(PathBuf::from(home).join(".local/share/lsw"))
}

fn socket_path(store: &StateStore) -> PathBuf {
    if let Some(configured) = env::var_os("LSW_DAEMON_SOCKET") {
        return PathBuf::from(configured);
    }
    store.root().join("run/lswd.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supervisor() -> Supervisor {
        Supervisor::new(
            StateStore::new(std::env::temp_dir().join("lswd-dispatch-test")),
            HostCapabilities::unavailable(lsw_core::HostPlatform::Linux),
        )
    }

    #[test]
    fn ping_protocol_is_versioned() {
        assert_eq!(
            dispatch("PING", &mut supervisor()).expect("ping should work"),
            vec![
                "PONG".to_owned(),
                format!("PROTOCOL={DAEMON_PROTOCOL_VERSION}"),
                "FEATURES=suspend,resume".to_owned()
            ]
        );
    }

    #[test]
    fn line_escaping_preserves_protocol_boundaries() {
        assert_eq!(escape_line("one\ntwo%"), "one%0Atwo%25");
    }

    #[test]
    fn mutating_commands_are_strictly_parsed() {
        assert!(dispatch("STOP everything now", &mut supervisor()).is_err());
        assert!(dispatch("START x invalid", &mut supervisor()).is_err());
        assert!(dispatch("SUSPEND x now", &mut supervisor()).is_err());
        assert!(dispatch("RESUME", &mut supervisor()).is_err());
    }
}
