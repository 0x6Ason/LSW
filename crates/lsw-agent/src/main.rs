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
#[cfg(windows)]
use std::sync::Weak;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use lsw_core::{
    constant_time_token_eq, decode_file_length, decode_resize, encode_exit, encode_file_length,
    encode_process_id, read_frame, write_frame, ClientHello, FileGetRequest, FilePutRequest, Frame,
    FrameKind, LiveShareConfigureRequest, LiveShareStatus, ProcessEnvironment, ServerHello,
    SessionKind, SessionLease, SessionLeaseState, SessionOptions, SessionSignal, StartRequest,
    TerminalStartRequest, UserCreateRequest, UserSetRoleRequest, WindowsSudoConfigureRequest,
    WindowsSudoStatus, AGENT_GUEST_PORT, AGENT_PROTOCOL_VERSION, CAPABILITY_DETACHED_RUN_V1,
    CAPABILITY_PROCESS_ENVIRONMENT_V1, CAPABILITY_SESSION_CONTROL_V1, CAPABILITY_SESSION_LEASE_V1,
    CAPABILITY_SESSION_SIGNAL_V1, SESSION_CANCEL_EXIT_CODE,
};
#[cfg(windows)]
use lsw_core::{
    TerminalSize, CAPABILITY_CONPTY_V1, CAPABILITY_LIVE_SHARE_V1,
    CAPABILITY_MAINTENANCE_HIBERNATE_V1, CAPABILITY_MAINTENANCE_TRIM_V1,
    CAPABILITY_POWER_HIBERNATE_V1, CAPABILITY_TERMINAL_RESIZE_V1, CAPABILITY_USER_ACCOUNT_ROLE_V1,
    CAPABILITY_USER_ACCOUNT_V1, CAPABILITY_WINDOWS_SUDO_V1, CLONE_IDENTITY_MARKER_FILE,
    CLONE_IDENTITY_NAME_FILE, CLONE_IDENTITY_TOKEN_FILE, LICENSE_HELPER_GUEST_PORT,
    MAINTENANCE_HELPER_GUEST_PORT, USER_HELPER_GUEST_PORT,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
const STREAM_CHUNK_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_SESSIONS: usize = 32;
#[cfg(windows)]
const IDENTITY_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8 * 60);
#[cfg(windows)]
const IDENTITY_DISCOVERY_FAST_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(windows)]
const IDENTITY_DISCOVERY_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(windows)]
const IDENTITY_DISCOVERY_SLOW_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(windows)]
const LICENSE_HELPER_PORT: u16 = LICENSE_HELPER_GUEST_PORT;
#[cfg(windows)]
const LICENSE_HELPER_SERVICE: &str = "LSWLicenseHelper";
#[cfg(windows)]
const USER_HELPER_PORT: u16 = USER_HELPER_GUEST_PORT;
#[cfg(windows)]
const USER_HELPER_SERVICE: &str = "LSWUserHelper";
#[cfg(windows)]
const MAINTENANCE_HELPER_PORT: u16 = MAINTENANCE_HELPER_GUEST_PORT;
#[cfg(windows)]
const MAINTENANCE_HELPER_SERVICE: &str = "LSWMaintenanceHelper";

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
    #[cfg(windows)]
    append_windows_service_log(message);
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}

#[cfg(windows)]
fn append_windows_service_log(message: std::fmt::Arguments<'_>) {
    const MAX_LOG_BYTES: u64 = 64 * 1024;

    let Some(program_data) = env::var_os("ProgramData") else {
        return;
    };
    let path = PathBuf::from(program_data).join("LSW").join("agent.log");
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES) {
        let _ = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path);
    }
    let Ok(mut log) = fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let _ = writeln!(log, "{timestamp} {message}");
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

    let mut stream =
        match connect_windows_helper(LICENSE_HELPER_SERVICE, LICENSE_HELPER_PORT, "activation") {
            Ok(stream) => stream,
            Err(error) => {
                key.fill(0);
                return Err(error);
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
fn connect_windows_helper(
    service: &str,
    port: u16,
    operation: &str,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let start = || {
        let _ = Command::new("sc.exe")
            .args(["start", service])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    start();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut next_start_attempt = Instant::now() + Duration::from_secs(1);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => {
                if Instant::now() >= next_start_attempt {
                    start();
                    next_start_attempt = Instant::now() + Duration::from_secs(1);
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(format!("could not reach the {operation} helper: {error}").into())
            }
        }
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
                // A socket accepted from the stoppable nonblocking listener can
                // inherit nonblocking mode on Windows. The bounded helper
                // protocol uses ordinary blocking reads with explicit timeouts.
                stream.set_nonblocking(false)?;
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

#[cfg(windows)]
fn run_user_helper(
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
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                stream.set_write_timeout(Some(Duration::from_secs(30)))?;
                if handle_user_helper_connection(&mut stream, &token)? {
                    return Ok(());
                }
            }
            Ok((_, _)) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(
                        "Windows account helper timed out without an authenticated request".into(),
                    );
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(windows)]
fn handle_user_helper_connection(
    stream: &mut TcpStream,
    expected_token: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut hello_frame = read_frame(stream)?;
    if hello_frame.kind != FrameKind::Hello {
        send_error(stream, "the first account-helper frame must be HELLO")?;
        return Ok(false);
    }
    let hello = ClientHello::decode(&hello_frame.payload);
    hello_frame.payload.fill(0);
    let hello = match hello {
        Ok(hello) => hello,
        Err(error) => {
            send_error(stream, &error.to_string())?;
            return Ok(false);
        }
    };
    if hello.version != AGENT_PROTOCOL_VERSION
        || !constant_time_token_eq(&hello.token, expected_token)
    {
        send_error(stream, "account-helper authentication failed")?;
        return Ok(false);
    }
    let server_hello = ServerHello {
        version: AGENT_PROTOCOL_VERSION,
        capabilities: vec![
            CAPABILITY_USER_ACCOUNT_V1.to_owned(),
            CAPABILITY_USER_ACCOUNT_ROLE_V1.to_owned(),
        ],
    };
    write_frame(
        stream,
        &Frame::new(FrameKind::HelloOk, server_hello.encode()?),
    )?;

    let mut frame = read_frame(stream)?;
    let result = match frame.kind {
        FrameKind::UserCreate => {
            let request = UserCreateRequest::decode(&frame.payload);
            frame.payload.fill(0);
            let mut request = match request {
                Ok(request) => request,
                Err(error) => {
                    send_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            };
            let result = windows_user::create_local_user(
                &request.user_name,
                &request.password,
                request.administrator,
            );
            request.password.fill(0);
            result
        }
        FrameKind::UserSetRole => {
            let request = UserSetRoleRequest::decode(&frame.payload);
            frame.payload.fill(0);
            let request = match request {
                Ok(request) => request,
                Err(error) => {
                    send_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            };
            windows_user::set_local_user_role(&request.user_name, request.role)
        }
        _ => {
            frame.payload.fill(0);
            send_error(
                stream,
                "account helper accepts only USER_CREATE or USER_SET_ROLE",
            )?;
            return Ok(true);
        }
    };
    match result {
        Ok(()) => write_frame(stream, &Frame::new(FrameKind::Pong, Vec::new()))?,
        Err(error) => send_error(stream, &error.to_string())?,
    }
    Ok(true)
}

#[cfg(windows)]
fn run_maintenance_helper(
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
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                stream.set_write_timeout(Some(Duration::from_secs(10)))?;
                if handle_maintenance_helper_connection(&mut stream, &token)? {
                    return Ok(());
                }
            }
            Ok((_, _)) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(
                        "Windows maintenance helper timed out without an authenticated request"
                            .into(),
                    );
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(windows)]
fn handle_maintenance_helper_connection(
    stream: &mut TcpStream,
    expected_token: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut hello_frame = read_frame(stream)?;
    if hello_frame.kind != FrameKind::Hello {
        send_error(stream, "the first maintenance-helper frame must be HELLO")?;
        return Ok(false);
    }
    let hello = ClientHello::decode(&hello_frame.payload);
    hello_frame.payload.fill(0);
    let hello = match hello {
        Ok(hello) => hello,
        Err(error) => {
            send_error(stream, &error.to_string())?;
            return Ok(false);
        }
    };
    if hello.version != AGENT_PROTOCOL_VERSION
        || !constant_time_token_eq(&hello.token, expected_token)
    {
        send_error(stream, "maintenance-helper authentication failed")?;
        return Ok(false);
    }
    let server_hello = ServerHello {
        version: AGENT_PROTOCOL_VERSION,
        capabilities: vec![
            CAPABILITY_MAINTENANCE_TRIM_V1.to_owned(),
            CAPABILITY_MAINTENANCE_HIBERNATE_V1.to_owned(),
            CAPABILITY_WINDOWS_SUDO_V1.to_owned(),
            CAPABILITY_LIVE_SHARE_V1.to_owned(),
        ],
    };
    write_frame(
        stream,
        &Frame::new(FrameKind::HelloOk, server_hello.encode()?),
    )?;

    let mut frame = read_frame(stream)?;
    match frame.kind {
        FrameKind::MaintenanceTrim
        | FrameKind::MaintenanceHibernate
        | FrameKind::WindowsSudoQuery
        | FrameKind::LiveShareQuery
            if !frame.payload.is_empty() =>
        {
            frame.payload.fill(0);
            send_error(
                stream,
                "this fixed maintenance request must have an empty payload",
            )?;
        }
        FrameKind::MaintenanceTrim => match perform_windows_trim() {
            Ok(()) => write_frame(stream, &Frame::new(FrameKind::Pong, Vec::new()))?,
            Err(error) => send_error(stream, &error.to_string())?,
        },
        FrameKind::MaintenanceHibernate => match enable_windows_hibernation() {
            Ok(()) => {
                write_frame(stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
                request_windows_hibernation()?;
            }
            Err(error) => send_error(stream, &error.to_string())?,
        },
        FrameKind::WindowsSudoQuery => match windows_sudo::status() {
            Ok(status) => write_frame(
                stream,
                &Frame::new(FrameKind::WindowsSudoStatus, status.encode()),
            )?,
            Err(error) => send_error(stream, &error.to_string())?,
        },
        FrameKind::WindowsSudoConfigure => {
            let request = match WindowsSudoConfigureRequest::decode(&frame.payload) {
                Ok(request) => request,
                Err(error) => {
                    frame.payload.fill(0);
                    send_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            };
            frame.payload.fill(0);
            match windows_sudo::configure(request.enable) {
                Ok(()) => write_frame(stream, &Frame::new(FrameKind::Pong, Vec::new()))?,
                Err(error) => send_error(stream, &error.to_string())?,
            }
        }
        FrameKind::LiveShareQuery => match query_windows_live_share() {
            Ok(status) => write_frame(
                stream,
                &Frame::new(FrameKind::LiveShareStatus, status.encode()),
            )?,
            Err(error) => send_error(stream, &error.to_string())?,
        },
        FrameKind::LiveShareConfigure => {
            let request = match LiveShareConfigureRequest::decode(&frame.payload) {
                Ok(request) => request,
                Err(error) => {
                    frame.payload.fill(0);
                    send_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            };
            frame.payload.fill(0);
            match configure_windows_live_share(request.enable, expected_token) {
                Ok(()) => write_frame(stream, &Frame::new(FrameKind::Pong, Vec::new()))?,
                Err(error) => send_error(stream, &error.to_string())?,
            }
        }
        _ => {
            frame.payload.fill(0);
            send_error(
                stream,
                "maintenance helper accepts only fixed maintenance, Windows sudo, and live-share requests",
            )?;
        }
    }
    Ok(true)
}

#[cfg(windows)]
fn query_windows_live_share() -> Result<LiveShareStatus, Box<dyn std::error::Error>> {
    let script = r#"$ErrorActionPreference='Stop'
$Mapping=Get-SmbGlobalMapping -LocalPath 'L:' -ErrorAction SilentlyContinue
if ($null -eq $Mapping) { exit 3 }
if ($Mapping.RemotePath -ne '\\10.0.2.4\qemu') { exit 4 }
exit 0
"#;
    let status = run_fixed_powershell(script)?;
    match status {
        0 => Ok(LiveShareStatus { mapped: true }),
        3 => Ok(LiveShareStatus { mapped: false }),
        4 => Err("Windows drive L: is already mapped to a different location".into()),
        code => {
            Err(format!("Windows could not query the global L: mapping (exit code {code})").into())
        }
    }
}

#[cfg(windows)]
fn configure_windows_live_share(
    enable: bool,
    credential: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if credential.len() != 64
        || !credential
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("live-share credential must be 64 lowercase hexadecimal characters".into());
    }
    let body = if enable {
        r#"$Remote='\\10.0.2.4\qemu'
$Mapping=Get-SmbGlobalMapping -LocalPath 'L:' -ErrorAction SilentlyContinue
if ($null -ne $Mapping -and $Mapping.RemotePath -ne $Remote) {
    throw 'Windows drive L: is already mapped to a different location'
}
if ($null -eq $Mapping) {
    $Password=ConvertTo-SecureString $env:LSW_LIVE_SMB_CREDENTIAL -AsPlainText -Force
    $Credential=New-Object Management.Automation.PSCredential('lsw',$Password)
    New-SmbGlobalMapping -LocalPath 'L:' -RemotePath $Remote -Credential $Credential -RequireIntegrity $true -RequirePrivacy $true -Persistent $true | Out-Null
}
$Label='HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\L\DefaultLabel'
New-Item -Path $Label -Force | Out-Null
Set-Item -Path $Label -Value 'Linux'
"#
    } else {
        r#"$Mapping=Get-SmbGlobalMapping -LocalPath 'L:' -ErrorAction SilentlyContinue
if ($null -ne $Mapping) {
    if ($Mapping.RemotePath -ne '\\10.0.2.4\qemu') {
        throw 'Windows drive L: is mapped to a location not owned by LSW'
    }
    Remove-SmbGlobalMapping -LocalPath 'L:' -Force
}
$Label='HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\DriveIcons\L'
Remove-Item -LiteralPath $Label -Recurse -Force -ErrorAction SilentlyContinue
"#
    };
    let script = format!(
        "$ErrorActionPreference='Stop'\ntry {{\n{body}}} catch {{\n\
         [Console]::Error.WriteLine(($_ | Out-String))\nexit 1\n}}\n"
    );
    let code = run_fixed_powershell_inner(&script, enable.then_some(credential))?;
    if code == 0 {
        Ok(())
    } else {
        Err(format!("Windows could not update the global L: mapping (exit code {code})").into())
    }
}

#[cfg(windows)]
fn run_fixed_powershell(script: &str) -> Result<i32, Box<dyn std::error::Error>> {
    run_fixed_powershell_inner(script, None)
}

#[cfg(windows)]
fn run_fixed_powershell_inner(
    script: &str,
    credential: Option<&str>,
) -> Result<i32, Box<dyn std::error::Error>> {
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(credential) = credential {
        command.env("LSW_LIVE_SMB_CREDENTIAL", credential);
    }
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .ok_or("Windows PowerShell standard input was unavailable")?
        .write_all(script.as_bytes())?;
    let output = child.wait_with_output()?;
    if output.stdout.len().saturating_add(output.stderr.len()) > 64 * 1024 {
        return Err("Windows live-share operation returned an oversized error".into());
    }
    if !output.status.success() && !matches!(output.status.code(), Some(3 | 4)) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut detail = match (stdout.trim(), stderr.trim()) {
            ("", "") => format!("PowerShell exited with {}", output.status),
            (stdout, "") => stdout.to_owned(),
            ("", stderr) => stderr.to_owned(),
            (stdout, stderr) => format!("{stderr}\n{stdout}"),
        };
        if let Some(credential) = credential {
            detail = detail.replace(credential, "<redacted>");
        }
        return Err(format!("Windows live-share operation failed: {detail}").into());
    }
    Ok(output.status.code().unwrap_or(-1))
}

#[cfg(windows)]
fn perform_windows_trim() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference='Stop'; Optimize-Volume -DriveLetter C -ReTrim",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    if output.stderr.len() > 64 * 1024 {
        return Err("Windows TRIM returned an oversized error".into());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    Err(format!("Windows TRIM failed: {}", detail.trim()).into())
}

#[cfg(windows)]
fn enable_windows_hibernation() -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("powercfg.exe")
        .args(["/hibernate", "on"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Windows could not enable hibernation (powercfg exit code {})",
            status.code().unwrap_or(-1)
        )
        .into())
    }
}

#[cfg(windows)]
fn request_windows_hibernation() -> Result<(), Box<dyn std::error::Error>> {
    Command::new("shutdown.exe")
        .arg("/h")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
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
    if !(matches!(action, "status" | "online") || action == "activate" && valid_product_key(key)) {
        return Err("unsupported activation helper operation".into());
    }
    run_license_powershell(action, key)
}

#[cfg(windows)]
fn run_license_powershell(action: &str, key: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let script = env::current_exe()?.with_file_name("license-helper.ps1");
    let metadata = fs::symlink_metadata(&script)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > 64 * 1024
    {
        return Err("the installed activation helper script is invalid".into());
    }
    let mut child = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg(action)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("PowerShell stdin was unavailable")?;
    if !key.is_empty() {
        stdin.write_all(key)?;
        stdin.write_all(b"\n")?;
    }
    drop(stdin);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err("Windows WMI licensing operation failed".into());
    }
    if output.stdout.len() > 16 * 1024 {
        return Err("Windows WMI licensing response was too large".into());
    }
    let output = String::from_utf8(output.stdout)?;
    if !output.lines().any(|line| line.starts_with("STATUS=")) {
        return Err("Windows WMI licensing operation returned no status".into());
    }
    Ok(output)
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
                ServiceKind::UserHelper => windows_user_service::run(configuration),
                ServiceKind::MaintenanceHelper => windows_maintenance_service::run(configuration),
            };
        }
        #[cfg(not(windows))]
        {
            return Err(match configuration.service_kind {
                ServiceKind::Agent => "--service is only supported on Windows",
                ServiceKind::LicenseHelper => "--license-helper is only supported on Windows",
                ServiceKind::UserHelper => "--user-helper is only supported on Windows",
                ServiceKind::MaintenanceHelper => {
                    "--maintenance-helper is only supported on Windows"
                }
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
    #[cfg(windows)]
    let identity_applied = apply_clone_identity(&configuration.token_file)?;
    let token = Arc::new(Mutex::new(read_token(&configuration.token_file)?));
    #[cfg(windows)]
    if !identity_applied {
        watch_for_clone_identity(configuration.token_file.clone(), Arc::downgrade(&token))?;
    }
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

#[cfg(windows)]
fn apply_clone_identity(token_file: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let identity = find_clone_identity(
        windows_path::volume_roots()
            .unwrap_or_default()
            .into_iter()
            .map(|root| root.join("lsw")),
    )?;
    let identity = match identity {
        Some(identity) => Some(identity),
        None => find_clone_identity(
            (b'D'..=b'Z')
                .map(char::from)
                .map(|letter| PathBuf::from(format!("{letter}:\\lsw"))),
        )?,
    };
    let Some(identity) = identity else {
        return Ok(false);
    };

    let name = fs::read_to_string(identity.join(CLONE_IDENTITY_NAME_FILE))?
        .trim()
        .to_owned();
    let valid_name = (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && name
            .as_bytes()
            .first()
            .zip(name.as_bytes().last())
            .is_some_and(|(first, last)| {
                first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric()
            });
    if !valid_name {
        return Err("clone identity name is invalid".into());
    }
    let mut token = read_token(&identity.join(CLONE_IDENTITY_TOKEN_FILE))?.into_bytes();
    let token_parent = token_file
        .parent()
        .ok_or("configured token path has no parent directory")?;
    if !token_parent.is_dir() {
        return Err("configured token parent is not a directory".into());
    }
    token.push(b'\n');
    let token_result = windows_path::replace_file(token_file, &token);
    token.fill(0);
    token_result?;
    windows_path::replace_file(
        &token_parent.join("instance.name"),
        format!("{name}\n").as_bytes(),
    )?;
    Ok(true)
}

#[cfg(windows)]
fn find_clone_identity(
    roots: impl IntoIterator<Item = PathBuf>,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    let mut identity = None;
    for root in roots {
        let marker = root.join(CLONE_IDENTITY_MARKER_FILE);
        if fs::read_to_string(&marker)
            .map(|value| value.trim() == "LSW-CLONE-IDENTITY")
            .unwrap_or(false)
            && identity.replace(root).is_some()
        {
            return Err("more than one LSW clone identity volume is attached".into());
        }
    }
    Ok(identity)
}

#[cfg(windows)]
fn watch_for_clone_identity(
    token_file: PathBuf,
    token: Weak<Mutex<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    watch_for_clone_identity_with_timing(
        token_file,
        token,
        IDENTITY_DISCOVERY_TIMEOUT,
        IDENTITY_DISCOVERY_FAST_TIMEOUT,
        IDENTITY_DISCOVERY_INTERVAL,
        IDENTITY_DISCOVERY_SLOW_INTERVAL,
    )
}

#[cfg(windows)]
fn watch_for_clone_identity_with_timing(
    token_file: PathBuf,
    token: Weak<Mutex<String>>,
    discovery_timeout: Duration,
    fast_timeout: Duration,
    fast_interval: Duration,
    slow_interval: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    thread::Builder::new()
        .name("lsw-identity-watch".to_owned())
        .spawn(move || {
            let started = Instant::now();
            let deadline = started + discovery_timeout;
            let fast_deadline = started + fast_timeout.min(discovery_timeout);
            while Instant::now() < deadline {
                let Some(token) = token.upgrade() else {
                    return;
                };
                match apply_clone_identity(&token_file) {
                    Ok(true) => match read_token(&token_file) {
                        Ok(replacement) => match token.lock() {
                            Ok(mut current) => {
                                *current = replacement;
                                if !cfg!(test) {
                                    write_stderr(format_args!(
                                        "lsw-agent: applied late-mounted boot identity"
                                    ));
                                }
                            }
                            Err(_) => write_stderr(format_args!(
                                "lsw-agent: boot identity token lock was poisoned"
                            )),
                        },
                        Err(error) => write_stderr(format_args!(
                            "lsw-agent: could not load the applied boot identity: {error}"
                        )),
                    },
                    Ok(false) => {
                        drop(token);
                        let now = Instant::now();
                        let interval = if now < fast_deadline {
                            fast_interval
                        } else {
                            slow_interval
                        };
                        thread::sleep(interval.min(deadline.saturating_duration_since(now)));
                        continue;
                    }
                    Err(error) => write_stderr(format_args!(
                        "lsw-agent: could not apply the late-mounted boot identity: {error}"
                    )),
                }
                return;
            }
        })?;
    Ok(())
}

fn run_stoppable_listener(
    listener: &TcpListener,
    configuration: &Configuration,
    token: &Arc<Mutex<String>>,
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
    token: &Arc<Mutex<String>>,
    active_sessions: &Arc<AtomicUsize>,
) -> bool {
    let session_token = match token.lock() {
        Ok(token) => token.clone(),
        Err(_) => {
            write_stderr(format_args!("lsw-agent: agent token lock was poisoned"));
            return configuration.once;
        }
    };
    if configuration.once {
        if let Err(error) = handle_connection(stream, &session_token) {
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
    Signal(SessionSignal),
    Disconnect,
    Heartbeat(Instant),
    ProtocolError(String),
}

#[derive(Debug, Eq, PartialEq)]
enum SessionEnd {
    Normal,
    Cancelled,
    Signalled(i32),
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
    // Windows can propagate a listening socket's nonblocking mode to accepted
    // sockets. Service mode makes the listener nonblocking so SCM stop signals
    // remain responsive; each independent session must return to blocking I/O
    // before its bounded handshake starts.
    stream.set_nonblocking(false)?;
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
    let (mut request_frame, session_mode, environment, detached) = if request_frame.kind
        == FrameKind::SessionOptions
    {
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
        if request_frame.kind == FrameKind::SessionLease {
            send_error(
                &mut stream,
                "SESSION_OPTIONS accepts only one SESSION_LEASE",
            )?;
            return Err("client sent duplicate SESSION_LEASE frames".into());
        }
        let environment = if request_frame.kind == FrameKind::ProcessEnvironment {
            let environment = match ProcessEnvironment::decode(&request_frame.payload) {
                Ok(environment) => environment,
                Err(error) => {
                    send_error(&mut stream, &error.to_string())?;
                    return Err(error.into());
                }
            };
            request_frame = read_frame(&mut stream)?;
            environment
        } else {
            ProcessEnvironment::default()
        };
        let detached = if request_frame.kind == FrameKind::SessionDetach {
            if !request_frame.payload.is_empty() {
                send_error(&mut stream, "SESSION_DETACH payload must be empty")?;
                return Err("client sent an invalid SESSION_DETACH payload".into());
            }
            request_frame = read_frame(&mut stream)?;
            true
        } else {
            false
        };
        if !matches!(
            request_frame.kind,
            FrameKind::Start | FrameKind::TerminalStart
        ) {
            send_error(
                &mut stream,
                "SESSION_OPTIONS must be followed by optional SESSION_LEASE, PROCESS_ENVIRONMENT, and SESSION_DETACH frames, then START or TERMINAL_START",
            )?;
            return Err("client sent an invalid controlled-session request".into());
        }
        (
            request_frame,
            SessionMode::Controlled { options, lease },
            environment,
            detached,
        )
    } else {
        (
            request_frame,
            SessionMode::Legacy,
            ProcessEnvironment::default(),
            false,
        )
    };
    match request_frame.kind {
        FrameKind::Start => run_process_request(
            stream,
            &request_frame.payload,
            session_mode,
            &environment,
            detached,
        ),
        FrameKind::TerminalStart => {
            if detached || !environment.is_empty() {
                send_error(
                    &mut stream,
                    "terminal sessions do not accept detached mode or environment injection",
                )?;
                return Err("client sent invalid terminal-session options".into());
            }
            run_terminal_request(stream, &request_frame.payload, session_mode)
        }
        FrameKind::FilePut => receive_file(stream, &request_frame.payload),
        FrameKind::FileGet => send_file(stream, &request_frame.payload),
        FrameKind::PowerHibernate => {
            hibernate_guest(stream, &request_frame.payload, expected_token)
        }
        FrameKind::UserCreate => create_user(stream, &mut request_frame.payload, expected_token),
        FrameKind::UserSetRole => set_user_role(stream, &request_frame.payload, expected_token),
        FrameKind::MaintenanceTrim => {
            maintenance_trim(stream, &request_frame.payload, expected_token)
        }
        FrameKind::WindowsSudoQuery => {
            windows_sudo_query(stream, &request_frame.payload, expected_token)
        }
        FrameKind::WindowsSudoConfigure => {
            windows_sudo_configure(stream, &request_frame.payload, expected_token)
        }
        FrameKind::LiveShareQuery => {
            live_share_query(stream, &request_frame.payload, expected_token)
        }
        FrameKind::LiveShareConfigure => {
            live_share_configure(stream, &request_frame.payload, expected_token)
        }
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
        CAPABILITY_PROCESS_ENVIRONMENT_V1.to_owned(),
        CAPABILITY_DETACHED_RUN_V1.to_owned(),
        CAPABILITY_SESSION_SIGNAL_V1.to_owned(),
    ];
    #[cfg(windows)]
    let capabilities = {
        let mut capabilities = capabilities;
        capabilities.push(CAPABILITY_CONPTY_V1.to_owned());
        capabilities.push(CAPABILITY_TERMINAL_RESIZE_V1.to_owned());
        capabilities.push(CAPABILITY_POWER_HIBERNATE_V1.to_owned());
        capabilities.push(CAPABILITY_USER_ACCOUNT_V1.to_owned());
        capabilities.push(CAPABILITY_USER_ACCOUNT_ROLE_V1.to_owned());
        capabilities.push(CAPABILITY_MAINTENANCE_TRIM_V1.to_owned());
        capabilities.push(CAPABILITY_WINDOWS_SUDO_V1.to_owned());
        capabilities.push(CAPABILITY_LIVE_SHARE_V1.to_owned());
        capabilities
    };
    capabilities
}

fn create_user(
    mut stream: TcpStream,
    payload: &mut [u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = UserCreateRequest::decode(payload);
    payload.fill(0);
    let mut request = match request {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error.into());
        }
    };
    #[cfg(windows)]
    let result = forward_user_create(&request, expected_token);
    #[cfg(not(windows))]
    let result: Result<(), Box<dyn std::error::Error>> = {
        let _ = expected_token;
        Err("Windows user creation is unavailable on this platform".into())
    };
    request.password.fill(0);
    if let Err(error) = result {
        send_error(&mut stream, &error.to_string())?;
        return Err(error);
    }
    write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
    Ok(())
}

#[cfg(windows)]
fn forward_user_create(
    request: &UserCreateRequest,
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = connect_user_helper(expected_token, CAPABILITY_USER_ACCOUNT_V1)?;
    let mut frame = Frame::new(FrameKind::UserCreate, request.encode()?);
    let write_result = write_frame(&mut stream, &frame);
    frame.payload.fill(0);
    write_result?;
    read_user_helper_response(&mut stream)
}

fn set_user_role(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = match UserSetRoleRequest::decode(payload) {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error.into());
        }
    };
    #[cfg(windows)]
    let result = forward_user_set_role(&request, expected_token);
    #[cfg(not(windows))]
    let result: Result<(), Box<dyn std::error::Error>> = {
        let _ = (&request, expected_token);
        Err("Windows user role changes are unavailable on this platform".into())
    };
    if let Err(error) = result {
        send_error(&mut stream, &error.to_string())?;
        return Err(error);
    }
    write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
    Ok(())
}

#[cfg(windows)]
fn forward_user_set_role(
    request: &UserSetRoleRequest,
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = connect_user_helper(expected_token, CAPABILITY_USER_ACCOUNT_ROLE_V1)?;
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::UserSetRole, request.encode()?),
    )?;
    read_user_helper_response(&mut stream)
}

#[cfg(windows)]
fn connect_user_helper(
    expected_token: &str,
    required_capability: &str,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let mut stream = connect_windows_helper(USER_HELPER_SERVICE, USER_HELPER_PORT, "account")?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token: expected_token.to_owned(),
    };
    write_frame(&mut stream, &Frame::new(FrameKind::Hello, hello.encode()?))?;
    let response = read_frame(&mut stream)?;
    if response.kind != FrameKind::HelloOk {
        return Err("Windows account helper rejected authentication".into());
    }
    let hello = ServerHello::decode(&response.payload)?;
    if hello.version != AGENT_PROTOCOL_VERSION
        || !hello
            .capabilities
            .iter()
            .any(|capability| capability == required_capability)
    {
        return Err("Windows account helper returned incompatible capabilities".into());
    }
    Ok(stream)
}

#[cfg(windows)]
fn read_user_helper_response(stream: &mut TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let response = read_frame(stream)?;
    match response.kind {
        FrameKind::Pong if response.payload.is_empty() => Ok(()),
        FrameKind::Error => Err(format!(
            "Windows account helper refused the request: {}",
            String::from_utf8_lossy(&response.payload)
        )
        .into()),
        _ => Err("Windows account helper returned an invalid response".into()),
    }
}

fn maintenance_trim(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !payload.is_empty() {
        send_error(&mut stream, "MAINTENANCE_TRIM payload must be empty")?;
        return Err("client sent an invalid maintenance request".into());
    }
    #[cfg(windows)]
    let result = forward_maintenance_trim(expected_token);
    #[cfg(not(windows))]
    let result: Result<(), Box<dyn std::error::Error>> = {
        let _ = expected_token;
        Err("Windows maintenance is unavailable on this platform".into())
    };
    if let Err(error) = result {
        send_error(&mut stream, &error.to_string())?;
        return Err(error);
    }
    write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
    Ok(())
}

fn windows_sudo_query(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !payload.is_empty() {
        send_error(&mut stream, "WINDOWS_SUDO_QUERY payload must be empty")?;
        return Err("client sent an invalid Windows sudo query".into());
    }
    #[cfg(windows)]
    let result = forward_windows_sudo_query(expected_token);
    #[cfg(not(windows))]
    let result: Result<WindowsSudoStatus, Box<dyn std::error::Error>> = {
        let _ = expected_token;
        Err("Windows sudo is unavailable on this platform".into())
    };
    match result {
        Ok(status) => write_frame(
            &mut stream,
            &Frame::new(FrameKind::WindowsSudoStatus, status.encode()),
        )?,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error);
        }
    }
    Ok(())
}

fn windows_sudo_configure(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = match WindowsSudoConfigureRequest::decode(payload) {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error.into());
        }
    };
    #[cfg(windows)]
    let result = forward_windows_sudo_configure(expected_token, request);
    #[cfg(not(windows))]
    let result: Result<(), Box<dyn std::error::Error>> = {
        let _ = (expected_token, request);
        Err("Windows sudo is unavailable on this platform".into())
    };
    if let Err(error) = result {
        send_error(&mut stream, &error.to_string())?;
        return Err(error);
    }
    write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
    Ok(())
}

fn live_share_query(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !payload.is_empty() {
        send_error(&mut stream, "LIVE_SHARE_QUERY payload must be empty")?;
        return Err("client sent an invalid live-share query".into());
    }
    #[cfg(windows)]
    let result = forward_live_share_query(expected_token);
    #[cfg(not(windows))]
    let result: Result<LiveShareStatus, Box<dyn std::error::Error>> = {
        let _ = expected_token;
        Err("Windows live-share mapping is unavailable on this platform".into())
    };
    match result {
        Ok(status) => write_frame(
            &mut stream,
            &Frame::new(FrameKind::LiveShareStatus, status.encode()),
        )?,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error);
        }
    }
    Ok(())
}

fn live_share_configure(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = match LiveShareConfigureRequest::decode(payload) {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error.into());
        }
    };
    #[cfg(windows)]
    let result = forward_live_share_configure(expected_token, request);
    #[cfg(not(windows))]
    let result: Result<(), Box<dyn std::error::Error>> = {
        let _ = (expected_token, request);
        Err("Windows live-share mapping is unavailable on this platform".into())
    };
    if let Err(error) = result {
        send_error(&mut stream, &error.to_string())?;
        return Err(error);
    }
    write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
    Ok(())
}

#[cfg(windows)]
fn forward_maintenance_trim(expected_token: &str) -> Result<(), Box<dyn std::error::Error>> {
    forward_maintenance_operation(
        expected_token,
        FrameKind::MaintenanceTrim,
        CAPABILITY_MAINTENANCE_TRIM_V1,
    )
}

#[cfg(windows)]
fn forward_maintenance_operation(
    expected_token: &str,
    request_kind: FrameKind,
    required_capability: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !matches!(
        request_kind,
        FrameKind::MaintenanceTrim | FrameKind::MaintenanceHibernate
    ) {
        return Err("invalid fixed maintenance operation".into());
    }
    let mut stream = connect_maintenance_helper(expected_token, required_capability)?;
    write_frame(&mut stream, &Frame::new(request_kind, Vec::new()))?;
    read_maintenance_fixed_response(&mut stream)
}

#[cfg(windows)]
fn forward_windows_sudo_query(
    expected_token: &str,
) -> Result<WindowsSudoStatus, Box<dyn std::error::Error>> {
    let mut stream = connect_maintenance_helper(expected_token, CAPABILITY_WINDOWS_SUDO_V1)?;
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::WindowsSudoQuery, Vec::new()),
    )?;
    let response = read_frame(&mut stream)?;
    match response.kind {
        FrameKind::WindowsSudoStatus => Ok(WindowsSudoStatus::decode(&response.payload)?),
        FrameKind::Error => Err(format!(
            "Windows maintenance helper refused the sudo query: {}",
            String::from_utf8_lossy(&response.payload)
        )
        .into()),
        _ => Err("Windows maintenance helper returned an invalid sudo status".into()),
    }
}

#[cfg(windows)]
fn forward_windows_sudo_configure(
    expected_token: &str,
    request: WindowsSudoConfigureRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = connect_maintenance_helper(expected_token, CAPABILITY_WINDOWS_SUDO_V1)?;
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::WindowsSudoConfigure, request.encode()),
    )?;
    read_maintenance_fixed_response(&mut stream)
}

#[cfg(windows)]
fn forward_live_share_query(
    expected_token: &str,
) -> Result<LiveShareStatus, Box<dyn std::error::Error>> {
    let mut stream = connect_maintenance_helper(expected_token, CAPABILITY_LIVE_SHARE_V1)?;
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::LiveShareQuery, Vec::new()),
    )?;
    let response = read_frame(&mut stream)?;
    match response.kind {
        FrameKind::LiveShareStatus => Ok(LiveShareStatus::decode(&response.payload)?),
        FrameKind::Error => Err(format!(
            "Windows maintenance helper refused the live-share query: {}",
            String::from_utf8_lossy(&response.payload)
        )
        .into()),
        _ => Err("Windows maintenance helper returned an invalid live-share status".into()),
    }
}

#[cfg(windows)]
fn forward_live_share_configure(
    expected_token: &str,
    request: LiveShareConfigureRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = connect_maintenance_helper(expected_token, CAPABILITY_LIVE_SHARE_V1)?;
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::LiveShareConfigure, request.encode()),
    )?;
    read_maintenance_fixed_response(&mut stream)
}

#[cfg(windows)]
fn connect_maintenance_helper(
    expected_token: &str,
    required_capability: &str,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let mut stream = connect_windows_helper(
        MAINTENANCE_HELPER_SERVICE,
        MAINTENANCE_HELPER_PORT,
        "maintenance",
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token: expected_token.to_owned(),
    };
    write_frame(&mut stream, &Frame::new(FrameKind::Hello, hello.encode()?))?;
    let response = read_frame(&mut stream)?;
    if response.kind != FrameKind::HelloOk {
        return Err("Windows maintenance helper rejected authentication".into());
    }
    let hello = ServerHello::decode(&response.payload)?;
    if hello.version != AGENT_PROTOCOL_VERSION
        || !hello
            .capabilities
            .iter()
            .any(|capability| capability == required_capability)
    {
        return Err("Windows maintenance helper returned incompatible capabilities".into());
    }
    Ok(stream)
}

#[cfg(windows)]
fn read_maintenance_fixed_response(
    stream: &mut TcpStream,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = read_frame(stream)?;
    match response.kind {
        FrameKind::Pong if response.payload.is_empty() => Ok(()),
        FrameKind::Error => Err(format!(
            "Windows maintenance helper refused the request: {}",
            String::from_utf8_lossy(&response.payload)
        )
        .into()),
        _ => Err("Windows maintenance helper returned an invalid response".into()),
    }
}

#[cfg(windows)]
fn hibernate_guest(
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
fn hibernate_guest(
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

fn run_process_request(
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

fn ensure_no_link_boundary(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
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

    fn id(&self) -> u32 {
        self.process.id()
    }

    fn try_wait_and_cleanup(&mut self) -> io::Result<Option<ExitStatus>> {
        let Some(status) = self.process.try_wait()? else {
            return Ok(None);
        };
        self.terminate_tree(SESSION_CANCEL_EXIT_CODE)?;
        Ok(Some(status))
    }

    fn wait_and_cleanup(&mut self) -> io::Result<ExitStatus> {
        let status = self.process.wait()?;
        self.terminate_tree(SESSION_CANCEL_EXIT_CODE)?;
        Ok(status)
    }

    /// Returns the leader status and whether this call won the race to issue
    /// process-tree termination.
    fn terminate(&mut self) -> io::Result<(ExitStatus, bool)> {
        self.terminate_with(SESSION_CANCEL_EXIT_CODE)
    }

    fn terminate_with(&mut self, exit_code: i32) -> io::Result<(ExitStatus, bool)> {
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
    fn terminate_tree(&mut self, exit_code: i32) -> io::Result<()> {
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
    fn kill(&mut self) -> io::Result<()> {
        self.terminate_tree(SESSION_CANCEL_EXIT_CODE)
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
        if self.terminate_tree(SESSION_CANCEL_EXIT_CODE).is_ok() {
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

fn spawn_request(
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

fn spawn_shell(
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

fn spawn_program(
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
fn spawn_terminal_input_bridge(
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
mod windows_maintenance_service;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_service;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_sudo;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_path;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_user;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_user_service;

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
    UserHelper,
    MaintenanceHelper,
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
                "--user-helper" => {
                    service = true;
                    service_kind = ServiceKind::UserHelper;
                }
                "--maintenance-helper" => {
                    service = true;
                    service_kind = ServiceKind::MaintenanceHelper;
                }
                "--help" | "-h" => {
                    println!(
                        "lsw-agent --token-file PATH [--listen IP:PORT] [--max-sessions N] [--once] [--service]\n\
                         The default listener is 0.0.0.0:35040 inside the restricted guest network.\n\
                         --service runs LSWAgent under the Windows Service Control Manager."
                    );
                    std::process::exit(0);
                }
                unknown => return Err(format!("unknown agent option {unknown:?}").into()),
            }
            index += 1;
        }
        if matches!(
            service_kind,
            ServiceKind::LicenseHelper | ServiceKind::UserHelper | ServiceKind::MaintenanceHelper
        ) && !listen.ip().is_loopback()
        {
            return Err("privileged helper listeners must use guest loopback".into());
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
