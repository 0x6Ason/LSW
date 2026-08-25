// SPDX-License-Identifier: GPL-3.0-or-later

//! Authenticated, low-lifetime broker inside one interactive Windows session.

use std::env;
use std::ffi::OsString;
use std::io::{self, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    constant_time_token_eq, encode_process_id, read_frame, write_frame, ClientHello,
    DesktopLiveShareRequest, Frame, FrameKind, GuiIconRequest, GuiInputEvent, GuiStartRequest,
    GuiWindowClosed, GuiWindowResize, LiveShareStatus, ServerHello, AGENT_PROTOCOL_VERSION,
    CAPABILITY_DESKTOP_LIVE_SHARE_V1, CAPABILITY_GUI_ICON_V1, CAPABILITY_GUI_LAUNCH_V1,
    CAPABILITY_GUI_WINDOW_V1, DESKTOP_COMPANION_GUEST_PORT,
};

use super::{
    gui_damage::DamageTracker, send_error, windows_capture, windows_live_share,
    DESKTOP_COMPANION_IDLE_TIMEOUT, HANDSHAKE_TIMEOUT,
};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_ICON_BYTES: usize = 2 * 1024 * 1024;
const MAX_ICON_ERROR_BYTES: usize = 64 * 1024;
const ICON_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(16);
const GUI_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const DESKTOP_TOKEN_ENV: &str = "LSW_DESKTOP_TOKEN";
const LIVE_SHARE_TOKEN_ENV: &str = "LSW_LIVE_SHARE_TOKEN";
const DESKTOP_USER_ENV: &str = "LSW_DESKTOP_USER";

pub(super) fn run(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let listen = match arguments {
        [option, value] if option == "--listen" => value
            .to_str()
            .ok_or("desktop companion listen address must be valid UTF-8")?
            .parse::<SocketAddr>()?,
        _ => return Err("usage: lsw-agent --desktop-companion --listen 127.0.0.1:35044".into()),
    };
    if !listen.ip().is_loopback() || listen.port() != DESKTOP_COMPANION_GUEST_PORT {
        return Err("desktop companion must use the fixed loopback endpoint".into());
    }
    let token = required_scoped_environment(DESKTOP_TOKEN_ENV)?;
    let live_share_token = required_scoped_environment(LIVE_SHARE_TOKEN_ENV)?;
    let expected_user =
        env::var(DESKTOP_USER_ENV).map_err(|_| "desktop companion user identity is missing")?;
    lsw_core::validate_windows_user_name(&expected_user)?;

    let listener = TcpListener::bind(listen)?;
    listener.set_nonblocking(true)?;
    let mut children = Vec::<Child>::new();
    let mut idle_deadline = Instant::now() + DESKTOP_COMPANION_IDLE_TIMEOUT;
    loop {
        reap_children(&mut children);
        let mapped = windows_live_share::query().unwrap_or(false);
        if mapped || !children.is_empty() {
            idle_deadline = Instant::now() + DESKTOP_COMPANION_IDLE_TIMEOUT;
        }
        match listener.accept() {
            Ok((mut stream, peer)) if peer.ip().is_loopback() => {
                if let Err(error) = handle_connection(
                    &mut stream,
                    &token,
                    &live_share_token,
                    &expected_user,
                    &mut children,
                ) {
                    let _ = send_error(&mut stream, &error.to_string());
                    super::write_stderr(format_args!(
                        "lsw desktop companion rejected a connection: {error}"
                    ));
                }
                idle_deadline = Instant::now() + DESKTOP_COMPANION_IDLE_TIMEOUT;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if children.is_empty() && !mapped && Instant::now() >= idle_deadline {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    expected_token: &str,
    live_share_token: &str,
    expected_user: &str,
    children: &mut Vec<Child>,
) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let hello = read_frame(stream)?;
    if hello.kind != FrameKind::Hello {
        send_error(stream, "the first desktop-companion frame must be HELLO")?;
        return Ok(());
    }
    let hello = ClientHello::decode(&hello.payload)?;
    if hello.version != AGENT_PROTOCOL_VERSION
        || !constant_time_token_eq(&hello.token, expected_token)
    {
        send_error(stream, "desktop companion authentication failed")?;
        return Ok(());
    }
    let hello = ServerHello {
        version: AGENT_PROTOCOL_VERSION,
        capabilities: vec![
            CAPABILITY_GUI_LAUNCH_V1.to_owned(),
            CAPABILITY_GUI_ICON_V1.to_owned(),
            CAPABILITY_GUI_WINDOW_V1.to_owned(),
            CAPABILITY_DESKTOP_LIVE_SHARE_V1.to_owned(),
        ],
    };
    write_frame(stream, &Frame::new(FrameKind::HelloOk, hello.encode()?))?;

    let request = read_frame(stream)?;
    match request.kind {
        FrameKind::GuiStart => {
            let request = GuiStartRequest::decode(&request.payload)?;
            require_user(&request.user_name, expected_user)?;
            if request.mount_live_share {
                windows_live_share::configure(true, live_share_token)?;
            }
            let child = spawn_gui(&request)?;
            let process_id = child.id();
            children.push(child);
            write_frame(
                stream,
                &Frame::new(FrameKind::Started, encode_process_id(process_id)),
            )?;
        }
        FrameKind::GuiWindowOpen => {
            let request = GuiStartRequest::decode(&request.payload)?;
            require_user(&request.user_name, expected_user)?;
            if request.mount_live_share {
                windows_live_share::configure(true, live_share_token)?;
            }
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;
            stream_gui_window(stream, &request)?;
        }
        FrameKind::GuiIcon => {
            let request = GuiIconRequest::decode(&request.payload)?;
            require_user(&request.user_name, expected_user)?;
            match extract_icon(&request.program) {
                Ok(icon) => write_frame(stream, &Frame::new(FrameKind::GuiIconData, icon))?,
                Err(error) => send_error(stream, &error.to_string())?,
            }
        }
        FrameKind::DesktopLiveShareConfigure => {
            let request = DesktopLiveShareRequest::decode(&request.payload)?;
            require_user(&request.user_name, expected_user)?;
            match windows_live_share::configure(request.enable, live_share_token) {
                Ok(()) => write_frame(
                    stream,
                    &Frame::new(
                        FrameKind::LiveShareStatus,
                        LiveShareStatus {
                            mapped: windows_live_share::query()?,
                        }
                        .encode(),
                    ),
                )?,
                Err(error) => send_error(stream, &error.to_string())?,
            }
        }
        _ => send_error(
            stream,
            "desktop companion accepts only GUI_START, GUI_WINDOW_OPEN, GUI_ICON, or DESKTOP_LIVE_SHARE_CONFIGURE",
        )?,
    }
    Ok(())
}

enum GuiControl {
    Frame(Frame),
    Disconnected(String),
}

fn stream_gui_window(
    stream: &mut TcpStream,
    request: &GuiStartRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut child = spawn_gui(request)?;
    let process_id = child.id();
    let result = stream_gui_window_inner(stream, process_id, &mut child);
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn stream_gui_window_inner(
    stream: &mut TcpStream,
    process_id: u32,
    child: &mut Child,
) -> Result<(), Box<dyn std::error::Error>> {
    let window = windows_capture::find_process_window(process_id, child)?;
    let mut capture = windows_capture::CaptureSession::start(&window)?;
    let first_deadline = Instant::now() + FIRST_CAPTURE_TIMEOUT;
    let first = loop {
        if let Some(frame) = capture.next_frame(Duration::from_millis(250))? {
            break frame;
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("GUI process exited with {status} before its first frame").into());
        }
        if Instant::now() >= first_deadline {
            return Err("timed out waiting for the first Windows Graphics Capture frame".into());
        }
    };
    let mut width = first.width;
    let mut height = first.height;
    write_frame(
        stream,
        &Frame::new(
            FrameKind::GuiWindowReady,
            window.ready(process_id, width, height)?.encode()?,
        ),
    )?;
    let mut damage = DamageTracker::default();
    send_damages(stream, damage.update(width, height, &first.bgra)?)?;

    let mut reader = stream.try_clone()?;
    let (control_sender, control_receiver) = mpsc::sync_channel(128);
    let reader_thread = thread::spawn(move || loop {
        match read_frame(&mut reader) {
            Ok(frame) => {
                if control_sender.send(GuiControl::Frame(frame)).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = control_sender.send(GuiControl::Disconnected(error.to_string()));
                return;
            }
        }
    });

    let mut close_deadline = None;
    let mut window_missing_since = None;
    let result = 'session: loop {
        'control: loop {
            match control_receiver.try_recv() {
                Ok(GuiControl::Frame(frame)) => match frame.kind {
                    FrameKind::GuiWindowInput => {
                        if let Err(error) = GuiInputEvent::decode(&frame.payload)
                            .map_err(|error| error.into())
                            .and_then(|event| window.input(event).map_err(|error| error.into()))
                        {
                            break 'session Err(error);
                        }
                    }
                    FrameKind::GuiWindowResize => {
                        if let Err(error) = GuiWindowResize::decode(&frame.payload)
                            .map_err(|error| error.into())
                            .and_then(|resize| window.resize(resize).map_err(|error| error.into()))
                        {
                            break 'session Err(error);
                        }
                    }
                    FrameKind::GuiWindowClose if frame.payload.is_empty() => {
                        if let Err(error) = window.close() {
                            break 'session Err(error.into());
                        }
                        close_deadline = Some(Instant::now() + GUI_CLOSE_TIMEOUT);
                    }
                    FrameKind::GuiWindowClose => {
                        break 'session Err("GUI_WINDOW_CLOSE payload must be empty".into());
                    }
                    _ => {
                        break 'session Err("invalid frame in a seamless GUI session".into());
                    }
                },
                Ok(GuiControl::Disconnected(error)) => {
                    break 'session Err(format!("seamless GUI client disconnected: {error}").into());
                }
                Err(TryRecvError::Empty) => break 'control,
                Err(TryRecvError::Disconnected) => {
                    break 'session Err("seamless GUI input channel closed".into());
                }
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let exit_code = status.code().unwrap_or(1);
                if let Err(error) = send_gui_closed(stream, exit_code) {
                    break 'session Err(error);
                }
                break 'session Ok(());
            }
            Ok(None) => {}
            Err(error) => break 'session Err(error.into()),
        }
        if close_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            if let Err(error) = child.kill() {
                break 'session Err(error.into());
            }
            match child.wait() {
                Ok(status) => {
                    if let Err(error) = send_gui_closed(stream, status.code().unwrap_or(1)) {
                        break 'session Err(error);
                    }
                    break 'session Ok(());
                }
                Err(error) => break 'session Err(error.into()),
            }
        }
        if window.is_open() {
            window_missing_since = None;
        } else {
            let missing = window_missing_since.get_or_insert_with(Instant::now);
            if missing.elapsed() >= GUI_CLOSE_TIMEOUT {
                if let Err(error) = child.kill() {
                    break 'session Err(error.into());
                }
                match child.wait() {
                    Ok(status) => {
                        if let Err(error) = send_gui_closed(stream, status.code().unwrap_or(1)) {
                            break 'session Err(error);
                        }
                        break 'session Ok(());
                    }
                    Err(error) => break 'session Err(error.into()),
                }
            }
        }
        match capture.next_frame(CAPTURE_POLL_INTERVAL) {
            Ok(Some(frame)) => {
                if (frame.width, frame.height) != (width, height) {
                    width = frame.width;
                    height = frame.height;
                    let ready = window
                        .ready(process_id, width, height)
                        .and_then(|ready| Ok(ready.encode()?));
                    match ready.and_then(|payload| {
                        write_frame(stream, &Frame::new(FrameKind::GuiWindowReady, payload))
                            .map_err(|error| error.into())
                    }) {
                        Ok(()) => {}
                        Err(error) => break 'session Err(error),
                    }
                }
                match damage
                    .update(width, height, &frame.bgra)
                    .map_err(|error| error.into())
                    .and_then(|damages| send_damages(stream, damages))
                {
                    Ok(()) => {}
                    Err(error) => break 'session Err(error),
                }
            }
            Ok(None) => {}
            Err(error) => break 'session Err(error),
        }
    };
    if let Err(error) = &result {
        let _ = send_error(stream, &error.to_string());
    }
    let _ = stream.shutdown(Shutdown::Both);
    let _ = reader_thread.join();
    result
}

fn send_damages(
    stream: &mut TcpStream,
    damages: Vec<lsw_core::GuiWindowDamage>,
) -> Result<(), Box<dyn std::error::Error>> {
    for damage in damages {
        write_frame(
            stream,
            &Frame::new(FrameKind::GuiWindowDamage, damage.encode()?),
        )?;
    }
    Ok(())
}

fn send_gui_closed(
    stream: &mut TcpStream,
    exit_code: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(
        stream,
        &Frame::new(
            FrameKind::GuiWindowClosed,
            GuiWindowClosed { exit_code }.encode().to_vec(),
        ),
    )?;
    Ok(())
}

fn spawn_gui(request: &GuiStartRequest) -> Result<Child, Box<dyn std::error::Error>> {
    let mut command = Command::new(&request.request.argv[0]);
    command
        .args(&request.request.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    if let Some(directory) = &request.request.working_directory {
        command.current_dir(directory);
    }
    for (name, value) in &request.environment.variables {
        command.env(name, value);
    }
    remove_companion_secrets(&mut command);
    Ok(command.spawn()?)
}

fn extract_icon(program: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const SCRIPT: &str = concat!(
        "$ErrorActionPreference='Stop';",
        "Add-Type -AssemblyName System.Drawing;",
        "$p=[Environment]::GetEnvironmentVariable('LSW_ICON_SOURCE','Process');",
        "if(-not [IO.Path]::IsPathRooted($p)){$p=(Get-Command -CommandType Application $p).Source};",
        "$i=[Drawing.Icon]::ExtractAssociatedIcon($p);",
        "if($null -eq $i){exit 3};",
        "$s=[Console]::OpenStandardOutput();$i.Save($s);$i.Dispose();$s.Flush()"
    );
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .env("LSW_ICON_SOURCE", program)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    remove_companion_secrets(&mut command);
    command.env("LSW_ICON_SOURCE", program);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or("icon helper stdout is unavailable")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("icon helper stderr is unavailable")?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, MAX_ICON_BYTES + 1));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, MAX_ICON_ERROR_BYTES));
    let deadline = Instant::now() + ICON_DISCOVERY_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("Windows application icon discovery timed out".into());
        }
        thread::sleep(Duration::from_millis(25));
    };
    let output = stdout_reader
        .join()
        .map_err(|_| "icon stdout reader panicked")??;
    let error = stderr_reader
        .join()
        .map_err(|_| "icon stderr reader panicked")??;
    if !status.success() {
        return Err(format!(
            "Windows could not discover an icon for {program:?}: {}",
            String::from_utf8_lossy(&error).trim()
        )
        .into());
    }
    if output.len() < 6 || output.len() > MAX_ICON_BYTES || output[..4] != [0, 0, 1, 0] {
        return Err("Windows returned an invalid or oversized application icon".into());
    }
    Ok(output)
}

fn read_bounded(reader: impl Read, maximum: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take(u64::try_from(maximum).expect("icon output bound fits u64"))
        .read_to_end(&mut output)?;
    Ok(output)
}

fn remove_companion_secrets(command: &mut Command) {
    command
        .env_remove(DESKTOP_TOKEN_ENV)
        .env_remove(LIVE_SHARE_TOKEN_ENV)
        .env_remove(DESKTOP_USER_ENV);
}

fn reap_children(children: &mut Vec<Child>) {
    children.retain_mut(|child| match child.try_wait() {
        Ok(Some(_)) => false,
        Ok(None) => true,
        Err(_) => false,
    });
}

fn require_user(observed: &str, expected: &str) -> Result<(), Box<dyn std::error::Error>> {
    if observed.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err("desktop companion request does not match its Windows user session".into())
    }
}

fn required_scoped_environment(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value = env::var(name).map_err(|_| format!("desktop companion is missing {name}"))?;
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(format!("desktop companion received an invalid {name}").into());
    }
    Ok(value)
}
