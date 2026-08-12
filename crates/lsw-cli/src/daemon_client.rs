// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::StateStore;

const RESPONSE_LIMIT: u64 = 2 * 1024 * 1024;
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

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

    pub fn ensure_running(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self
            .request("PING")
            .map(|lines| {
                lines.iter().any(|line| line == "PONG")
                    && lines.iter().any(|line| line == "PROTOCOL=1")
            })
            .unwrap_or(false)
        {
            return Ok(());
        }

        self.store.initialize()?;
        let default_socket = env::var_os("LSW_DAEMON_SOCKET").is_none();
        let run_directory = self
            .socket
            .parent()
            .ok_or("daemon socket path has no parent directory")?;
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
        let daemon = daemon_program()?;
        let mut child = Command::new(&daemon)
            .env("LSW_STATE_DIR", self.store.root())
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()?;

        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        loop {
            if self
                .request("PING")
                .map(|lines| lines.iter().any(|line| line == "PROTOCOL=1"))
                .unwrap_or(false)
            {
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                return Err(format!(
                    "lswd exited during startup with {status}; inspect {}",
                    log_path.display()
                )
                .into());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for lswd at {}; inspect {}",
                    self.socket.display(),
                    log_path.display()
                )
                .into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn request_checked(
        &self,
        request: &str,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.ensure_running()?;
        self.request(request)
    }

    pub fn request(&self, request: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        if request.is_empty() || request.len() > 4095 || request.contains(['\r', '\n']) {
            return Err("invalid daemon request".into());
        }
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        writeln!(stream, "{request}")?;
        stream.flush()?;

        let mut reader = BufReader::new(stream).take(RESPONSE_LIMIT + 1);
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Err("lswd closed the connection without a response".into());
        }
        let header = header.trim_end_matches(&['\r', '\n'][..]);
        let success = header == "OK";
        let daemon_error = header.strip_prefix("ERR ").map(percent_decode);
        if !success && daemon_error.is_none() {
            return Err("lswd returned an invalid response header".into());
        }

        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let length = reader.read_line(&mut line)?;
            if length == 0 {
                return Err("lswd response was not terminated".into());
            }
            if reader.limit() == 0 && !line.ends_with('\n') {
                return Err("lswd response exceeded the size limit".into());
            }
            let line = line.trim_end_matches(&['\r', '\n'][..]);
            if line == "." {
                break;
            }
            lines.push(percent_decode(line));
        }
        if let Some(error) = daemon_error {
            return Err(error.into());
        }
        Ok(lines)
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

fn daemon_program() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(configured) = env::var_os("LSW_DAEMON") {
        return Ok(PathBuf::from(configured));
    }
    if let Ok(current) = env::current_exe() {
        if let Some(parent) = current.parent() {
            let sibling = parent.join("lswd");
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    Ok(PathBuf::from("lswd"))
}

fn percent_decode(value: &str) -> String {
    value
        .replace("%09", "\t")
        .replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%25", "%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_unescaping_reverses_daemon_encoding_order() {
        assert_eq!(percent_decode("a%09b%0Ac%25"), "a\tb\nc%");
        assert_eq!(percent_decode("literal%2Fvalue"), "literal%2Fvalue");
    }
}
