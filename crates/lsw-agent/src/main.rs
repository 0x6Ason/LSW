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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use lsw_core::{
    constant_time_token_eq, decode_file_length, decode_resize, encode_exit, encode_file_length,
    read_frame, write_frame, ClientHello, FileGetRequest, FilePutRequest, Frame, FrameKind,
    ServerHello, SessionKind, SessionLease, SessionLeaseState, SessionOptions, StartRequest,
    TerminalStartRequest, AGENT_GUEST_PORT, AGENT_PROTOCOL_VERSION, CAPABILITY_SESSION_CONTROL_V1,
    CAPABILITY_SESSION_LEASE_V1, SESSION_CANCEL_EXIT_CODE,
};
#[cfg(windows)]
use lsw_core::{TerminalSize, CAPABILITY_CONPTY_V1, CAPABILITY_TERMINAL_RESIZE_V1};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
const STREAM_CHUNK_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_SESSIONS: usize = 32;

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
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}

fn run(arguments: Vec<std::ffi::OsString>) -> Result<(), Box<dyn std::error::Error>> {
    let configuration = Configuration::parse(&arguments)?;

    if configuration.service {
        #[cfg(windows)]
        {
            return windows_service::run(configuration);
        }
        #[cfg(not(windows))]
        {
            return Err("--service is only supported on Windows".into());
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
    let token = Arc::new(read_token(&configuration.token_file)?);
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

fn run_stoppable_listener(
    listener: &TcpListener,
    configuration: &Configuration,
    token: &Arc<String>,
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
    token: &Arc<String>,
    active_sessions: &Arc<AtomicUsize>,
) -> bool {
    if configuration.once {
        if let Err(error) = handle_connection(stream, token) {
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

    let session_token = Arc::clone(token);
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
    Disconnect,
    Heartbeat(Instant),
    ProtocolError(String),
}

#[derive(Debug, Eq, PartialEq)]
enum SessionEnd {
    Normal,
    Cancelled,
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
    let (request_frame, session_mode) = if request_frame.kind == FrameKind::SessionOptions {
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
        if !matches!(
            request_frame.kind,
            FrameKind::Start | FrameKind::TerminalStart
        ) {
            send_error(
                &mut stream,
                "SESSION_OPTIONS may be followed by one SESSION_LEASE, then must be followed by START or TERMINAL_START",
            )?;
            return Err("client sent an invalid controlled-session request".into());
        }
        (request_frame, SessionMode::Controlled { options, lease })
    } else {
        (request_frame, SessionMode::Legacy)
    };
    match request_frame.kind {
        FrameKind::Start => run_process_request(stream, &request_frame.payload, session_mode),
        FrameKind::TerminalStart => {
            run_terminal_request(stream, &request_frame.payload, session_mode)
        }
        FrameKind::FilePut => receive_file(stream, &request_frame.payload),
        FrameKind::FileGet => send_file(stream, &request_frame.payload),
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
    ];
    #[cfg(windows)]
    let capabilities = {
        let mut capabilities = capabilities;
        capabilities.push(CAPABILITY_CONPTY_V1.to_owned());
        capabilities.push(CAPABILITY_TERMINAL_RESIZE_V1.to_owned());
        capabilities
    };
    capabilities
}

fn run_process_request(
    mut stream: TcpStream,
    payload: &[u8],
    session_mode: SessionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = StartRequest::decode(payload)?;
    let mut child = match spawn_request(&request) {
        Ok(child) => child,
        Err(error) => {
            send_error(&mut stream, &format!("could not start process: {error}"))?;
            return Err(error);
        }
    };

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

    fn try_wait_and_cleanup(&mut self) -> io::Result<Option<ExitStatus>> {
        let Some(status) = self.process.try_wait()? else {
            return Ok(None);
        };
        self.terminate_tree()?;
        Ok(Some(status))
    }

    fn wait_and_cleanup(&mut self) -> io::Result<ExitStatus> {
        let status = self.process.wait()?;
        self.terminate_tree()?;
        Ok(status)
    }

    /// Returns the leader status and whether this call won the race to issue
    /// process-tree termination.
    fn terminate(&mut self) -> io::Result<(ExitStatus, bool)> {
        if let Some(status) = self.process.try_wait()? {
            self.terminate_tree()?;
            return Ok((status, false));
        }
        self.terminate_tree()?;
        Ok((self.process.wait()?, true))
    }

    /// Process-tree ownership is one-shot. A Unix process-group id may be
    /// reused after its last member exits, so a later signal from Drop could
    /// otherwise target an unrelated group. A successful cleanup disarms it;
    /// a failed cleanup restores it only so Drop can make one best-effort retry.
    fn terminate_tree(&mut self) -> io::Result<()> {
        let Some(tree) = self.tree.take() else {
            return Ok(());
        };
        match tree.terminate(SESSION_CANCEL_EXIT_CODE) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.tree = Some(tree);
                Err(error)
            }
        }
    }

    #[cfg(all(test, unix))]
    fn kill(&mut self) -> io::Result<()> {
        self.terminate_tree()
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
        if self.terminate_tree().is_ok() {
            let _ = self.process.wait();
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
mod process_tree {
    use std::io;
    use std::os::raw::c_int;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    const SIGKILL: c_int = 9;
    // ESRCH is 3 on the Unix platforms supported by Rust. It means the group
    // is already empty, which is a successful teardown outcome.
    const ESRCH: i32 = 3;

    extern "C" {
        fn kill(pid: c_int, signal: c_int) -> c_int;
    }

    pub(super) struct Prepared;

    impl Prepared {
        pub(super) fn new(command: &mut Command) -> io::Result<Self> {
            // `process_group(0)` arranges setpgid(0, 0) after fork and before
            // exec. This closes the ordinary spawn-before-ownership race, but
            // does not prevent guest code from later creating a new session or
            // process group; process-group ownership is not a sandbox.
            command.process_group(0);
            Ok(Self)
        }

        pub(super) fn attach_and_start(self, child: &Child) -> io::Result<Owner> {
            let process_group = c_int::try_from(child.id()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "child process id exceeds pid_t")
            })?;
            Ok(Owner { process_group })
        }
    }

    pub(super) struct Owner {
        process_group: c_int,
    }

    impl Owner {
        pub(super) fn terminate(&self, _exit_code: i32) -> io::Result<()> {
            // A negative pid addresses the process group. process_group is a
            // validated positive child pid, so negation cannot overflow.
            if unsafe { kill(-self.process_group, SIGKILL) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ESRCH) {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod process_tree {
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use std::ptr;

    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
    #[cfg(test)]
    const JOB_OBJECT_BASIC_PROCESS_ID_LIST_CLASS: u32 = 3;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: u32 = 0x0000_0002;
    const RESUME_THREAD_FAILED: u32 = u32::MAX;
    #[cfg(test)]
    const SYNCHRONIZE: u32 = 0x0010_0000;
    #[cfg(test)]
    const WAIT_OBJECT_0: u32 = 0;
    #[cfg(test)]
    const WAIT_TIMEOUT: u32 = 258;
    #[cfg(test)]
    const ERROR_INVALID_PARAMETER: i32 = 87;

    type Handle = RawHandle;

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[cfg(test)]
    #[repr(C)]
    #[derive(Default)]
    struct JobObjectBasicProcessIdList {
        number_of_assigned_processes: u32,
        number_of_process_ids_in_list: u32,
        process_ids: [usize; 8],
    }

    #[repr(C)]
    struct ThreadEntry32 {
        size: u32,
        usage: u32,
        thread_id: u32,
        owner_process_id: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }

    impl ThreadEntry32 {
        fn empty() -> io::Result<Self> {
            Ok(Self {
                size: u32::try_from(size_of::<Self>()).map_err(|_| {
                    io::Error::new(io::ErrorKind::Other, "THREADENTRY32 is too large")
                })?,
                usage: 0,
                thread_id: 0,
                owner_process_id: 0,
                base_priority: 0,
                delta_priority: 0,
                flags: 0,
            })
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "AssignProcessToJobObject"]
        fn assign_process_to_job_object(job: Handle, process: Handle) -> i32;
        #[link_name = "CreateJobObjectW"]
        fn create_job_object_w(attributes: *mut c_void, name: *const u16) -> Handle;
        #[link_name = "CreateToolhelp32Snapshot"]
        fn create_toolhelp32_snapshot(flags: u32, process_id: u32) -> Handle;
        #[link_name = "OpenThread"]
        fn open_thread(access: u32, inherit_handle: i32, thread_id: u32) -> Handle;
        #[cfg(test)]
        #[link_name = "OpenProcess"]
        fn open_process(access: u32, inherit_handle: i32, process_id: u32) -> Handle;
        #[cfg(test)]
        #[link_name = "QueryInformationJobObject"]
        fn query_information_job_object(
            job: Handle,
            information_class: u32,
            information: *mut c_void,
            information_length: u32,
            return_length: *mut u32,
        ) -> i32;
        #[link_name = "ResumeThread"]
        fn resume_thread(thread: Handle) -> u32;
        #[link_name = "SetInformationJobObject"]
        fn set_information_job_object(
            job: Handle,
            information_class: u32,
            information: *const c_void,
            information_length: u32,
        ) -> i32;
        #[link_name = "TerminateJobObject"]
        fn terminate_job_object(job: Handle, exit_code: u32) -> i32;
        #[link_name = "Thread32First"]
        fn thread32_first(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
        #[link_name = "Thread32Next"]
        fn thread32_next(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
        #[cfg(test)]
        #[link_name = "WaitForSingleObject"]
        fn wait_for_single_object(handle: Handle, milliseconds: u32) -> u32;
    }

    pub(super) struct Prepared {
        job: Job,
    }

    impl Prepared {
        pub(super) fn new(command: &mut Command) -> io::Result<Self> {
            let job = Job::new()?;
            command.creation_flags(CREATE_SUSPENDED);
            Ok(Self { job })
        }

        pub(super) fn attach_and_start(self, child: &Child) -> io::Result<Owner> {
            self.job.assign(child.as_raw_handle())?;
            resume_only_suspended_thread(child.id())?;
            Ok(Owner { job: self.job })
        }
    }

    pub(super) struct Owner {
        job: Job,
    }

    impl Owner {
        pub(super) fn terminate(&self, exit_code: i32) -> io::Result<()> {
            self.job.terminate(exit_code)
        }

        #[cfg(test)]
        pub(super) fn process_ids(&self) -> io::Result<Vec<u32>> {
            self.job.process_ids()
        }
    }

    pub(super) struct Job {
        handle: OwnedHandle,
    }

    impl Job {
        pub(super) fn new() -> io::Result<Self> {
            let handle = unsafe { create_job_object_w(ptr::null_mut(), ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
            let mut information = JobObjectExtendedLimitInformation::default();
            information.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let information_length = u32::try_from(size_of::<JobObjectExtendedLimitInformation>())
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::Other, "Job information is too large")
                })?;
            if unsafe {
                set_information_job_object(
                    handle.as_raw_handle(),
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                    (&information as *const JobObjectExtendedLimitInformation).cast::<c_void>(),
                    information_length,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle })
        }

        pub(super) fn assign(&self, process: RawHandle) -> io::Result<()> {
            if unsafe { assign_process_to_job_object(self.handle.as_raw_handle(), process) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub(super) fn terminate(&self, exit_code: i32) -> io::Result<()> {
            let exit_code = u32::try_from(exit_code).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Job exit code must be non-negative",
                )
            })?;
            if unsafe { terminate_job_object(self.handle.as_raw_handle(), exit_code) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        #[cfg(test)]
        fn process_ids(&self) -> io::Result<Vec<u32>> {
            let mut information = JobObjectBasicProcessIdList::default();
            let information_length = u32::try_from(size_of::<JobObjectBasicProcessIdList>())
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::Other, "Job process list is too large")
                })?;
            if unsafe {
                query_information_job_object(
                    self.handle.as_raw_handle(),
                    JOB_OBJECT_BASIC_PROCESS_ID_LIST_CLASS,
                    (&mut information as *mut JobObjectBasicProcessIdList).cast::<c_void>(),
                    information_length,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let listed =
                usize::try_from(information.number_of_process_ids_in_list).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Job process count is invalid")
                })?;
            if listed > information.process_ids.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Job process list exceeded its test buffer",
                ));
            }
            information.process_ids[..listed]
                .iter()
                .map(|process_id| {
                    u32::try_from(*process_id).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "Job process id exceeds u32")
                    })
                })
                .collect()
        }
    }

    /// `std::process::Child` does not expose the primary thread handle. A
    /// CREATE_SUSPENDED process has exactly one thread before any guest code
    /// runs, so identify it by owner PID and fail closed if the snapshot shows
    /// zero or multiple candidates.
    fn resume_only_suspended_thread(process_id: u32) -> io::Result<()> {
        let snapshot = unsafe { create_toolhelp32_snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == (-1_isize as RawHandle) {
            return Err(io::Error::last_os_error());
        }
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry = ThreadEntry32::empty()?;
        if unsafe { thread32_first(snapshot.as_raw_handle(), &mut entry) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut suspended_thread_id = None;
        loop {
            if entry.owner_process_id == process_id
                && suspended_thread_id.replace(entry.thread_id).is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "suspended process unexpectedly has more than one thread",
                ));
            }
            if unsafe { thread32_next(snapshot.as_raw_handle(), &mut entry) } == 0 {
                break;
            }
        }
        let thread_id = suspended_thread_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not find the suspended process primary thread",
            )
        })?;
        let thread = unsafe { open_thread(THREAD_SUSPEND_RESUME, 0, thread_id) };
        if thread.is_null() {
            return Err(io::Error::last_os_error());
        }
        let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
        resume_thread_handle(thread.as_raw_handle())
    }

    pub(super) fn resume_thread_handle(thread: RawHandle) -> io::Result<()> {
        if unsafe { resume_thread(thread) } == RESUME_THREAD_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    pub(super) fn wait_for_process_id_exit(process_id: u32, milliseconds: u32) -> io::Result<bool> {
        let process = unsafe { open_process(SYNCHRONIZE, 0, process_id) };
        if process.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER) {
                Ok(true)
            } else {
                Err(error)
            };
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        match unsafe { wait_for_single_object(process.as_raw_handle(), milliseconds) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }

    #[cfg(test)]
    pub(super) fn ffi_layout_sizes() -> (usize, usize, usize) {
        (
            size_of::<JobObjectBasicLimitInformation>(),
            size_of::<JobObjectExtendedLimitInformation>(),
            size_of::<ThreadEntry32>(),
        )
    }
}

#[cfg(not(any(unix, windows)))]
mod process_tree {
    use std::io;
    use std::process::{Child, Command};

    pub(super) struct Prepared;

    impl Prepared {
        pub(super) fn new(_command: &mut Command) -> io::Result<Self> {
            Ok(Self)
        }

        pub(super) fn attach_and_start(self, _child: &Child) -> io::Result<Owner> {
            Ok(Owner)
        }
    }

    pub(super) struct Owner;

    impl Owner {
        pub(super) fn terminate(&self, _exit_code: i32) -> io::Result<()> {
            Ok(())
        }
    }
}

fn spawn_request(request: &StartRequest) -> Result<SessionChild, Box<dyn std::error::Error>> {
    match request.kind {
        SessionKind::Shell => spawn_shell(request),
        SessionKind::Exec | SessionKind::Run => {
            let (program, arguments) = request
                .argv
                .split_first()
                .ok_or("command request did not contain a program")?;
            spawn_program(program, arguments, request.working_directory.as_deref())
                .map_err(Into::into)
        }
    }
}

fn spawn_shell(request: &StartRequest) -> Result<SessionChild, Box<dyn std::error::Error>> {
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
) -> io::Result<SessionChild> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    let _ = input_shutdown.shutdown(if session_end == SessionEnd::LeaseExpired {
        Shutdown::Both
    } else {
        Shutdown::Read
    });
    join_input_bridge(input_thread)?;
    join_bridge(stdout_thread, peer_unavailable)?;
    join_bridge(stderr_thread, peer_unavailable)?;
    finish_session(&writer, session_end, status.code().unwrap_or(255))
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
        Arc::clone(&console),
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
    let _ = input_shutdown.shutdown(if session_end == SessionEnd::LeaseExpired {
        Shutdown::Both
    } else {
        Shutdown::Read
    });
    join_terminal_input(input_thread)?;
    drop(console);
    join_bridge(output_thread, peer_unavailable)?;
    finish_session(&writer, session_end, code)
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
        if let Err(error) = job.terminate(SESSION_CANCEL_EXIT_CODE) {
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
    console: Arc<windows_conpty::PseudoConsole>,
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
mod windows_conpty {
    use std::ffi::{c_void, OsStr};
    use std::fs::File;
    use std::io;
    use std::mem::{size_of, size_of_val};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr;
    use std::sync::Arc;

    use lsw_core::{SessionKind, StartRequest, TerminalSize};

    type Handle = RawHandle;
    type PseudoConsoleHandle = RawHandle;

    const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const INFINITE: u32 = u32::MAX;
    const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2_length: u16,
        reserved2: *mut u8,
        standard_input: Handle,
        standard_output: Handle,
        standard_error: Handle,
    }

    impl StartupInfoW {
        fn empty() -> Self {
            Self {
                cb: 0,
                reserved: ptr::null_mut(),
                desktop: ptr::null_mut(),
                title: ptr::null_mut(),
                x: 0,
                y: 0,
                x_size: 0,
                y_size: 0,
                x_count_chars: 0,
                y_count_chars: 0,
                fill_attribute: 0,
                flags: 0,
                show_window: 0,
                reserved2_length: 0,
                reserved2: ptr::null_mut(),
                standard_input: ptr::null_mut(),
                standard_output: ptr::null_mut(),
                standard_error: ptr::null_mut(),
            }
        }
    }

    #[repr(C)]
    struct StartupInfoExW {
        startup_info: StartupInfoW,
        attribute_list: *mut c_void,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: Handle,
        thread: Handle,
        process_id: u32,
        thread_id: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "CloseHandle"]
        fn close_handle(handle: Handle) -> i32;
        #[link_name = "ClosePseudoConsole"]
        fn close_pseudo_console(console: PseudoConsoleHandle);
        #[link_name = "CreatePipe"]
        fn create_pipe(
            read_pipe: *mut Handle,
            write_pipe: *mut Handle,
            attributes: *mut c_void,
            size: u32,
        ) -> i32;
        #[link_name = "CreateProcessW"]
        fn create_process_w(
            application_name: *const u16,
            command_line: *mut u16,
            process_attributes: *mut c_void,
            thread_attributes: *mut c_void,
            inherit_handles: i32,
            creation_flags: u32,
            environment: *mut c_void,
            current_directory: *const u16,
            startup_info: *mut StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        #[link_name = "CreatePseudoConsole"]
        fn create_pseudo_console(
            size: Coord,
            input: Handle,
            output: Handle,
            flags: u32,
            console: *mut PseudoConsoleHandle,
        ) -> i32;
        #[link_name = "DeleteProcThreadAttributeList"]
        fn delete_proc_thread_attribute_list(list: *mut c_void);
        #[link_name = "GetExitCodeProcess"]
        fn get_exit_code_process(process: Handle, exit_code: *mut u32) -> i32;
        #[link_name = "InitializeProcThreadAttributeList"]
        fn initialize_proc_thread_attribute_list(
            list: *mut c_void,
            attribute_count: u32,
            flags: u32,
            size: *mut usize,
        ) -> i32;
        #[link_name = "ResizePseudoConsole"]
        fn resize_pseudo_console(console: PseudoConsoleHandle, size: Coord) -> i32;
        #[link_name = "TerminateProcess"]
        fn terminate_process_ffi(process: Handle, exit_code: u32) -> i32;
        #[link_name = "UpdateProcThreadAttribute"]
        fn update_proc_thread_attribute(
            list: *mut c_void,
            flags: u32,
            attribute: usize,
            value: *mut c_void,
            size: usize,
            previous_value: *mut c_void,
            return_size: *mut usize,
        ) -> i32;
        #[link_name = "WaitForSingleObject"]
        fn wait_for_single_object(handle: Handle, milliseconds: u32) -> u32;
    }

    pub(super) struct PseudoConsole {
        handle: PseudoConsoleHandle,
    }

    // An HPCON is an opaque kernel handle. The bridge serializes resize calls
    // on one input thread, and Arc keeps the handle alive until that thread exits.
    unsafe impl Send for PseudoConsole {}
    unsafe impl Sync for PseudoConsole {}

    impl PseudoConsole {
        fn create(size: TerminalSize, input: Handle, output: Handle) -> io::Result<Self> {
            let mut handle = ptr::null_mut();
            let result =
                unsafe { create_pseudo_console(coord(size), input, output, 0, &mut handle) };
            if result < 0 {
                return Err(hresult_error("CreatePseudoConsole", result));
            }
            if handle.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "CreatePseudoConsole returned a null handle",
                ));
            }
            Ok(Self { handle })
        }

        pub(super) fn resize(&self, size: TerminalSize) -> io::Result<()> {
            let result = unsafe { resize_pseudo_console(self.handle, coord(size)) };
            if result < 0 {
                Err(hresult_error("ResizePseudoConsole", result))
            } else {
                Ok(())
            }
        }
    }

    impl Drop for PseudoConsole {
        fn drop(&mut self) {
            unsafe { close_pseudo_console(self.handle) };
        }
    }

    struct AttributeList {
        list: *mut c_void,
        _storage: Vec<usize>,
    }

    impl AttributeList {
        fn for_pseudo_console(console: &PseudoConsole) -> io::Result<Self> {
            let mut byte_count = 0_usize;
            unsafe {
                initialize_proc_thread_attribute_list(ptr::null_mut(), 1, 0, &mut byte_count)
            };
            if byte_count == 0 {
                return Err(io::Error::last_os_error());
            }
            let word_size = size_of::<usize>();
            let word_count = byte_count.checked_add(word_size - 1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "attribute list is too large")
            })? / word_size;
            let mut storage = vec![0_usize; word_count];
            let list = storage.as_mut_ptr().cast::<c_void>();
            if unsafe { initialize_proc_thread_attribute_list(list, 1, 0, &mut byte_count) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let attributes = Self {
                list,
                _storage: storage,
            };
            if unsafe {
                update_proc_thread_attribute(
                    attributes.list,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                    console.handle,
                    size_of::<PseudoConsoleHandle>(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(attributes)
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            unsafe { delete_proc_thread_attribute_list(self.list) };
        }
    }

    pub(super) struct ConPtyProcess {
        pub(super) process: OwnedHandle,
        pub(super) input: File,
        pub(super) output: File,
        pub(super) console: Arc<PseudoConsole>,
        pub(super) job: super::process_tree::Job,
    }

    pub(super) fn spawn_shell(
        request: &StartRequest,
        size: TerminalSize,
    ) -> io::Result<ConPtyProcess> {
        if request.kind != SessionKind::Shell {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ConPTY requires a shell request",
            ));
        }
        let mut last_not_found = None;
        for candidate in &request.argv {
            let arguments = match candidate.to_ascii_lowercase().as_str() {
                "pwsh" | "pwsh.exe" => &["-NoLogo"][..],
                "powershell" | "powershell.exe" => &["-NoLogo"][..],
                "cmd" | "cmd.exe" => &["/Q"][..],
                _ => &[][..],
            };
            match spawn_program(
                candidate,
                arguments,
                request.working_directory.as_deref(),
                size,
            ) {
                Ok(process) => return Ok(process),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    last_not_found = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_not_found.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no shell candidate was supplied")
        }))
    }

    fn spawn_program(
        program: &str,
        arguments: &[&str],
        working_directory: Option<&str>,
        size: TerminalSize,
    ) -> io::Result<ConPtyProcess> {
        let (pseudo_input, input) = new_pipe()?;
        let (output, pseudo_output) = new_pipe()?;
        let job = super::process_tree::Job::new()?;
        let console = Arc::new(PseudoConsole::create(
            size,
            pseudo_input.as_raw_handle(),
            pseudo_output.as_raw_handle(),
        )?);
        drop(pseudo_input);
        drop(pseudo_output);

        let attributes = AttributeList::for_pseudo_console(&console)?;
        let mut startup = StartupInfoExW {
            startup_info: StartupInfoW::empty(),
            attribute_list: attributes.list,
        };
        startup.startup_info.cb = u32::try_from(size_of_val(&startup))
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "STARTUPINFOEXW is too large"))?;

        let command_line = super::windows_command_line(program, arguments);
        let mut command_line = OsStr::new(&command_line)
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let current_directory = working_directory.map(|directory| {
            OsStr::new(directory)
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        });
        let current_directory_pointer = current_directory
            .as_ref()
            .map_or(ptr::null(), |directory| directory.as_ptr());
        let mut information = ProcessInformation {
            process: ptr::null_mut(),
            thread: ptr::null_mut(),
            process_id: 0,
            thread_id: 0,
        };
        let created = unsafe {
            create_process_w(
                ptr::null(),
                command_line.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
                ptr::null_mut(),
                current_directory_pointer,
                &mut startup.startup_info,
                &mut information,
            )
        };
        if created == 0 {
            close_if_present(information.process);
            close_if_present(information.thread);
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(information.process) };
        let thread = unsafe { OwnedHandle::from_raw_handle(information.thread) };
        if let Err(error) = job.assign(process.as_raw_handle()) {
            let cleanup = terminate_process(&process, super::SESSION_CANCEL_EXIT_CODE as u32)
                .and_then(|()| wait_for_process(&process).map(|_| ()));
            return Err(process_setup_error("Job assignment", error, cleanup));
        }
        if let Err(error) = super::process_tree::resume_thread_handle(thread.as_raw_handle()) {
            let cleanup = job
                .terminate(super::SESSION_CANCEL_EXIT_CODE)
                .and_then(|()| wait_for_process(&process).map(|_| ()));
            return Err(process_setup_error("primary-thread resume", error, cleanup));
        }
        drop(thread);
        Ok(ConPtyProcess {
            process,
            input,
            output,
            console,
            job,
        })
    }

    pub(super) fn wait_for_process(process: &OwnedHandle) -> io::Result<i32> {
        wait_for_process_timeout(process, INFINITE)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "an infinite process wait unexpectedly timed out",
            )
        })
    }

    pub(super) fn wait_for_process_timeout(
        process: &OwnedHandle,
        milliseconds: u32,
    ) -> io::Result<Option<i32>> {
        let wait_result = unsafe { wait_for_single_object(process.as_raw_handle(), milliseconds) };
        if wait_result == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait_result != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut exit_code = 0_u32;
        if unsafe { get_exit_code_process(process.as_raw_handle(), &mut exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(exit_code as i32))
    }

    pub(super) fn terminate_process(process: &OwnedHandle, exit_code: u32) -> io::Result<()> {
        if unsafe { terminate_process_ffi(process.as_raw_handle(), exit_code) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn process_setup_error(
        operation: &str,
        error: io::Error,
        cleanup: io::Result<()>,
    ) -> io::Error {
        match cleanup {
            Ok(()) => error,
            Err(cleanup_error) => io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "{operation} failed: {error}; suspended process cleanup also failed: {cleanup_error}"
                ),
            ),
        }
    }

    fn new_pipe() -> io::Result<(File, File)> {
        let mut read = ptr::null_mut();
        let mut write = ptr::null_mut();
        if unsafe { create_pipe(&mut read, &mut write, ptr::null_mut(), 0) } == 0 {
            close_if_present(read);
            close_if_present(write);
            return Err(io::Error::last_os_error());
        }
        let read = unsafe { File::from_raw_handle(read) };
        let write = unsafe { File::from_raw_handle(write) };
        Ok((read, write))
    }

    fn coord(size: TerminalSize) -> Coord {
        Coord {
            x: size.columns as i16,
            y: size.rows as i16,
        }
    }

    fn hresult_error(operation: &str, result: i32) -> io::Error {
        io::Error::new(
            io::ErrorKind::Other,
            format!("{operation} failed with HRESULT 0x{:08x}", result as u32),
        )
    }

    fn close_if_present(handle: Handle) {
        if !handle.is_null() {
            unsafe {
                close_handle(handle);
            }
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_service {
    use std::ffi::c_void;
    use std::io;
    use std::ptr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, SyncSender};
    use std::sync::Mutex;

    use super::{run_agent, Configuration};

    const SERVICE_NAME: [u16; 9] = [
        b'L' as u16,
        b'S' as u16,
        b'W' as u16,
        b'A' as u16,
        b'g' as u16,
        b'e' as u16,
        b'n' as u16,
        b't' as u16,
        0,
    ];

    const SERVICE_WIN32_OWN_PROCESS: u32 = 0x0000_0010;
    const SERVICE_STOPPED: u32 = 0x0000_0001;
    const SERVICE_START_PENDING: u32 = 0x0000_0002;
    const SERVICE_STOP_PENDING: u32 = 0x0000_0003;
    const SERVICE_RUNNING: u32 = 0x0000_0004;
    const SERVICE_ACCEPT_STOP: u32 = 0x0000_0001;
    const SERVICE_ACCEPT_SHUTDOWN: u32 = 0x0000_0004;
    const SERVICE_CONTROL_STOP: u32 = 0x0000_0001;
    const SERVICE_CONTROL_INTERROGATE: u32 = 0x0000_0004;
    const SERVICE_CONTROL_SHUTDOWN: u32 = 0x0000_0005;
    const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;
    const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;

    type ServiceMainFunction = extern "system" fn(u32, *mut *mut u16);
    type HandlerFunction = extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32;
    type ServiceStatusHandle = *mut c_void;

    #[repr(C)]
    struct ServiceTableEntryW {
        service_name: *mut u16,
        service_main: Option<ServiceMainFunction>,
    }

    #[repr(C)]
    struct ServiceStatus {
        service_type: u32,
        current_state: u32,
        controls_accepted: u32,
        win32_exit_code: u32,
        service_specific_exit_code: u32,
        checkpoint: u32,
        wait_hint: u32,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn StartServiceCtrlDispatcherW(service_table: *const ServiceTableEntryW) -> i32;
        fn RegisterServiceCtrlHandlerExW(
            service_name: *const u16,
            handler: Option<HandlerFunction>,
            context: *mut c_void,
        ) -> ServiceStatusHandle;
        fn SetServiceStatus(
            status_handle: ServiceStatusHandle,
            service_status: *const ServiceStatus,
        ) -> i32;
    }

    static CONFIGURATION: Mutex<Option<Configuration>> = Mutex::new(None);
    static STOP_SENDER: Mutex<Option<SyncSender<()>>> = Mutex::new(None);
    static SERVICE_ERROR: Mutex<Option<String>> = Mutex::new(None);
    static STATUS_REPORT_LOCK: Mutex<()> = Mutex::new(());
    static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);
    static CURRENT_STATE: AtomicUsize = AtomicUsize::new(SERVICE_STOPPED as usize);

    pub(super) fn run(configuration: Configuration) -> Result<(), Box<dyn std::error::Error>> {
        *CONFIGURATION
            .lock()
            .map_err(|_| "Windows service configuration lock was poisoned")? = Some(configuration);
        *STOP_SENDER
            .lock()
            .map_err(|_| "Windows service stop lock was poisoned")? = None;
        *SERVICE_ERROR
            .lock()
            .map_err(|_| "Windows service error lock was poisoned")? = None;
        STATUS_HANDLE.store(0, Ordering::Release);
        CURRENT_STATE.store(SERVICE_STOPPED as usize, Ordering::Release);

        let mut service_name = SERVICE_NAME.to_vec();
        let service_table = [
            ServiceTableEntryW {
                service_name: service_name.as_mut_ptr(),
                service_main: Some(service_main),
            },
            ServiceTableEntryW {
                service_name: ptr::null_mut(),
                service_main: None,
            },
        ];
        let dispatched = unsafe { StartServiceCtrlDispatcherW(service_table.as_ptr()) };
        if dispatched == 0 {
            if let Ok(mut configuration) = CONFIGURATION.lock() {
                configuration.take();
            }
            return Err(format!(
                "could not connect LSWAgent to the Windows Service Control Manager: {}; \
                 --service must be started by SCM",
                io::Error::last_os_error()
            )
            .into());
        }

        STATUS_HANDLE.store(0, Ordering::Release);
        let error = SERVICE_ERROR
            .lock()
            .map_err(|_| "Windows service error lock was poisoned")?
            .take();
        match error {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    extern "system" fn service_main(_argument_count: u32, _arguments: *mut *mut u16) {
        let result = service_main_inner();
        let failed = result.is_err();
        let (win32_exit_code, service_specific_exit_code) = if failed {
            (ERROR_SERVICE_SPECIFIC_ERROR, 1)
        } else {
            (0, 0)
        };
        let stopped_result = report_status(
            SERVICE_STOPPED,
            0,
            win32_exit_code,
            service_specific_exit_code,
            0,
            0,
        );

        if let Err(error) = result {
            record_error(error.to_string());
        }
        if let Err(error) = stopped_result {
            record_error(format!("could not report LSWAgent as stopped: {error}"));
        }
    }

    fn service_main_inner() -> Result<(), Box<dyn std::error::Error>> {
        let status_handle = unsafe {
            RegisterServiceCtrlHandlerExW(
                SERVICE_NAME.as_ptr(),
                Some(service_control_handler),
                ptr::null_mut(),
            )
        };
        if status_handle.is_null() {
            return Err(format!(
                "could not register the LSWAgent service control handler: {}",
                io::Error::last_os_error()
            )
            .into());
        }
        STATUS_HANDLE.store(status_handle as usize, Ordering::Release);
        report_status(SERVICE_START_PENDING, 0, 0, 0, 1, 10_000)?;

        let configuration = CONFIGURATION
            .lock()
            .map_err(|_| "Windows service configuration lock was poisoned")?
            .take()
            .ok_or("Windows service configuration was not initialized")?;
        let (stop_sender, stop_receiver) = mpsc::sync_channel(1);
        *STOP_SENDER
            .lock()
            .map_err(|_| "Windows service stop lock was poisoned")? = Some(stop_sender);

        let result = run_agent(configuration, Some(&stop_receiver), |_address| {
            report_status(
                SERVICE_RUNNING,
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
                0,
                0,
                0,
                0,
            )?;
            Ok(())
        });

        if let Ok(mut stop_sender) = STOP_SENDER.lock() {
            stop_sender.take();
        }
        if CURRENT_STATE.load(Ordering::Acquire) == SERVICE_RUNNING as usize {
            report_status(SERVICE_STOP_PENDING, 0, 0, 0, 1, 10_000)?;
        }
        result
    }

    extern "system" fn service_control_handler(
        control: u32,
        _event_type: u32,
        _event_data: *mut c_void,
        _context: *mut c_void,
    ) -> u32 {
        match control {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                let _ = report_status(SERVICE_STOP_PENDING, 0, 0, 0, 1, 10_000);
                if let Ok(stop_sender) = STOP_SENDER.lock() {
                    if let Some(stop_sender) = stop_sender.as_ref() {
                        let _ = stop_sender.try_send(());
                    }
                }
                0
            }
            // SCM already retains the last status sent with SetServiceStatus;
            // acknowledging INTERROGATE asks it to return that cached state.
            SERVICE_CONTROL_INTERROGATE => 0,
            _ => ERROR_CALL_NOT_IMPLEMENTED,
        }
    }

    fn report_status(
        state: u32,
        controls_accepted: u32,
        win32_exit_code: u32,
        service_specific_exit_code: u32,
        checkpoint: u32,
        wait_hint: u32,
    ) -> io::Result<()> {
        let _status_guard = STATUS_REPORT_LOCK.lock().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "service status lock was poisoned")
        })?;
        let status_handle = STATUS_HANDLE.load(Ordering::Acquire) as ServiceStatusHandle;
        if status_handle.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "service status handle is not initialized",
            ));
        }
        let status = ServiceStatus {
            service_type: SERVICE_WIN32_OWN_PROCESS,
            current_state: state,
            controls_accepted,
            win32_exit_code,
            service_specific_exit_code,
            checkpoint,
            wait_hint,
        };
        if unsafe { SetServiceStatus(status_handle, &status) } == 0 {
            return Err(io::Error::last_os_error());
        }
        CURRENT_STATE.store(state as usize, Ordering::Release);
        Ok(())
    }

    fn record_error(message: String) {
        if let Ok(mut service_error) = SERVICE_ERROR.lock() {
            if service_error.is_none() {
                *service_error = Some(message);
            }
        }
    }
}

struct Configuration {
    listen: SocketAddr,
    token_file: PathBuf,
    once: bool,
    max_sessions: usize,
    service: bool,
}

impl Configuration {
    fn parse(arguments: &[std::ffi::OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut listen = SocketAddr::from(([0, 0, 0, 0], AGENT_GUEST_PORT));
        let mut token_file = env::var_os("LSW_AGENT_TOKEN_FILE").map(PathBuf::from);
        let mut once = false;
        let mut max_sessions = DEFAULT_MAX_SESSIONS;
        let mut service = false;
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
                "--help" | "-h" => {
                    println!(
                        "lsw-agent --token-file PATH [--listen IP:PORT] [--max-sessions N] [--once] [--service]\n\
                         The default listener is 0.0.0.0:5040 inside the restricted guest network.\n\
                         --service runs LSWAgent under the Windows Service Control Manager."
                    );
                    std::process::exit(0);
                }
                unknown => return Err(format!("unknown agent option {unknown:?}").into()),
            }
            index += 1;
        }
        Ok(Self {
            listen,
            token_file: token_file
                .ok_or("--token-file PATH or LSW_AGENT_TOKEN_FILE is required")?,
            once,
            max_sessions,
            service,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn shell_fallback_reaches_a_known_program() {
        let request = StartRequest {
            kind: SessionKind::Shell,
            argv: vec!["definitely-not-an-lsw-shell".to_owned(), "sh".to_owned()],
            working_directory: None,
        };
        let mut child = spawn_request(&request).expect("sh fallback should start");
        child.kill().expect("fixture process should stop");
        assert!(
            child.tree.is_none(),
            "successful teardown must disarm owner"
        );
        child
            .kill()
            .expect("a repeated cleanup request must be a no-op");
        child.wait().expect("fixture process should be reaped");
        assert!(
            child.tree.is_none(),
            "wait and Drop must not re-signal PGID"
        );
        // Exercise Drop with an already-reaped leader and disarmed owner. It
        // must not call Child::kill on a potentially reused Unix PID.
        drop(child);
    }

    #[test]
    fn token_parser_rejects_short_or_uppercase_secrets() {
        let root = std::env::temp_dir().join(format!("lsw-agent-token-{}", std::process::id()));
        fs::write(&root, "abcd").expect("fixture should be written");
        assert!(read_token(&root).is_err());
        fs::write(&root, "A".repeat(64)).expect("fixture should be updated");
        assert!(read_token(&root).is_err());
        fs::remove_file(root).expect("fixture should be removed");
    }

    #[test]
    fn session_limit_is_bounded() {
        let arguments = vec![
            "--token-file".into(),
            "token.txt".into(),
            "--max-sessions".into(),
            "4".into(),
        ];
        let configuration = Configuration::parse(&arguments).expect("configuration should parse");
        assert_eq!(configuration.max_sessions, 4);

        let invalid = vec![
            "--token-file".into(),
            "token.txt".into(),
            "--max-sessions".into(),
            "0".into(),
        ];
        assert!(Configuration::parse(&invalid).is_err());
    }

    #[test]
    fn service_mode_composes_with_existing_options() {
        let arguments = vec![
            "--service".into(),
            "--token-file".into(),
            "C:\\ProgramData\\LSW\\agent.token".into(),
            "--listen".into(),
            "127.0.0.1:55040".into(),
            "--max-sessions".into(),
            "8".into(),
        ];
        let configuration = Configuration::parse(&arguments).expect("configuration should parse");

        assert!(configuration.service);
        assert_eq!(configuration.listen, "127.0.0.1:55040".parse().unwrap());
        assert_eq!(configuration.max_sessions, 8);
        assert_eq!(
            configuration.token_file,
            PathBuf::from("C:\\ProgramData\\LSW\\agent.token")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn service_mode_fails_before_accessing_files_off_windows() {
        let error = run(vec![
            "--service".into(),
            "--token-file".into(),
            "a-file-that-must-not-be-read".into(),
        ])
        .expect_err("service mode must be Windows-only");

        assert_eq!(error.to_string(), "--service is only supported on Windows");
    }

    #[cfg(windows)]
    #[test]
    fn service_mode_requires_scm_before_accessing_files() {
        let error = run(vec![
            "--service".into(),
            "--token-file".into(),
            "a-file-that-must-not-be-read".into(),
        ])
        .expect_err("a normal test process must not connect to SCM as a service");

        assert!(
            error
                .to_string()
                .contains("--service must be started by SCM"),
            "unexpected SCM rejection: {error}"
        );
    }

    #[test]
    fn shutdown_channel_wakes_or_disconnects_the_accept_wait() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(()).expect("shutdown should be queued");
        assert!(wait_for_shutdown(&receiver, Duration::from_secs(1)));

        let (sender, receiver) = mpsc::sync_channel(1);
        drop(sender);
        assert!(shutdown_requested(&receiver));
    }

    #[test]
    fn normal_exit_wins_a_cancel_race() {
        assert_eq!(cancel_session_end(false), SessionEnd::Normal);
        assert_eq!(cancel_session_end(true), SessionEnd::Cancelled);
    }

    #[test]
    fn normal_exit_wins_a_lease_expiry_race() {
        assert_eq!(lease_session_end(false), SessionEnd::Normal);
        assert_eq!(lease_session_end(true), SessionEnd::LeaseExpired);
    }

    #[test]
    fn windows_command_line_quotes_empty_and_space_containing_arguments() {
        assert_eq!(windows_command_line("", &[]), "\"\"");
        assert_eq!(
            windows_command_line("C:\\Program Files\\pwsh.exe", &["hello world"]),
            "\"C:\\Program Files\\pwsh.exe\" \"hello world\""
        );
    }

    #[test]
    fn windows_command_line_quotes_quotes_and_trailing_backslashes() {
        assert_eq!(
            windows_command_line("tool.exe", &["a\"b"]),
            "tool.exe \"a\\\"b\""
        );
        assert_eq!(
            windows_command_line("tool.exe", &["C:\\path with space\\"]),
            "tool.exe \"C:\\path with space\\\\\""
        );
        assert_eq!(
            windows_command_line("tool.exe", &["plain\\"]),
            "tool.exe plain\\"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_job_terminates_a_spawned_descendant() {
        let mut child = spawn_program(
            "cmd.exe",
            &["/D", "/Q", "/C", "ping.exe -n 30 127.0.0.1 >NUL"],
            None,
        )
        .expect("cmd should start inside a Job Object");
        let leader = child.process.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        // Observe kernel-owned Job membership instead of treating shell output
        // as readiness. This keeps the lifecycle assertion independent of
        // PowerShell cold-start and Start-Process scheduling delays.
        let descendant = loop {
            let process_ids = child
                .tree
                .as_ref()
                .expect("spawned process should retain its Job Object")
                .process_ids()
                .expect("Job Object process membership should be queryable");
            if let Some(process_id) = process_ids
                .into_iter()
                .find(|process_id| *process_id != leader)
            {
                break process_id;
            }
            assert!(
                Instant::now() < deadline,
                "cmd did not start its ping descendant within 10 seconds"
            );
            thread::sleep(Duration::from_millis(10));
        };

        let (_, terminated) = child
            .terminate()
            .expect("Job Object termination should succeed");
        assert!(terminated);
        assert!(
            process_tree::wait_for_process_id_exit(descendant, 2_000)
                .expect("descendant state should be queryable"),
            "the Job Object descendant was still running after session cancellation"
        );
    }

    #[cfg(all(windows, target_pointer_width = "64"))]
    #[test]
    fn windows_job_ffi_layout_matches_the_x64_sdk_abi() {
        assert_eq!(process_tree::ffi_layout_sizes(), (64, 144, 28));
    }

    #[cfg(windows)]
    #[test]
    fn windows_conpty_process_starts_inside_a_job() {
        let request = StartRequest {
            kind: SessionKind::Shell,
            argv: vec!["cmd.exe".to_owned()],
            working_directory: None,
        };
        let process = windows_conpty::spawn_shell(
            &request,
            TerminalSize {
                columns: 80,
                rows: 25,
            },
        )
        .expect("ConPTY shell should start suspended, join its Job, and resume");
        process
            .job
            .terminate(SESSION_CANCEL_EXIT_CODE)
            .expect("ConPTY Job termination should succeed");
        assert_eq!(
            windows_conpty::wait_for_process(&process.process)
                .expect("ConPTY process should become signalled"),
            SESSION_CANCEL_EXIT_CODE
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_agent_does_not_advertise_conpty() {
        let capabilities = agent_capabilities();
        assert!(!capabilities
            .iter()
            .any(|capability| capability == lsw_core::CAPABILITY_CONPTY_V1));
        assert!(!capabilities
            .iter()
            .any(|capability| capability == lsw_core::CAPABILITY_TERMINAL_RESIZE_V1));
        assert!(capabilities
            .iter()
            .any(|capability| capability == lsw_core::CAPABILITY_SESSION_CONTROL_V1));
        assert!(capabilities
            .iter()
            .any(|capability| capability == lsw_core::CAPABILITY_SESSION_LEASE_V1));
    }

    #[cfg(unix)]
    fn controlled_test_connection(
        token: String,
    ) -> (TcpStream, Receiver<Result<(), String>>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let expected_token = token.clone();
        let active_sessions = Arc::new(AtomicUsize::new(1));
        let server_sessions = Arc::clone(&active_sessions);
        let (done_sender, done_receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = {
                let _slot = SessionSlot(server_sessions);
                let (stream, _) = listener.accept().expect("fixture should connect");
                handle_connection(stream, &expected_token).map_err(|error| error.to_string())
            };
            let _ = done_sender.send(result);
        });

        let mut stream = TcpStream::connect(address).expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout should apply");
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token,
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
        )
        .expect("hello should be sent");
        let response = read_frame(&mut stream).expect("hello response should arrive");
        assert_eq!(response.kind, FrameKind::HelloOk);
        let hello = ServerHello::decode(&response.payload).expect("server hello should decode");
        assert!(hello
            .capabilities
            .iter()
            .any(|capability| capability == CAPABILITY_SESSION_CONTROL_V1));
        assert!(hello
            .capabilities
            .iter()
            .any(|capability| capability == CAPABILITY_SESSION_LEASE_V1));
        (stream, done_receiver, active_sessions)
    }

    #[cfg(unix)]
    fn send_session_options(stream: &mut TcpStream) {
        let options = SessionOptions {
            cancel_on_disconnect: true,
        };
        write_frame(
            stream,
            &Frame::new(FrameKind::SessionOptions, options.encode()),
        )
        .expect("session options should be sent");
    }

    #[cfg(unix)]
    fn send_session_lease(stream: &mut TcpStream, timeout_millis: u32) {
        let lease = SessionLease::new(timeout_millis).expect("test lease should be valid");
        write_frame(stream, &Frame::new(FrameKind::SessionLease, lease.encode()))
            .expect("session lease should be sent");
    }

    #[cfg(unix)]
    fn send_exec(stream: &mut TcpStream, argv: &[&str]) {
        let request = StartRequest {
            kind: SessionKind::Exec,
            argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
            working_directory: None,
        };
        write_frame(
            stream,
            &Frame::new(FrameKind::Start, request.encode().unwrap()),
        )
        .expect("start should be sent");
    }

    #[cfg(unix)]
    fn send_waiting_descendant_tree(stream: &mut TcpStream) {
        // outer sh -> inner sh -> sleep. Every process inherits the session
        // output pipes; killing only the outer process would make bridge joins
        // and the session slot hang until sleep exits.
        send_exec(
            stream,
            &[
                "sh",
                "-c",
                "sh -c 'sleep 30 & printf tree-ready; wait' & wait",
            ],
        );
        let ready = read_frame(stream).expect("descendant readiness should arrive");
        assert_eq!(ready.kind, FrameKind::Stdout);
        assert_eq!(ready.payload, b"tree-ready");
    }

    #[cfg(unix)]
    fn collect_process(stream: &mut TcpStream) -> (Vec<u8>, i32) {
        let mut stdout = Vec::new();
        loop {
            let frame = read_frame(stream).expect("process response should arrive");
            match frame.kind {
                FrameKind::Stdout => stdout.extend(frame.payload),
                FrameKind::Stderr => {}
                FrameKind::Exit => return (stdout, lsw_core::decode_exit(&frame.payload).unwrap()),
                other => panic!("unexpected process frame {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    fn assert_session_released(
        done: Receiver<Result<(), String>>,
        active_sessions: Arc<AtomicUsize>,
    ) {
        done.recv_timeout(Duration::from_secs(2))
            .expect("server session should finish promptly")
            .expect("server session should succeed");
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_cancel_terminates_a_controlled_process() {
        let (mut stream, done, active_sessions) = controlled_test_connection("d".repeat(64));
        send_session_options(&mut stream);
        send_waiting_descendant_tree(&mut stream);
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::SessionCancel, Vec::new()),
        )
        .expect("cancel should be sent");

        assert_eq!(
            collect_process(&mut stream),
            (Vec::new(), SESSION_CANCEL_EXIT_CODE)
        );
        drop(stream);
        assert_session_released(done, active_sessions);
    }

    #[cfg(unix)]
    #[test]
    fn session_control_is_unavailable_before_authentication() {
        let expected_token = "7".repeat(64);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("fixture should connect");
            handle_connection(stream, &expected_token).map_err(|error| error.to_string())
        });
        let mut stream = TcpStream::connect(address).expect("client should connect");
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token: "8".repeat(64),
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
        )
        .expect("hello should be sent");
        let response = read_frame(&mut stream).expect("authentication error should arrive");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("authentication"));
        assert!(server
            .join()
            .expect("fixture should not panic")
            .expect_err("authentication should fail")
            .contains("authentication"));
    }

    #[cfg(unix)]
    #[test]
    fn controlled_disconnect_terminates_process_and_releases_slot() {
        let (mut stream, done, active_sessions) = controlled_test_connection("e".repeat(64));
        send_session_options(&mut stream);
        send_waiting_descendant_tree(&mut stream);
        drop(stream);

        assert_session_released(done, active_sessions);
    }

    #[cfg(unix)]
    #[test]
    fn leased_session_expires_and_releases_its_process_tree() {
        let (mut stream, done, active_sessions) = controlled_test_connection("4".repeat(64));
        send_session_options(&mut stream);
        send_session_lease(&mut stream, 1_000);
        send_waiting_descendant_tree(&mut stream);

        assert!(
            read_frame(&mut stream).is_err(),
            "lease expiry closes the half-open transport instead of risking a blocking error write"
        );
        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .expect("leased server session should finish promptly")
            .expect_err("lease expiry should fail the session")
            .contains("lease expired"));
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn lease_expiry_unblocks_output_backpressure_and_releases_slot() {
        let (mut stream, done, active_sessions) = controlled_test_connection("8".repeat(64));
        send_session_options(&mut stream);
        send_session_lease(&mut stream, 1_000);
        send_exec(&mut stream, &["sh", "-c", "exec yes lsw-lease-output"]);

        // Deliberately never read process output. The agent output bridge will
        // eventually block in TCP write while holding its shared writer lock.
        // Lease expiry must use socket shutdown, not that lock, to free it.
        assert!(done
            .recv_timeout(Duration::from_secs(3))
            .expect("backpressured leased session should finish promptly")
            .expect_err("lease expiry should fail the session")
            .contains("lease expired"));
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
        drop(stream);
    }

    #[cfg(unix)]
    #[test]
    fn timely_heartbeats_keep_an_idle_leased_session_alive() {
        let (mut stream, done, active_sessions) = controlled_test_connection("5".repeat(64));
        send_session_options(&mut stream);
        send_session_lease(&mut stream, 2_000);
        send_waiting_descendant_tree(&mut stream);

        // The total duration exceeds one lease, while every individual gap is
        // comfortably below it. This proves idle-but-healthy sessions survive.
        for _ in 0..9 {
            thread::sleep(Duration::from_millis(250));
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::SessionHeartbeat, Vec::new()),
            )
            .expect("heartbeat should be sent");
        }
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::SessionCancel, Vec::new()),
        )
        .expect("cancel should be sent");
        assert_eq!(
            collect_process(&mut stream),
            (Vec::new(), SESSION_CANCEL_EXIT_CODE)
        );
        drop(stream);
        assert_session_released(done, active_sessions);
    }

    #[cfg(unix)]
    #[test]
    fn heartbeat_requires_a_leased_session_and_empty_payload() {
        for (token, with_lease, payload) in [("a", false, Vec::new()), ("b", true, vec![1])] {
            let (mut stream, done, active_sessions) = controlled_test_connection(token.repeat(64));
            send_session_options(&mut stream);
            if with_lease {
                send_session_lease(&mut stream, 5_000);
            }
            send_waiting_descendant_tree(&mut stream);
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::SessionHeartbeat, payload),
            )
            .expect("invalid heartbeat should be sent");

            let response = read_frame(&mut stream).expect("protocol error should arrive");
            assert_eq!(response.kind, FrameKind::Error);
            assert!(String::from_utf8_lossy(&response.payload).contains("requires a leased"));
            assert!(done
                .recv_timeout(Duration::from_secs(2))
                .expect("server session should finish promptly")
                .is_err());
            assert_eq!(active_sessions.load(Ordering::Acquire), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn lease_requires_options_and_can_appear_only_once_before_start() {
        let (mut legacy, legacy_done, legacy_sessions) = controlled_test_connection("c".repeat(64));
        send_session_lease(&mut legacy, 5_000);
        let response = read_frame(&mut legacy).expect("legacy lease should be rejected");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("unsupported request"));
        assert!(legacy_done
            .recv_timeout(Duration::from_secs(2))
            .expect("legacy request should finish promptly")
            .is_err());
        assert_eq!(legacy_sessions.load(Ordering::Acquire), 0);

        let (mut duplicate, duplicate_done, duplicate_sessions) =
            controlled_test_connection("d".repeat(64));
        send_session_options(&mut duplicate);
        send_session_lease(&mut duplicate, 5_000);
        send_session_lease(&mut duplicate, 5_000);
        let response = read_frame(&mut duplicate).expect("duplicate lease should be rejected");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("one SESSION_LEASE"));
        assert!(duplicate_done
            .recv_timeout(Duration::from_secs(2))
            .expect("duplicate request should finish promptly")
            .is_err());
        assert_eq!(duplicate_sessions.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn controlled_frames_reject_nonempty_cancel_payloads() {
        let (mut stream, done, active_sessions) = controlled_test_connection("9".repeat(64));
        send_session_options(&mut stream);
        send_waiting_descendant_tree(&mut stream);
        write_frame(&mut stream, &Frame::new(FrameKind::SessionCancel, [1]))
            .expect("malformed cancel should be sent");

        let response = read_frame(&mut stream).expect("protocol error should arrive");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("empty payload"));
        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .expect("server session should finish promptly")
            .is_err());
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn normal_leader_exit_cleans_background_descendants_and_releases_slot() {
        let (mut stream, done, active_sessions) = controlled_test_connection("6".repeat(64));
        send_session_options(&mut stream);
        // The shell exits normally without waiting for this background sleep.
        // The sleep inherits stdout/stderr, so an agent that owns only the
        // leader blocks in its output bridge instead of sending EXIT.
        send_exec(&mut stream, &["sh", "-c", "sleep 30 & printf normal-tree"]);

        assert_eq!(collect_process(&mut stream), (b"normal-tree".to_vec(), 0));
        drop(stream);
        assert_session_released(done, active_sessions);
    }

    #[cfg(unix)]
    #[test]
    fn session_options_only_prefix_process_start_requests() {
        for invalid_kind in [
            FrameKind::SessionOptions,
            FrameKind::Ping,
            FrameKind::FileGet,
            FrameKind::FilePut,
            FrameKind::SessionCancel,
            FrameKind::StdinClose,
        ] {
            let token_byte = match invalid_kind {
                FrameKind::SessionOptions => 'a',
                FrameKind::Ping => 'b',
                FrameKind::FileGet => 'c',
                FrameKind::FilePut => 'd',
                FrameKind::SessionCancel => 'e',
                FrameKind::StdinClose => 'f',
                _ => unreachable!(),
            };
            let (mut stream, done, active_sessions) =
                controlled_test_connection(token_byte.to_string().repeat(64));
            send_session_options(&mut stream);
            let payload = if invalid_kind == FrameKind::SessionOptions {
                SessionOptions {
                    cancel_on_disconnect: true,
                }
                .encode()
            } else {
                Vec::new()
            };
            write_frame(&mut stream, &Frame::new(invalid_kind, payload))
                .expect("invalid controlled request should be sent");
            let response = read_frame(&mut stream).expect("protocol error should arrive");
            assert_eq!(response.kind, FrameKind::Error);
            assert!(String::from_utf8_lossy(&response.payload).contains("must be followed"));
            assert!(done
                .recv_timeout(Duration::from_secs(2))
                .expect("server session should finish promptly")
                .is_err());
            assert_eq!(active_sessions.load(Ordering::Acquire), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unknown_session_option_flags_are_rejected_before_spawn() {
        let (mut stream, done, active_sessions) = controlled_test_connection("2".repeat(64));
        write_frame(&mut stream, &Frame::new(FrameKind::SessionOptions, [2]))
            .expect("unknown options should be sent");
        let response = read_frame(&mut stream).expect("protocol error should arrive");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("unknown flags"));

        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .expect("server session should finish promptly")
            .expect_err("unknown option flags should fail")
            .contains("unknown flags"));
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_control_frame_is_rejected_without_starting_a_process() {
        let (mut stream, done, active_sessions) = controlled_test_connection("3".repeat(64));
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::SessionCancel, Vec::new()),
        )
        .expect("legacy cancel should be sent");
        let response = read_frame(&mut stream).expect("protocol error should arrive");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("unsupported request"));
        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .expect("server session should finish promptly")
            .is_err());
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn controlled_stdin_close_delivers_eof_without_cancelling() {
        let (mut stream, done, active_sessions) = controlled_test_connection("f".repeat(64));
        send_session_options(&mut stream);
        send_exec(
            &mut stream,
            &["sh", "-c", "IFS= read -r value; printf controlled-eof"],
        );
        write_frame(&mut stream, &Frame::new(FrameKind::StdinClose, Vec::new()))
            .expect("stdin close should be sent");

        assert_eq!(
            collect_process(&mut stream),
            (b"controlled-eof".to_vec(), 0)
        );
        drop(stream);
        assert_session_released(done, active_sessions);
    }

    #[cfg(unix)]
    #[test]
    fn controlled_child_stdin_failure_terminates_process_and_releases_slot() {
        let (mut stream, done, active_sessions) = controlled_test_connection("0".repeat(64));
        send_session_options(&mut stream);
        send_exec(
            &mut stream,
            &["sh", "-c", "exec 0<&-; printf ready; exec sleep 5"],
        );

        let ready = read_frame(&mut stream).expect("child readiness should arrive");
        assert_eq!(ready.kind, FrameKind::Stdout);
        assert_eq!(ready.payload, b"ready");
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Stdin, b"input after child closed stdin"),
        )
        .expect("stdin payload should be sent");

        let response = read_frame(&mut stream).expect("protocol error should arrive");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("child stdin"));
        drop(stream);
        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .expect("server session should finish promptly")
            .expect_err("child stdin failure should fail the session")
            .contains("child stdin"));
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_half_close_remains_stdin_eof_not_cancellation() {
        let (mut stream, done, active_sessions) = controlled_test_connection("1".repeat(64));
        send_exec(
            &mut stream,
            &["sh", "-c", "IFS= read -r value; printf legacy-eof"],
        );
        stream
            .shutdown(Shutdown::Write)
            .expect("legacy write side should close");

        assert_eq!(collect_process(&mut stream), (b"legacy-eof".to_vec(), 0));
        drop(stream);
        assert_session_released(done, active_sessions);
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_loopback_exec_streams_output_and_exit_status() {
        let token = "a".repeat(64);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let expected_token = token.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("fixture should connect");
            handle_connection(stream, &expected_token).expect("agent request should succeed");
        });

        let mut stream = TcpStream::connect(address).expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout should apply");
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token,
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
        )
        .expect("hello should be sent");
        let response = read_frame(&mut stream).expect("hello response should arrive");
        assert_eq!(response.kind, FrameKind::HelloOk);
        ServerHello::decode(&response.payload).expect("server hello should decode");

        let request = StartRequest {
            kind: SessionKind::Exec,
            argv: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf stdout-bytes; printf stderr-bytes >&2; exit 7".to_owned(),
            ],
            working_directory: None,
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Start, request.encode().unwrap()),
        )
        .expect("start should be sent");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = loop {
            let frame = read_frame(&mut stream).expect("process frame should arrive");
            match frame.kind {
                FrameKind::Stdout => stdout.extend(frame.payload),
                FrameKind::Stderr => stderr.extend(frame.payload),
                FrameKind::Exit => break lsw_core::decode_exit(&frame.payload).unwrap(),
                other => panic!("unexpected process frame {other:?}"),
            }
        };
        assert_eq!(stdout, b"stdout-bytes");
        assert_eq!(stderr, b"stderr-bytes");
        assert_eq!(exit, 7);
        drop(stream);
        server.join().expect("agent fixture should finish");
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_loopback_file_transfer_preserves_unicode_and_bytes() {
        fn connect(token: &str) -> (TcpStream, thread::JoinHandle<Result<(), String>>) {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
            let address = listener
                .local_addr()
                .expect("listener should have an address");
            let expected_token = token.to_owned();
            let server = thread::spawn(move || {
                let (stream, _) = listener.accept().expect("fixture should connect");
                handle_connection(stream, &expected_token).map_err(|error| error.to_string())
            });
            let mut stream = TcpStream::connect(address).expect("client should connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout should apply");
            let hello = ClientHello {
                version: AGENT_PROTOCOL_VERSION,
                token: token.to_owned(),
            };
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
            )
            .expect("hello should be sent");
            let response = read_frame(&mut stream).expect("hello response should arrive");
            assert_eq!(response.kind, FrameKind::HelloOk);
            ServerHello::decode(&response.payload).expect("server hello should decode");
            (stream, server)
        }

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-agent-e2e-{nonce}"));
        fs::create_dir(&root).expect("fixture directory should be created");
        let destination = root.join("香港-資料.bin");
        let destination_text = destination.to_string_lossy().into_owned();
        let contents = b"binary\0payload\xff\nUTF-8:\xe9\xa6\x99\xe6\xb8\xaf";
        let token = "b".repeat(64);

        let (mut upload, upload_server) = connect(&token);
        let put = FilePutRequest {
            destination: destination_text.clone(),
            length: contents.len() as u64,
        };
        write_frame(
            &mut upload,
            &Frame::new(FrameKind::FilePut, put.encode().unwrap()),
        )
        .expect("upload request should be sent");
        assert_eq!(
            read_frame(&mut upload)
                .expect("upload ready should arrive")
                .kind,
            FrameKind::Pong
        );
        for chunk in contents.chunks(7) {
            write_frame(
                &mut upload,
                &Frame::new(FrameKind::FileData, chunk.to_vec()),
            )
            .expect("upload data should be sent");
        }
        write_frame(
            &mut upload,
            &Frame::new(
                FrameKind::FileDone,
                encode_file_length(contents.len() as u64),
            ),
        )
        .expect("upload completion should be sent");
        let completion = read_frame(&mut upload).expect("upload completion should arrive");
        assert_eq!(completion.kind, FrameKind::FileDone);
        assert_eq!(
            decode_file_length(&completion.payload).unwrap(),
            contents.len() as u64
        );
        drop(upload);
        upload_server
            .join()
            .expect("upload fixture should finish")
            .expect("upload should succeed");
        assert_eq!(fs::read(&destination).unwrap(), contents);

        let (mut download, download_server) = connect(&token);
        let get = FileGetRequest {
            source: destination_text,
        };
        write_frame(
            &mut download,
            &Frame::new(FrameKind::FileGet, get.encode().unwrap()),
        )
        .expect("download request should be sent");
        let mut received = Vec::new();
        loop {
            let frame = read_frame(&mut download).expect("download frame should arrive");
            match frame.kind {
                FrameKind::FileData => received.extend(frame.payload),
                FrameKind::FileDone => {
                    assert_eq!(
                        decode_file_length(&frame.payload).unwrap(),
                        received.len() as u64
                    );
                    break;
                }
                other => panic!("unexpected download frame {other:?}"),
            }
        }
        drop(download);
        download_server
            .join()
            .expect("download fixture should finish")
            .expect("download should succeed");
        assert_eq!(received, contents);

        fs::remove_dir_all(root).expect("fixture directory should be removable");
    }

    #[cfg(unix)]
    #[test]
    fn independent_authenticated_sessions_run_concurrently() {
        fn connect(address: SocketAddr, token: &str) -> TcpStream {
            let mut stream = TcpStream::connect(address).expect("client should connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout should apply");
            let hello = ClientHello {
                version: AGENT_PROTOCOL_VERSION,
                token: token.to_owned(),
            };
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
            )
            .expect("hello should be sent");
            assert_eq!(
                read_frame(&mut stream)
                    .expect("hello response should arrive")
                    .kind,
                FrameKind::HelloOk
            );
            stream
        }

        fn start(stream: &mut TcpStream, script: &str) {
            let request = StartRequest {
                kind: SessionKind::Exec,
                argv: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
                working_directory: None,
            };
            write_frame(
                stream,
                &Frame::new(FrameKind::Start, request.encode().unwrap()),
            )
            .expect("start should be sent");
        }

        fn collect(stream: &mut TcpStream) -> (Vec<u8>, i32) {
            let mut stdout = Vec::new();
            loop {
                let frame = read_frame(stream).expect("process frame should arrive");
                match frame.kind {
                    FrameKind::Stdout => stdout.extend(frame.payload),
                    FrameKind::Stderr => {}
                    FrameKind::Exit => {
                        return (stdout, lsw_core::decode_exit(&frame.payload).unwrap())
                    }
                    other => panic!("unexpected process frame {other:?}"),
                }
            }
        }

        let token = "c".repeat(64);
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let expected_token = token.clone();
        let server = thread::spawn(move || {
            let mut sessions = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().expect("fixture should connect");
                let token = expected_token.clone();
                sessions.push(thread::spawn(move || {
                    handle_connection(stream, &token).map_err(|error| error.to_string())
                }));
            }
            for session in sessions {
                session
                    .join()
                    .expect("session should not panic")
                    .expect("session should succeed");
            }
        });

        let mut blocked = connect(address, &token);
        start(
            &mut blocked,
            "IFS= read -r value; printf 'first-%s' \"$value\"",
        );

        let mut independent = connect(address, &token);
        start(&mut independent, "printf second");
        assert_eq!(collect(&mut independent), (b"second".to_vec(), 0));

        write_frame(
            &mut blocked,
            &Frame::new(FrameKind::Stdin, b"ready\n".to_vec()),
        )
        .expect("blocked session input should be sent");
        assert_eq!(collect(&mut blocked), (b"first-ready".to_vec(), 0));
        drop(independent);
        drop(blocked);
        server.join().expect("server fixture should finish");
    }
}
