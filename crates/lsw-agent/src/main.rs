// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::io::{self, Read, Write};
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
    ServerHello, SessionKind, StartRequest, AGENT_GUEST_PORT, AGENT_PROTOCOL_VERSION,
};

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
        capabilities: vec![
            "exec-pipes-v1".to_owned(),
            "shell-fallback-v1".to_owned(),
            "stderr-v1".to_owned(),
            "file-transfer-v1".to_owned(),
        ],
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
        FrameKind::FilePut => receive_file(stream, &request_frame.payload),
        FrameKind::FileGet => send_file(stream, &request_frame.payload),
        _ => {
            send_error(&mut stream, "unsupported request after HELLO")?;
            Err("client sent an unsupported request after authentication".into())
        }
    }
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
}
