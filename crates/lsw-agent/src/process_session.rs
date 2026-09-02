// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[cfg(windows)]
pub(super) fn hibernate_guest(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !payload.is_empty() {
        send_error(&mut stream, "POWER_HIBERNATE payload must be empty")?;
        return Err("client sent an invalid hibernate request".into());
    }
    if let Err(error) = forward_maintenance_operation(
        expected_token,
        FrameKind::MaintenanceHibernate,
        CAPABILITY_MAINTENANCE_HIBERNATE_V1,
    ) {
        send_error(&mut stream, &error.to_string())?;
        return Err(error);
    }
    write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
    Ok(())
}

#[cfg(not(windows))]
pub(super) fn hibernate_guest(
    mut stream: TcpStream,
    payload: &[u8],
    _expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !payload.is_empty() {
        send_error(&mut stream, "POWER_HIBERNATE payload must be empty")?;
    } else {
        send_error(&mut stream, "hibernation is available only on Windows")?;
    }
    Err("client requested hibernation from a non-Windows agent".into())
}

pub(super) fn run_process_request(
    mut stream: TcpStream,
    payload: &[u8],
    session_mode: SessionMode,
    environment: &ProcessEnvironment,
    detached: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = StartRequest::decode(payload)?;
    if detached && request.kind != SessionKind::Run {
        send_error(&mut stream, "detached mode requires a RUN request")?;
        return Err("client requested detached mode for a non-run session".into());
    }
    let mut child = match spawn_request(&request, environment, detached) {
        Ok(child) => child,
        Err(error) => {
            send_error(&mut stream, &format!("could not start process: {error}"))?;
            return Err(error);
        }
    };

    if detached {
        let process_id = child.id();
        let reaper = thread::Builder::new()
            .name(format!("lsw-agent-detached-{process_id}"))
            .spawn(move || {
                if let Err(error) = child.wait_and_cleanup() {
                    write_stderr(format_args!(
                        "lsw-agent: detached process {process_id} cleanup failed: {error}"
                    ));
                }
            });
        if let Err(error) = reaper {
            send_error(
                &mut stream,
                &format!("could not retain detached process: {error}"),
            )?;
            return Err(error.into());
        }
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Started, encode_process_id(process_id)),
        )?;
        return Ok(());
    }
    bridge_process(&mut child, stream, session_mode)?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn run_terminal_request(
    mut stream: TcpStream,
    payload: &[u8],
    session_mode: SessionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let terminal_request = TerminalStartRequest::decode(payload)?;
    if terminal_request.request.kind != SessionKind::Shell {
        send_error(
            &mut stream,
            "ConPTY is currently supported only for shell sessions",
        )?;
        return Err("client requested ConPTY for a non-shell session".into());
    }
    let process =
        match windows_conpty::spawn_shell(&terminal_request.request, terminal_request.size) {
            Ok(process) => process,
            Err(error) => {
                send_error(
                    &mut stream,
                    &format!("could not start ConPTY shell: {error}"),
                )?;
                return Err(error.into());
            }
        };
    bridge_terminal(process, stream, session_mode)
}

#[cfg(not(windows))]
pub(super) fn run_terminal_request(
    mut stream: TcpStream,
    payload: &[u8],
    _session_mode: SessionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    TerminalStartRequest::decode(payload)?;
    send_error(
        &mut stream,
        "ConPTY is available only from a Windows guest agent",
    )?;
    Err("client requested ConPTY from a non-Windows agent".into())
}

pub(super) fn receive_file(
    mut stream: TcpStream,
    payload: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let request = FilePutRequest::decode(payload)?;
    let destination = PathBuf::from(&request.destination);
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_no_link_boundary(parent)?;
    if !parent.is_dir() {
        send_error(&mut stream, "destination parent directory does not exist")?;
        return Err("destination parent directory does not exist".into());
    }
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            send_error(
                &mut stream,
                "destination already exists; overwrite is disabled",
            )?;
            return Err("destination already exists; overwrite is disabled".into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let temporary = upload_temporary_path(&destination)?;
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        #[cfg(windows)]
        let mut file = windows_path::UploadFile::create(&temporary).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not create the atomic upload staging file: {error}"),
            )
        })?;
        #[cfg(not(windows))]
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
        let mut received = 0_u64;
        loop {
            let frame = read_frame(&mut stream)?;
            match frame.kind {
                FrameKind::FileData => {
                    received = received
                        .checked_add(frame.payload.len() as u64)
                        .ok_or("received file length overflowed")?;
                    if received > request.length {
                        return Err("received more file data than declared".into());
                    }
                    file.write_all(&frame.payload)?;
                }
                FrameKind::FileDone => {
                    let declared = decode_file_length(&frame.payload)?;
                    if declared != request.length || received != request.length {
                        return Err(format!(
                            "file length mismatch: expected {}, received {received}",
                            request.length
                        )
                        .into());
                    }
                    file.flush()?;
                    file.sync_all().map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!("could not flush the atomic upload staging file: {error}"),
                        )
                    })?;
                    #[cfg(windows)]
                    file.publish_new(&destination).map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!("could not publish the atomic upload: {error}"),
                        )
                    })?;
                    #[cfg(not(windows))]
                    {
                        drop(file);
                        fs::hard_link(&temporary, &destination)?;
                        fs::remove_file(&temporary)?;
                    }
                    write_frame(
                        &mut stream,
                        &Frame::new(FrameKind::FileDone, encode_file_length(received)),
                    )?;
                    return Ok(());
                }
                _ => return Err("unexpected frame during file upload".into()),
            }
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    if let Err(error) = &result {
        let _ = send_error(&mut stream, &error.to_string());
    }
    result
}

pub(super) fn send_file(
    mut stream: TcpStream,
    payload: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let request = FileGetRequest::decode(payload)?;
    let source = PathBuf::from(request.source);
    ensure_no_link_boundary(&source)?;
    let metadata = match fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            metadata
        }
        Ok(_) => {
            send_error(&mut stream, "source is not a regular file")?;
            return Err("source is not a regular file".into());
        }
        Err(error) => {
            send_error(&mut stream, &format!("could not open source: {error}"))?;
            return Err(error.into());
        }
    };
    let mut file = fs::File::open(source)?;
    let mut sent = 0_u64;
    let mut buffer = [0_u8; STREAM_CHUNK_BYTES];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::FileData, buffer[..length].to_vec()),
        )?;
        sent += length as u64;
    }
    if sent != metadata.len() {
        send_error(&mut stream, "source changed while it was being read")?;
        return Err("source changed while it was being read".into());
    }
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::FileDone, encode_file_length(sent)),
    )?;
    Ok(())
}

pub(super) fn ensure_no_link_boundary(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    windows_path::ensure_no_reparse_components(path)?;
    #[cfg(not(windows))]
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "transfer path crosses a symbolic link: {}",
                    ancestor.display()
                )
                .into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn upload_temporary_path(
    destination: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let file_name = destination
        .file_name()
        .ok_or("file destination has no file name")?
        .to_string_lossy();
    Ok(destination.with_file_name(format!(
        ".{file_name}.lsw-upload-{}-{nonce}",
        std::process::id()
    )))
}

/// A process whose ordinary descendants share one agent-session owner.
///
/// The platform owner is established before user code can run: Unix children
/// enter a fresh process group in `Command`'s pre-exec path, while Windows
/// children start suspended and are resumed only after assignment to a Job
/// Object. Unix process groups are lifecycle bookkeeping, not a sandbox: a
/// process with sufficient permission can deliberately leave its group. Tree
/// teardown also happens after a normal leader exit, because an ordinary
/// descendant may otherwise keep the session's stdout/stderr pipes open.
pub(super) struct SessionChild {
    pub(super) process: Child,
    pub(super) tree: Option<process_tree::Owner>,
}

impl SessionChild {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        let prepared = process_tree::Prepared::new(command)?;
        let mut process = command.spawn()?;
        match prepared.attach_and_start(&process) {
            Ok(tree) => Ok(Self {
                process,
                tree: Some(tree),
            }),
            Err(error) => {
                // A Windows child is still suspended if Job assignment or
                // primary-thread resume failed. Do not leave it behind.
                let _ = process.kill();
                let _ = process.wait();
                Err(error)
            }
        }
    }

    pub(super) fn id(&self) -> u32 {
        self.process.id()
    }

    pub(super) fn try_wait_and_cleanup(&mut self) -> io::Result<Option<ExitStatus>> {
        let Some(status) = self.process.try_wait()? else {
            return Ok(None);
        };
        self.terminate_tree(SESSION_CANCEL_EXIT_CODE)?;
        Ok(Some(status))
    }

    pub(super) fn wait_and_cleanup(&mut self) -> io::Result<ExitStatus> {
        let status = self.process.wait()?;
        self.terminate_tree(SESSION_CANCEL_EXIT_CODE)?;
        Ok(status)
    }

    /// Returns the leader status and whether this call won the race to issue
    /// process-tree termination.
    pub(super) fn terminate(&mut self) -> io::Result<(ExitStatus, bool)> {
        self.terminate_with(SESSION_CANCEL_EXIT_CODE)
    }

    pub(super) fn terminate_with(&mut self, exit_code: i32) -> io::Result<(ExitStatus, bool)> {
        if let Some(status) = self.process.try_wait()? {
            self.terminate_tree(exit_code)?;
            return Ok((status, false));
        }
        self.terminate_tree(exit_code)?;
        Ok((self.process.wait()?, true))
    }

    /// Process-tree ownership is one-shot. A Unix process-group id may be
    /// reused after its last member exits, so a later signal from Drop could
    /// otherwise target an unrelated group. A successful cleanup disarms it;
    /// a failed cleanup restores it only so Drop can make one best-effort retry.
    pub(super) fn terminate_tree(&mut self, exit_code: i32) -> io::Result<()> {
        let Some(tree) = self.tree.take() else {
            return Ok(());
        };
        match tree.terminate(exit_code) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.tree = Some(tree);
                Err(error)
            }
        }
    }

    #[cfg(all(test, unix))]
    pub(super) fn kill(&mut self) -> io::Result<()> {
        self.terminate_tree(SESSION_CANCEL_EXIT_CODE)
    }

    #[cfg(all(test, unix))]
    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_and_cleanup()
    }
}

impl Drop for SessionChild {
    fn drop(&mut self) {
        // This guard covers bridge setup errors and panics. Explicit lifecycle
        // paths report failures; Drop is deliberately best-effort. Once the
        // owner is disarmed an explicit path has already terminated the group;
        // do not signal the leader PID again because Unix may have reused it.
        if self.tree.is_none() {
            return;
        }
        if self.terminate_tree(SESSION_CANCEL_EXIT_CODE).is_ok() {
            let _ = self.process.wait();
        }
    }
}
