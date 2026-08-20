// SPDX-License-Identifier: GPL-3.0-or-later

#![deny(unsafe_code)]

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use lsw_core::{
    constant_time_token_eq, decode_file_length, decode_resize, encode_exit, encode_file_length,
    read_frame, write_frame, ClientHello, FileGetRequest, FilePutRequest, Frame, FrameKind,
    ServerHello, SessionKind, SessionLease, SessionLeaseState, SessionOptions, StartRequest,
    TerminalStartRequest, AGENT_GUEST_PORT, AGENT_PROTOCOL_VERSION, CAPABILITY_SESSION_CONTROL_V1,
    CAPABILITY_SESSION_LEASE_V1, SESSION_CANCEL_EXIT_CODE,
};
#[cfg(windows)]
use lsw_core::{TerminalSize, CAPABILITY_CONPTY_V1, CAPABILITY_TERMINAL_RESIZE_V1};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
const STREAM_CHUNK_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_SESSIONS: usize = 32;
#[cfg(windows)]
const LICENSE_HELPER_PORT: u16 = 5041;
#[cfg(windows)]
const LICENSE_HELPER_SERVICE: &str = "LSWLicenseHelper";

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            write_stderr(format_args!("lsw-agent: {error}"));
            ExitCode::FAILURE
        }
    }
}

fn write_stderr(message: std::fmt::Arguments<'_>) {
    // Windows services commonly have no valid standard handles. Logging must
    // remain best-effort so a missing console cannot unwind the service main.
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}

#[cfg(windows)]
fn run_license_client(arguments: &[std::ffi::OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let (action, token_file) =
        match arguments {
            [action, option, token_file] if option == "--token-file" => (
                action
                    .to_str()
                    .ok_or("license action must be valid UTF-8")?,
                PathBuf::from(token_file),
            ),
            _ => return Err(
                "usage: lsw-agent --license-client status|activate|online|open --token-file PATH"
                    .into(),
            ),
        };
    if action == "open" {
        Command::new("explorer.exe")
            .arg("ms-settings:activation")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        println!("Requested Windows Activation settings.");
        return Ok(());
    }
    if !matches!(action, "status" | "activate" | "online") {
        return Err("unknown license action".into());
    }

    let token = read_token(&token_file)?;
    let mut key = Vec::new();
    if action == "activate" {
        io::stdin().take(65).read_to_end(&mut key)?;
        while key.last().is_some_and(|byte| matches!(byte, b'\r' | b'\n')) {
            key.pop();
        }
        if !valid_product_key(&key) {
            key.fill(0);
            return Err("invalid product-key input".into());
        }
    }

    let _ = Command::new("sc.exe")
        .args(["start", LICENSE_HELPER_SERVICE])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", LICENSE_HELPER_PORT)) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                key.fill(0);
                return Err(format!("could not reach the activation helper: {error}").into());
            }
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.write_all(action.as_bytes())?;
    stream.write_all(b"\n")?;
    if !key.is_empty() {
        stream.write_all(&key)?;
        stream.write_all(b"\n")?;
    }
    key.fill(0);
    stream.shutdown(Shutdown::Write)?;

    let mut response = Vec::new();
    stream.take(64 * 1024 + 1).read_to_end(&mut response)?;
    if response.len() > 64 * 1024 {
        return Err("activation helper response was too large".into());
    }
    let response = String::from_utf8(response)?;
    if let Some(output) = response.strip_prefix("OK\n") {
        print!("{output}");
        Ok(())
    } else {
        Err("activation helper rejected the operation".into())
    }
}

#[cfg(windows)]
fn run_license_helper(
    configuration: Configuration,
    ready: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let token = read_token(&configuration.token_file)?;
    let listener = TcpListener::bind(configuration.listen)?;
    listener.set_nonblocking(true)?;
    ready()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match listener.accept() {
            Ok((mut stream, peer)) if peer.ip().is_loopback() => {
                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                stream.set_write_timeout(Some(Duration::from_secs(120)))?;
                if handle_license_helper_connection(&mut stream, &token)? {
                    return Ok(());
                }
            }
            Ok((_, _)) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(
                        "activation helper timed out without an authenticated request".into(),
                    );
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(windows)]
fn handle_license_helper_connection(
    stream: &mut TcpStream,
    expected_token: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let token = read_bounded_line(stream, 128)?;
    if !constant_time_token_eq(expected_token, &token) {
        stream.write_all(b"ERR\n")?;
        return Ok(false);
    }
    let action = read_bounded_line(stream, 32)?;
    let mut key = if action == "activate" {
        read_bounded_line(stream, 64)?.into_bytes()
    } else {
        Vec::new()
    };
    if action == "activate" && !valid_product_key(&key) {
        key.fill(0);
        stream.write_all(b"ERR\n")?;
        return Ok(true);
    }
    let result = perform_windows_license_operation(&action, &key);
    key.fill(0);
    match result {
        Ok(output) => {
            stream.write_all(b"OK\n")?;
            stream.write_all(output.as_bytes())?;
        }
        Err(_) => stream.write_all(b"ERR\n")?,
    }
    Ok(true)
}

#[cfg(windows)]
fn read_bounded_line(
    stream: &mut TcpStream,
    limit: usize,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if bytes.len() >= limit {
            return Err("activation helper request line was too long".into());
        }
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] != b'\r' {
            bytes.push(byte[0]);
        }
    }
    Ok(String::from_utf8(bytes)?)
}

#[cfg(any(windows, test))]
fn valid_product_key(key: &[u8]) -> bool {
    key.len() == 29
        && key.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 5 | 11 | 17 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_uppercase() || byte.is_ascii_digit()
            }
        })
}

#[cfg(windows)]
fn perform_windows_license_operation(
    action: &str,
    key: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    const STATUS_SCRIPT: &[u8] = br#"$ErrorActionPreference = 'Stop'
$ApplicationId = '55c92734-d682-4d71-983e-d6ec3f16059f'
$Product = Get-CimInstance -ClassName SoftwareLicensingProduct | Where-Object {
    $_.ApplicationID -eq $ApplicationId -and $_.Name -like 'Windows*' -and $null -ne $_.PartialProductKey
} | Sort-Object -Property LicenseStatus -Descending | Select-Object -First 1
if ($null -eq $Product -or $Product.LicenseStatus -ne 1) {
    Write-Output 'STATUS=unlicensed'
} else {
    Write-Output 'STATUS=licensed'
}
if ($null -ne $Product) { Write-Output ('LICENSE_STATUS={0}' -f $Product.LicenseStatus) }
"#;
    const ACTIVATE_PREFIX: &[u8] = br#"$ErrorActionPreference = 'Stop'
$Key = '"#;
    const ACTIVATE_SUFFIX: &[u8] = br#"'
$Service = Get-CimInstance -ClassName SoftwareLicensingService
$Install = Invoke-CimMethod -InputObject $Service -MethodName InstallProductKey -Arguments @{ ProductKey = $Key }
if ($Install.ReturnValue -ne 0) { throw 'InstallProductKey failed' }
$ApplicationId = '55c92734-d682-4d71-983e-d6ec3f16059f'
$PartialKey = $Key.Substring($Key.Length - 5)
$Product = Get-CimInstance -ClassName SoftwareLicensingProduct | Where-Object {
    $_.ApplicationID -eq $ApplicationId -and $_.PartialProductKey -eq $PartialKey
} | Select-Object -First 1
if ($null -eq $Product) { throw 'Installed Windows product was not found' }
$Activation = Invoke-CimMethod -InputObject $Product -MethodName Activate
if ($Activation.ReturnValue -ne 0) { throw 'Activate failed' }
Write-Output 'STATUS=activation-requested'
"#;
    const ONLINE_SCRIPT: &[u8] = br#"$ErrorActionPreference = 'Stop'
$ApplicationId = '55c92734-d682-4d71-983e-d6ec3f16059f'
$Product = Get-CimInstance -ClassName SoftwareLicensingProduct | Where-Object {
    $_.ApplicationID -eq $ApplicationId -and $_.Name -like 'Windows*' -and $null -ne $_.PartialProductKey
} | Sort-Object -Property LicenseStatus -Descending | Select-Object -First 1
if ($null -eq $Product) { throw 'No installed Windows product key was found' }
$Activation = Invoke-CimMethod -InputObject $Product -MethodName Activate
if ($Activation.ReturnValue -ne 0) { throw 'Activate failed' }
Write-Output 'STATUS=activation-requested'
"#;

    let mut script = match action {
        "status" => STATUS_SCRIPT.to_vec(),
        "online" => ONLINE_SCRIPT.to_vec(),
        "activate" if valid_product_key(key) => {
            let mut script = ACTIVATE_PREFIX.to_vec();
            script.extend_from_slice(key);
            script.extend_from_slice(ACTIVATE_SUFFIX);
            script
        }
        _ => return Err("unsupported activation helper operation".into()),
    };
    let result = run_license_powershell(&script);
    script.fill(0);
    result
}

#[cfg(windows)]
fn run_license_powershell(script: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let mut child = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("PowerShell stdin was unavailable")?
        .write_all(script)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err("Windows WMI licensing operation failed".into());
    }
    if output.stdout.len() > 16 * 1024 {
        return Err("Windows WMI licensing response was too large".into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn run(arguments: Vec<std::ffi::OsString>) -> Result<(), Box<dyn std::error::Error>> {
    if arguments
        .first()
        .is_some_and(|argument| argument == "--license-client")
    {
        #[cfg(windows)]
        {
            return run_license_client(&arguments[1..]);
        }
        #[cfg(not(windows))]
        {
            return Err("--license-client is only supported on Windows".into());
        }
    }
    let configuration = Configuration::parse(&arguments)?;

    if configuration.service {
        #[cfg(windows)]
        {
            return match configuration.service_kind {
                ServiceKind::Agent => windows_service::run(configuration),
                ServiceKind::LicenseHelper => windows_license_service::run(configuration),
            };
        }
        #[cfg(not(windows))]
        {
            return Err(match configuration.service_kind {
                ServiceKind::Agent => "--service is only supported on Windows",
                ServiceKind::LicenseHelper => "--license-helper is only supported on Windows",
            }
            .into());
        }
    }

    run_agent(configuration, None, |address| {
        println!("lsw-agent listening on {address}");
        Ok(())
    })
}

fn run_agent(
    configuration: Configuration,
    shutdown: Option<&Receiver<()>>,
    ready: impl FnOnce(SocketAddr) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let token = Arc::new(read_token(&configuration.token_file)?);
    let listener = TcpListener::bind(configuration.listen)?;
    let active_sessions = Arc::new(AtomicUsize::new(0));
    let local_address = listener.local_addr()?;

    if shutdown.is_some_and(shutdown_requested) {
        return Ok(());
    }
    ready(local_address)?;

    if let Some(shutdown) = shutdown {
        listener.set_nonblocking(true)?;
        return run_stoppable_listener(
            &listener,
            &configuration,
            &token,
            &active_sessions,
            shutdown,
        );
    }

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                if dispatch_connection(stream, &configuration, &token, &active_sessions) {
                    return Ok(());
                }
            }
            Err(error) => write_stderr(format_args!("lsw-agent: accept failed: {error}")),
        }
    }
    Ok(())
}

fn run_stoppable_listener(
    listener: &TcpListener,
    configuration: &Configuration,
    token: &Arc<String>,
    active_sessions: &Arc<AtomicUsize>,
    shutdown: &Receiver<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    const ACCEPT_RETRY_INTERVAL: Duration = Duration::from_millis(100);

    loop {
        if shutdown_requested(shutdown) {
            return Ok(());
        }

        match listener.accept() {
            Ok((stream, _)) => {
                if dispatch_connection(stream, configuration, token, active_sessions) {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if wait_for_shutdown(shutdown, ACCEPT_RETRY_INTERVAL) {
                    return Ok(());
                }
            }
            Err(error) => {
                write_stderr(format_args!("lsw-agent: accept failed: {error}"));
                if wait_for_shutdown(shutdown, ACCEPT_RETRY_INTERVAL) {
                    return Ok(());
                }
            }
        }
    }
}

fn shutdown_requested(shutdown: &Receiver<()>) -> bool {
    match shutdown.try_recv() {
        Ok(()) | Err(mpsc::TryRecvError::Disconnected) => true,
        Err(mpsc::TryRecvError::Empty) => false,
    }
}

fn wait_for_shutdown(shutdown: &Receiver<()>, timeout: Duration) -> bool {
    match shutdown.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => false,
    }
}

fn dispatch_connection(
    stream: TcpStream,
    configuration: &Configuration,
    token: &Arc<String>,
    active_sessions: &Arc<AtomicUsize>,
) -> bool {
    if configuration.once {
        if let Err(error) = handle_connection(stream, token) {
            write_stderr(format_args!("lsw-agent: session failed: {error}"));
        }
        return true;
    }

    let previous = active_sessions.fetch_add(1, Ordering::AcqRel);
    if previous >= configuration.max_sessions {
        active_sessions.fetch_sub(1, Ordering::AcqRel);
        write_stderr(format_args!(
            "lsw-agent: refusing connection: {} sessions are already active",
            configuration.max_sessions
        ));
        return false;
    }

    let session_token = Arc::clone(token);
    let session_counter = Arc::clone(active_sessions);
    let spawn_result = thread::Builder::new()
        .name("lsw-agent-session".to_owned())
        .spawn(move || {
            let _slot = SessionSlot(session_counter);
            if let Err(error) = handle_connection(stream, &session_token) {
                write_stderr(format_args!("lsw-agent: session failed: {error}"));
            }
        });
    if let Err(error) = spawn_result {
        active_sessions.fetch_sub(1, Ordering::AcqRel);
        write_stderr(format_args!(
            "lsw-agent: could not start session thread: {error}"
        ));
    }
    false
}

struct SessionSlot(Arc<AtomicUsize>);

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionMode {
    Legacy,
    Controlled {
        options: SessionOptions,
        lease: Option<SessionLease>,
    },
}

impl SessionMode {
    fn is_controlled(self) -> bool {
        matches!(self, Self::Controlled { .. })
    }

    fn cancel_on_disconnect(self) -> bool {
        match self {
            Self::Legacy => false,
            Self::Controlled { options, .. } => options.cancel_on_disconnect,
        }
    }

    fn lease(self) -> Option<SessionLease> {
        match self {
            Self::Legacy => None,
            Self::Controlled { lease, .. } => lease,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum SessionControlEvent {
    Cancel,
    Disconnect,
    Heartbeat(Instant),
    ProtocolError(String),
}

#[derive(Debug, Eq, PartialEq)]
enum SessionEnd {
    Normal,
    Cancelled,
    Disconnected,
    LeaseExpired,
    ProtocolError(String),
}

struct SessionLeaseMonitor {
    origin: Instant,
    state: SessionLeaseState,
}

impl SessionLeaseMonitor {
    fn new(lease: SessionLease) -> Self {
        Self {
            origin: Instant::now(),
            state: SessionLeaseState::new(lease, 0),
        }
    }

    fn millis_at(&self, instant: Instant) -> u64 {
        u64::try_from(instant.saturating_duration_since(self.origin).as_millis())
            .unwrap_or(u64::MAX)
    }

    fn observe_heartbeat(&mut self, observed_at: Instant) -> bool {
        let elapsed = self.millis_at(observed_at);
        self.state.observe_heartbeat(elapsed)
    }

    fn is_expired(&self, now: Instant) -> bool {
        self.state.is_expired(self.millis_at(now))
    }

    fn wait_duration(&self, now: Instant) -> Duration {
        let remaining = self
            .state
            .deadline_millis()
            .saturating_sub(self.millis_at(now));
        PROCESS_POLL_INTERVAL.min(Duration::from_millis(remaining))
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
    let (request_frame, session_mode) = if request_frame.kind == FrameKind::SessionOptions {
        let options = match SessionOptions::decode(&request_frame.payload) {
            Ok(options) => options,
            Err(error) => {
                send_error(&mut stream, &error.to_string())?;
                return Err(error.into());
            }
        };
        let mut request_frame = read_frame(&mut stream)?;
        let lease = if request_frame.kind == FrameKind::SessionLease {
            let lease = match SessionLease::decode(&request_frame.payload) {
                Ok(lease) => lease,
                Err(error) => {
                    send_error(&mut stream, &error.to_string())?;
                    return Err(error.into());
                }
            };
            request_frame = read_frame(&mut stream)?;
            Some(lease)
        } else {
            None
        };
        if !matches!(
            request_frame.kind,
            FrameKind::Start | FrameKind::TerminalStart
        ) {
            send_error(
                &mut stream,
                "SESSION_OPTIONS may be followed by one SESSION_LEASE, then must be followed by START or TERMINAL_START",
            )?;
            return Err("client sent an invalid controlled-session request".into());
        }
        (request_frame, SessionMode::Controlled { options, lease })
    } else {
        (request_frame, SessionMode::Legacy)
    };
    match request_frame.kind {
        FrameKind::Start => run_process_request(stream, &request_frame.payload, session_mode),
        FrameKind::TerminalStart => {
            run_terminal_request(stream, &request_frame.payload, session_mode)
        }
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
        CAPABILITY_SESSION_CONTROL_V1.to_owned(),
        CAPABILITY_SESSION_LEASE_V1.to_owned(),
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
    session_mode: SessionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = StartRequest::decode(payload)?;
    let mut child = match spawn_request(&request) {
        Ok(child) => child,
        Err(error) => {
            send_error(&mut stream, &format!("could not start process: {error}"))?;
            return Err(error);
        }
    };

    bridge_process(&mut child, stream, session_mode)?;
    Ok(())
}

#[cfg(windows)]
fn run_terminal_request(
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
fn run_terminal_request(
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

/// A process whose ordinary descendants share one agent-session owner.
///
/// The platform owner is established before user code can run: Unix children
/// enter a fresh process group in `Command`'s pre-exec path, while Windows
/// children start suspended and are resumed only after assignment to a Job
/// Object. Unix process groups are lifecycle bookkeeping, not a sandbox: a
/// process with sufficient permission can deliberately leave its group. Tree
/// teardown also happens after a normal leader exit, because an ordinary
/// descendant may otherwise keep the session's stdout/stderr pipes open.
struct SessionChild {
    process: Child,
    tree: Option<process_tree::Owner>,
}

impl SessionChild {
    fn spawn(command: &mut Command) -> io::Result<Self> {
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

    fn try_wait_and_cleanup(&mut self) -> io::Result<Option<ExitStatus>> {
        let Some(status) = self.process.try_wait()? else {
            return Ok(None);
        };
        self.terminate_tree()?;
        Ok(Some(status))
    }

    fn wait_and_cleanup(&mut self) -> io::Result<ExitStatus> {
        let status = self.process.wait()?;
        self.terminate_tree()?;
        Ok(status)
    }

    /// Returns the leader status and whether this call won the race to issue
    /// process-tree termination.
    fn terminate(&mut self) -> io::Result<(ExitStatus, bool)> {
        if let Some(status) = self.process.try_wait()? {
            self.terminate_tree()?;
            return Ok((status, false));
        }
        self.terminate_tree()?;
        Ok((self.process.wait()?, true))
    }

    /// Process-tree ownership is one-shot. A Unix process-group id may be
    /// reused after its last member exits, so a later signal from Drop could
    /// otherwise target an unrelated group. A successful cleanup disarms it;
    /// a failed cleanup restores it only so Drop can make one best-effort retry.
    fn terminate_tree(&mut self) -> io::Result<()> {
        let Some(tree) = self.tree.take() else {
            return Ok(());
        };
        match tree.terminate(SESSION_CANCEL_EXIT_CODE) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.tree = Some(tree);
                Err(error)
            }
        }
    }

    #[cfg(all(test, unix))]
    fn kill(&mut self) -> io::Result<()> {
        self.terminate_tree()
    }

    #[cfg(all(test, unix))]
    fn wait(&mut self) -> io::Result<ExitStatus> {
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
        if self.terminate_tree().is_ok() {
            let _ = self.process.wait();
        }
    }
}

#[cfg(unix)]
#[path = "process_tree_unix.rs"]
#[allow(unsafe_code)]
mod process_tree;

#[cfg(windows)]
#[path = "process_tree_windows.rs"]
#[allow(unsafe_code)]
mod process_tree;

#[cfg(not(any(unix, windows)))]
#[path = "process_tree_fallback.rs"]
mod process_tree;

fn spawn_request(request: &StartRequest) -> Result<SessionChild, Box<dyn std::error::Error>> {
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

fn spawn_shell(request: &StartRequest) -> Result<SessionChild, Box<dyn std::error::Error>> {
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
) -> io::Result<SessionChild> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }
    SessionChild::spawn(&mut command)
}

fn bridge_process(
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
    let _ = input_shutdown.shutdown(if session_end == SessionEnd::LeaseExpired {
        Shutdown::Both
    } else {
        Shutdown::Read
    });
    join_input_bridge(input_thread)?;
    join_bridge(stdout_thread, peer_unavailable)?;
    join_bridge(stderr_thread, peer_unavailable)?;
    finish_session(&writer, session_end, status.code().unwrap_or(255))
}

#[cfg(windows)]
fn bridge_terminal(
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
        Arc::clone(&console),
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
    let _ = input_shutdown.shutdown(if session_end == SessionEnd::LeaseExpired {
        Shutdown::Both
    } else {
        Shutdown::Read
    });
    join_terminal_input(input_thread)?;
    drop(console);
    join_bridge(output_thread, peer_unavailable)?;
    finish_session(&writer, session_end, code)
}

fn wait_for_child(
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

fn cancel_session_end(process_was_terminated: bool) -> SessionEnd {
    if process_was_terminated {
        SessionEnd::Cancelled
    } else {
        SessionEnd::Normal
    }
}

fn lease_session_end(process_was_terminated: bool) -> SessionEnd {
    if process_was_terminated {
        SessionEnd::LeaseExpired
    } else {
        SessionEnd::Normal
    }
}

/// Returns the exit status and whether this call actually issued a kill.
fn terminate_child(child: &mut SessionChild) -> io::Result<(ExitStatus, bool)> {
    child.terminate()
}

#[cfg(windows)]
fn wait_for_terminal_process(
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
        if let Err(error) = job.terminate(SESSION_CANCEL_EXIT_CODE) {
            if let Some(code) = windows_conpty::wait_for_process_timeout(process, 0)? {
                return Ok((code, SessionEnd::Normal));
            }
            return Err(error.into());
        }
        return Ok((windows_conpty::wait_for_process(process)?, session_end));
    }
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
fn spawn_terminal_input_bridge(
    mut stream: TcpStream,
    input: impl Write + Send + 'static,
    console: Arc<windows_conpty::PseudoConsole>,
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
fn terminal_bridge_failure(
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

fn report_protocol_error(
    sender: &SyncSender<SessionControlEvent>,
    session_mode: SessionMode,
    message: &str,
) {
    if session_mode.is_controlled() {
        let _ = sender.send(SessionControlEvent::ProtocolError(message.to_owned()));
    }
}

fn join_input_bridge(thread: thread::JoinHandle<()>) -> Result<(), Box<dyn std::error::Error>> {
    thread
        .join()
        .map_err(|_| "process input bridge panicked".into())
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
    ignore_stream_error: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match thread.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) if ignore_stream_error => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err("process I/O bridge panicked".into()),
    }
}

fn finish_session(
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
mod windows_conpty;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_license_service;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_service;

struct Configuration {
    listen: SocketAddr,
    token_file: PathBuf,
    once: bool,
    max_sessions: usize,
    service: bool,
    service_kind: ServiceKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceKind {
    Agent,
    LicenseHelper,
}

impl Configuration {
    fn parse(arguments: &[std::ffi::OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut listen = SocketAddr::from(([0, 0, 0, 0], AGENT_GUEST_PORT));
        let mut token_file = env::var_os("LSW_AGENT_TOKEN_FILE").map(PathBuf::from);
        let mut once = false;
        let mut max_sessions = DEFAULT_MAX_SESSIONS;
        let mut service = false;
        let mut service_kind = ServiceKind::Agent;
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
                "--service" => service = true,
                "--license-helper" => {
                    service = true;
                    service_kind = ServiceKind::LicenseHelper;
                }
                "--help" | "-h" => {
                    println!(
                        "lsw-agent --token-file PATH [--listen IP:PORT] [--max-sessions N] [--once] [--service]\n\
                         The default listener is 0.0.0.0:5040 inside the restricted guest network.\n\
                         --service runs LSWAgent under the Windows Service Control Manager."
                    );
                    std::process::exit(0);
                }
                unknown => return Err(format!("unknown agent option {unknown:?}").into()),
            }
            index += 1;
        }
        if service_kind == ServiceKind::LicenseHelper && !listen.ip().is_loopback() {
            return Err("the license helper listener must use guest loopback".into());
        }
        Ok(Self {
            listen,
            token_file: token_file
                .ok_or("--token-file PATH or LSW_AGENT_TOKEN_FILE is required")?,
            once,
            max_sessions,
            service,
            service_kind,
        })
    }
}

#[cfg(test)]
mod tests;
