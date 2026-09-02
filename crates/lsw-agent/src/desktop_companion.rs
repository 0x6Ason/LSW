// SPDX-License-Identifier: GPL-3.0-or-later

//! Authenticated, low-lifetime broker inside one interactive Windows session.

use std::env;
use std::ffi::OsString;
use std::io::{self, Read};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    constant_time_token_eq, encode_process_id, read_frame, write_frame, ClientHello,
    DesktopLiveShareRequest, Frame, FrameKind, GuiIconRequest, GuiInputEvent, GuiStartRequest,
    GuiWindowAction, GuiWindowClosed, GuiWindowDragHint, GuiWindowResize, LiveShareStatus,
    ServerHello, AGENT_PROTOCOL_VERSION, CAPABILITY_DESKTOP_LIVE_SHARE_V1, CAPABILITY_GUI_ICON_V1,
    CAPABILITY_GUI_LAUNCH_V1, CAPABILITY_GUI_WINDOW_V3, DESKTOP_COMPANION_GUEST_PORT,
};

mod gui_stream;
use gui_stream::*;

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
const GUI_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONTROLS_PER_CAPTURE_POLL: usize = 64;
const MAX_CONNECTION_WORKERS: usize = 16;
const MAX_GUI_WINDOW_SESSIONS: usize = 16;
#[cfg(test)]
const MAX_DETACHED_GUI_WINDOWS: usize = MAX_GUI_WINDOW_SESSIONS;
const DESKTOP_TOKEN_ENV: &str = "LSW_DESKTOP_TOKEN";
const LIVE_SHARE_TOKEN_ENV: &str = "LSW_LIVE_SHARE_TOKEN";
const DESKTOP_USER_ENV: &str = "LSW_DESKTOP_USER";

trait RecoverableWindowIdentity {
    fn current_identity(&self) -> Option<(u32, u64)>;
}

impl RecoverableWindowIdentity for windows_capture::WindowHandle {
    fn current_identity(&self) -> Option<(u32, u64)> {
        self.validate_identity()
            .ok()
            .map(|()| (self.process_id(), self.id()))
    }
}

struct DetachedGuiWindow<W> {
    request: GuiStartRequest,
    process_id: u32,
    window_id: u64,
    window: W,
}

impl<W: RecoverableWindowIdentity> DetachedGuiWindow<W> {
    fn new(request: GuiStartRequest, window: W) -> Result<Self, Box<dyn std::error::Error>> {
        let (process_id, window_id) = window
            .current_identity()
            .ok_or("cannot retain a stale GUI window for recovery")?;
        Ok(Self {
            request,
            process_id,
            window_id,
            window,
        })
    }
}

struct DetachedGuiRegistry<W> {
    entries: Vec<DetachedGuiWindow<W>>,
    active_requests: Vec<GuiStartRequest>,
}

impl<W> Default for DetachedGuiRegistry<W> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            active_requests: Vec::new(),
        }
    }
}

impl<W: RecoverableWindowIdentity> DetachedGuiRegistry<W> {
    fn purge_stale(&mut self) {
        self.entries.retain(|entry| {
            entry.window.current_identity() == Some((entry.process_id, entry.window_id))
        });
    }

    fn claim(
        &mut self,
        request: &GuiStartRequest,
    ) -> Result<Option<W>, Box<dyn std::error::Error>> {
        self.purge_stale();
        if self.active_requests.iter().any(|active| active == request) {
            return Err(
                "this exact seamless GUI request already has an active presenter session".into(),
            );
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| &entry.request == request)
        {
            let window = self.entries.swap_remove(index).window;
            self.active_requests.push(request.clone());
            return Ok(Some(window));
        }
        if self.active_requests.len() + self.entries.len() >= MAX_GUI_WINDOW_SESSIONS {
            return Err(format!(
                "the desktop companion already owns {MAX_GUI_WINDOW_SESSIONS} active or recoverable GUI windows; close one before launching another"
            )
            .into());
        }
        self.active_requests.push(request.clone());
        Ok(None)
    }

    fn retain_claimed(
        &mut self,
        entry: DetachedGuiWindow<W>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let active_index = self
            .active_requests
            .iter()
            .position(|active| active == &entry.request)
            .ok_or("cannot retain a GUI window without its active request claim")?;
        if self
            .entries
            .iter()
            .any(|existing| existing.request == entry.request)
        {
            return Err(
                "the desktop companion already retains this exact GUI request; refusing ambiguous recovery state"
                    .into(),
            );
        }
        if self.active_requests.len() + self.entries.len() > MAX_GUI_WINDOW_SESSIONS {
            return Err(format!(
                "the GUI recovery registry exceeded its {MAX_GUI_WINDOW_SESSIONS} window limit"
            )
            .into());
        }
        self.active_requests.swap_remove(active_index);
        self.entries.push(entry);
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.active_requests.is_empty()
    }

    #[cfg(test)]
    fn insert(
        &mut self,
        request: GuiStartRequest,
        window: W,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.purge_stale();
        if self.active_requests.iter().any(|active| active == &request)
            || self.entries.iter().any(|entry| entry.request == request)
        {
            return Err("this exact GUI request already exists in the registry".into());
        }
        if self.active_requests.len() + self.entries.len() >= MAX_GUI_WINDOW_SESSIONS {
            return Err("the test GUI recovery registry is full".into());
        }
        self.entries.push(DetachedGuiWindow::new(request, window)?);
        Ok(())
    }

    #[cfg(test)]
    fn take(&mut self, request: &GuiStartRequest) -> Option<W> {
        self.purge_stale();
        let index = self
            .entries
            .iter()
            .position(|entry| &entry.request == request)?;
        Some(self.entries.swap_remove(index).window)
    }

    #[cfg(test)]
    fn ensure_launch_capacity(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.purge_stale();
        if self.active_requests.len() + self.entries.len() >= MAX_GUI_WINDOW_SESSIONS {
            return Err("the test GUI recovery registry is full".into());
        }
        Ok(())
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct SharedGuiRecoveryRegistry<W> {
    inner: Arc<Mutex<DetachedGuiRegistry<W>>>,
}

impl<W> Clone for SharedGuiRecoveryRegistry<W> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<W> Default for SharedGuiRecoveryRegistry<W> {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(DetachedGuiRegistry::default())),
        }
    }
}

impl<W: RecoverableWindowIdentity> SharedGuiRecoveryRegistry<W> {
    fn claim(
        &self,
        request: &GuiStartRequest,
    ) -> Result<(GuiSessionClaim<W>, Option<W>), Box<dyn std::error::Error>> {
        let recovered = lock_unpoisoned(&self.inner).claim(request)?;
        Ok((
            GuiSessionClaim {
                registry: self.clone(),
                request: Some(request.clone()),
            },
            recovered,
        ))
    }

    fn purge_stale(&self) {
        lock_unpoisoned(&self.inner).purge_stale();
    }

    fn is_empty(&self) -> bool {
        lock_unpoisoned(&self.inner).is_empty()
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let registry = lock_unpoisoned(&self.inner);
        (registry.active_requests.len(), registry.entries.len())
    }
}

struct GuiSessionClaim<W> {
    registry: SharedGuiRecoveryRegistry<W>,
    request: Option<GuiStartRequest>,
}

impl<W: RecoverableWindowIdentity> GuiSessionClaim<W> {
    fn retain(mut self, window: W) -> Result<(), Box<dyn std::error::Error>> {
        let request = self
            .request
            .as_ref()
            .expect("an armed GUI session claim has its request")
            .clone();
        // Validate the pinned PID/HWND outside the registry mutex. The lock is
        // used only for the atomic active-to-detached state transition.
        let entry = DetachedGuiWindow::new(request.clone(), window)?;
        lock_unpoisoned(&self.registry.inner).retain_claimed(entry)?;
        self.request = None;
        Ok(())
    }
}

impl<W> Drop for GuiSessionClaim<W> {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            let mut registry = lock_unpoisoned(&self.registry.inner);
            if let Some(index) = registry
                .active_requests
                .iter()
                .position(|active| active == &request)
            {
                registry.active_requests.swap_remove(index);
            }
        }
    }
}

type GuiRecoveryRegistry = SharedGuiRecoveryRegistry<windows_capture::WindowHandle>;

fn retain_recovered_window_after_setup_failure<W: RecoverableWindowIdentity>(
    claim: GuiSessionClaim<W>,
    window: W,
    failure: String,
) -> Box<dyn std::error::Error> {
    match claim.retain(window) {
        Ok(()) => failure.into(),
        Err(retain) => format!("{failure}; could not retain it again: {retain}").into(),
    }
}

struct DesktopCompanionState {
    expected_token: String,
    live_share_token: String,
    expected_user: String,
    children: Mutex<Vec<Child>>,
    gui_recovery: GuiRecoveryRegistry,
    live_share_configuration: Mutex<()>,
    active_workers: Arc<AtomicUsize>,
}

impl DesktopCompanionState {
    fn new(expected_token: String, live_share_token: String, expected_user: String) -> Self {
        Self {
            expected_token,
            live_share_token,
            expected_user,
            children: Mutex::new(Vec::new()),
            gui_recovery: GuiRecoveryRegistry::default(),
            live_share_configuration: Mutex::new(()),
            active_workers: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn reap_and_has_children(&self) -> bool {
        let mut children = lock_unpoisoned(&self.children);
        reap_children(&mut children);
        !children.is_empty()
    }

    fn active_worker_count(&self) -> usize {
        self.active_workers.load(Ordering::Acquire)
    }

    fn configure_live_share(&self, enable: bool) -> Result<bool, Box<dyn std::error::Error>> {
        // Keep configure and the status query in one transaction so concurrent
        // mount requests cannot report each other's state.
        let _configuration = lock_unpoisoned(&self.live_share_configuration);
        windows_live_share::configure(enable, &self.live_share_token)?;
        windows_live_share::query()
    }
}

struct ConnectionWorkerPermit {
    active_workers: Arc<AtomicUsize>,
}

impl ConnectionWorkerPermit {
    fn try_acquire(active_workers: &Arc<AtomicUsize>) -> Option<Self> {
        let mut observed = active_workers.load(Ordering::Acquire);
        loop {
            if observed >= MAX_CONNECTION_WORKERS {
                return None;
            }
            match active_workers.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Self {
                        active_workers: Arc::clone(active_workers),
                    })
                }
                Err(current) => observed = current,
            }
        }
    }
}

impl Drop for ConnectionWorkerPermit {
    fn drop(&mut self) {
        self.active_workers.fetch_sub(1, Ordering::AcqRel);
    }
}

fn dispatch_connection_worker(
    mut stream: TcpStream,
    state: Arc<DesktopCompanionState>,
) -> io::Result<bool> {
    stream.set_nonblocking(false)?;
    let Some(permit) = ConnectionWorkerPermit::try_acquire(&state.active_workers) else {
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let _ = send_error(
            &mut stream,
            "the desktop companion is at its 16-connection worker limit; retry shortly",
        );
        let _ = stream.shutdown(Shutdown::Both);
        return Ok(false);
    };
    thread::Builder::new()
        .name("lsw-desktop-connection".to_owned())
        .spawn(move || {
            let _permit = permit;
            if let Err(error) = handle_connection(&mut stream, &state) {
                let _ = send_error(&mut stream, &error.to_string());
                super::write_stderr(format_args!(
                    "lsw desktop companion rejected a connection: {error}"
                ));
            }
            let _ = stream.shutdown(Shutdown::Both);
        })?;
    Ok(true)
}

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

    // windows-rs caches WinRT activation factories for the process. Keep one
    // apartment alive until the companion exits so a completed capture cannot
    // unload GraphicsCapture.dll beneath the cache used by the next session.
    // Capture workers still initialize and balance their own thread apartment.
    let _process_winrt_apartment = windows_capture::ApartmentGuard::initialize()?;

    let state = Arc::new(DesktopCompanionState::new(
        token,
        live_share_token,
        expected_user,
    ));
    let listener = TcpListener::bind(listen)?;
    listener.set_nonblocking(true)?;
    let mut idle_deadline = Instant::now() + DESKTOP_COMPANION_IDLE_TIMEOUT;
    loop {
        let has_children = state.reap_and_has_children();
        state.gui_recovery.purge_stale();
        let mapped = windows_live_share::query().unwrap_or(false);
        let has_gui_windows = !state.gui_recovery.is_empty();
        let has_workers = state.active_worker_count() != 0;
        if mapped || has_children || has_gui_windows || has_workers {
            idle_deadline = Instant::now() + DESKTOP_COMPANION_IDLE_TIMEOUT;
        }
        match listener.accept() {
            Ok((stream, peer)) if peer.ip().is_loopback() => {
                if dispatch_connection_worker(stream, Arc::clone(&state))? {
                    idle_deadline = Instant::now() + DESKTOP_COMPANION_IDLE_TIMEOUT;
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if state.active_worker_count() == 0
            && !state.reap_and_has_children()
            && state.gui_recovery.is_empty()
            && !mapped
            && Instant::now() >= idle_deadline
        {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    state: &DesktopCompanionState,
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
        || !constant_time_token_eq(&hello.token, &state.expected_token)
    {
        send_error(stream, "desktop companion authentication failed")?;
        return Ok(());
    }
    let hello = ServerHello {
        version: AGENT_PROTOCOL_VERSION,
        capabilities: vec![
            CAPABILITY_GUI_LAUNCH_V1.to_owned(),
            CAPABILITY_GUI_ICON_V1.to_owned(),
            CAPABILITY_GUI_WINDOW_V3.to_owned(),
            CAPABILITY_DESKTOP_LIVE_SHARE_V1.to_owned(),
        ],
    };
    write_frame(stream, &Frame::new(FrameKind::HelloOk, hello.encode()?))?;

    let request = read_frame(stream)?;
    match request.kind {
        FrameKind::GuiStart => {
            let request = GuiStartRequest::decode(&request.payload)?;
            require_user(&request.user_name, &state.expected_user)?;
            if request.mount_live_share {
                state.configure_live_share(true)?;
            }
            let child = spawn_gui(&request)?;
            let process_id = child.id();
            lock_unpoisoned(&state.children).push(child);
            write_frame(
                stream,
                &Frame::new(FrameKind::Started, encode_process_id(process_id)),
            )?;
        }
        FrameKind::GuiWindowOpen => {
            let request = GuiStartRequest::decode(&request.payload)?;
            require_user(&request.user_name, &state.expected_user)?;
            if request.mount_live_share {
                state.configure_live_share(true)?;
            }
            let (claim, recovered) = state.gui_recovery.claim(&request)?;
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(Some(GUI_WRITE_TIMEOUT))?;
            stream_gui_window(stream, &request, claim, recovered)?;
        }
        FrameKind::GuiIcon => {
            let request = GuiIconRequest::decode(&request.payload)?;
            require_user(&request.user_name, &state.expected_user)?;
            match extract_icon(&request.program) {
                Ok(icon) => write_frame(stream, &Frame::new(FrameKind::GuiIconData, icon))?,
                Err(error) => send_error(stream, &error.to_string())?,
            }
        }
        FrameKind::DesktopLiveShareConfigure => {
            let request = DesktopLiveShareRequest::decode(&request.payload)?;
            require_user(&request.user_name, &state.expected_user)?;
            match state.configure_live_share(request.enable) {
                Ok(mapped) => write_frame(
                    stream,
                    &Frame::new(
                        FrameKind::LiveShareStatus,
                        LiveShareStatus { mapped }.encode(),
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

#[cfg(test)]
mod tests;
