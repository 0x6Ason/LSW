// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufReader};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    read_daemon_reply, write_daemon_request, DaemonInfo, DaemonReply, DaemonRequest,
    DaemonResponse, InstanceStatus, LaunchPhase, LswError, StateStore, DAEMON_PROTOCOL_VERSION,
};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a daemon request did not produce a response.
#[derive(Debug)]
pub enum DaemonError {
    /// The socket could not be reached or the connection failed mid-request.
    Io(io::Error),
    /// The daemon answered with something that is not a well-formed reply.
    Protocol(String),
    /// The daemon understood the request and refused it.
    Rejected(String),
    /// A daemon is listening but speaks another protocol version.
    Incompatible { socket: PathBuf, daemon: u16 },
    /// `lswd` could not be started on demand.
    Startup(String),
    /// The daemon answered a different request kind than the one sent.
    UnexpectedResponse(DaemonResponse),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "lswd connection failed: {error}"),
            Self::Protocol(reason) => write!(formatter, "lswd protocol error: {reason}"),
            Self::Rejected(reason) => formatter.write_str(reason),
            Self::Incompatible { socket, daemon } => write!(
                formatter,
                "lswd at {} speaks daemon protocol {daemon} but this lsw needs {DAEMON_PROTOCOL_VERSION}; \
                 stop it with `lsw shutdown` and `lsw daemon disable`, or wait for it to exit idle",
                socket.display()
            ),
            Self::Startup(reason) => formatter.write_str(reason),
            Self::UnexpectedResponse(response) => {
                write!(formatter, "lswd answered with an unexpected response: {response:?}")
            }
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for DaemonError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<LswError> for DaemonError {
    fn from(error: LswError) -> Self {
        match error {
            LswError::Io(error) => Self::Io(error),
            other => Self::Protocol(other.to_string()),
        }
    }
}

pub struct DaemonClient<'a> {
    store: &'a StateStore,
    socket: PathBuf,
}

impl<'a> DaemonClient<'a> {
    pub fn new(store: &'a StateStore) -> Self {
        let socket = env::var_os("LSW_DAEMON_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| store.root().join("run/lswd.sock"));
        Self { store, socket }
    }

    /// Starts `lswd` on demand unless a compatible daemon already answers.
    pub fn ensure_running(&self) -> Result<(), DaemonError> {
        match self.ping() {
            Ok(_) => return Ok(()),
            Err(error @ DaemonError::Incompatible { .. }) => return Err(error),
            Err(_) => {}
        }

        self.store.initialize()?;
        let default_socket = env::var_os("LSW_DAEMON_SOCKET").is_none();
        let run_directory = self.socket.parent().ok_or_else(|| {
            DaemonError::Startup("daemon socket path has no parent directory".to_owned())
        })?;
        if !run_directory.exists() {
            fs::create_dir_all(run_directory)?;
            fs::set_permissions(run_directory, fs::Permissions::from_mode(0o700))?;
        } else if default_socket {
            fs::set_permissions(run_directory, fs::Permissions::from_mode(0o700))?;
        }

        let log_path = self.store.root().join("lswd.log");
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log_path)?;
        let stderr = stdout.try_clone()?;
        let daemon = daemon_program();
        let mut command = Command::new("setsid");
        command
            .arg(daemon)
            .env("LSW_STATE_DIR", self.store.root())
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        // util-linux setsid execs lswd in a new session without a shell or an
        // untracked fork. A one-shot WSL launcher can then exit without sending
        // SIGHUP to the daemon and its QEMU children.
        let mut child = command.spawn()?;

        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        loop {
            match self.ping() {
                Ok(_) => return Ok(()),
                Err(error @ DaemonError::Incompatible { .. }) => return Err(error),
                Err(_) => {}
            }
            if let Some(status) = child.try_wait()? {
                return Err(DaemonError::Startup(format!(
                    "lswd exited during startup with {status}; inspect {}",
                    log_path.display()
                )));
            }
            if Instant::now() >= deadline {
                return Err(DaemonError::Startup(format!(
                    "timed out waiting for lswd at {}; inspect {}",
                    self.socket.display(),
                    log_path.display()
                )));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Sends one request, starting the daemon first when necessary.
    pub fn call_checked(&self, request: &DaemonRequest) -> Result<DaemonResponse, DaemonError> {
        self.ensure_running()?;
        self.call(request)
    }

    /// Sends one request to a daemon that is expected to be listening.
    pub fn call(&self, request: &DaemonRequest) -> Result<DaemonResponse, DaemonError> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        write_daemon_request(&mut stream, request)?;
        let mut reader = BufReader::new(stream);
        match read_daemon_reply(&mut reader)? {
            Some(DaemonReply::Ok(response)) => Ok(response),
            Some(DaemonReply::Error(reason)) => Err(DaemonError::Rejected(reason)),
            None => Err(DaemonError::Protocol(
                "lswd closed the connection without a response".to_owned(),
            )),
        }
    }

    /// Probes the daemon without starting it and requires a matching protocol.
    pub fn ping(&self) -> Result<DaemonInfo, DaemonError> {
        match self.call(&DaemonRequest::Ping)? {
            DaemonResponse::Pong(info) if info.is_compatible() => Ok(info),
            DaemonResponse::Pong(info) => Err(DaemonError::Incompatible {
                socket: self.socket.clone(),
                daemon: info.protocol,
            }),
            other => Err(DaemonError::UnexpectedResponse(other)),
        }
    }

    /// The reconciled state of an instance.
    pub fn status(&self, name: &str) -> Result<InstanceStatus, DaemonError> {
        match self.call_checked(&DaemonRequest::Status {
            name: name.to_owned(),
        })? {
            DaemonResponse::Status(status) => Ok(status),
            other => Err(DaemonError::UnexpectedResponse(other)),
        }
    }

    pub fn start(&self, name: &str, phase: LaunchPhase) -> Result<DaemonResponse, DaemonError> {
        self.call_checked(&DaemonRequest::Start {
            name: name.to_owned(),
            phase,
        })
    }

    pub fn stop(&self, name: &str, force: bool) -> Result<DaemonResponse, DaemonError> {
        self.call_checked(&DaemonRequest::Stop {
            name: name.to_owned(),
            force,
        })
    }

    pub fn suspend(&self, name: &str) -> Result<DaemonResponse, DaemonError> {
        self.call_checked(&DaemonRequest::Suspend {
            name: name.to_owned(),
        })
    }

    pub fn resume(&self, name: &str) -> Result<DaemonResponse, DaemonError> {
        self.call_checked(&DaemonRequest::Resume {
            name: name.to_owned(),
        })
    }

    pub fn hibernate(&self, name: &str) -> Result<DaemonResponse, DaemonError> {
        self.call_checked(&DaemonRequest::Hibernate {
            name: name.to_owned(),
        })
    }

    pub fn record_activity(&self, name: &str) -> Result<DaemonResponse, DaemonError> {
        self.call_checked(&DaemonRequest::Activity {
            name: name.to_owned(),
        })
    }

    pub fn balloon(&self, name: &str, memory_mib: u32) -> Result<DaemonResponse, DaemonError> {
        self.call_checked(&DaemonRequest::Balloon {
            name: name.to_owned(),
            memory_mib,
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

fn daemon_program() -> PathBuf {
    if let Some(configured) = env::var_os("LSW_DAEMON") {
        return PathBuf::from(configured);
    }
    if let Ok(current) = env::current_exe() {
        if let Some(parent) = current.parent() {
            let sibling = parent.join("lswd");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("lswd")
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixListener;

    use lsw_core::{read_daemon_request, write_daemon_reply, Liveness};

    use super::*;

    fn client_with_socket(socket: PathBuf) -> (StateStore, PathBuf) {
        (
            StateStore::new(std::env::temp_dir().join("lsw-host-daemon-test")),
            socket,
        )
    }

    fn serve_one(listener: UnixListener, reply: DaemonReply) -> thread::JoinHandle<DaemonRequest> {
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("client connects");
            let mut reader = BufReader::new(stream.try_clone().expect("stream clones"));
            let request = read_daemon_request(&mut reader)
                .expect("request decodes")
                .expect("request is present");
            let mut writer = stream;
            write_daemon_reply(&mut writer, &reply).expect("reply is written");
            writer.flush().expect("reply is flushed");
            request
        })
    }

    fn temporary_socket(label: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("lsw-host-daemon-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("socket directory");
        directory.join("lswd.sock")
    }

    #[test]
    fn typed_status_requests_decode_the_typed_reply() {
        let socket = temporary_socket("status");
        let listener = UnixListener::bind(&socket).expect("listener binds");
        let (store, socket) = client_with_socket(socket);
        let server = serve_one(
            listener,
            DaemonReply::Ok(DaemonResponse::Status(InstanceStatus {
                state: lsw_core::InstanceState::Suspended,
                qmp: Some("paused".to_owned()),
                active: Liveness::Active,
            })),
        );
        let client = DaemonClient {
            store: &store,
            socket,
        };
        let status = match client.call(&DaemonRequest::Status {
            name: "win-dev".to_owned(),
        }) {
            Ok(DaemonResponse::Status(status)) => status,
            other => panic!("unexpected result {other:?}"),
        };
        assert_eq!(status.state, lsw_core::InstanceState::Suspended);
        assert_eq!(status.qmp.as_deref(), Some("paused"));
        assert_eq!(
            server.join().expect("server finishes"),
            DaemonRequest::Status {
                name: "win-dev".to_owned()
            }
        );
    }

    #[test]
    fn rejections_and_incompatible_daemons_are_distinct_errors() {
        let socket = temporary_socket("reject");
        let listener = UnixListener::bind(&socket).expect("listener binds");
        let (store, socket) = client_with_socket(socket);
        let server = serve_one(listener, DaemonReply::Error("no such instance".to_owned()));
        let client = DaemonClient {
            store: &store,
            socket: socket.clone(),
        };
        assert!(matches!(
            client.call(&DaemonRequest::Suspend {
                name: "x".to_owned()
            }),
            Err(DaemonError::Rejected(reason)) if reason == "no such instance"
        ));
        server.join().expect("server finishes");

        let listener = UnixListener::bind(&socket).unwrap_or_else(|_| {
            fs::remove_file(&socket).expect("stale socket removed");
            UnixListener::bind(&socket).expect("listener rebinds")
        });
        let server = serve_one(
            listener,
            DaemonReply::Ok(DaemonResponse::Pong(DaemonInfo {
                protocol: DAEMON_PROTOCOL_VERSION - 1,
                features: Vec::new(),
                pid: 1,
                rss_kib: None,
            })),
        );
        assert!(matches!(
            client.ping(),
            Err(DaemonError::Incompatible { daemon, .. }) if daemon == DAEMON_PROTOCOL_VERSION - 1
        ));
        server.join().expect("server finishes");
        assert!(matches!(client.ping(), Err(DaemonError::Io(_))));
    }
}
