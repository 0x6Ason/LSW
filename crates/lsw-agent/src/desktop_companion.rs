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

enum GuiControl {
    Frame(Frame),
    Disconnected(String),
}

fn stream_gui_window(
    stream: &mut TcpStream,
    request: &GuiStartRequest,
    claim: GuiSessionClaim<windows_capture::WindowHandle>,
    recovered: Option<windows_capture::WindowHandle>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(mut window) = recovered {
        // An abnormal disconnect normally releases every injected key/button
        // before retention. Retry that fail-safe boundary before exposing the
        // reattached HWND in case the original SendInput release failed.
        if let Err(error) = window.release_injected_input() {
            return Err(retain_recovered_window_after_setup_failure(
                claim,
                window,
                format!(
                    "could not release input before reattaching the recovered GUI window: {error}"
                ),
            ));
        }
        let process_id = window.process_id();
        let _dpi_awareness = match windows_capture::ThreadDpiAwareness::per_monitor_v2() {
            Ok(awareness) => awareness,
            Err(error) => {
                return Err(retain_recovered_window_after_setup_failure(
                    claim,
                    window,
                    format!(
                        "could not enter physical-pixel DPI mode before reattaching the recovered GUI window: {error}"
                    ),
                ));
            }
        };
        return stream_selected_gui_window(stream, window, process_id, None, claim);
    }
    let existing_windows = windows_capture::visible_windows()?;
    if aam_activation_is_eligible(request) {
        if let Some(activation) =
            windows_capture::activate_packaged_alias(&request.request.argv[0])?
        {
            // Enter physical pixels after AAM has created the application so
            // LSW cannot impose a DPI policy on the activated process.
            let _dpi_awareness = windows_capture::ThreadDpiAwareness::per_monitor_v2()?;
            let (window, window_process_id) =
                windows_capture::find_activated_window(activation, &existing_windows)?;
            return stream_selected_gui_window(stream, window, window_process_id, None, claim);
        }
    }

    let mut child = spawn_gui(request)?;
    let process_id = child.id();
    // Enter the physical-pixel coordinate space only after spawning the child
    // so LSW cannot accidentally impose a DPI policy on the application.
    let _dpi_awareness = windows_capture::ThreadDpiAwareness::per_monitor_v2()?;
    let (window, window_process_id) =
        windows_capture::find_process_window(process_id, &existing_windows, &mut child)?;
    let result = stream_selected_gui_window(
        stream,
        window,
        window_process_id,
        Some((&mut child, process_id)),
        claim,
    );
    // Reap a launcher that exited naturally, but never force-kill a GUI
    // process: it may be showing a native save confirmation or own unsaved
    // data that must remain available for explicit recovery.
    let _ = child.try_wait();
    result
}

fn aam_activation_is_eligible(request: &GuiStartRequest) -> bool {
    request.request.argv.len() == 1
        && request.request.working_directory.is_none()
        && request.environment.is_empty()
}

fn stream_selected_gui_window(
    stream: &mut TcpStream,
    mut window: windows_capture::WindowHandle,
    window_process_id: u32,
    mut launcher: Option<(&mut Child, u32)>,
    claim: GuiSessionClaim<windows_capture::WindowHandle>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut explicit_close_requested = false;
    let mut reader_thread = None;
    let session_result: Result<(), Box<dyn std::error::Error>> = (|| {
        let mut capture = windows_capture::CaptureSession::start(&window)?;
        let first_deadline = Instant::now() + FIRST_CAPTURE_TIMEOUT;
        let first = loop {
            if let Some(frame) = capture.next_frame(&window, Duration::from_millis(250))? {
                break frame;
            }
            if !window.is_open() {
                return Err(
                    "GUI window closed before its first Windows Graphics Capture frame".into(),
                );
            }
            if Instant::now() >= first_deadline {
                return Err(
                    "timed out waiting for the first Windows Graphics Capture frame".into(),
                );
            }
        };
        let mut width = first.width;
        let mut height = first.height;
        window.set_capture_size(width, height);
        write_frame(
            stream,
            &Frame::new(
                FrameKind::GuiWindowReady,
                window.ready(window_process_id, width, height)?.encode()?,
            ),
        )?;
        if let Some(action) = initial_window_state_action(window.is_maximized()?) {
            // A packaged desktop app can remember a maximized state from its last
            // run. Mirror that state immediately after Ready so the new host
            // presenter never starts out divergent from the guest HWND.
            send_gui_action(stream, action)?;
        }
        let mut damage = DamageTracker::default();
        send_damages(stream, damage.update(width, height, &first.bgra)?)?;

        let mut reader = stream.try_clone()?;
        let (control_sender, control_receiver) = mpsc::sync_channel(128);
        reader_thread = Some(thread::spawn(move || loop {
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
        }));

        'session: loop {
            if !window.is_open() {
                break acknowledge_closed_gui_window(
                    stream,
                    &mut window,
                    &mut launcher,
                    window_process_id,
                );
            }
            'control: for _ in 0..MAX_CONTROLS_PER_CAPTURE_POLL {
                match control_receiver.try_recv() {
                    Ok(GuiControl::Frame(frame)) => match frame.kind {
                        FrameKind::GuiWindowInput => {
                            let outcome = GuiInputEvent::decode(&frame.payload)
                                .map_err(|error| error.into())
                                .and_then(|event| {
                                    window.input(event).map_err(|error| error.into())
                                });
                            match outcome {
                                Ok(windows_capture::GuiInputOutcome::Action(action)) => {
                                    if let Err(error) = send_gui_action(stream, action) {
                                        break 'session Err(error);
                                    }
                                }
                                Ok(windows_capture::GuiInputOutcome::DragHint(hint)) => {
                                    if let Err(error) = send_gui_drag_hint(stream, hint) {
                                        break 'session Err(error);
                                    }
                                }
                                Ok(windows_capture::GuiInputOutcome::None) => {}
                                Err(_) if !window.is_open() => {
                                    break 'session acknowledge_closed_gui_window(
                                        stream,
                                        &mut window,
                                        &mut launcher,
                                        window_process_id,
                                    );
                                }
                                Err(error) => break 'session Err(error),
                            }
                        }
                        FrameKind::GuiWindowResize => {
                            if let Err(error) = GuiWindowResize::decode(&frame.payload)
                                .map_err(|error| error.into())
                                .and_then(|resize| {
                                    window.resize(resize).map_err(|error| error.into())
                                })
                            {
                                if !window.is_open() {
                                    break 'session acknowledge_closed_gui_window(
                                        stream,
                                        &mut window,
                                        &mut launcher,
                                        window_process_id,
                                    );
                                }
                                break 'session Err(error);
                            }
                        }
                        FrameKind::GuiWindowAction => {
                            let maximized = match GuiWindowAction::decode(&frame.payload) {
                                Ok(GuiWindowAction::Maximize) => true,
                                Ok(GuiWindowAction::Restore) => false,
                                Ok(_) => {
                                    break 'session Err(
                                        "host may send only explicit maximize or restore state"
                                            .into(),
                                    )
                                }
                                Err(error) => break 'session Err(error.into()),
                            };
                            if let Err(error) = window.set_maximized(maximized) {
                                if !window.is_open() {
                                    break 'session acknowledge_closed_gui_window(
                                        stream,
                                        &mut window,
                                        &mut launcher,
                                        window_process_id,
                                    );
                                }
                                break 'session Err(error.into());
                            }
                        }
                        FrameKind::GuiWindowClose if frame.payload.is_empty() => {
                            explicit_close_requested = true;
                            if let Err(error) = window.release_injected_input() {
                                break 'session Err(error.into());
                            }
                            // SendInput queues release edges asynchronously.
                            // Do not let a subsequently posted WM_CLOSE overtake
                            // the releases and expose a stuck key/button to the
                            // application's FormClosing/save-confirmation path.
                            if let Err(error) = window.settle_released_input() {
                                break 'session Err(error.into());
                            }
                            if let Err(error) = window.close() {
                                if window.is_open() {
                                    break 'session Err(error.into());
                                }
                            }
                        }
                        FrameKind::GuiWindowClose => {
                            break 'session Err("GUI_WINDOW_CLOSE payload must be empty".into());
                        }
                        _ => {
                            break 'session Err("invalid frame in a seamless GUI session".into());
                        }
                    },
                    Ok(GuiControl::Disconnected(error)) => {
                        break 'session Err(
                            format!("seamless GUI client disconnected: {error}").into()
                        );
                    }
                    Err(TryRecvError::Empty) => break 'control,
                    Err(TryRecvError::Disconnected) => {
                        break 'session Err("seamless GUI input channel closed".into());
                    }
                }
            }

            if !window.is_open() {
                break acknowledge_closed_gui_window(
                    stream,
                    &mut window,
                    &mut launcher,
                    window_process_id,
                );
            }
            match capture.next_frame(&window, CAPTURE_POLL_INTERVAL) {
                Ok(Some(frame)) => {
                    if (frame.width, frame.height) != (width, height) {
                        width = frame.width;
                        height = frame.height;
                        window.set_capture_size(width, height);
                        let ready = window
                            .ready(window_process_id, width, height)
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
                Err(_) if !window.is_open() => {
                    break 'session acknowledge_closed_gui_window(
                        stream,
                        &mut window,
                        &mut launcher,
                        window_process_id,
                    );
                }
                Err(error) => break 'session Err(error),
            }
        }
    })();

    let end_policy = gui_session_end_policy(explicit_close_requested, window.is_open());
    let session_result = if session_result.is_ok() && end_policy == GuiSessionEndPolicy::Detach {
        Err("seamless GUI session ended without closing its live HWND".into())
    } else {
        session_result
    };
    let result = match end_policy {
        GuiSessionEndPolicy::NaturalGuestClose => merge_session_results(
            session_result,
            window
                .release_injected_input()
                .map_err(|error| error.into()),
            "could not release injected input after the guest window closed",
        ),
        GuiSessionEndPolicy::ExplicitHostClose => merge_session_results(
            session_result,
            window
                .release_injected_input()
                .map_err(|error| error.into()),
            "could not release injected input after explicit GUI close",
        ),
        GuiSessionEndPolicy::Detach => {
            let release = window
                .release_injected_input()
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() });
            let detached_result = merge_session_results(
                session_result,
                release,
                "could not release injected input before retaining the GUI window",
            );
            let retain = if window.is_open() {
                claim.retain(window)
            } else {
                Ok(())
            };
            merge_session_results(
                detached_result,
                retain,
                "could not retain the live GUI window for recovery",
            )
        }
    };
    if let Err(error) = &result {
        let _ = send_error(stream, &error.to_string());
    }
    let _ = stream.shutdown(Shutdown::Both);
    let reader_result = reader_thread.map_or(Ok(()), |thread| {
        thread
            .join()
            .map_err(|_| "seamless GUI control reader panicked".into())
    });
    merge_session_results(result, reader_result, "GUI control reader cleanup failed")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuiSessionEndPolicy {
    NaturalGuestClose,
    ExplicitHostClose,
    Detach,
}

fn gui_session_end_policy(
    explicit_close_requested: bool,
    window_is_live: bool,
) -> GuiSessionEndPolicy {
    if window_is_live {
        GuiSessionEndPolicy::Detach
    } else if explicit_close_requested {
        GuiSessionEndPolicy::ExplicitHostClose
    } else {
        GuiSessionEndPolicy::NaturalGuestClose
    }
}

fn merge_session_results(
    primary: Result<(), Box<dyn std::error::Error>>,
    secondary: Result<(), Box<dyn std::error::Error>>,
    secondary_context: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match (primary, secondary) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(format!("{secondary_context}: {error}").into()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(secondary)) => {
            Err(format!("{error}; {secondary_context}: {secondary}").into())
        }
    }
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

fn send_gui_action(
    stream: &mut TcpStream,
    action: GuiWindowAction,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(
        stream,
        &Frame::new(FrameKind::GuiWindowAction, action.encode()),
    )?;
    Ok(())
}

fn send_gui_drag_hint(
    stream: &mut TcpStream,
    hint: GuiWindowDragHint,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(
        stream,
        &Frame::new(FrameKind::GuiWindowDragHint, hint.encode()?),
    )?;
    Ok(())
}

fn initial_window_state_action(is_maximized: bool) -> Option<GuiWindowAction> {
    is_maximized.then_some(GuiWindowAction::Maximize)
}

fn acknowledge_closed_gui_window(
    stream: &mut TcpStream,
    window: &mut windows_capture::WindowHandle,
    launcher: &mut Option<(&mut Child, u32)>,
    window_process_id: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    window.release_injected_input()?;
    let exit_code = match launcher.as_mut() {
        Some((child, process_id)) => {
            observed_gui_close_exit_code(child, window_process_id == *process_id)?
        }
        None => 0,
    };
    send_gui_closed(stream, exit_code)
}

fn observed_gui_close_exit_code(child: &mut Child, launcher_owns_window: bool) -> io::Result<i32> {
    let observed = if launcher_owns_window {
        child.try_wait()?.and_then(|status| status.code())
    } else {
        None
    };
    Ok(close_ack_exit_code(observed))
}

fn close_ack_exit_code(observed: Option<i32>) -> i32 {
    // A destroyed requested HWND is the user-visible completion condition. A
    // launcher still draining at that instant is not reported as a failed GUI
    // action; teardown never force-kills a process with unsaved data.
    observed.unwrap_or(0)
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
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use lsw_core::{
        read_frame, write_frame, ClientHello, Frame, FrameKind, ProcessEnvironment, ServerHello,
        SessionKind, StartRequest, AGENT_PROTOCOL_VERSION,
    };

    use super::{
        aam_activation_is_eligible, close_ack_exit_code, initial_window_state_action,
        retain_recovered_window_after_setup_failure, ConnectionWorkerPermit, DesktopCompanionState,
        DetachedGuiRegistry, GuiSessionEndPolicy, GuiStartRequest, RecoverableWindowIdentity,
        SharedGuiRecoveryRegistry, MAX_CONNECTION_WORKERS, MAX_DETACHED_GUI_WINDOWS,
        MAX_GUI_WINDOW_SESSIONS,
    };

    #[derive(Debug)]
    struct FakeWindow {
        identity: Option<(u32, u64)>,
    }

    impl RecoverableWindowIdentity for FakeWindow {
        fn current_identity(&self) -> Option<(u32, u64)> {
            self.identity
        }
    }

    fn gui_request(program: &str) -> GuiStartRequest {
        GuiStartRequest {
            user_name: "desktop-user".to_owned(),
            request: StartRequest {
                kind: SessionKind::Run,
                argv: vec![program.to_owned()],
                working_directory: None,
            },
            environment: ProcessEnvironment::new(Vec::new()).unwrap(),
            mount_live_share: false,
        }
    }

    #[test]
    fn closed_window_ack_preserves_observed_code_without_failing_a_draining_launcher() {
        assert_eq!(close_ack_exit_code(Some(23)), 23);
        assert_eq!(close_ack_exit_code(None), 0);
    }

    #[test]
    fn initially_maximized_guest_requests_an_idempotent_host_state() {
        assert_eq!(
            initial_window_state_action(true),
            Some(lsw_core::GuiWindowAction::Maximize)
        );
        assert_eq!(initial_window_state_action(false), None);
    }

    #[test]
    fn aam_activation_is_limited_to_an_unmodified_single_program_request() {
        let request = gui_request("notepad.exe");
        assert!(aam_activation_is_eligible(&request));

        let mut extra_argument = request.clone();
        extra_argument.request.argv.push("note.txt".to_owned());
        assert!(!aam_activation_is_eligible(&extra_argument));

        let mut working_directory = request.clone();
        working_directory.request.working_directory = Some(r"C:\Temp".to_owned());
        assert!(!aam_activation_is_eligible(&working_directory));

        let mut environment = request;
        environment.environment =
            ProcessEnvironment::new(vec![("MODE".to_owned(), "test".to_owned())]).unwrap();
        assert!(!aam_activation_is_eligible(&environment));
    }

    #[test]
    fn dirty_outer_close_cross_product_never_finishes_a_live_window() {
        for (explicit_close_requested, window_is_live, expected) in [
            (false, true, GuiSessionEndPolicy::Detach),
            (false, false, GuiSessionEndPolicy::NaturalGuestClose),
            (true, true, GuiSessionEndPolicy::Detach),
            (true, false, GuiSessionEndPolicy::ExplicitHostClose),
        ] {
            assert_eq!(
                super::gui_session_end_policy(explicit_close_requested, window_is_live),
                expected
            );
        }
    }

    #[test]
    fn dirty_outer_close_retains_only_the_exact_request_for_recovery() {
        let request = gui_request("notepad.exe");
        let other = gui_request("mspaint.exe");
        let mut registry = DetachedGuiRegistry::default();
        assert_eq!(
            super::gui_session_end_policy(true, true),
            GuiSessionEndPolicy::Detach
        );
        registry
            .insert(
                request.clone(),
                FakeWindow {
                    identity: Some((42, 73)),
                },
            )
            .unwrap();

        assert!(registry.take(&other).is_none());
        assert_eq!(
            registry.take(&request).unwrap().current_identity(),
            Some((42, 73))
        );
        assert!(registry.take(&request).is_none());
    }

    #[test]
    fn failed_reattach_setup_restores_the_exact_pinned_window() {
        let request = gui_request("notepad.exe");
        let other = gui_request("mspaint.exe");
        let registry = SharedGuiRecoveryRegistry::<FakeWindow>::default();
        let (seed, recovered) = registry.claim(&request).unwrap();
        assert!(recovered.is_none());
        seed.retain(FakeWindow {
            identity: Some((42, 73)),
        })
        .unwrap();
        let (claim, recovered) = registry.claim(&request).unwrap();
        let recovered = recovered.unwrap();

        let error = retain_recovered_window_after_setup_failure(
            claim,
            recovered,
            "reattach setup failed".to_owned(),
        );
        assert_eq!(error.to_string(), "reattach setup failed");
        let (other_claim, other_window) = registry.claim(&other).unwrap();
        assert!(other_window.is_none());
        drop(other_claim);
        let (claim, recovered) = registry.claim(&request).unwrap();
        assert_eq!(recovered.unwrap().current_identity(), Some((42, 73)));
        drop(claim);
    }

    #[test]
    fn detached_registry_reattaches_only_the_identical_request_once() {
        let request = gui_request("notepad.exe");
        let other = gui_request("mspaint.exe");
        let mut registry = DetachedGuiRegistry::default();
        registry
            .insert(
                request.clone(),
                FakeWindow {
                    identity: Some((42, 73)),
                },
            )
            .unwrap();

        assert!(registry.take(&other).is_none());
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(
            registry.take(&request).unwrap().current_identity(),
            Some((42, 73))
        );
        assert!(registry.take(&request).is_none());
    }

    #[test]
    fn detached_registry_requires_every_gui_request_field_to_match() {
        let request = gui_request("notepad.exe");
        let mut registry = DetachedGuiRegistry::default();
        registry
            .insert(
                request.clone(),
                FakeWindow {
                    identity: Some((42, 73)),
                },
            )
            .unwrap();

        let mut different_user = request.clone();
        different_user.user_name = "other-user".to_owned();
        assert!(registry.take(&different_user).is_none());

        let mut different_cwd = request.clone();
        different_cwd.request.working_directory = Some(r"C:\Temp".to_owned());
        assert!(registry.take(&different_cwd).is_none());

        let mut different_environment = request.clone();
        different_environment.environment =
            ProcessEnvironment::new(vec![("MODE".to_owned(), "test".to_owned())]).unwrap();
        assert!(registry.take(&different_environment).is_none());

        let mut different_mount = request.clone();
        different_mount.mount_live_share = true;
        assert!(registry.take(&different_mount).is_none());

        assert_eq!(registry.entries.len(), 1);
        assert_eq!(
            registry.take(&request).unwrap().current_identity(),
            Some((42, 73))
        );
    }

    #[test]
    fn detached_registry_purges_changed_pid_or_hwnd_identity() {
        let request = gui_request("notepad.exe");
        let mut registry = DetachedGuiRegistry::default();
        registry
            .insert(
                request.clone(),
                FakeWindow {
                    identity: Some((42, 73)),
                },
            )
            .unwrap();
        registry.entries[0].window.identity = Some((43, 73));
        assert!(registry.take(&request).is_none());
        assert!(registry.entries.is_empty());

        registry
            .insert(
                request.clone(),
                FakeWindow {
                    identity: Some((42, 73)),
                },
            )
            .unwrap();
        registry.entries[0].window.identity = Some((42, 74));
        assert!(registry.take(&request).is_none());
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn detached_registry_is_bounded_and_rejects_duplicate_request_state() {
        let mut registry = DetachedGuiRegistry::default();
        let duplicate = gui_request("duplicate.exe");
        registry
            .insert(
                duplicate.clone(),
                FakeWindow {
                    identity: Some((1, 1)),
                },
            )
            .unwrap();
        assert!(registry
            .insert(
                duplicate,
                FakeWindow {
                    identity: Some((2, 2)),
                },
            )
            .is_err());

        for index in 1..MAX_DETACHED_GUI_WINDOWS {
            registry
                .insert(
                    gui_request(&format!("program-{index}.exe")),
                    FakeWindow {
                        identity: Some((u32::try_from(index).unwrap() + 10, index as u64 + 10)),
                    },
                )
                .unwrap();
        }
        assert_eq!(registry.entries.len(), MAX_DETACHED_GUI_WINDOWS);
        assert!(registry.ensure_launch_capacity().is_err());
        assert!(registry
            .insert(
                gui_request("overflow.exe"),
                FakeWindow {
                    identity: Some((999, 999)),
                },
            )
            .is_err());
    }

    #[test]
    fn shared_registry_claim_is_exclusive_and_recovers_the_exact_window() {
        let request = gui_request("notepad.exe");
        let registry = SharedGuiRecoveryRegistry::<FakeWindow>::default();

        let (claim, recovered) = registry.claim(&request).unwrap();
        assert!(recovered.is_none());
        assert_eq!(registry.counts(), (1, 0));
        assert!(registry.claim(&request).is_err());

        claim
            .retain(FakeWindow {
                identity: Some((42, 73)),
            })
            .unwrap();
        assert_eq!(registry.counts(), (0, 1));

        let (reattached, recovered) = registry.claim(&request).unwrap();
        let recovered = recovered.expect("the exact detached HWND should be recovered");
        assert_eq!(recovered.current_identity(), Some((42, 73)));
        assert_eq!(registry.counts(), (1, 0));
        assert!(registry.claim(&request).is_err());

        reattached.retain(recovered).unwrap();
        assert_eq!(registry.counts(), (0, 1));
    }

    #[test]
    fn shared_registry_bounds_active_and_detached_windows_together() {
        let registry = SharedGuiRecoveryRegistry::<FakeWindow>::default();
        let detached_count = MAX_GUI_WINDOW_SESSIONS / 2;
        for index in 0..detached_count {
            let request = gui_request(&format!("detached-{index}.exe"));
            let (claim, recovered) = registry.claim(&request).unwrap();
            assert!(recovered.is_none());
            claim
                .retain(FakeWindow {
                    identity: Some((u32::try_from(index).unwrap() + 1, index as u64 + 1)),
                })
                .unwrap();
        }

        let mut active = Vec::new();
        for index in detached_count..MAX_GUI_WINDOW_SESSIONS {
            let request = gui_request(&format!("active-{index}.exe"));
            let (claim, recovered) = registry.claim(&request).unwrap();
            assert!(recovered.is_none());
            active.push(claim);
        }
        assert_eq!(registry.counts(), (detached_count, detached_count));
        assert!(registry.claim(&gui_request("overflow.exe")).is_err());

        drop(active.pop());
        let (replacement, recovered) = registry.claim(&gui_request("replacement.exe")).unwrap();
        assert!(recovered.is_none());
        active.push(replacement);
        assert_eq!(
            registry.counts().0 + registry.counts().1,
            MAX_GUI_WINDOW_SESSIONS
        );

        // Reattach is an atomic detached-to-active transition and therefore
        // remains possible even while the combined registry is full.
        let detached = gui_request("detached-0.exe");
        let (reattached, recovered) = registry.claim(&detached).unwrap();
        let recovered = recovered.expect("full registries must still permit exact recovery");
        assert!(registry.claim(&detached).is_err());
        assert_eq!(
            registry.counts().0 + registry.counts().1,
            MAX_GUI_WINDOW_SESSIONS
        );
        reattached.retain(recovered).unwrap();
    }

    #[test]
    fn gui_session_claim_is_released_when_its_worker_panics() {
        let request = gui_request("notepad.exe");
        let registry = SharedGuiRecoveryRegistry::<FakeWindow>::default();
        let worker_registry = registry.clone();
        let worker_request = request.clone();
        let worker = thread::spawn(move || {
            let (_claim, recovered) = worker_registry.claim(&worker_request).unwrap();
            assert!(recovered.is_none());
            panic!("deliberate GUI worker panic");
        });
        assert!(worker.join().is_err());
        assert_eq!(registry.counts(), (0, 0));

        let (claim, recovered) = registry.claim(&request).unwrap();
        assert!(recovered.is_none());
        drop(claim);
        assert_eq!(registry.counts(), (0, 0));
    }

    #[test]
    fn connection_worker_permits_are_hard_bounded_and_raii_released() {
        let active = Arc::new(AtomicUsize::new(0));
        let permits = (0..MAX_CONNECTION_WORKERS)
            .map(|_| {
                ConnectionWorkerPermit::try_acquire(&active)
                    .expect("every worker below the hard limit should be admitted")
            })
            .collect::<Vec<_>>();
        assert_eq!(active.load(Ordering::Acquire), MAX_CONNECTION_WORKERS);
        assert!(ConnectionWorkerPermit::try_acquire(&active).is_none());

        drop(permits);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    fn authenticate_desktop_connection(stream: &mut TcpStream, token: &str) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_frame(
            stream,
            &Frame::new(
                FrameKind::Hello,
                ClientHello {
                    version: AGENT_PROTOCOL_VERSION,
                    token: token.to_owned(),
                }
                .encode()
                .unwrap(),
            ),
        )
        .unwrap();
        let hello = read_frame(stream).expect("the worker should answer without another session");
        assert_eq!(hello.kind, FrameKind::HelloOk);
        ServerHello::decode(&hello.payload).unwrap();
    }

    #[test]
    fn two_desktop_connections_do_not_block_each_other() {
        let token = "a".repeat(64);
        let state = Arc::new(DesktopCompanionState::new(
            token.clone(),
            "b".repeat(64),
            "desktop-user".to_owned(),
        ));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (stream, peer) = listener.accept().unwrap();
                assert!(peer.ip().is_loopback());
                assert!(
                    super::dispatch_connection_worker(stream, Arc::clone(&server_state)).unwrap()
                );
            }
        });

        let mut blocked = TcpStream::connect(address).unwrap();
        authenticate_desktop_connection(&mut blocked, &token);
        // The first worker remains blocked waiting for its request frame.
        let mut independent = TcpStream::connect(address).unwrap();
        authenticate_desktop_connection(&mut independent, &token);
        server.join().unwrap();
        assert_eq!(state.active_worker_count(), 2);

        for stream in [&mut independent, &mut blocked] {
            write_frame(stream, &Frame::new(FrameKind::Ping, Vec::new())).unwrap();
            assert_eq!(read_frame(stream).unwrap().kind, FrameKind::Error);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while state.active_worker_count() != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(state.active_worker_count(), 0);
    }
}
