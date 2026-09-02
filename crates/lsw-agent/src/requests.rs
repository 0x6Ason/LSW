// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) fn handle_connection(
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
        FrameKind::MaintenanceShutdown => {
            maintenance_shutdown(stream, &request_frame.payload, expected_token)
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
        FrameKind::GuiStart => gui_start(stream, &request_frame.payload, expected_token),
        FrameKind::GuiWindowOpen => gui_window(stream, &request_frame.payload, expected_token),
        FrameKind::GuiIcon => gui_icon(stream, &request_frame.payload, expected_token),
        FrameKind::DesktopLiveShareConfigure => {
            desktop_live_share_configure(stream, &request_frame.payload, expected_token)
        }
        _ => {
            send_error(&mut stream, "unsupported request after HELLO")?;
            Err("client sent an unsupported request after authentication".into())
        }
    }
}

pub(super) fn agent_capabilities() -> Vec<String> {
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
        capabilities.push(CAPABILITY_MAINTENANCE_SHUTDOWN_V1.to_owned());
        capabilities.push(CAPABILITY_WINDOWS_SUDO_V1.to_owned());
        capabilities.push(CAPABILITY_LIVE_SHARE_V1.to_owned());
        capabilities.push(CAPABILITY_GUI_LAUNCH_V1.to_owned());
        capabilities.push(CAPABILITY_GUI_ICON_V1.to_owned());
        capabilities.push(CAPABILITY_GUI_WINDOW_V3.to_owned());
        capabilities.push(CAPABILITY_DESKTOP_LIVE_SHARE_V1.to_owned());
        capabilities
    };
    capabilities
}

pub(super) fn create_user(
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
pub(super) fn forward_user_create(
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

pub(super) fn set_user_role(
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
pub(super) fn forward_user_set_role(
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
pub(super) fn connect_user_helper(
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
pub(super) fn read_user_helper_response(
    stream: &mut TcpStream,
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn maintenance_trim(
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

pub(super) fn maintenance_shutdown(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !payload.is_empty() {
        send_error(&mut stream, "MAINTENANCE_SHUTDOWN payload must be empty")?;
        return Err("client sent an invalid shutdown request".into());
    }
    #[cfg(windows)]
    let result = forward_maintenance_shutdown(expected_token);
    #[cfg(not(windows))]
    let result: Result<(), Box<dyn std::error::Error>> = {
        let _ = expected_token;
        Err("Windows shutdown is unavailable on this platform".into())
    };
    if let Err(error) = result {
        send_error(&mut stream, &error.to_string())?;
        return Err(error);
    }
    write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))?;
    Ok(())
}

pub(super) fn windows_sudo_query(
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

pub(super) fn windows_sudo_configure(
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

pub(super) fn live_share_query(
    mut stream: TcpStream,
    payload: &[u8],
    _expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !payload.is_empty() {
        send_error(&mut stream, "LIVE_SHARE_QUERY payload must be empty")?;
        return Err("client sent an invalid live-share query".into());
    }
    #[cfg(windows)]
    let result = windows_live_share::query().map(|mapped| LiveShareStatus { mapped });
    #[cfg(not(windows))]
    let result: Result<LiveShareStatus, Box<dyn std::error::Error>> = {
        let _ = _expected_token;
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

pub(super) fn live_share_configure(
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
    let result =
        lsw_core::derive_scoped_credential(expected_token, lsw_core::LIVE_SHARE_CREDENTIAL_SCOPE)
            .map_err(|error| error.into())
            .and_then(|credential| windows_live_share::configure(request.enable, &credential));
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
