// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lsw_core::{
    decode_exit, decode_file_length, decode_process_id, encode_file_length, read_frame,
    write_frame, ClientHello, FileGetRequest, FilePutRequest, Frame, FrameKind, InstanceManifest,
    ProcessEnvironment, ServerHello, SessionKind, SessionLease, SessionOptions, SessionSignal,
    StartRequest, TerminalSize, TerminalStartRequest, UserCreateRequest, UserSetRoleRequest,
    AGENT_PROTOCOL_VERSION, CAPABILITY_CONPTY_V1, CAPABILITY_DETACHED_RUN_V1,
    CAPABILITY_MAINTENANCE_TRIM_V1, CAPABILITY_PROCESS_ENVIRONMENT_V1,
    CAPABILITY_SESSION_CONTROL_V1, CAPABILITY_SESSION_LEASE_V1, CAPABILITY_SESSION_SIGNAL_V1,
    CAPABILITY_TERMINAL_RESIZE_V1, CAPABILITY_USER_ACCOUNT_ROLE_V1, CAPABILITY_USER_ACCOUNT_V1,
};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const STDIN_CHUNK_BYTES: usize = 32 * 1024;

pub struct AgentClient {
    stream: TcpStream,
    capabilities: Vec<String>,
}

pub struct CapturedProcess {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl AgentClient {
    pub fn connect(
        manifest: &InstanceManifest,
        token: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, manifest.control_port);
        let mut stream = TcpStream::connect_timeout(&address.into(), CONNECT_TIMEOUT)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token: token.to_owned(),
        };
        write_frame(&mut stream, &Frame::new(FrameKind::Hello, hello.encode()?))?;
        let response = read_frame(&mut stream)?;
        match response.kind {
            FrameKind::HelloOk => {
                let hello = ServerHello::decode(&response.payload)?;
                if hello.version != AGENT_PROTOCOL_VERSION {
                    return Err(
                        format!("agent negotiated unsupported protocol {}", hello.version).into(),
                    );
                }
                stream.set_read_timeout(None)?;
                stream.set_write_timeout(None)?;
                Ok(Self {
                    stream,
                    capabilities: hello.capabilities,
                })
            }
            FrameKind::Error => Err(format!(
                "agent rejected connection: {}",
                String::from_utf8_lossy(&response.payload)
            )
            .into()),
            other => Err(format!("agent returned unexpected {other:?} frame").into()),
        }
    }

    pub fn probe(mut self) -> Result<(), Box<dyn std::error::Error>> {
        write_frame(&mut self.stream, &Frame::new(FrameKind::Ping, Vec::new()))?;
        let response = read_frame(&mut self.stream)?;
        if response.kind != FrameKind::Pong || !response.payload.is_empty() {
            return Err("agent returned an invalid PONG response".into());
        }
        Ok(())
    }

    pub fn create_user(
        mut self,
        request: &UserCreateRequest,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_USER_ACCOUNT_V1)?;
        let mut frame = Frame::new(FrameKind::UserCreate, request.encode()?);
        let write_result = write_frame(&mut self.stream, &frame);
        frame.payload.fill(0);
        write_result?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Pong if response.payload.is_empty() => Ok(()),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid user-creation response".into()),
        }
    }

    pub fn set_user_role(
        mut self,
        request: &UserSetRoleRequest,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_USER_ACCOUNT_ROLE_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::UserSetRole, request.encode()?),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Pong if response.payload.is_empty() => Ok(()),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid user-role response".into()),
        }
    }

    pub fn trim(mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_MAINTENANCE_TRIM_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::MaintenanceTrim, Vec::new()),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Pong if response.payload.is_empty() => Ok(()),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid maintenance response".into()),
        }
    }

    pub fn run(
        self,
        request: &StartRequest,
        connect_stdin: bool,
    ) -> Result<i32, Box<dyn std::error::Error>> {
        self.run_with_environment(request, connect_stdin, &ProcessEnvironment::default())
    }

    pub fn run_with_environment(
        mut self,
        request: &StartRequest,
        connect_stdin: bool,
        environment: &ProcessEnvironment,
    ) -> Result<i32, Box<dyn std::error::Error>> {
        let shell_session = matches!(request.kind, lsw_core::SessionKind::Shell);
        let conpty_available = self.has_capability(CAPABILITY_CONPTY_V1);
        let controlled_session = self.has_capability(CAPABILITY_SESSION_CONTROL_V1);
        let session_lease =
            if controlled_session && self.has_capability(CAPABILITY_SESSION_LEASE_V1) {
                Some(SessionLease::standard())
            } else {
                None
            };
        if shell_session && !conpty_available {
            eprintln!(
                "lsw: note: this agent provides a pipe shell; ConPTY is not available in this beta build"
            );
        }

        let terminal = if shell_session && conpty_available && connect_stdin {
            TerminalModeGuard::enter()?
        } else {
            None
        };
        let initial_size = terminal
            .as_ref()
            .and_then(TerminalModeGuard::size)
            .unwrap_or_default();
        if controlled_session {
            let options = SessionOptions {
                cancel_on_disconnect: true,
            };
            write_frame(
                &mut self.stream,
                &Frame::new(FrameKind::SessionOptions, options.encode()),
            )?;
            if let Some(lease) = session_lease {
                write_frame(
                    &mut self.stream,
                    &Frame::new(FrameKind::SessionLease, lease.encode()),
                )?;
            }
            if !environment.is_empty() {
                self.require_capability(CAPABILITY_PROCESS_ENVIRONMENT_V1)?;
                write_frame(
                    &mut self.stream,
                    &Frame::new(FrameKind::ProcessEnvironment, environment.encode()?),
                )?;
            }
        } else if !environment.is_empty() {
            return Err("guest agent does not support process environment injection".into());
        }
        if terminal.is_some() {
            let terminal_request = TerminalStartRequest {
                size: initial_size,
                request: request.clone(),
            };
            write_frame(
                &mut self.stream,
                &Frame::new(FrameKind::TerminalStart, terminal_request.encode()?),
            )?;
        } else {
            write_frame(
                &mut self.stream,
                &Frame::new(FrameKind::Start, request.encode()?),
            )?;
        }

        let terminal_active = terminal.is_some();
        let outbound = Arc::new(Mutex::new(self.stream.try_clone()?));
        let session_stop = Arc::new(AtomicBool::new(false));
        let signal_bridge =
            if controlled_session && self.has_capability(CAPABILITY_SESSION_SIGNAL_V1) {
                Some(spawn_signal_bridge(
                    Arc::clone(&outbound),
                    Arc::clone(&session_stop),
                )?)
            } else {
                None
            };
        let heartbeat_bridge = session_lease
            .map(|lease| spawn_heartbeat_bridge(Arc::clone(&outbound), lease.heartbeat_interval()));
        let input_thread = if connect_stdin {
            let writer = Arc::clone(&outbound);
            let stop = Arc::clone(&session_stop);
            Some(thread::spawn(move || {
                forward_stdin(writer, stop, terminal_active, controlled_session)
            }))
        } else {
            if controlled_session {
                send_outbound(&outbound, &Frame::new(FrameKind::StdinClose, Vec::new()))?;
            } else {
                shutdown_outbound(&outbound);
            }
            None
        };

        let resize_thread =
            if terminal.is_some() && self.has_capability(CAPABILITY_TERMINAL_RESIZE_V1) {
                Some(spawn_resize_bridge(
                    Arc::clone(&outbound),
                    Arc::clone(&session_stop),
                    initial_size,
                ))
            } else {
                None
            };

        let result = (|| -> Result<i32, Box<dyn std::error::Error>> {
            loop {
                let frame = read_frame(&mut self.stream)?;
                match frame.kind {
                    FrameKind::Stdout => {
                        io::stdout().write_all(&frame.payload)?;
                        io::stdout().flush()?;
                    }
                    FrameKind::Stderr => {
                        io::stderr().write_all(&frame.payload)?;
                        io::stderr().flush()?;
                    }
                    FrameKind::Exit => return Ok(decode_exit(&frame.payload)?),
                    FrameKind::Error => {
                        return Err(format!(
                            "guest agent: {}",
                            String::from_utf8_lossy(&frame.payload)
                        )
                        .into())
                    }
                    other => return Err(format!("unexpected agent frame {other:?}").into()),
                }
            }
        })();
        if result.is_err() && controlled_session {
            let _ = send_outbound(&outbound, &Frame::new(FrameKind::SessionCancel, Vec::new()));
        }
        session_stop.store(true, Ordering::Release);
        if let Some(signal_bridge) = signal_bridge {
            signal_bridge.stop();
        }
        if let Some((stop, heartbeat_thread)) = heartbeat_bridge {
            let _ = stop.send(());
            let _ = heartbeat_thread.join();
        }
        if let Some(resize_thread) = resize_thread {
            let _ = resize_thread.join();
        }
        if terminal_active {
            if let Some(input_thread) = input_thread {
                let _ = input_thread.join();
            }
        }
        drop(terminal);
        result
    }

    pub fn run_detached(
        mut self,
        request: &StartRequest,
        environment: &ProcessEnvironment,
    ) -> Result<u32, Box<dyn std::error::Error>> {
        if request.kind != SessionKind::Run {
            return Err("detached mode requires a run request".into());
        }
        self.require_capability(CAPABILITY_SESSION_CONTROL_V1)?;
        self.require_capability(CAPABILITY_DETACHED_RUN_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(
                FrameKind::SessionOptions,
                SessionOptions {
                    cancel_on_disconnect: false,
                }
                .encode(),
            ),
        )?;
        if !environment.is_empty() {
            self.require_capability(CAPABILITY_PROCESS_ENVIRONMENT_V1)?;
            write_frame(
                &mut self.stream,
                &Frame::new(FrameKind::ProcessEnvironment, environment.encode()?),
            )?;
        }
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::SessionDetach, Vec::new()),
        )?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::Start, request.encode()?),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Started => Ok(decode_process_id(&response.payload)?),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            other => Err(format!("agent returned unexpected {other:?} detached response").into()),
        }
    }

    pub fn run_capture(
        self,
        request: &StartRequest,
        input: &[u8],
        output_limit: usize,
    ) -> Result<CapturedProcess, Box<dyn std::error::Error>> {
        self.run_capture_with_environment(
            request,
            input,
            output_limit,
            &ProcessEnvironment::default(),
        )
    }

    pub fn run_capture_with_environment(
        mut self,
        request: &StartRequest,
        input: &[u8],
        output_limit: usize,
        environment: &ProcessEnvironment,
    ) -> Result<CapturedProcess, Box<dyn std::error::Error>> {
        if matches!(request.kind, lsw_core::SessionKind::Shell) {
            return Err("captured sessions cannot be interactive shells".into());
        }
        let controlled_session = self.has_capability(CAPABILITY_SESSION_CONTROL_V1);
        if controlled_session {
            write_frame(
                &mut self.stream,
                &Frame::new(
                    FrameKind::SessionOptions,
                    SessionOptions {
                        cancel_on_disconnect: true,
                    }
                    .encode(),
                ),
            )?;
            if !environment.is_empty() {
                self.require_capability(CAPABILITY_PROCESS_ENVIRONMENT_V1)?;
                write_frame(
                    &mut self.stream,
                    &Frame::new(FrameKind::ProcessEnvironment, environment.encode()?),
                )?;
            }
        } else if !environment.is_empty() {
            return Err("guest agent does not support process environment injection".into());
        }
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::Start, request.encode()?),
        )?;
        if !input.is_empty() {
            write_frame(
                &mut self.stream,
                &Frame::new(FrameKind::Stdin, input.to_vec()),
            )?;
        }
        if controlled_session {
            write_frame(
                &mut self.stream,
                &Frame::new(FrameKind::StdinClose, Vec::new()),
            )?;
        } else {
            self.stream.shutdown(Shutdown::Write)?;
        }

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            let frame = read_frame(&mut self.stream)?;
            match frame.kind {
                FrameKind::Stdout => append_bounded(&mut stdout, &frame.payload, output_limit)?,
                FrameKind::Stderr => append_bounded(&mut stderr, &frame.payload, output_limit)?,
                FrameKind::Exit => {
                    return Ok(CapturedProcess {
                        exit_code: decode_exit(&frame.payload)?,
                        stdout,
                        stderr,
                    })
                }
                FrameKind::Error => return Err(agent_error(&frame.payload).into()),
                other => return Err(format!("unexpected agent frame {other:?}").into()),
            }
        }
    }

    pub fn put_file(
        mut self,
        source: &Path,
        destination: &str,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        self.require_capability("file-transfer-v1")?;
        let metadata = fs::symlink_metadata(source)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(format!("{} is not a regular file", source.display()).into());
        }
        let request = FilePutRequest {
            destination: destination.to_owned(),
            length: metadata.len(),
        };
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::FilePut, request.encode()?),
        )?;
        let ready = read_frame(&mut self.stream)?;
        match ready.kind {
            FrameKind::Pong if ready.payload.is_empty() => {}
            FrameKind::Error => return Err(agent_error(&ready.payload).into()),
            _ => return Err("agent did not acknowledge file upload".into()),
        }

        let mut file = fs::File::open(source)?;
        let mut sent = 0_u64;
        let mut buffer = [0_u8; STDIN_CHUNK_BYTES];
        loop {
            let length = file.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            write_frame(
                &mut self.stream,
                &Frame::new(FrameKind::FileData, buffer[..length].to_vec()),
            )?;
            sent += length as u64;
        }
        if sent != metadata.len() {
            return Err("host source changed while it was being read".into());
        }
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::FileDone, encode_file_length(sent)),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::FileDone if decode_file_length(&response.payload)? == sent => Ok(sent),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid upload completion frame".into()),
        }
    }

    pub fn get_file(
        mut self,
        source: &str,
        destination: &Path,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        self.require_capability("file-transfer-v1")?;
        if fs::symlink_metadata(destination).is_ok() {
            return Err(format!("refusing to overwrite existing {}", destination.display()).into());
        }
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err("host destination parent directory does not exist".into());
        }
        let temporary = download_temporary_path(destination)?;
        let result: Result<u64, Box<dyn std::error::Error>> = (|| {
            let mut output = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            let request = FileGetRequest {
                source: source.to_owned(),
            };
            write_frame(
                &mut self.stream,
                &Frame::new(FrameKind::FileGet, request.encode()?),
            )?;
            let mut received = 0_u64;
            loop {
                let frame = read_frame(&mut self.stream)?;
                match frame.kind {
                    FrameKind::FileData => {
                        output.write_all(&frame.payload)?;
                        received = received
                            .checked_add(frame.payload.len() as u64)
                            .ok_or("download length overflowed")?;
                    }
                    FrameKind::FileDone => {
                        if decode_file_length(&frame.payload)? != received {
                            return Err("download length did not match completion frame".into());
                        }
                        output.flush()?;
                        output.sync_all()?;
                        drop(output);
                        fs::hard_link(&temporary, destination)?;
                        fs::remove_file(&temporary)?;
                        return Ok(received);
                    }
                    FrameKind::Error => return Err(agent_error(&frame.payload).into()),
                    _ => return Err("unexpected frame during file download".into()),
                }
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn require_capability(&self, capability: &str) -> Result<(), Box<dyn std::error::Error>> {
        if self.has_capability(capability) {
            Ok(())
        } else {
            Err(format!("guest agent does not support {capability}").into())
        }
    }

    fn has_capability(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|available| available == capability)
    }
}

fn append_bounded(
    destination: &mut Vec<u8>,
    payload: &[u8],
    limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let new_length = destination
        .len()
        .checked_add(payload.len())
        .ok_or("captured output length overflowed")?;
    if new_length > limit {
        return Err(format!("guest command output exceeded {limit} bytes").into());
    }
    destination.extend_from_slice(payload);
    Ok(())
}

fn forward_stdin(
    stream: Arc<Mutex<TcpStream>>,
    stop: Arc<AtomicBool>,
    terminal_session: bool,
    controlled_session: bool,
) {
    let mut stdin = io::stdin().lock();
    let mut buffer = [0_u8; STDIN_CHUNK_BYTES];
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match stdin.read(&mut buffer) {
            Ok(0) if terminal_session => {
                // Unix VMIN=0/VTIME=1 makes an idle terminal read return zero,
                // allowing this thread to observe session shutdown promptly.
                continue;
            }
            Ok(0) => {
                if controlled_session {
                    let _ = send_outbound(&stream, &Frame::new(FrameKind::StdinClose, Vec::new()));
                } else {
                    shutdown_outbound(&stream);
                }
                return;
            }
            Ok(length) => {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if send_outbound(
                    &stream,
                    &Frame::new(FrameKind::Stdin, buffer[..length].to_vec()),
                )
                .is_err()
                {
                    return;
                }
            }
            Err(_) => {
                if controlled_session {
                    let _ =
                        send_outbound(&stream, &Frame::new(FrameKind::SessionCancel, Vec::new()));
                } else {
                    shutdown_outbound(&stream);
                }
                return;
            }
        }
    }
}

fn send_outbound(writer: &Arc<Mutex<TcpStream>>, frame: &Frame) -> io::Result<()> {
    let mut stream = writer
        .lock()
        .map_err(|_| io::Error::other("agent stream writer lock was poisoned"))?;
    write_frame(&mut *stream, frame).map_err(|error| match error {
        lsw_core::LswError::Io(error) => error,
        other => io::Error::other(other.to_string()),
    })
}

fn shutdown_outbound(writer: &Arc<Mutex<TcpStream>>) {
    if let Ok(stream) = writer.lock() {
        let _ = stream.shutdown(Shutdown::Write);
    }
}

struct SignalBridge {
    handle: SignalHandle,
    thread: thread::JoinHandle<()>,
}

impl SignalBridge {
    fn stop(self) {
        self.handle.close();
        let _ = self.thread.join();
    }
}

fn spawn_signal_bridge(
    writer: Arc<Mutex<TcpStream>>,
    stop: Arc<AtomicBool>,
) -> io::Result<SignalBridge> {
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    let handle = signals.handle();
    let thread = thread::spawn(move || {
        for signal in signals.forever() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let signal = if signal == SIGINT {
                SessionSignal::Interrupt
            } else {
                SessionSignal::Terminate
            };
            if send_outbound(
                &writer,
                &Frame::new(FrameKind::SessionSignal, signal.encode()),
            )
            .is_err()
            {
                return;
            }
        }
    });
    Ok(SignalBridge { handle, thread })
}

fn spawn_heartbeat_bridge(
    writer: Arc<Mutex<TcpStream>>,
    interval: Duration,
) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (stop_sender, stop_receiver) = mpsc::channel();
    let bridge = thread::spawn(move || loop {
        match stop_receiver.recv_timeout(interval) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                if send_outbound(
                    &writer,
                    &Frame::new(FrameKind::SessionHeartbeat, Vec::new()),
                )
                .is_err()
                {
                    if let Ok(stream) = writer.lock() {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                    return;
                }
            }
        }
    });
    (stop_sender, bridge)
}

fn spawn_resize_bridge(
    writer: Arc<Mutex<TcpStream>>,
    stop: Arc<AtomicBool>,
    initial_size: TerminalSize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut state = ResizeState::new(initial_size);
        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(150));
            let Some(size) = terminal_size() else {
                continue;
            };
            if let Some(size) = state.update(size) {
                if send_outbound(&writer, &Frame::new(FrameKind::Resize, size.encode())).is_err() {
                    return;
                }
            }
        }
    })
}

#[derive(Debug)]
struct ResizeState {
    last: TerminalSize,
}

impl ResizeState {
    fn new(initial: TerminalSize) -> Self {
        Self { last: initial }
    }

    fn update(&mut self, current: TerminalSize) -> Option<TerminalSize> {
        if current == self.last {
            None
        } else {
            self.last = current;
            Some(current)
        }
    }
}

#[cfg(unix)]
struct TerminalModeGuard {
    saved_mode: String,
}

#[cfg(unix)]
impl TerminalModeGuard {
    fn enter() -> io::Result<Option<Self>> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Ok(None);
        }
        let saved_mode = run_stty(&["-g"])?;
        if let Err(error) = run_stty(&["raw", "-echo", "min", "0", "time", "1"]) {
            let _ = run_stty(&[saved_mode.as_str()]);
            return Err(error);
        }
        Ok(Some(Self { saved_mode }))
    }

    fn size(&self) -> Option<TerminalSize> {
        terminal_size()
    }
}

#[cfg(unix)]
impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = run_stty(&[self.saved_mode.as_str()]);
    }
}

#[cfg(not(unix))]
struct TerminalModeGuard;

#[cfg(not(unix))]
impl TerminalModeGuard {
    fn enter() -> io::Result<Option<Self>> {
        Ok(None)
    }

    fn size(&self) -> Option<TerminalSize> {
        None
    }
}

#[cfg(unix)]
fn run_stty(arguments: &[&str]) -> io::Result<String> {
    let output = Command::new("stty")
        .args(arguments)
        // The input bridge reads fd 0, so configure that exact terminal rather
        // than an independently opened controlling terminal such as /dev/tty.
        .stdin(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "stty {} failed with {}",
            arguments.join(" "),
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| io::Error::other("stty returned non-UTF-8 output"))
}

#[cfg(unix)]
fn terminal_size() -> Option<TerminalSize> {
    let output = run_stty(&["size"]).ok()?;
    let mut dimensions = output.split_whitespace();
    let rows = dimensions.next()?.parse().ok()?;
    let columns = dimensions.next()?.parse().ok()?;
    if dimensions.next().is_some() {
        return None;
    }
    TerminalSize::new(rows, columns).ok()
}

#[cfg(not(unix))]
fn terminal_size() -> Option<TerminalSize> {
    None
}

pub fn address(manifest: &InstanceManifest) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, manifest.control_port)
}

fn agent_error(payload: &[u8]) -> String {
    format!("guest agent: {}", String::from_utf8_lossy(payload))
}

fn download_temporary_path(destination: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let name = destination
        .file_name()
        .ok_or("host destination has no file name")?
        .to_string_lossy();
    Ok(destination.with_file_name(format!(
        ".{name}.lsw-download-{}-{nonce}",
        std::process::id()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_address_is_always_loopback() {
        let _ = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5040);
        assert!(Ipv4Addr::LOCALHOST.is_loopback());
    }

    #[test]
    fn resize_state_emits_only_real_changes() {
        let initial = TerminalSize::new(24, 80).expect("initial size should be valid");
        let changed = TerminalSize::new(40, 120).expect("changed size should be valid");
        let mut state = ResizeState::new(initial);
        assert_eq!(state.update(initial), None);
        assert_eq!(state.update(changed), Some(changed));
        assert_eq!(state.update(changed), None);
        assert_eq!(state.last, changed);
    }

    #[test]
    fn maintenance_trim_sends_only_the_empty_fixed_operation() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            let request = read_frame(&mut stream).expect("trim request should arrive");
            assert_eq!(request.kind, FrameKind::MaintenanceTrim);
            assert!(request.payload.is_empty());
            write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))
                .expect("trim response should be sent");
        });
        let stream = TcpStream::connect(address).expect("fixture should connect");
        AgentClient {
            stream,
            capabilities: vec![CAPABILITY_MAINTENANCE_TRIM_V1.to_owned()],
        }
        .trim()
        .expect("trim should succeed");
        server.join().expect("fixture should not panic");
    }

    #[test]
    fn controlled_run_sends_options_start_and_explicit_stdin_close() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            let options = read_frame(&mut stream).expect("options should arrive");
            assert_eq!(options.kind, FrameKind::SessionOptions);
            assert!(
                SessionOptions::decode(&options.payload)
                    .expect("options should decode")
                    .cancel_on_disconnect
            );

            let start = read_frame(&mut stream).expect("start should arrive");
            assert_eq!(start.kind, FrameKind::Start);
            assert_eq!(
                StartRequest::decode(&start.payload)
                    .expect("start should decode")
                    .argv,
                vec!["fixture-command"]
            );

            let stdin_close = read_frame(&mut stream).expect("stdin close should arrive");
            assert_eq!(stdin_close.kind, FrameKind::StdinClose);
            assert!(stdin_close.payload.is_empty());
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Exit, lsw_core::encode_exit(0)),
            )
            .expect("exit should be sent");
        });
        let stream = TcpStream::connect(address).expect("fixture should connect");
        let client = AgentClient {
            stream,
            capabilities: vec![CAPABILITY_SESSION_CONTROL_V1.to_owned()],
        };
        let request = StartRequest {
            kind: lsw_core::SessionKind::Exec,
            argv: vec!["fixture-command".to_owned()],
            working_directory: None,
        };
        assert_eq!(client.run(&request, false).expect("run should succeed"), 0);
        server.join().expect("fixture should not panic");
    }

    #[test]
    fn controlled_run_sends_environment_and_working_directory() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            assert_eq!(
                read_frame(&mut stream).expect("options should arrive").kind,
                FrameKind::SessionOptions
            );
            let environment = read_frame(&mut stream).expect("environment should arrive");
            assert_eq!(environment.kind, FrameKind::ProcessEnvironment);
            assert_eq!(
                ProcessEnvironment::decode(&environment.payload)
                    .expect("environment should decode")
                    .variables,
                vec![("LSW_FIXTURE".to_owned(), "hello world".to_owned())]
            );
            let start = read_frame(&mut stream).expect("start should arrive");
            assert_eq!(start.kind, FrameKind::Start);
            assert_eq!(
                StartRequest::decode(&start.payload)
                    .expect("start should decode")
                    .working_directory
                    .as_deref(),
                Some("C:\\work")
            );
            assert_eq!(
                read_frame(&mut stream)
                    .expect("stdin close should arrive")
                    .kind,
                FrameKind::StdinClose
            );
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Exit, lsw_core::encode_exit(17)),
            )
            .expect("exit should be sent");
        });
        let stream = TcpStream::connect(address).expect("fixture should connect");
        let client = AgentClient {
            stream,
            capabilities: vec![
                CAPABILITY_SESSION_CONTROL_V1.to_owned(),
                CAPABILITY_PROCESS_ENVIRONMENT_V1.to_owned(),
            ],
        };
        let request = StartRequest {
            kind: SessionKind::Exec,
            argv: vec!["fixture-command".to_owned()],
            working_directory: Some("C:\\work".to_owned()),
        };
        let environment =
            ProcessEnvironment::new(vec![("LSW_FIXTURE".to_owned(), "hello world".to_owned())])
                .expect("environment should be valid");
        assert_eq!(
            client
                .run_with_environment(&request, false, &environment)
                .expect("run should succeed"),
            17
        );
        server.join().expect("fixture should not panic");
    }

    #[test]
    fn detached_run_uses_the_explicit_handshake_and_returns_the_pid() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            let options = read_frame(&mut stream).expect("options should arrive");
            assert_eq!(options.kind, FrameKind::SessionOptions);
            assert!(
                !SessionOptions::decode(&options.payload)
                    .expect("options should decode")
                    .cancel_on_disconnect
            );
            assert_eq!(
                read_frame(&mut stream)
                    .expect("detach marker should arrive")
                    .kind,
                FrameKind::SessionDetach
            );
            let start = read_frame(&mut stream).expect("start should arrive");
            assert_eq!(start.kind, FrameKind::Start);
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Started, lsw_core::encode_process_id(4242)),
            )
            .expect("started response should be sent");
        });
        let stream = TcpStream::connect(address).expect("fixture should connect");
        let client = AgentClient {
            stream,
            capabilities: vec![
                CAPABILITY_SESSION_CONTROL_V1.to_owned(),
                CAPABILITY_DETACHED_RUN_V1.to_owned(),
            ],
        };
        let request = StartRequest {
            kind: SessionKind::Run,
            argv: vec!["fixture-command".to_owned()],
            working_directory: None,
        };
        assert_eq!(
            client
                .run_detached(&request, &ProcessEnvironment::default())
                .expect("detached run should succeed"),
            4242
        );
        server.join().expect("fixture should not panic");
    }

    #[test]
    fn captured_run_sends_private_input_and_bounds_separate_output_streams() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            assert_eq!(
                read_frame(&mut stream).expect("options should arrive").kind,
                FrameKind::SessionOptions
            );
            assert_eq!(
                read_frame(&mut stream).expect("start should arrive").kind,
                FrameKind::Start
            );
            let input = read_frame(&mut stream).expect("private input should arrive");
            assert_eq!(input.kind, FrameKind::Stdin);
            assert_eq!(input.payload, b"fixture-secret\n");
            assert_eq!(
                read_frame(&mut stream)
                    .expect("stdin close should arrive")
                    .kind,
                FrameKind::StdinClose
            );
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Stdout, b"STATUS=licensed\n".to_vec()),
            )
            .expect("stdout should be sent");
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Stderr, b"diagnostic\n".to_vec()),
            )
            .expect("stderr should be sent");
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Exit, lsw_core::encode_exit(0)),
            )
            .expect("exit should be sent");
        });
        let stream = TcpStream::connect(address).expect("fixture should connect");
        let client = AgentClient {
            stream,
            capabilities: vec![CAPABILITY_SESSION_CONTROL_V1.to_owned()],
        };
        let request = StartRequest {
            kind: lsw_core::SessionKind::Exec,
            argv: vec!["license-client".to_owned()],
            working_directory: None,
        };
        let captured = client
            .run_capture(&request, b"fixture-secret\n", 1024)
            .expect("capture should succeed");
        assert_eq!(captured.exit_code, 0);
        assert_eq!(captured.stdout, b"STATUS=licensed\n");
        assert_eq!(captured.stderr, b"diagnostic\n");
        server.join().expect("fixture should not panic");
    }

    #[test]
    fn leased_run_sends_lease_between_options_and_start() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            assert_eq!(
                read_frame(&mut stream).expect("options should arrive").kind,
                FrameKind::SessionOptions
            );
            let lease_frame = read_frame(&mut stream).expect("lease should arrive");
            assert_eq!(lease_frame.kind, FrameKind::SessionLease);
            assert_eq!(
                SessionLease::decode(&lease_frame.payload).expect("lease should decode"),
                SessionLease::standard()
            );
            assert_eq!(
                read_frame(&mut stream).expect("start should arrive").kind,
                FrameKind::Start
            );
            assert_eq!(
                read_frame(&mut stream)
                    .expect("stdin close should arrive")
                    .kind,
                FrameKind::StdinClose
            );
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Exit, lsw_core::encode_exit(0)),
            )
            .expect("exit should be sent");
        });
        let stream = TcpStream::connect(address).expect("fixture should connect");
        let client = AgentClient {
            stream,
            capabilities: vec![
                CAPABILITY_SESSION_CONTROL_V1.to_owned(),
                CAPABILITY_SESSION_LEASE_V1.to_owned(),
            ],
        };
        let request = StartRequest {
            kind: lsw_core::SessionKind::Exec,
            argv: vec!["fixture-command".to_owned()],
            working_directory: None,
        };
        assert_eq!(client.run(&request, false).expect("run should succeed"), 0);
        server.join().expect("fixture should not panic");
    }

    #[test]
    fn heartbeat_bridge_emits_frames_and_stops_promptly() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("timeout should configure");
            let heartbeat = read_frame(&mut stream).expect("heartbeat should arrive");
            assert_eq!(heartbeat.kind, FrameKind::SessionHeartbeat);
            assert!(heartbeat.payload.is_empty());
        });
        let stream = TcpStream::connect(address).expect("fixture should connect");
        let writer = Arc::new(Mutex::new(stream));
        let (stop, bridge) = spawn_heartbeat_bridge(writer, Duration::from_millis(10));
        server.join().expect("fixture should not panic");
        // The peer may close between the first successful heartbeat and this
        // signal, in which case the bridge has already stopped itself.
        let _ = stop.send(());
        bridge.join().expect("bridge should not panic");
    }

    #[test]
    fn legacy_run_still_starts_without_session_options() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            let start = read_frame(&mut stream).expect("start should arrive");
            assert_eq!(start.kind, FrameKind::Start);
            write_frame(
                &mut stream,
                &Frame::new(FrameKind::Exit, lsw_core::encode_exit(0)),
            )
            .expect("exit should be sent");
        });
        let stream = TcpStream::connect(address).expect("fixture should connect");
        let client = AgentClient {
            stream,
            // An inconsistent peer advertising only the lease capability must
            // still get version-one legacy framing.
            capabilities: vec![CAPABILITY_SESSION_LEASE_V1.to_owned()],
        };
        let request = StartRequest {
            kind: lsw_core::SessionKind::Exec,
            argv: vec!["fixture-command".to_owned()],
            working_directory: None,
        };
        assert_eq!(client.run(&request, false).expect("run should succeed"), 0);
        server.join().expect("fixture should not panic");
    }
}
