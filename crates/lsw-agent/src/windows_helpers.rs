// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(windows)]
use super::*;

#[cfg(windows)]
pub(super) fn append_windows_service_log(message: std::fmt::Arguments<'_>) {
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
pub(super) fn run_license_client(
    arguments: &[std::ffi::OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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
pub(super) fn connect_windows_helper(
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
pub(super) fn run_license_helper(
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
pub(super) fn handle_license_helper_connection(
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
pub(super) fn read_bounded_line(
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
pub(super) fn run_user_helper(
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
pub(super) fn handle_user_helper_connection(
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
            CAPABILITY_DESKTOP_COMPANION_V1.to_owned(),
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
        FrameKind::DesktopCompanionStart => {
            let request = DesktopUserRequest::decode(&frame.payload);
            frame.payload.fill(0);
            let request = match request {
                Ok(request) => request,
                Err(error) => {
                    send_error(stream, &error.to_string())?;
                    return Ok(true);
                }
            };
            windows_desktop::launch_companion(&request.user_name, expected_token)
        }
        _ => {
            frame.payload.fill(0);
            send_error(
                stream,
                "account helper accepts only USER_CREATE, USER_SET_ROLE, or DESKTOP_COMPANION_START",
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
pub(super) fn run_maintenance_helper(
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
                match handle_maintenance_helper_connection(&mut stream, &token) {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(error) => return Err(error),
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
pub(super) fn handle_maintenance_helper_connection(
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
            CAPABILITY_MAINTENANCE_SHUTDOWN_V1.to_owned(),
            CAPABILITY_WINDOWS_SUDO_V1.to_owned(),
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
        | FrameKind::MaintenanceShutdown
        | FrameKind::WindowsSudoQuery
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
        FrameKind::MaintenanceShutdown => match request_windows_shutdown() {
            Ok(()) => write_frame(stream, &Frame::new(FrameKind::Pong, Vec::new()))?,
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
        _ => {
            frame.payload.fill(0);
            send_error(
                stream,
                "maintenance helper accepts only fixed maintenance and Windows sudo requests",
            )?;
        }
    }
    Ok(true)
}

#[cfg(windows)]
pub(super) fn perform_windows_trim() -> Result<(), Box<dyn std::error::Error>> {
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
pub(super) fn enable_windows_hibernation() -> Result<(), Box<dyn std::error::Error>> {
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
pub(super) fn request_windows_hibernation() -> Result<(), Box<dyn std::error::Error>> {
    Command::new("shutdown.exe")
        .arg("/h")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn request_windows_shutdown() -> Result<(), Box<dyn std::error::Error>> {
    Command::new("shutdown.exe")
        .args(WINDOWS_SHUTDOWN_ARGUMENTS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(any(windows, test))]
pub(super) fn valid_product_key(key: &[u8]) -> bool {
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
pub(super) fn perform_windows_license_operation(
    action: &str,
    key: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    if !(matches!(action, "status" | "online") || action == "activate" && valid_product_key(key)) {
        return Err("unsupported activation helper operation".into());
    }
    run_license_powershell(action, key)
}

#[cfg(windows)]
pub(super) fn run_license_powershell(
    action: &str,
    key: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
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
