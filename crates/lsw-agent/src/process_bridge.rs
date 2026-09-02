// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) fn spawn_request(
    request: &StartRequest,
    environment: &ProcessEnvironment,
    detached: bool,
) -> Result<SessionChild, Box<dyn std::error::Error>> {
    match request.kind {
        SessionKind::Shell => spawn_shell(request, environment),
        SessionKind::Exec | SessionKind::Run => {
            let (program, arguments) = request
                .argv
                .split_first()
                .ok_or("command request did not contain a program")?;
            spawn_program(
                program,
                arguments,
                request.working_directory.as_deref(),
                environment,
                detached,
            )
            .map_err(Into::into)
        }
    }
}

pub(super) fn spawn_shell(
    request: &StartRequest,
    environment: &ProcessEnvironment,
) -> Result<SessionChild, Box<dyn std::error::Error>> {
    let mut last_not_found = None;
    for candidate in &request.argv {
        let shell_arguments = match candidate.to_ascii_lowercase().as_str() {
            "pwsh" | "pwsh.exe" => &["-NoLogo"][..],
            "powershell" | "powershell.exe" => &["-NoLogo"][..],
            "cmd" | "cmd.exe" => &["/Q"][..],
            _ => &[][..],
        };
        match spawn_program(
            candidate,
            shell_arguments,
            request.working_directory.as_deref(),
            environment,
            false,
        ) {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == io::ErrorKind::NotFound => last_not_found = Some(error),
            Err(error) => return Err(error.into()),
        }
    }
    Err(last_not_found
        .map(|error| error.into())
        .unwrap_or_else(|| "no shell candidate was supplied".into()))
}

pub(super) fn spawn_program(
    program: &str,
    arguments: &[impl AsRef<std::ffi::OsStr>],
    working_directory: Option<&str>,
    environment: &ProcessEnvironment,
    detached: bool,
) -> io::Result<SessionChild> {
    let mut command = Command::new(program);
    command.args(arguments).envs(
        environment
            .variables
            .iter()
            .map(|(name, value)| (name, value)),
    );
    if detached {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    } else {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }
    SessionChild::spawn(&mut command)
}

pub(super) fn bridge_process(
    child: &mut SessionChild,
    stream: TcpStream,
    session_mode: SessionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let child_stdin = child
        .process
        .stdin
        .take()
        .ok_or("child stdin was not piped")?;
    let child_stdout = child
        .process
        .stdout
        .take()
        .ok_or("child stdout was not piped")?;
    let child_stderr = child
        .process
        .stderr
        .take()
        .ok_or("child stderr was not piped")?;
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let input_shutdown = stream.try_clone()?;
    // A one-event queue applies backpressure to authenticated heartbeat floods
    // without allowing them to delay cancellation by an unbounded amount.
    let (control_sender, control_receiver) = mpsc::sync_channel(1);

    let stdout_thread = spawn_output_bridge(child_stdout, Arc::clone(&writer), FrameKind::Stdout);
    let stderr_thread = spawn_output_bridge(child_stderr, Arc::clone(&writer), FrameKind::Stderr);
    let input_thread = spawn_input_bridge(stream, child_stdin, session_mode, control_sender);

    let (status, session_end) = wait_for_child(child, &control_receiver, session_mode)?;
    drop(control_receiver);
    let peer_unavailable = matches!(
        session_end,
        SessionEnd::Disconnected | SessionEnd::LeaseExpired
    );
    if peer_unavailable {
        let _ = input_shutdown.shutdown(Shutdown::Both);
    }
    let result = (|| {
        join_bridge(stdout_thread, peer_unavailable)?;
        join_bridge(stderr_thread, peer_unavailable)?;
        finish_session(&writer, session_end, status.code().unwrap_or(255))
    })();
    // On Windows, shutting down only the read half of one duplicated socket
    // handle does not reliably wake a blocking recv on another handle. Send
    // the final output/EXIT first, then close both directions so the input
    // bridge cannot retain a completed session until its next heartbeat.
    let _ = input_shutdown.shutdown(Shutdown::Both);
    let input_result = join_input_bridge(input_thread);
    result?;
    input_result
}

#[cfg(windows)]
pub(super) fn bridge_terminal(
    process: windows_conpty::ConPtyProcess,
    stream: TcpStream,
    session_mode: SessionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let windows_conpty::ConPtyProcess {
        process,
        input,
        output,
        console,
        job,
    } = process;
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let input_shutdown = stream.try_clone()?;
    let (control_sender, control_receiver) = mpsc::sync_channel(1);
    let output_thread = spawn_output_bridge(output, Arc::clone(&writer), FrameKind::Stdout);
    let input_thread = spawn_terminal_input_bridge(
        stream,
        input,
        Arc::downgrade(&console),
        session_mode,
        control_sender,
    );

    let (code, session_end) =
        wait_for_terminal_process(&process, &job, &control_receiver, session_mode)?;
    drop(control_receiver);
    let peer_unavailable = matches!(
        session_end,
        SessionEnd::Disconnected | SessionEnd::LeaseExpired
    );
    drop(console);
    if peer_unavailable {
        let _ = input_shutdown.shutdown(Shutdown::Both);
    }
    let result = (|| {
        join_bridge(output_thread, peer_unavailable)?;
        finish_session(&writer, session_end, code)
    })();
    // A Weak console reference lets normal output drain before EXIT while the
    // socket remains writable. Close both directions only after the terminal
    // completion frame so a blocking recv on a duplicated Winsock handle wakes
    // reliably without truncating the protocol.
    let _ = input_shutdown.shutdown(Shutdown::Both);
    let input_result = join_terminal_input(input_thread);
    result?;
    input_result
}

pub(super) fn wait_for_child(
    child: &mut SessionChild,
    controls: &Receiver<SessionControlEvent>,
    session_mode: SessionMode,
) -> Result<(ExitStatus, SessionEnd), Box<dyn std::error::Error>> {
    let mut lease = session_mode.lease().map(SessionLeaseMonitor::new);
    let mut controls_connected = true;
    loop {
        if let Some(status) = child.try_wait_and_cleanup()? {
            return Ok((status, SessionEnd::Normal));
        }
        let wait_duration = lease.as_ref().map_or(PROCESS_POLL_INTERVAL, |lease| {
            lease.wait_duration(Instant::now())
        });
        if !controls_connected {
            thread::sleep(wait_duration);
            if lease
                .as_ref()
                .is_some_and(|lease| lease.is_expired(Instant::now()))
            {
                let (status, terminated) = terminate_child(child)?;
                return Ok((status, lease_session_end(terminated)));
            }
            continue;
        }
        match controls.recv_timeout(wait_duration) {
            Ok(SessionControlEvent::Cancel) => {
                let (status, terminated) = terminate_child(child)?;
                return Ok((status, cancel_session_end(terminated)));
            }
            Ok(SessionControlEvent::Signal(signal)) => {
                let (status, terminated) = child.terminate_with(signal.exit_code())?;
                let end = if terminated {
                    SessionEnd::Signalled(signal.exit_code())
                } else {
                    SessionEnd::Normal
                };
                return Ok((status, end));
            }
            Ok(SessionControlEvent::Disconnect) => {
                return Ok((terminate_child(child)?.0, SessionEnd::Disconnected))
            }
            Ok(SessionControlEvent::ProtocolError(message)) => {
                return Ok((
                    terminate_child(child)?.0,
                    SessionEnd::ProtocolError(message),
                ))
            }
            Ok(SessionControlEvent::Heartbeat(observed_at)) => {
                let Some(lease) = lease.as_mut() else {
                    return Ok((
                        terminate_child(child)?.0,
                        SessionEnd::ProtocolError(
                            "heartbeat was reported for a session without a lease".to_owned(),
                        ),
                    ));
                };
                if !lease.observe_heartbeat(observed_at) {
                    let (status, terminated) = terminate_child(child)?;
                    return Ok((status, lease_session_end(terminated)));
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if lease
                    .as_ref()
                    .is_some_and(|lease| lease.is_expired(Instant::now()))
                {
                    let (status, terminated) = terminate_child(child)?;
                    return Ok((status, lease_session_end(terminated)));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if lease.is_none() {
                    return Ok((child.wait_and_cleanup()?, SessionEnd::Normal));
                }
                controls_connected = false;
            }
        }
    }
}

pub(super) fn cancel_session_end(process_was_terminated: bool) -> SessionEnd {
    if process_was_terminated {
        SessionEnd::Cancelled
    } else {
        SessionEnd::Normal
    }
}

pub(super) fn lease_session_end(process_was_terminated: bool) -> SessionEnd {
    if process_was_terminated {
        SessionEnd::LeaseExpired
    } else {
        SessionEnd::Normal
    }
}

/// Returns the exit status and whether this call actually issued a kill.
pub(super) fn terminate_child(child: &mut SessionChild) -> io::Result<(ExitStatus, bool)> {
    child.terminate()
}

#[cfg(windows)]
pub(super) fn wait_for_terminal_process(
    process: &std::os::windows::io::OwnedHandle,
    job: &process_tree::Job,
    controls: &Receiver<SessionControlEvent>,
    session_mode: SessionMode,
) -> Result<(i32, SessionEnd), Box<dyn std::error::Error>> {
    let mut lease = session_mode.lease().map(SessionLeaseMonitor::new);
    let mut controls_connected = true;
    loop {
        let wait_duration = lease.as_ref().map_or(PROCESS_POLL_INTERVAL, |lease| {
            lease.wait_duration(Instant::now())
        });
        let poll_milliseconds = u32::try_from(wait_duration.as_millis())
            .expect("the process poll interval fits in a u32");
        if let Some(code) = windows_conpty::wait_for_process_timeout(process, poll_milliseconds)? {
            job.terminate(SESSION_CANCEL_EXIT_CODE)?;
            return Ok((code, SessionEnd::Normal));
        }
        if !controls_connected {
            if lease
                .as_ref()
                .is_some_and(|lease| lease.is_expired(Instant::now()))
            {
                if let Some(code) = windows_conpty::wait_for_process_timeout(process, 0)? {
                    job.terminate(SESSION_CANCEL_EXIT_CODE)?;
                    return Ok((code, SessionEnd::Normal));
                }
                job.terminate(SESSION_CANCEL_EXIT_CODE)?;
                return Ok((
                    windows_conpty::wait_for_process(process)?,
                    SessionEnd::LeaseExpired,
                ));
            }
            continue;
        }
        let session_end = match controls.try_recv() {
            Ok(SessionControlEvent::Cancel) => SessionEnd::Cancelled,
            Ok(SessionControlEvent::Signal(signal)) => SessionEnd::Signalled(signal.exit_code()),
            Ok(SessionControlEvent::Disconnect) => SessionEnd::Disconnected,
            Ok(SessionControlEvent::ProtocolError(message)) => SessionEnd::ProtocolError(message),
            Ok(SessionControlEvent::Heartbeat(observed_at)) => {
                let Some(lease) = lease.as_mut() else {
                    return Err("heartbeat was reported for a session without a lease".into());
                };
                if lease.observe_heartbeat(observed_at) {
                    continue;
                }
                SessionEnd::LeaseExpired
            }
            Err(mpsc::TryRecvError::Empty) => {
                if lease
                    .as_ref()
                    .is_some_and(|lease| lease.is_expired(Instant::now()))
                {
                    SessionEnd::LeaseExpired
                } else {
                    continue;
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                if lease.is_none() {
                    let code = windows_conpty::wait_for_process(process)?;
                    job.terminate(SESSION_CANCEL_EXIT_CODE)?;
                    return Ok((code, SessionEnd::Normal));
                }
                controls_connected = false;
                continue;
            }
        };
        if let Some(code) = windows_conpty::wait_for_process_timeout(process, 0)? {
            job.terminate(SESSION_CANCEL_EXIT_CODE)?;
            return Ok((code, SessionEnd::Normal));
        }
        let termination_code = match session_end {
            SessionEnd::Signalled(code) => code,
            _ => SESSION_CANCEL_EXIT_CODE,
        };
        if let Err(error) = job.terminate(termination_code) {
            if let Some(code) = windows_conpty::wait_for_process_timeout(process, 0)? {
                return Ok((code, SessionEnd::Normal));
            }
            return Err(error.into());
        }
        return Ok((windows_conpty::wait_for_process(process)?, session_end));
    }
}

pub(super) fn spawn_output_bridge(
    mut input: impl Read + Send + 'static,
    writer: Arc<Mutex<TcpStream>>,
    kind: FrameKind,
) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || {
        let mut buffer = [0_u8; STREAM_CHUNK_BYTES];
        loop {
            let length = input.read(&mut buffer).map_err(|error| error.to_string())?;
            if length == 0 {
                return Ok(());
            }
            send_shared(&writer, &Frame::new(kind, buffer[..length].to_vec()))
                .map_err(|error| error.to_string())?;
        }
    })
}

pub(super) fn spawn_input_bridge(
    mut stream: TcpStream,
    child_stdin: impl Write + Send + 'static,
    session_mode: SessionMode,
    control_sender: SyncSender<SessionControlEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut child_stdin = Some(child_stdin);
        loop {
            match read_frame(&mut stream) {
                Ok(frame) if frame.kind == FrameKind::Stdin => {
                    let Some(stdin) = child_stdin.as_mut() else {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "STDIN was sent after STDIN_CLOSE",
                        );
                        return;
                    };
                    if let Err(error) = stdin.write_all(&frame.payload).and_then(|()| stdin.flush())
                    {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            &format!("could not write child stdin: {error}"),
                        );
                        return;
                    }
                }
                Ok(frame) if frame.kind == FrameKind::Resize => {
                    // Pipe sessions cannot resize. ConPTY-capable agents will negotiate
                    // a separate capability before acting on this frame.
                    if let Err(error) = decode_resize(&frame.payload) {
                        report_protocol_error(&control_sender, session_mode, &error.to_string());
                        return;
                    }
                }
                Ok(frame) if frame.kind == FrameKind::StdinClose => {
                    if !session_mode.is_controlled() || !frame.payload.is_empty() {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "STDIN_CLOSE requires a controlled session and an empty payload",
                        );
                        return;
                    }
                    if child_stdin.take().is_none() {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "STDIN_CLOSE was sent more than once",
                        );
                        return;
                    }
                }
                Ok(frame) if frame.kind == FrameKind::SessionCancel => {
                    if !session_mode.is_controlled() || !frame.payload.is_empty() {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "SESSION_CANCEL requires a controlled session and an empty payload",
                        );
                        return;
                    }
                    drop(child_stdin.take());
                    let _ = control_sender.send(SessionControlEvent::Cancel);
                    return;
                }
                Ok(frame) if frame.kind == FrameKind::SessionSignal => {
                    if !session_mode.is_controlled() {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "SESSION_SIGNAL requires a controlled session",
                        );
                        return;
                    }
                    let signal = match SessionSignal::decode(&frame.payload) {
                        Ok(signal) => signal,
                        Err(error) => {
                            report_protocol_error(
                                &control_sender,
                                session_mode,
                                &error.to_string(),
                            );
                            return;
                        }
                    };
                    drop(child_stdin.take());
                    let _ = control_sender.send(SessionControlEvent::Signal(signal));
                    return;
                }
                Ok(frame) if frame.kind == FrameKind::SessionHeartbeat => {
                    if session_mode.lease().is_none() || !frame.payload.is_empty() {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "SESSION_HEARTBEAT requires a leased session and an empty payload",
                        );
                        return;
                    }
                    if control_sender
                        .send(SessionControlEvent::Heartbeat(Instant::now()))
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(frame) => {
                    report_protocol_error(
                        &control_sender,
                        session_mode,
                        &format!("unexpected {:?} frame in process session", frame.kind),
                    );
                    return;
                }
                Err(lsw_core::LswError::Io(_)) => {
                    drop(child_stdin.take());
                    if session_mode.cancel_on_disconnect() {
                        let _ = control_sender.send(SessionControlEvent::Disconnect);
                    }
                    return;
                }
                Err(error) => {
                    report_protocol_error(&control_sender, session_mode, &error.to_string());
                    return;
                }
            }
        }
    })
}

#[cfg(windows)]
pub(super) fn spawn_terminal_input_bridge(
    mut stream: TcpStream,
    input: impl Write + Send + 'static,
    console: Weak<windows_conpty::PseudoConsole>,
    session_mode: SessionMode,
    control_sender: SyncSender<SessionControlEvent>,
) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || {
        let mut input = Some(input);
        loop {
            match read_frame(&mut stream) {
                Ok(frame) if frame.kind == FrameKind::Stdin => {
                    let Some(input) = input.as_mut() else {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "STDIN was sent after STDIN_CLOSE",
                        );
                        return Ok(());
                    };
                    if let Err(error) = input.write_all(&frame.payload).and_then(|()| input.flush())
                    {
                        return terminal_bridge_failure(
                            &control_sender,
                            session_mode,
                            error.to_string(),
                        );
                    }
                }
                Ok(frame) if frame.kind == FrameKind::Resize => {
                    let size = match TerminalSize::decode(&frame.payload) {
                        Ok(size) => size,
                        Err(error) => {
                            return terminal_bridge_failure(
                                &control_sender,
                                session_mode,
                                error.to_string(),
                            )
                        }
                    };
                    let Some(console) = console.upgrade() else {
                        return Ok(());
                    };
                    if let Err(error) = console.resize(size) {
                        return terminal_bridge_failure(
                            &control_sender,
                            session_mode,
                            error.to_string(),
                        );
                    }
                }
                Ok(frame) if frame.kind == FrameKind::StdinClose => {
                    if !session_mode.is_controlled() || !frame.payload.is_empty() {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "STDIN_CLOSE requires a controlled session and an empty payload",
                        );
                        return Ok(());
                    }
                    if input.take().is_none() {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "STDIN_CLOSE was sent more than once",
                        );
                        return Ok(());
                    }
                }
                Ok(frame) if frame.kind == FrameKind::SessionCancel => {
                    if !session_mode.is_controlled() || !frame.payload.is_empty() {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "SESSION_CANCEL requires a controlled session and an empty payload",
                        );
                        return Ok(());
                    }
                    drop(input.take());
                    let _ = control_sender.send(SessionControlEvent::Cancel);
                    return Ok(());
                }
                Ok(frame) if frame.kind == FrameKind::SessionSignal => {
                    if !session_mode.is_controlled() {
                        return terminal_bridge_failure(
                            &control_sender,
                            session_mode,
                            "SESSION_SIGNAL requires a controlled session".to_owned(),
                        );
                    }
                    let signal = match SessionSignal::decode(&frame.payload) {
                        Ok(signal) => signal,
                        Err(error) => {
                            return terminal_bridge_failure(
                                &control_sender,
                                session_mode,
                                error.to_string(),
                            )
                        }
                    };
                    drop(input.take());
                    let _ = control_sender.send(SessionControlEvent::Signal(signal));
                    return Ok(());
                }
                Ok(frame) if frame.kind == FrameKind::SessionHeartbeat => {
                    if session_mode.lease().is_none() || !frame.payload.is_empty() {
                        report_protocol_error(
                            &control_sender,
                            session_mode,
                            "SESSION_HEARTBEAT requires a leased session and an empty payload",
                        );
                        return Ok(());
                    }
                    if control_sender
                        .send(SessionControlEvent::Heartbeat(Instant::now()))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Ok(frame) => {
                    let message = format!("unexpected {:?} frame in terminal session", frame.kind);
                    if session_mode.is_controlled() {
                        let _ = control_sender.send(SessionControlEvent::ProtocolError(message));
                        return Ok(());
                    }
                    return Err(message);
                }
                Err(lsw_core::LswError::Io(_)) => {
                    drop(input.take());
                    if session_mode.cancel_on_disconnect() {
                        let _ = control_sender.send(SessionControlEvent::Disconnect);
                    }
                    return Ok(());
                }
                Err(error) => {
                    if session_mode.is_controlled() {
                        let _ = control_sender
                            .send(SessionControlEvent::ProtocolError(error.to_string()));
                        return Ok(());
                    }
                    return Err(error.to_string());
                }
            }
        }
    })
}

#[cfg(windows)]
pub(super) fn terminal_bridge_failure(
    sender: &SyncSender<SessionControlEvent>,
    session_mode: SessionMode,
    message: String,
) -> Result<(), String> {
    if session_mode.is_controlled() {
        let _ = sender.send(SessionControlEvent::ProtocolError(message));
        Ok(())
    } else {
        Err(message)
    }
}

pub(super) fn report_protocol_error(
    sender: &SyncSender<SessionControlEvent>,
    session_mode: SessionMode,
    message: &str,
) {
    if session_mode.is_controlled() {
        let _ = sender.send(SessionControlEvent::ProtocolError(message.to_owned()));
    }
}

pub(super) fn join_input_bridge(
    thread: thread::JoinHandle<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    thread
        .join()
        .map_err(|_| "process input bridge panicked".into())
}

#[cfg(windows)]
pub(super) fn join_terminal_input(
    thread: thread::JoinHandle<Result<(), String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    match thread.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err("terminal input bridge panicked".into()),
    }
}

pub(super) fn join_bridge(
    thread: thread::JoinHandle<Result<(), String>>,
    ignore_stream_error: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match thread.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) if ignore_stream_error => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err("process I/O bridge panicked".into()),
    }
}

pub(super) fn finish_session(
    writer: &Arc<Mutex<TcpStream>>,
    session_end: SessionEnd,
    normal_exit_code: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    match session_end {
        SessionEnd::Normal => send_shared(
            writer,
            &Frame::new(FrameKind::Exit, encode_exit(normal_exit_code)),
        ),
        SessionEnd::Cancelled => send_shared(
            writer,
            &Frame::new(FrameKind::Exit, encode_exit(SESSION_CANCEL_EXIT_CODE)),
        ),
        SessionEnd::Signalled(code) => {
            send_shared(writer, &Frame::new(FrameKind::Exit, encode_exit(code)))
        }
        SessionEnd::Disconnected => Ok(()),
        // The socket was shut down before joining the output bridges so a
        // half-open peer that stopped reading cannot retain this session slot.
        // Do not try to reacquire the possibly contended writer mutex here.
        SessionEnd::LeaseExpired => Err("session heartbeat lease expired".into()),
        SessionEnd::ProtocolError(message) => {
            send_shared(
                writer,
                &Frame::new(FrameKind::Error, message.as_bytes().to_vec()),
            )?;
            Err(message.into())
        }
    }
}

pub(super) fn send_shared(
    writer: &Arc<Mutex<TcpStream>>,
    frame: &Frame,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = writer
        .lock()
        .map_err(|_| "agent stream writer lock was poisoned")?;
    write_frame(&mut *writer, frame)?;
    Ok(())
}

pub(super) fn send_error(
    stream: &mut TcpStream,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(
        stream,
        &Frame::new(FrameKind::Error, message.as_bytes().to_vec()),
    )?;
    Ok(())
}

pub(super) fn read_token(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let token = fs::read_to_string(path)?.trim().to_owned();
    if token.len() != 64
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("agent token must contain exactly 64 lowercase hexadecimal characters".into());
    }
    Ok(token)
}

#[cfg(any(windows, test))]
pub(super) fn windows_command_line(program: &str, arguments: &[&str]) -> String {
    let mut command_line = String::new();
    append_windows_quoted_argument(&mut command_line, program);
    for argument in arguments {
        command_line.push(' ');
        append_windows_quoted_argument(&mut command_line, argument);
    }
    command_line
}

#[cfg(any(windows, test))]
pub(super) fn append_windows_quoted_argument(command_line: &mut String, argument: &str) {
    let needs_quotes = argument.is_empty()
        || argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"');
    if !needs_quotes {
        command_line.push_str(argument);
        return;
    }

    command_line.push('"');
    let mut backslashes = 0_usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                command_line.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                command_line.push('"');
                backslashes = 0;
            }
            _ => {
                command_line.extend(std::iter::repeat('\\').take(backslashes));
                backslashes = 0;
                command_line.push(character);
            }
        }
    }
    command_line.extend(std::iter::repeat('\\').take(backslashes * 2));
    command_line.push('"');
}
