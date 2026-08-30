// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{self, IsTerminal, Read, Write};
use std::net::{Shutdown, TcpStream};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use lsw_core::{
    decode_exit, decode_process_id, read_frame, write_frame, Frame, FrameKind, ProcessEnvironment,
    SessionKind, SessionLease, SessionOptions, SessionSignal, StartRequest, TerminalSize,
    TerminalStartRequest, CAPABILITY_CONPTY_V1, CAPABILITY_DETACHED_RUN_V1,
    CAPABILITY_PROCESS_ENVIRONMENT_V1, CAPABILITY_SESSION_CONTROL_V1, CAPABILITY_SESSION_LEASE_V1,
    CAPABILITY_SESSION_SIGNAL_V1, CAPABILITY_TERMINAL_RESIZE_V1,
};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};

use super::{agent_error, AgentClient};

const STDIN_CHUNK_BYTES: usize = 32 * 1024;

pub struct CapturedProcess {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl AgentClient {
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

pub(super) fn spawn_heartbeat_bridge(
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
pub(super) struct ResizeState {
    pub(super) last: TerminalSize,
}

impl ResizeState {
    pub(super) fn new(initial: TerminalSize) -> Self {
        Self { last: initial }
    }

    pub(super) fn update(&mut self, current: TerminalSize) -> Option<TerminalSize> {
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
