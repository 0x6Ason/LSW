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
    encode_process_id, read_frame, write_frame, ClientHello, DesktopLiveShareRequest,
    FileGetRequest, FilePutRequest, Frame, FrameKind, GuiIconRequest, GuiStartRequest,
    LiveShareConfigureRequest, LiveShareStatus, ProcessEnvironment, ServerHello, SessionKind,
    SessionLease, SessionLeaseState, SessionOptions, SessionSignal, StartRequest,
    TerminalStartRequest, UserCreateRequest, UserSetRoleRequest, WindowsSudoConfigureRequest,
    WindowsSudoStatus, AGENT_GUEST_PORT, AGENT_PROTOCOL_VERSION, CAPABILITY_DETACHED_RUN_V1,
    CAPABILITY_PROCESS_ENVIRONMENT_V1, CAPABILITY_SESSION_CONTROL_V1, CAPABILITY_SESSION_LEASE_V1,
    CAPABILITY_SESSION_SIGNAL_V1, SESSION_CANCEL_EXIT_CODE,
};
#[cfg(windows)]
use lsw_core::{
    DesktopUserRequest, GuiInputEvent, GuiWindowAction, GuiWindowClosed, GuiWindowDamage,
    GuiWindowDragHint, GuiWindowReady, GuiWindowResize, TerminalSize, CAPABILITY_CONPTY_V1,
    CAPABILITY_DESKTOP_COMPANION_V1, CAPABILITY_DESKTOP_LIVE_SHARE_V1, CAPABILITY_GUI_ICON_V1,
    CAPABILITY_GUI_LAUNCH_V1, CAPABILITY_GUI_WINDOW_V3, CAPABILITY_LIVE_SHARE_V1,
    CAPABILITY_MAINTENANCE_HIBERNATE_V1, CAPABILITY_MAINTENANCE_SHUTDOWN_V1,
    CAPABILITY_MAINTENANCE_TRIM_V1, CAPABILITY_POWER_HIBERNATE_V1, CAPABILITY_TERMINAL_RESIZE_V1,
    CAPABILITY_USER_ACCOUNT_ROLE_V1, CAPABILITY_USER_ACCOUNT_V1, CAPABILITY_WINDOWS_SUDO_V1,
    CLONE_IDENTITY_MARKER_FILE, CLONE_IDENTITY_NAME_FILE, CLONE_IDENTITY_TOKEN_FILE,
    DESKTOP_COMPANION_CREDENTIAL_SCOPE, DESKTOP_COMPANION_GUEST_PORT, LICENSE_HELPER_GUEST_PORT,
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
#[cfg(windows)]
const DESKTOP_COMPANION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(windows)]
const DESKTOP_COMPANION_START_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(any(windows, test))]
const WINDOWS_SHUTDOWN_ARGUMENTS: [&str; 5] = ["/s", "/t", "0", "/d", "p:0:0"];

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

mod gui_requests;
mod process_bridge;
mod process_session;
mod requests;
mod server;
mod windows_helpers;

use gui_requests::*;
use process_bridge::*;
use process_session::*;
use requests::*;
use server::*;
#[cfg(any(windows, test))]
use windows_helpers::*;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_conpty;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_license_service;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_live_share;

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

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_desktop;

#[cfg(windows)]
mod desktop_companion;
#[cfg(windows)]
mod windows_capture;

#[cfg(any(windows, test))]
mod gui_damage;

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
