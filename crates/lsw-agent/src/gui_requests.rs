// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) fn gui_start(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = match GuiStartRequest::decode(payload) {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error.into());
        }
    };
    #[cfg(windows)]
    let result = forward_gui_start(&request, expected_token);
    #[cfg(not(windows))]
    let result: Result<u32, Box<dyn std::error::Error>> = {
        let _ = (&request, expected_token);
        Err("Windows GUI launch is unavailable on this platform".into())
    };
    match result {
        Ok(process_id) => write_frame(
            &mut stream,
            &Frame::new(FrameKind::Started, encode_process_id(process_id)),
        )?,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error);
        }
    }
    Ok(())
}

pub(super) fn gui_window(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = match GuiStartRequest::decode(payload) {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error.into());
        }
    };
    #[cfg(windows)]
    {
        let result = forward_gui_window(&mut stream, &request, expected_token);
        if let Err(error) = result {
            let _ = send_error(&mut stream, &error.to_string());
            return Err(error);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (request, expected_token);
        send_error(
            &mut stream,
            "Windows seamless GUI capture is unavailable on this platform",
        )?;
        Err("Windows seamless GUI capture is unavailable on this platform".into())
    }
}

pub(super) fn gui_icon(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = match GuiIconRequest::decode(payload) {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error.into());
        }
    };
    #[cfg(windows)]
    let result = forward_gui_icon(&request, expected_token);
    #[cfg(not(windows))]
    let result: Result<Vec<u8>, Box<dyn std::error::Error>> = {
        let _ = (&request, expected_token);
        Err("Windows GUI icon discovery is unavailable on this platform".into())
    };
    match result {
        Ok(icon) => write_frame(&mut stream, &Frame::new(FrameKind::GuiIconData, icon))?,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error);
        }
    }
    Ok(())
}

pub(super) fn desktop_live_share_configure(
    mut stream: TcpStream,
    payload: &[u8],
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = match DesktopLiveShareRequest::decode(payload) {
        Ok(request) => request,
        Err(error) => {
            send_error(&mut stream, &error.to_string())?;
            return Err(error.into());
        }
    };
    #[cfg(windows)]
    let result = forward_desktop_live_share(&request, expected_token);
    #[cfg(not(windows))]
    let result: Result<LiveShareStatus, Box<dyn std::error::Error>> = {
        let _ = (&request, expected_token);
        Err("Windows desktop live-share mapping is unavailable on this platform".into())
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

#[cfg(windows)]
pub(super) fn forward_gui_window(
    host: &mut TcpStream,
    request: &GuiStartRequest,
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut desktop = connect_or_start_desktop_companion(
        &request.user_name,
        expected_token,
        CAPABILITY_GUI_WINDOW_V3,
    )?;
    desktop.set_read_timeout(None)?;
    desktop.set_write_timeout(None)?;
    write_frame(
        &mut desktop,
        &Frame::new(FrameKind::GuiWindowOpen, request.encode()?),
    )?;

    let mut host_reader = host.try_clone()?;
    let desktop_writer = desktop.try_clone()?;
    let input_relay = thread::spawn(move || relay_gui_controls(&mut host_reader, desktop_writer));

    let relay_result = loop {
        let frame = match read_frame(&mut desktop) {
            Ok(frame) => frame,
            Err(error) => break Err(error.into()),
        };
        if let Err(error) = validate_gui_event_frame(&frame) {
            break Err(error);
        }
        let terminal = matches!(frame.kind, FrameKind::GuiWindowClosed | FrameKind::Error);
        if let Err(error) = write_frame(host, &frame) {
            break Err(error.into());
        }
        if terminal {
            break Ok(());
        }
    };
    let _ = host.shutdown(Shutdown::Both);
    let _ = desktop.shutdown(Shutdown::Both);
    let input_result = input_relay
        .join()
        .map_err(|_| "seamless GUI input relay panicked".to_owned())?;
    match (relay_result, input_result) {
        (Err(error), Err(input_error)) => {
            Err(format!("{error}; input relay: {input_error}").into())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), _) => Ok(()),
    }
}

#[cfg(windows)]
pub(super) fn relay_gui_controls(
    host_reader: &mut TcpStream,
    mut desktop_writer: TcpStream,
) -> Result<(), String> {
    let result: Result<(), String> = (|| loop {
        let frame = read_frame(host_reader).map_err(|error| error.to_string())?;
        validate_gui_control_frame(&frame).map_err(|error| error.to_string())?;
        write_frame(&mut desktop_writer, &frame).map_err(|error| error.to_string())?;
    })();
    if result.is_err() {
        // Host EOF, protocol rejection, or a failed forward means no explicit
        // GUI_WINDOW_CLOSE/ack handshake can complete. Wake the companion's
        // reader immediately so it releases injected input and detaches the
        // presenter while leaving the guest window alive for a bounded exact
        // PID/HWND reattach.
        let _ = desktop_writer.shutdown(Shutdown::Both);
    }
    result
}

#[cfg(windows)]
pub(super) fn validate_gui_control_frame(frame: &Frame) -> Result<(), Box<dyn std::error::Error>> {
    match frame.kind {
        FrameKind::GuiWindowInput => {
            GuiInputEvent::decode(&frame.payload)?;
        }
        FrameKind::GuiWindowResize => {
            GuiWindowResize::decode(&frame.payload)?;
        }
        FrameKind::GuiWindowAction => {
            let action = GuiWindowAction::decode(&frame.payload)?;
            if !matches!(action, GuiWindowAction::Maximize | GuiWindowAction::Restore) {
                return Err("host may send only explicit maximize or restore state".into());
            }
        }
        FrameKind::GuiWindowClose if frame.payload.is_empty() => {}
        FrameKind::GuiWindowClose => return Err("GUI_WINDOW_CLOSE payload must be empty".into()),
        _ => return Err("invalid host frame in a seamless GUI session".into()),
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn validate_gui_event_frame(frame: &Frame) -> Result<(), Box<dyn std::error::Error>> {
    match frame.kind {
        FrameKind::GuiWindowReady => {
            GuiWindowReady::decode(&frame.payload)?;
        }
        FrameKind::GuiWindowDamage => {
            GuiWindowDamage::decode(&frame.payload)?;
        }
        FrameKind::GuiWindowDragHint => {
            GuiWindowDragHint::decode(&frame.payload)?;
        }
        FrameKind::GuiWindowClosed => {
            GuiWindowClosed::decode(&frame.payload)?;
        }
        FrameKind::GuiWindowAction => {
            GuiWindowAction::decode(&frame.payload)?;
        }
        FrameKind::Error => {}
        _ => return Err("invalid desktop-companion frame in a seamless GUI session".into()),
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn forward_gui_start(
    request: &GuiStartRequest,
    expected_token: &str,
) -> Result<u32, Box<dyn std::error::Error>> {
    let mut stream = connect_or_start_desktop_companion(
        &request.user_name,
        expected_token,
        CAPABILITY_GUI_LAUNCH_V1,
    )?;
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::GuiStart, request.encode()?),
    )?;
    let response = read_frame(&mut stream)?;
    match response.kind {
        FrameKind::Started => Ok(lsw_core::decode_process_id(&response.payload)?),
        FrameKind::Error => Err(format!(
            "Windows desktop companion refused GUI launch: {}",
            String::from_utf8_lossy(&response.payload)
        )
        .into()),
        _ => Err("Windows desktop companion returned an invalid GUI response".into()),
    }
}

#[cfg(windows)]
pub(super) fn forward_gui_icon(
    request: &GuiIconRequest,
    expected_token: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut stream = connect_or_start_desktop_companion(
        &request.user_name,
        expected_token,
        CAPABILITY_GUI_ICON_V1,
    )?;
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::GuiIcon, request.encode()?),
    )?;
    let response = read_frame(&mut stream)?;
    match response.kind {
        FrameKind::GuiIconData => Ok(response.payload),
        FrameKind::Error => Err(format!(
            "Windows desktop companion could not discover the icon: {}",
            String::from_utf8_lossy(&response.payload)
        )
        .into()),
        _ => Err("Windows desktop companion returned an invalid icon response".into()),
    }
}

#[cfg(windows)]
pub(super) fn forward_desktop_live_share(
    request: &DesktopLiveShareRequest,
    expected_token: &str,
) -> Result<LiveShareStatus, Box<dyn std::error::Error>> {
    let mut stream = connect_or_start_desktop_companion(
        &request.user_name,
        expected_token,
        CAPABILITY_DESKTOP_LIVE_SHARE_V1,
    )?;
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::DesktopLiveShareConfigure, request.encode()?),
    )?;
    let response = read_frame(&mut stream)?;
    match response.kind {
        FrameKind::LiveShareStatus => Ok(LiveShareStatus::decode(&response.payload)?),
        FrameKind::Error => Err(format!(
            "Windows desktop companion could not configure Linux (L:): {}",
            String::from_utf8_lossy(&response.payload)
        )
        .into()),
        _ => Err("Windows desktop companion returned an invalid live-share response".into()),
    }
}

#[cfg(windows)]
pub(super) fn connect_or_start_desktop_companion(
    user_name: &str,
    expected_token: &str,
    required_capability: &str,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let credential =
        lsw_core::derive_scoped_credential(expected_token, DESKTOP_COMPANION_CREDENTIAL_SCOPE)?;
    if let Ok(stream) = connect_desktop_companion(&credential, required_capability) {
        return Ok(stream);
    }
    let mut helper = connect_user_helper(expected_token, CAPABILITY_DESKTOP_COMPANION_V1)?;
    let request = DesktopUserRequest {
        user_name: user_name.to_owned(),
    };
    write_frame(
        &mut helper,
        &Frame::new(FrameKind::DesktopCompanionStart, request.encode()?),
    )?;
    read_user_helper_response(&mut helper)?;

    let deadline = Instant::now() + DESKTOP_COMPANION_START_TIMEOUT;
    loop {
        let last_error = match connect_desktop_companion(&credential, required_capability) {
            Ok(stream) => return Ok(stream),
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for the Windows desktop companion; last error: {last_error}"
            )
            .into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
pub(super) fn connect_desktop_companion(
    credential: &str,
    required_capability: &str,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let address = SocketAddr::from(([127, 0, 0, 1], DESKTOP_COMPANION_GUEST_PORT));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(250))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token: credential.to_owned(),
    };
    write_frame(&mut stream, &Frame::new(FrameKind::Hello, hello.encode()?))?;
    let response = read_frame(&mut stream)?;
    if response.kind != FrameKind::HelloOk {
        return Err("Windows desktop companion rejected authentication".into());
    }
    let hello = ServerHello::decode(&response.payload)?;
    if hello.version != AGENT_PROTOCOL_VERSION
        || !hello
            .capabilities
            .iter()
            .any(|capability| capability == required_capability)
    {
        return Err("Windows desktop companion returned incompatible capabilities".into());
    }
    Ok(stream)
}

#[cfg(windows)]
pub(super) fn forward_maintenance_trim(
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    forward_maintenance_operation(
        expected_token,
        FrameKind::MaintenanceTrim,
        CAPABILITY_MAINTENANCE_TRIM_V1,
    )
}

#[cfg(windows)]
pub(super) fn forward_maintenance_shutdown(
    expected_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    forward_maintenance_operation(
        expected_token,
        FrameKind::MaintenanceShutdown,
        CAPABILITY_MAINTENANCE_SHUTDOWN_V1,
    )
}

#[cfg(windows)]
pub(super) fn forward_maintenance_operation(
    expected_token: &str,
    request_kind: FrameKind,
    required_capability: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !matches!(
        request_kind,
        FrameKind::MaintenanceTrim
            | FrameKind::MaintenanceHibernate
            | FrameKind::MaintenanceShutdown
    ) {
        return Err("invalid fixed maintenance operation".into());
    }
    let mut stream = connect_maintenance_helper(expected_token, required_capability)?;
    write_frame(&mut stream, &Frame::new(request_kind, Vec::new()))?;
    read_maintenance_fixed_response(&mut stream)
}

#[cfg(windows)]
pub(super) fn forward_windows_sudo_query(
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
pub(super) fn forward_windows_sudo_configure(
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
pub(super) fn connect_maintenance_helper(
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
pub(super) fn read_maintenance_fixed_response(
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
