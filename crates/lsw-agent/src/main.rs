// SPDX-License-Identifier: GPL-3.0-or-later

#![deny(unsafe_code)]

use std::env;
use std::fs;
use std::io::{self, Read, Write};
#[cfg(windows)]
use std::net::Shutdown;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use lsw_core::{
    constant_time_token_eq, decode_file_length, decode_resize, encode_exit, encode_file_length,
    read_frame, write_frame, ClientHello, FileGetRequest, FilePutRequest, Frame, FrameKind,
    ServerHello, SessionKind, StartRequest, TerminalStartRequest, AGENT_GUEST_PORT,
    AGENT_PROTOCOL_VERSION,
};
#[cfg(windows)]
use lsw_core::{TerminalSize, CAPABILITY_CONPTY_V1, CAPABILITY_TERMINAL_RESIZE_V1};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_CHUNK_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_SESSIONS: usize = 32;

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lsw-agent: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<std::ffi::OsString>) -> Result<(), Box<dyn std::error::Error>> {
    let configuration = Configuration::parse(&arguments)?;
    let token = Arc::new(read_token(&configuration.token_file)?);
    let listener = TcpListener::bind(configuration.listen)?;
    let active_sessions = Arc::new(AtomicUsize::new(0));
    println!("lsw-agent listening on {}", listener.local_addr()?);

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                if configuration.once {
                    if let Err(error) = handle_connection(stream, &token) {
                        eprintln!("lsw-agent: session failed: {error}");
                    }
                    return Ok(());
                }

                let previous = active_sessions.fetch_add(1, Ordering::AcqRel);
                if previous >= configuration.max_sessions {
                    active_sessions.fetch_sub(1, Ordering::AcqRel);
                    eprintln!(
                        "lsw-agent: refusing connection: {} sessions are already active",
                        configuration.max_sessions
                    );
                    continue;
                }

                let session_token = Arc::clone(&token);
                let session_counter = Arc::clone(&active_sessions);
                let spawn_result = thread::Builder::new()
                    .name("lsw-agent-session".to_owned())
                    .spawn(move || {
                        let _slot = SessionSlot(session_counter);
                        if let Err(error) = handle_connection(stream, &session_token) {
                            eprintln!("lsw-agent: session failed: {error}");
                        }
                    });
                if let Err(error) = spawn_result {
                    active_sessions.fetch_sub(1, Ordering::AcqRel);
                    eprintln!("lsw-agent: could not start session thread: {error}");
                }
            }
            Err(error) => eprintln!("lsw-agent: accept failed: {error}"),
        }
    }
    Ok(())
}

struct SessionSlot(Arc<AtomicUsize>);

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle_connection(
    mut stream: TcpStream,
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let hello_frame = read_frame(&mut stream)?;
    if hello_frame.kind != FrameKind::Hello {
        send_error(&mut stream, "the first frame must be HELLO")?;
        return Err("client did not send HELLO first".into());
    }
    let hello = ClientHello::decode(&hello_frame.payload)?;
    if hello.version != AGENT_PROTOCOL_VERSION {
        send_error(
            &mut stream,
            &format!(
                "unsupported protocol {}; server requires {}",
                hello.version, AGENT_PROTOCOL_VERSION
            ),
        )?;
        return Err("client protocol version is not supported".into());
    }
    if !constant_time_token_eq(&hello.token, expected_token) {
        send_error(&mut stream, "authentication failed")?;
        return Err("client authentication failed".into());
    }

    let server_hello = ServerHello {
        version: AGENT_PROTOCOL_VERSION,
        capabilities: agent_capabilities(),
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::HelloOk, server_hello.encode()?),
    )?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    let request_frame = read_frame(&mut stream)?;
    if request_frame.kind == FrameKind::Ping {
        write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
        return Ok(());
    }
    match request_frame.kind {
        FrameKind::Start => run_process_request(stream, &request_frame.payload),
        FrameKind::TerminalStart => run_terminal_request(stream, &request_frame.payload),
        FrameKind::FilePut => receive_file(stream, &request_frame.payload),
        FrameKind::FileGet => send_file(stream, &request_frame.payload),
        _ => {
            send_error(&mut stream, "unsupported request after HELLO")?;
            Err("client sent an unsupported request after authentication".into())
        }
    }
}

fn agent_capabilities() -> Vec<String> {
    let capabilities = vec![
        "exec-pipes-v1".to_owned(),
        "shell-fallback-v1".to_owned(),
        "stderr-v1".to_owned(),
        "file-transfer-v1".to_owned(),
    ];
    #[cfg(windows)]
    let capabilities = {
        let mut capabilities = capabilities;
        capabilities.push(CAPABILITY_CONPTY_V1.to_owned());
        capabilities.push(CAPABILITY_TERMINAL_RESIZE_V1.to_owned());
        capabilities
    };
    capabilities
}

fn run_process_request(
    mut stream: TcpStream,
    payload: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let request = StartRequest::decode(payload)?;
    let mut child = match spawn_request(&request) {
        Ok(child) => child,
        Err(error) => {
            send_error(&mut stream, &format!("could not start process: {error}"))?;
            return Err(error);
        }
    };

    bridge_process(&mut child, stream)?;
    Ok(())
}

#[cfg(windows)]
fn run_terminal_request(
    mut stream: TcpStream,
    payload: &[u8],
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
    bridge_terminal(process, stream)
}

#[cfg(not(windows))]
fn run_terminal_request(
    mut stream: TcpStream,
    payload: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    TerminalStartRequest::decode(payload)?;
    send_error(
        &mut stream,
        "ConPTY is available only from a Windows guest agent",
    )?;
    Err("client requested ConPTY from a non-Windows agent".into())
}

fn receive_file(mut stream: TcpStream, payload: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let request = FilePutRequest::decode(payload)?;
    let destination = PathBuf::from(&request.destination);
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
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
                    file.sync_all()?;
                    drop(file);
                    fs::hard_link(&temporary, &destination)?;
                    fs::remove_file(&temporary)?;
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

fn send_file(mut stream: TcpStream, payload: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let request = FileGetRequest::decode(payload)?;
    let source = PathBuf::from(request.source);
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

fn upload_temporary_path(destination: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
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

fn spawn_request(request: &StartRequest) -> Result<Child, Box<dyn std::error::Error>> {
    match request.kind {
        SessionKind::Shell => spawn_shell(request),
        SessionKind::Exec | SessionKind::Run => {
            let (program, arguments) = request
                .argv
                .split_first()
                .ok_or("command request did not contain a program")?;
            spawn_program(program, arguments, request.working_directory.as_deref())
                .map_err(Into::into)
        }
    }
}

fn spawn_shell(request: &StartRequest) -> Result<Child, Box<dyn std::error::Error>> {
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

fn spawn_program(
    program: &str,
    arguments: &[impl AsRef<std::ffi::OsStr>],
    working_directory: Option<&str>,
) -> io::Result<Child> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }
    command.spawn()
}

fn bridge_process(child: &mut Child, stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let child_stdin = child.stdin.take().ok_or("child stdin was not piped")?;
    let child_stdout = child.stdout.take().ok_or("child stdout was not piped")?;
    let child_stderr = child.stderr.take().ok_or("child stderr was not piped")?;
    let writer = Arc::new(Mutex::new(stream.try_clone()?));

    let stdout_thread = spawn_output_bridge(child_stdout, Arc::clone(&writer), FrameKind::Stdout);
    let stderr_thread = spawn_output_bridge(child_stderr, Arc::clone(&writer), FrameKind::Stderr);
    let _input_thread = spawn_input_bridge(stream, child_stdin);

    let status = child.wait()?;
    join_bridge(stdout_thread)?;
    join_bridge(stderr_thread)?;
    let code = status.code().unwrap_or(255);
    send_shared(&writer, &Frame::new(FrameKind::Exit, encode_exit(code)))?;
    Ok(())
}

#[cfg(windows)]
fn bridge_terminal(
    process: windows_conpty::ConPtyProcess,
    stream: TcpStream,
) -> Result<(), Box<dyn std::error::Error>> {
    let windows_conpty::ConPtyProcess {
        process,
        input,
        output,
        console,
    } = process;
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let input_shutdown = stream.try_clone()?;
    let output_thread = spawn_output_bridge(output, Arc::clone(&writer), FrameKind::Stdout);
    let input_thread = spawn_terminal_input_bridge(stream, input, Arc::clone(&console));

    let code = windows_conpty::wait_for_process(&process)?;
    let _ = input_shutdown.shutdown(Shutdown::Read);
    // A disconnect only closes ConPTY input; it deliberately does not kill a
    // guest shell. A shell which ignores EOF can retain this session slot until
    // it exits. Adding an authenticated cancel/terminate frame is follow-up
    // protocol work because implicit process termination would be surprising.
    join_terminal_input(input_thread)?;
    drop(console);
    join_bridge(output_thread)?;
    send_shared(&writer, &Frame::new(FrameKind::Exit, encode_exit(code)))?;
    Ok(())
}

fn spawn_output_bridge(
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

fn spawn_input_bridge(
    mut stream: TcpStream,
    mut child_stdin: impl Write + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        match read_frame(&mut stream) {
            Ok(frame) if frame.kind == FrameKind::Stdin => {
                if child_stdin.write_all(&frame.payload).is_err() || child_stdin.flush().is_err() {
                    break;
                }
            }
            Ok(frame) if frame.kind == FrameKind::Resize => {
                // Pipe sessions cannot resize. ConPTY-capable agents will negotiate
                // a separate capability before acting on this frame.
                let _ = decode_resize(&frame.payload);
            }
            Ok(_) => break,
            Err(_) => break,
        }
    })
}

#[cfg(windows)]
fn spawn_terminal_input_bridge(
    mut stream: TcpStream,
    mut input: impl Write + Send + 'static,
    console: Arc<windows_conpty::PseudoConsole>,
) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || loop {
        match read_frame(&mut stream) {
            Ok(frame) if frame.kind == FrameKind::Stdin => {
                input
                    .write_all(&frame.payload)
                    .and_then(|()| input.flush())
                    .map_err(|error| error.to_string())?;
            }
            Ok(frame) if frame.kind == FrameKind::Resize => {
                let size =
                    TerminalSize::decode(&frame.payload).map_err(|error| error.to_string())?;
                console.resize(size).map_err(|error| error.to_string())?;
            }
            Ok(frame) => {
                return Err(format!(
                    "unexpected {:?} frame in terminal session",
                    frame.kind
                ))
            }
            Err(lsw_core::LswError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::NotConnected
                ) =>
            {
                return Ok(())
            }
            Err(error) => return Err(error.to_string()),
        }
    })
}

#[cfg(windows)]
fn join_terminal_input(
    thread: thread::JoinHandle<Result<(), String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    match thread.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err("terminal input bridge panicked".into()),
    }
}

fn join_bridge(
    thread: thread::JoinHandle<Result<(), String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    match thread.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err("process I/O bridge panicked".into()),
    }
}

fn send_shared(
    writer: &Arc<Mutex<TcpStream>>,
    frame: &Frame,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = writer
        .lock()
        .map_err(|_| "agent stream writer lock was poisoned")?;
    write_frame(&mut *writer, frame)?;
    Ok(())
}

fn send_error(stream: &mut TcpStream, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(
        stream,
        &Frame::new(FrameKind::Error, message.as_bytes().to_vec()),
    )?;
    Ok(())
}

fn read_token(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
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
fn windows_command_line(program: &str, arguments: &[&str]) -> String {
    let mut command_line = String::new();
    append_windows_quoted_argument(&mut command_line, program);
    for argument in arguments {
        command_line.push(' ');
        append_windows_quoted_argument(&mut command_line, argument);
    }
    command_line
}

#[cfg(any(windows, test))]
fn append_windows_quoted_argument(command_line: &mut String, argument: &str) {
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

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_conpty {
    use std::ffi::{c_void, OsStr};
    use std::fs::File;
    use std::io;
    use std::mem::{size_of, size_of_val};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr;
    use std::sync::Arc;

    use lsw_core::{SessionKind, StartRequest, TerminalSize};

    type Handle = RawHandle;
    type PseudoConsoleHandle = RawHandle;

    const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
    const INFINITE: u32 = u32::MAX;
    const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
    const WAIT_OBJECT_0: u32 = 0;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2_length: u16,
        reserved2: *mut u8,
        standard_input: Handle,
        standard_output: Handle,
        standard_error: Handle,
    }

    impl StartupInfoW {
        fn empty() -> Self {
            Self {
                cb: 0,
                reserved: ptr::null_mut(),
                desktop: ptr::null_mut(),
                title: ptr::null_mut(),
                x: 0,
                y: 0,
                x_size: 0,
                y_size: 0,
                x_count_chars: 0,
                y_count_chars: 0,
                fill_attribute: 0,
                flags: 0,
                show_window: 0,
                reserved2_length: 0,
                reserved2: ptr::null_mut(),
                standard_input: ptr::null_mut(),
                standard_output: ptr::null_mut(),
                standard_error: ptr::null_mut(),
            }
        }
    }

    #[repr(C)]
    struct StartupInfoExW {
        startup_info: StartupInfoW,
        attribute_list: *mut c_void,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: Handle,
        thread: Handle,
        process_id: u32,
        thread_id: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "CloseHandle"]
        fn close_handle(handle: Handle) -> i32;
        #[link_name = "ClosePseudoConsole"]
        fn close_pseudo_console(console: PseudoConsoleHandle);
        #[link_name = "CreatePipe"]
        fn create_pipe(
            read_pipe: *mut Handle,
            write_pipe: *mut Handle,
            attributes: *mut c_void,
            size: u32,
        ) -> i32;
        #[link_name = "CreateProcessW"]
        fn create_process_w(
            application_name: *const u16,
            command_line: *mut u16,
            process_attributes: *mut c_void,
            thread_attributes: *mut c_void,
            inherit_handles: i32,
            creation_flags: u32,
            environment: *mut c_void,
            current_directory: *const u16,
            startup_info: *mut StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        #[link_name = "CreatePseudoConsole"]
        fn create_pseudo_console(
            size: Coord,
            input: Handle,
            output: Handle,
            flags: u32,
            console: *mut PseudoConsoleHandle,
        ) -> i32;
        #[link_name = "DeleteProcThreadAttributeList"]
        fn delete_proc_thread_attribute_list(list: *mut c_void);
        #[link_name = "GetExitCodeProcess"]
        fn get_exit_code_process(process: Handle, exit_code: *mut u32) -> i32;
        #[link_name = "InitializeProcThreadAttributeList"]
        fn initialize_proc_thread_attribute_list(
            list: *mut c_void,
            attribute_count: u32,
            flags: u32,
            size: *mut usize,
        ) -> i32;
        #[link_name = "ResizePseudoConsole"]
        fn resize_pseudo_console(console: PseudoConsoleHandle, size: Coord) -> i32;
        #[link_name = "UpdateProcThreadAttribute"]
        fn update_proc_thread_attribute(
            list: *mut c_void,
            flags: u32,
            attribute: usize,
            value: *mut c_void,
            size: usize,
            previous_value: *mut c_void,
            return_size: *mut usize,
        ) -> i32;
        #[link_name = "WaitForSingleObject"]
        fn wait_for_single_object(handle: Handle, milliseconds: u32) -> u32;
    }

    pub(super) struct PseudoConsole {
        handle: PseudoConsoleHandle,
    }

    // An HPCON is an opaque kernel handle. The bridge serializes resize calls
    // on one input thread, and Arc keeps the handle alive until that thread exits.
    unsafe impl Send for PseudoConsole {}
    unsafe impl Sync for PseudoConsole {}

    impl PseudoConsole {
        fn create(size: TerminalSize, input: Handle, output: Handle) -> io::Result<Self> {
            let mut handle = ptr::null_mut();
            let result =
                unsafe { create_pseudo_console(coord(size), input, output, 0, &mut handle) };
            if result < 0 {
                return Err(hresult_error("CreatePseudoConsole", result));
            }
            if handle.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "CreatePseudoConsole returned a null handle",
                ));
            }
            Ok(Self { handle })
        }

        pub(super) fn resize(&self, size: TerminalSize) -> io::Result<()> {
            let result = unsafe { resize_pseudo_console(self.handle, coord(size)) };
            if result < 0 {
                Err(hresult_error("ResizePseudoConsole", result))
            } else {
                Ok(())
            }
        }
    }

    impl Drop for PseudoConsole {
        fn drop(&mut self) {
            unsafe { close_pseudo_console(self.handle) };
        }
    }

    struct AttributeList {
        list: *mut c_void,
        _storage: Vec<usize>,
    }

    impl AttributeList {
        fn for_pseudo_console(console: &PseudoConsole) -> io::Result<Self> {
            let mut byte_count = 0_usize;
            unsafe {
                initialize_proc_thread_attribute_list(ptr::null_mut(), 1, 0, &mut byte_count)
            };
            if byte_count == 0 {
                return Err(io::Error::last_os_error());
            }
            let word_size = size_of::<usize>();
            let word_count = byte_count.checked_add(word_size - 1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "attribute list is too large")
            })? / word_size;
            let mut storage = vec![0_usize; word_count];
            let list = storage.as_mut_ptr().cast::<c_void>();
            if unsafe { initialize_proc_thread_attribute_list(list, 1, 0, &mut byte_count) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let attributes = Self {
                list,
                _storage: storage,
            };
            if unsafe {
                update_proc_thread_attribute(
                    attributes.list,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                    console.handle,
                    size_of::<PseudoConsoleHandle>(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(attributes)
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            unsafe { delete_proc_thread_attribute_list(self.list) };
        }
    }

    pub(super) struct ConPtyProcess {
        pub(super) process: OwnedHandle,
        pub(super) input: File,
        pub(super) output: File,
        pub(super) console: Arc<PseudoConsole>,
    }

    pub(super) fn spawn_shell(
        request: &StartRequest,
        size: TerminalSize,
    ) -> io::Result<ConPtyProcess> {
        if request.kind != SessionKind::Shell {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ConPTY requires a shell request",
            ));
        }
        let mut last_not_found = None;
        for candidate in &request.argv {
            let arguments = match candidate.to_ascii_lowercase().as_str() {
                "pwsh" | "pwsh.exe" => &["-NoLogo"][..],
                "powershell" | "powershell.exe" => &["-NoLogo"][..],
                "cmd" | "cmd.exe" => &["/Q"][..],
                _ => &[][..],
            };
            match spawn_program(
                candidate,
                arguments,
                request.working_directory.as_deref(),
                size,
            ) {
                Ok(process) => return Ok(process),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    last_not_found = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_not_found.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no shell candidate was supplied")
        }))
    }

    fn spawn_program(
        program: &str,
        arguments: &[&str],
        working_directory: Option<&str>,
        size: TerminalSize,
    ) -> io::Result<ConPtyProcess> {
        let (pseudo_input, input) = new_pipe()?;
        let (output, pseudo_output) = new_pipe()?;
        let console = Arc::new(PseudoConsole::create(
            size,
            pseudo_input.as_raw_handle(),
            pseudo_output.as_raw_handle(),
        )?);
        drop(pseudo_input);
        drop(pseudo_output);

        let attributes = AttributeList::for_pseudo_console(&console)?;
        let mut startup = StartupInfoExW {
            startup_info: StartupInfoW::empty(),
            attribute_list: attributes.list,
        };
        startup.startup_info.cb = u32::try_from(size_of_val(&startup))
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "STARTUPINFOEXW is too large"))?;

        let command_line = super::windows_command_line(program, arguments);
        let mut command_line = OsStr::new(&command_line)
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let current_directory = working_directory.map(|directory| {
            OsStr::new(directory)
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        });
        let current_directory_pointer = current_directory
            .as_ref()
            .map_or(ptr::null(), |directory| directory.as_ptr());
        let mut information = ProcessInformation {
            process: ptr::null_mut(),
            thread: ptr::null_mut(),
            process_id: 0,
            thread_id: 0,
        };
        let created = unsafe {
            create_process_w(
                ptr::null(),
                command_line.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                ptr::null_mut(),
                current_directory_pointer,
                &mut startup.startup_info,
                &mut information,
            )
        };
        if created == 0 {
            close_if_present(information.process);
            close_if_present(information.thread);
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(information.process) };
        let thread = unsafe { OwnedHandle::from_raw_handle(information.thread) };
        drop(thread);
        Ok(ConPtyProcess {
            process,
            input,
            output,
            console,
        })
    }

    pub(super) fn wait_for_process(process: &OwnedHandle) -> io::Result<i32> {
        let wait_result = unsafe { wait_for_single_object(process.as_raw_handle(), INFINITE) };
        if wait_result != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut exit_code = 0_u32;
        if unsafe { get_exit_code_process(process.as_raw_handle(), &mut exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(exit_code as i32)
    }

    fn new_pipe() -> io::Result<(File, File)> {
        let mut read = ptr::null_mut();
        let mut write = ptr::null_mut();
        if unsafe { create_pipe(&mut read, &mut write, ptr::null_mut(), 0) } == 0 {
            close_if_present(read);
            close_if_present(write);
            return Err(io::Error::last_os_error());
        }
        let read = unsafe { File::from_raw_handle(read) };
        let write = unsafe { File::from_raw_handle(write) };
        Ok((read, write))
    }

    fn coord(size: TerminalSize) -> Coord {
        Coord {
            x: size.columns as i16,
            y: size.rows as i16,
        }
    }

    fn hresult_error(operation: &str, result: i32) -> io::Error {
        io::Error::new(
            io::ErrorKind::Other,
            format!("{operation} failed with HRESULT 0x{:08x}", result as u32),
        )
    }

    fn close_if_present(handle: Handle) {
        if !handle.is_null() {
            unsafe {
                close_handle(handle);
            }
        }
    }
}

struct Configuration {
    listen: SocketAddr,
    token_file: PathBuf,
    once: bool,
    max_sessions: usize,
}

impl Configuration {
    fn parse(arguments: &[std::ffi::OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut listen = SocketAddr::from(([0, 0, 0, 0], AGENT_GUEST_PORT));
        let mut token_file = env::var_os("LSW_AGENT_TOKEN_FILE").map(PathBuf::from);
        let mut once = false;
        let mut max_sessions = DEFAULT_MAX_SESSIONS;
        let mut index = 0;
        while index < arguments.len() {
            let option = arguments[index]
                .to_str()
                .ok_or("agent arguments must be valid UTF-8")?;
            match option {
                "--listen" => {
                    index += 1;
                    listen = arguments
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or("--listen requires an IP:PORT value")?
                        .parse()?;
                }
                "--token-file" => {
                    index += 1;
                    token_file = Some(PathBuf::from(
                        arguments.get(index).ok_or("--token-file requires a path")?,
                    ));
                }
                "--max-sessions" => {
                    index += 1;
                    max_sessions = arguments
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or("--max-sessions requires a number")?
                        .parse()?;
                    if !(1..=128).contains(&max_sessions) {
                        return Err("--max-sessions must be between 1 and 128".into());
                    }
                }
                "--once" => once = true,
                "--help" | "-h" => {
                    println!(
                        "lsw-agent --token-file PATH [--listen IP:PORT] [--max-sessions N] [--once]\n\
                         The default listener is 0.0.0.0:5040 inside the restricted guest network."
                    );
                    std::process::exit(0);
                }
                unknown => return Err(format!("unknown agent option {unknown:?}").into()),
            }
            index += 1;
        }
        Ok(Self {
            listen,
            token_file: token_file
                .ok_or("--token-file PATH or LSW_AGENT_TOKEN_FILE is required")?,
            once,
            max_sessions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_fallback_reaches_a_known_program() {
        let request = StartRequest {
            kind: SessionKind::Shell,
            argv: vec!["definitely-not-an-lsw-shell".to_owned(), "sh".to_owned()],
            working_directory: None,
        };
        let mut child = spawn_request(&request).expect("sh fallback should start");
        child.kill().expect("fixture process should stop");
        child.wait().expect("fixture process should be reaped");
    }

    #[test]
    fn token_parser_rejects_short_or_uppercase_secrets() {
        let root = std::env::temp_dir().join(format!("lsw-agent-token-{}", std::process::id()));
        fs::write(&root, "abcd").expect("fixture should be written");
        assert!(read_token(&root).is_err());
        fs::write(&root, "A".repeat(64)).expect("fixture should be updated");
        assert!(read_token(&root).is_err());
        fs::remove_file(root).expect("fixture should be removed");
    }

    #[test]
    fn session_limit_is_bounded() {
        let arguments = vec![
            "--token-file".into(),
            "token.txt".into(),
            "--max-sessions".into(),
            "4".into(),
        ];
        let configuration = Configuration::parse(&arguments).expect("configuration should parse");
        assert_eq!(configuration.max_sessions, 4);

        let invalid = vec![
            "--token-file".into(),
            "token.txt".into(),
            "--max-sessions".into(),
            "0".into(),
        ];
        assert!(Configuration::parse(&invalid).is_err());
    }

    #[test]
    fn windows_command_line_quotes_empty_and_space_containing_arguments() {
        assert_eq!(windows_command_line("", &[]), "\"\"");
        assert_eq!(
            windows_command_line("C:\\Program Files\\pwsh.exe", &["hello world"]),
            "\"C:\\Program Files\\pwsh.exe\" \"hello world\""
        );
    }

    #[test]
    fn windows_command_line_quotes_quotes_and_trailing_backslashes() {
        assert_eq!(
            windows_command_line("tool.exe", &["a\"b"]),
            "tool.exe \"a\\\"b\""
        );
        assert_eq!(
            windows_command_line("tool.exe", &["C:\\path with space\\"]),
            "tool.exe \"C:\\path with space\\\\\""
        );
        assert_eq!(
            windows_command_line("tool.exe", &["plain\\"]),
            "tool.exe plain\\"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_agent_does_not_advertise_conpty() {
        let capabilities = agent_capabilities();
        assert!(!capabilities
            .iter()
            .any(|capability| capability == lsw_core::CAPABILITY_CONPTY_V1));
        assert!(!capabilities
            .iter()
            .any(|capability| capability == lsw_core::CAPABILITY_TERMINAL_RESIZE_V1));
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_loopback_exec_streams_output_and_exit_status() {
        let token = "a".repeat(64);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let expected_token = token.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("fixture should connect");
            handle_connection(stream, &expected_token).expect("agent request should succeed");
        });

        let mut stream = TcpStream::connect(address).expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout should apply");
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token,
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
        )
        .expect("hello should be sent");
        let response = read_frame(&mut stream).expect("hello response should arrive");
        assert_eq!(response.kind, FrameKind::HelloOk);
        ServerHello::decode(&response.payload).expect("server hello should decode");

        let request = StartRequest {
            kind: SessionKind::Exec,
            argv: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf stdout-bytes; printf stderr-bytes >&2; exit 7".to_owned(),
            ],
            working_directory: None,
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Start, request.encode().unwrap()),
        )
        .expect("start should be sent");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = loop {
            let frame = read_frame(&mut stream).expect("process frame should arrive");
            match frame.kind {
                FrameKind::Stdout => stdout.extend(frame.payload),
                FrameKind::Stderr => stderr.extend(frame.payload),
                FrameKind::Exit => break lsw_core::decode_exit(&frame.payload).unwrap(),
                other => panic!("unexpected process frame {other:?}"),
            }
        };
        assert_eq!(stdout, b"stdout-bytes");
        assert_eq!(stderr, b"stderr-bytes");
        assert_eq!(exit, 7);
        drop(stream);
        server.join().expect("agent fixture should finish");
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_loopback_file_transfer_preserves_unicode_and_bytes() {
        fn connect(token: &str) -> (TcpStream, thread::JoinHandle<Result<(), String>>) {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
            let address = listener
                .local_addr()
                .expect("listener should have an address");
            let expected_token = token.to_owned();
            let server = thread::spawn(move || {
                let (stream, _) = listener.accept().expect("fixture should connect");
                handle_connection(stream, &expected_token).map_err(|error| error.to_string())
            });
            let mut stream = TcpStream::connect(address).expect("client should connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout should apply");
            let hello = ClientHello {
                version: AGENT_PROTOCOL_VERSION,
                token: token.to_owned(),
            };
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
            )
            .expect("hello should be sent");
            let response = read_frame(&mut stream).expect("hello response should arrive");
            assert_eq!(response.kind, FrameKind::HelloOk);
            ServerHello::decode(&response.payload).expect("server hello should decode");
            (stream, server)
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-agent-e2e-{nonce}"));
        fs::create_dir(&root).expect("fixture directory should be created");
        let destination = root.join("香港-資料.bin");
        let destination_text = destination.to_string_lossy().into_owned();
        let contents = b"binary\0payload\xff\nUTF-8:\xe9\xa6\x99\xe6\xb8\xaf";
        let token = "b".repeat(64);

        let (mut upload, upload_server) = connect(&token);
        let put = FilePutRequest {
            destination: destination_text.clone(),
            length: contents.len() as u64,
        };
        write_frame(
            &mut upload,
            &Frame::new(FrameKind::FilePut, put.encode().unwrap()),
        )
        .expect("upload request should be sent");
        assert_eq!(
            read_frame(&mut upload)
                .expect("upload ready should arrive")
                .kind,
            FrameKind::Pong
        );
        for chunk in contents.chunks(7) {
            write_frame(
                &mut upload,
                &Frame::new(FrameKind::FileData, chunk.to_vec()),
            )
            .expect("upload data should be sent");
        }
        write_frame(
            &mut upload,
            &Frame::new(
                FrameKind::FileDone,
                encode_file_length(contents.len() as u64),
            ),
        )
        .expect("upload completion should be sent");
        let completion = read_frame(&mut upload).expect("upload completion should arrive");
        assert_eq!(completion.kind, FrameKind::FileDone);
        assert_eq!(
            decode_file_length(&completion.payload).unwrap(),
            contents.len() as u64
        );
        drop(upload);
        upload_server
            .join()
            .expect("upload fixture should finish")
            .expect("upload should succeed");
        assert_eq!(fs::read(&destination).unwrap(), contents);

        let (mut download, download_server) = connect(&token);
        let get = FileGetRequest {
            source: destination_text,
        };
        write_frame(
            &mut download,
            &Frame::new(FrameKind::FileGet, get.encode().unwrap()),
        )
        .expect("download request should be sent");
        let mut received = Vec::new();
        loop {
            let frame = read_frame(&mut download).expect("download frame should arrive");
            match frame.kind {
                FrameKind::FileData => received.extend(frame.payload),
                FrameKind::FileDone => {
                    assert_eq!(
                        decode_file_length(&frame.payload).unwrap(),
                        received.len() as u64
                    );
                    break;
                }
                other => panic!("unexpected download frame {other:?}"),
            }
        }
        drop(download);
        download_server
            .join()
            .expect("download fixture should finish")
            .expect("download should succeed");
        assert_eq!(received, contents);

        fs::remove_dir_all(root).expect("fixture directory should be removable");
    }

    #[cfg(unix)]
    #[test]
    fn independent_authenticated_sessions_run_concurrently() {
        fn connect(address: SocketAddr, token: &str) -> TcpStream {
            let mut stream = TcpStream::connect(address).expect("client should connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout should apply");
            let hello = ClientHello {
                version: AGENT_PROTOCOL_VERSION,
                token: token.to_owned(),
            };
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
            )
            .expect("hello should be sent");
            assert_eq!(
                read_frame(&mut stream)
                    .expect("hello response should arrive")
                    .kind,
                FrameKind::HelloOk
            );
            stream
        }

        fn start(stream: &mut TcpStream, script: &str) {
            let request = StartRequest {
                kind: SessionKind::Exec,
                argv: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
                working_directory: None,
            };
            write_frame(
                stream,
                &Frame::new(FrameKind::Start, request.encode().unwrap()),
            )
            .expect("start should be sent");
        }

        fn collect(stream: &mut TcpStream) -> (Vec<u8>, i32) {
            let mut stdout = Vec::new();
            loop {
                let frame = read_frame(stream).expect("process frame should arrive");
                match frame.kind {
                    FrameKind::Stdout => stdout.extend(frame.payload),
                    FrameKind::Stderr => {}
                    FrameKind::Exit => {
                        return (stdout, lsw_core::decode_exit(&frame.payload).unwrap())
                    }
                    other => panic!("unexpected process frame {other:?}"),
                }
            }
        }

        let token = "c".repeat(64);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let expected_token = token.clone();
        let server = thread::spawn(move || {
            let mut sessions = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().expect("fixture should connect");
                let token = expected_token.clone();
                sessions.push(thread::spawn(move || {
                    handle_connection(stream, &token).map_err(|error| error.to_string())
                }));
            }
            for session in sessions {
                session
                    .join()
                    .expect("session should not panic")
                    .expect("session should succeed");
            }
        });

        let mut blocked = connect(address, &token);
        start(
            &mut blocked,
            "IFS= read -r value; printf 'first-%s' \"$value\"",
        );

        let mut independent = connect(address, &token);
        start(&mut independent, "printf second");
        assert_eq!(collect(&mut independent), (b"second".to_vec(), 0));

        write_frame(
            &mut blocked,
            &Frame::new(FrameKind::Stdin, b"ready\n".to_vec()),
        )
        .expect("blocked session input should be sent");
        assert_eq!(collect(&mut blocked), (b"first-ready".to_vec(), 0));
        drop(independent);
        drop(blocked);
        server.join().expect("server fixture should finish");
    }
}
