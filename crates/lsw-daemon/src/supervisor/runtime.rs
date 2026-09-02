// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QemuProcessEvidence {
    Live,
    Gone,
    Unknown,
}

pub(super) fn qemu_process_evidence(
    instance_dir: &Path,
    name: &str,
    owned_by_supervisor: bool,
) -> QemuProcessEvidence {
    if owned_by_supervisor {
        // poll() immediately precedes this check and removes every child for
        // which try_wait returned an exit status. Retaining the map entry is
        // positive evidence that this supervisor still owns a live child.
        return QemuProcessEvidence::Live;
    }

    qemu_process_evidence_with_proc(instance_dir, name, Path::new("/proc"))
}

pub(super) fn qemu_process_evidence_with_proc(
    instance_dir: &Path,
    name: &str,
    proc_root: &Path,
) -> QemuProcessEvidence {
    let pid_path = instance_dir.join("run/qemu.pid");
    let metadata = match fs::symlink_metadata(&pid_path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            metadata
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return scan_exact_qemu_process(proc_root, instance_dir, name)
        }
        Ok(_) | Err(_) => return QemuProcessEvidence::Unknown,
    };
    if metadata.len() == 0 || metadata.len() > 32 {
        return QemuProcessEvidence::Unknown;
    }
    let pid = match fs::read_to_string(&pid_path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|pid| *pid > 1)
    {
        Some(pid) => pid,
        None => return QemuProcessEvidence::Unknown,
    };
    let command_line = match fs::read(proc_root.join(pid.to_string()).join("cmdline")) {
        Ok(command_line) if !command_line.is_empty() => command_line,
        Ok(_) => return QemuProcessEvidence::Gone,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return QemuProcessEvidence::Gone,
        Err(_) => return QemuProcessEvidence::Unknown,
    };
    if qemu_command_matches_instance(&command_line, instance_dir, name) {
        QemuProcessEvidence::Live
    } else {
        // The recorded PID was reused or no longer identifies this exact QEMU.
        QemuProcessEvidence::Gone
    }
}

pub(super) fn scan_exact_qemu_process(
    proc_root: &Path,
    instance_dir: &Path,
    name: &str,
) -> QemuProcessEvidence {
    let entries = match fs::read_dir(proc_root) {
        Ok(entries) => entries,
        Err(_) => return QemuProcessEvidence::Unknown,
    };
    let mut inspection_failed = false;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                inspection_failed = true;
                continue;
            }
        };
        if entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|pid| *pid > 1)
            .is_none()
        {
            continue;
        }
        match fs::read(entry.path().join("cmdline")) {
            Ok(command_line)
                if !command_line.is_empty()
                    && qemu_command_matches_instance(&command_line, instance_dir, name) =>
            {
                return QemuProcessEvidence::Live;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => inspection_failed = true,
        }
    }
    if inspection_failed {
        QemuProcessEvidence::Unknown
    } else {
        QemuProcessEvidence::Gone
    }
}

pub(super) fn qemu_command_matches_instance(
    command_line: &[u8],
    instance_dir: &Path,
    name: &str,
) -> bool {
    let arguments = command_line
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8_lossy(argument))
        .collect::<Vec<_>>();
    let Some(program) = arguments.first() else {
        return false;
    };
    let Some(program_name) = Path::new(program.as_ref())
        .file_name()
        .and_then(|value| value.to_str())
    else {
        return false;
    };
    if !program_name.starts_with("qemu-system-") {
        return false;
    }

    let qmp = format!(
        "unix:{},server=on,wait=off",
        instance_dir.join("run/qmp.sock").display()
    );
    let pid_file = instance_dir.join("run/qemu.pid").display().to_string();
    command_has_exact_option(&arguments, "-name", name)
        && command_has_exact_option(&arguments, "-qmp", &qmp)
        && command_has_exact_option(&arguments, "-pidfile", &pid_file)
}

pub(super) fn command_has_exact_option(
    arguments: &[std::borrow::Cow<'_, str>],
    option: &str,
    value: &str,
) -> bool {
    arguments
        .windows(2)
        .any(|pair| pair[0].as_ref() == option && pair[1].as_ref() == value)
}

pub(super) fn spawn_command(
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

pub(super) fn append_log(path: PathBuf) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

pub(super) fn write_shutdown_marker(instance_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn write_hibernate_marker(
    instance_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    write_private_marker(&instance_dir.join("run/hibernate.requested"), "requested")
}

pub(super) fn write_private_marker(
    path: &Path,
    value: &str,
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn shutdown_was_requested(store: &StateStore, name: &str) -> bool {
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

pub(super) fn hibernate_was_requested(store: &StateStore, name: &str) -> bool {
    requested_marker_exists(store, name, "hibernate.requested")
}

pub(super) fn requested_marker_exists(store: &StateStore, name: &str, file: &str) -> bool {
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

pub(super) fn state_after_qemu_exit(
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

pub(super) fn remove_shutdown_marker(store: &StateStore, name: &str) {
    remove_requested_marker(store, name, "shutdown.requested");
}

pub(super) fn remove_hibernate_marker(store: &StateStore, name: &str) {
    remove_requested_marker(store, name, "hibernate.requested");
}

pub(super) fn remove_requested_marker(store: &StateStore, name: &str, file: &str) {
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

pub(super) fn read_activity(store: &StateStore, name: &str) -> Option<u64> {
    let path = store.instance_dir(name).ok()?.join("run/last-activity");
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub(super) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub(super) fn host_memory_pressure() -> bool {
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

pub(super) fn request_guest_hibernate(
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

pub(super) fn cleanup_stopped_runtime_artifacts(store: &StateStore, name: &str) {
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

pub(super) fn wait_for_socket(
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

pub(super) fn cleanup_runtime_sockets(
    instance_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn wait_or_kill(
    child: &mut Child,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    if wait_for_child_exit(child, timeout)?.is_some() {
        return Ok(());
    }
    child.kill()?;
    child.wait()?;
    Ok(())
}

pub(super) fn wait_for_child_exit(
    child: &mut Child,
    timeout: Duration,
) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(None)
}

pub(super) fn wait_for_qmp_disconnect(
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

pub(super) fn refuse_unproven_external_force_stop(
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

pub(super) fn stop_helpers(helpers: &mut [Child]) {
    for helper in helpers {
        if helper.try_wait().ok().flatten().is_none() {
            let _ = helper.kill();
        }
        let _ = helper.wait();
    }
}
