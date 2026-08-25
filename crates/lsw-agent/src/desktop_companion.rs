// SPDX-License-Identifier: GPL-3.0-or-later

//! Authenticated, low-lifetime broker inside one interactive Windows session.

use std::env;
use std::ffi::OsString;
use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    constant_time_token_eq, encode_process_id, read_frame, write_frame, ClientHello,
    DesktopLiveShareRequest, Frame, FrameKind, GuiIconRequest, GuiStartRequest, LiveShareStatus,
    ServerHello, AGENT_PROTOCOL_VERSION, CAPABILITY_DESKTOP_LIVE_SHARE_V1, CAPABILITY_GUI_ICON_V1,
    CAPABILITY_GUI_LAUNCH_V1, DESKTOP_COMPANION_GUEST_PORT,
};

use super::{send_error, windows_live_share, DESKTOP_COMPANION_IDLE_TIMEOUT, HANDSHAKE_TIMEOUT};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_ICON_BYTES: usize = 2 * 1024 * 1024;
const MAX_ICON_ERROR_BYTES: usize = 64 * 1024;
const ICON_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
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
            "desktop companion accepts only GUI_START, GUI_ICON, or DESKTOP_LIVE_SHARE_CONFIGURE",
        )?,
    }
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
