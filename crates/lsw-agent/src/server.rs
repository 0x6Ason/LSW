// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) fn run(arguments: Vec<std::ffi::OsString>) -> Result<(), Box<dyn std::error::Error>> {
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
    if arguments
        .first()
        .is_some_and(|argument| argument == "--desktop-companion")
    {
        #[cfg(windows)]
        {
            return desktop_companion::run(&arguments[1..]);
        }
        #[cfg(not(windows))]
        {
            return Err("--desktop-companion is only supported on Windows".into());
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

pub(super) fn run_agent(
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
pub(super) fn apply_clone_identity(token_file: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let identity = find_clone_identity(
        windows_path::volume_roots()
            .unwrap_or_default()
            .into_iter()
            .map(|root| root.join("lsw")),
    )?;
    if let Some(identity) = identity {
        return apply_clone_identity_at(token_file, &identity);
    }
    apply_clone_identity_from_roots(
        token_file,
        (b'D'..=b'Z')
            .map(char::from)
            .map(|letter| PathBuf::from(format!("{letter}:\\lsw"))),
    )
}

#[cfg(windows)]
pub(super) fn apply_clone_identity_from_roots(
    token_file: &Path,
    roots: impl IntoIterator<Item = PathBuf>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let identity = find_clone_identity(roots)?;
    let Some(identity) = identity else {
        return Ok(false);
    };
    apply_clone_identity_at(token_file, &identity)
}

#[cfg(windows)]
pub(super) fn apply_clone_identity_at(
    token_file: &Path,
    identity: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
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
pub(super) fn find_clone_identity(
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
pub(super) fn watch_for_clone_identity(
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
        apply_clone_identity,
    )
}

#[cfg(windows)]
pub(super) fn watch_for_clone_identity_with_timing<ApplyIdentity>(
    token_file: PathBuf,
    token: Weak<Mutex<String>>,
    discovery_timeout: Duration,
    fast_timeout: Duration,
    fast_interval: Duration,
    slow_interval: Duration,
    apply_identity: ApplyIdentity,
) -> Result<(), Box<dyn std::error::Error>>
where
    ApplyIdentity: Fn(&Path) -> Result<bool, Box<dyn std::error::Error>> + Send + 'static,
{
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
                match apply_identity(&token_file) {
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

pub(super) fn run_stoppable_listener(
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

pub(super) fn shutdown_requested(shutdown: &Receiver<()>) -> bool {
    match shutdown.try_recv() {
        Ok(()) | Err(mpsc::TryRecvError::Disconnected) => true,
        Err(mpsc::TryRecvError::Empty) => false,
    }
}

pub(super) fn wait_for_shutdown(shutdown: &Receiver<()>, timeout: Duration) -> bool {
    match shutdown.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => false,
    }
}

pub(super) fn dispatch_connection(
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

pub(super) struct SessionSlot(pub(super) Arc<AtomicUsize>);

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SessionMode {
    Legacy,
    Controlled {
        options: SessionOptions,
        lease: Option<SessionLease>,
    },
}

impl SessionMode {
    pub(super) fn is_controlled(self) -> bool {
        matches!(self, Self::Controlled { .. })
    }

    pub(super) fn cancel_on_disconnect(self) -> bool {
        match self {
            Self::Legacy => false,
            Self::Controlled { options, .. } => options.cancel_on_disconnect,
        }
    }

    pub(super) fn lease(self) -> Option<SessionLease> {
        match self {
            Self::Legacy => None,
            Self::Controlled { lease, .. } => lease,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum SessionControlEvent {
    Cancel,
    Signal(SessionSignal),
    Disconnect,
    Heartbeat(Instant),
    ProtocolError(String),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum SessionEnd {
    Normal,
    Cancelled,
    Signalled(i32),
    Disconnected,
    LeaseExpired,
    ProtocolError(String),
}

pub(super) struct SessionLeaseMonitor {
    origin: Instant,
    state: SessionLeaseState,
}

impl SessionLeaseMonitor {
    pub(super) fn new(lease: SessionLease) -> Self {
        Self {
            origin: Instant::now(),
            state: SessionLeaseState::new(lease, 0),
        }
    }

    pub(super) fn millis_at(&self, instant: Instant) -> u64 {
        u64::try_from(instant.saturating_duration_since(self.origin).as_millis())
            .unwrap_or(u64::MAX)
    }

    pub(super) fn observe_heartbeat(&mut self, observed_at: Instant) -> bool {
        let elapsed = self.millis_at(observed_at);
        self.state.observe_heartbeat(elapsed)
    }

    pub(super) fn is_expired(&self, now: Instant) -> bool {
        self.state.is_expired(self.millis_at(now))
    }

    pub(super) fn wait_duration(&self, now: Instant) -> Duration {
        let remaining = self
            .state
            .deadline_millis()
            .saturating_sub(self.millis_at(now));
        PROCESS_POLL_INTERVAL.min(Duration::from_millis(remaining))
    }
}
