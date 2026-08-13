// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const MAX_QMP_MESSAGE_BYTES: u64 = 256 * 1024;
const QMP_TIMEOUT: Duration = Duration::from_secs(3);

pub struct QmpClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl QmpClient {
    pub fn connect(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let stream = UnixStream::connect(path)?;
        Self::from_stream(stream)
    }

    fn from_stream(stream: UnixStream) -> Result<Self, Box<dyn std::error::Error>> {
        stream.set_read_timeout(Some(QMP_TIMEOUT))?;
        stream.set_write_timeout(Some(QMP_TIMEOUT))?;
        let writer = stream.try_clone()?;
        let mut client = Self {
            reader: BufReader::new(stream),
            writer,
        };
        let greeting = client.read_message()?;
        if !greeting.contains("\"QMP\"") {
            return Err("QMP socket returned an invalid greeting".into());
        }
        client.execute("qmp_capabilities")?;
        Ok(client)
    }

    pub fn status(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let response = self.execute("query-status")?;
        json_string_field(&response, "status")
            .ok_or_else(|| "QMP query-status response did not contain a status".into())
    }

    pub fn system_powerdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.execute("system_powerdown")?;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.execute("stop")?;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.execute("cont")?;
        Ok(())
    }

    pub fn quit(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        writeln!(self.writer, "{{\"execute\":\"quit\"}}")?;
        self.writer.flush()?;
        // QEMU is allowed to close the socket before sending the final response.
        match self.read_response() {
            Ok(_) => Ok(()),
            Err(error) if is_disconnect(error.as_ref()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn execute(&mut self, command: &str) -> Result<String, Box<dyn std::error::Error>> {
        if !command
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'-' | b'_'))
        {
            return Err("invalid QMP command name".into());
        }
        writeln!(self.writer, "{{\"execute\":\"{command}\"}}")?;
        self.writer.flush()?;
        self.read_response()
    }

    fn read_response(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        loop {
            let message = self.read_message()?;
            if message.contains("\"error\"") {
                return Err(format!("QMP command failed: {message}").into());
            }
            if message.contains("\"return\"") {
                return Ok(message);
            }
            // Asynchronous QMP events can arrive between a command and response.
        }
    }

    fn read_message(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let mut message = String::new();
        let bytes = (&mut self.reader)
            .take(MAX_QMP_MESSAGE_BYTES + 1)
            .read_line(&mut message)?;
        if bytes == 0 {
            return Err("QMP socket closed unexpectedly".into());
        }
        if bytes as u64 > MAX_QMP_MESSAGE_BYTES || !message.ends_with('\n') {
            return Err("QMP message was too large or unterminated".into());
        }
        Ok(message)
    }
}

fn is_disconnect(error: &dyn std::error::Error) -> bool {
    let message = error.to_string();
    message.contains("closed unexpectedly")
        || message.contains("Broken pipe")
        || message.contains("Connection reset")
}

fn json_string_field(document: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let mut remaining = document.get(document.find(&needle)? + needle.len()..)?;
    remaining = remaining.trim_start();
    remaining = remaining.strip_prefix(':')?.trim_start();
    remaining = remaining.strip_prefix('"')?;

    let mut result = String::new();
    let mut escaped = false;
    for character in remaining.chars() {
        if escaped {
            match character {
                '"' | '\\' | '/' => result.push(character),
                'b' => result.push('\u{0008}'),
                'f' => result.push('\u{000c}'),
                'n' => result.push('\n'),
                'r' => result.push('\r'),
                't' => result.push('\t'),
                _ => return None,
            }
            escaped = false;
        } else {
            match character {
                '\\' => escaped = true,
                '"' => return Some(result),
                control if control.is_control() => return None,
                other => result.push(other),
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::thread;

    #[test]
    fn extracts_qmp_status_without_accepting_invalid_json_strings() {
        assert_eq!(
            json_string_field(
                "{\"return\":{\"running\":true,\"status\":\"running\"}}",
                "status"
            ),
            Some("running".to_owned())
        );
        assert_eq!(
            json_string_field("{\"status\":\"guest\\\"paused\"}", "status"),
            Some("guest\"paused".to_owned())
        );
        assert_eq!(
            json_string_field("{\"status\":\"unterminated}", "status"),
            None
        );
    }

    #[test]
    fn qmp_client_negotiates_and_skips_async_events() {
        let (client, mut server) = UnixStream::pair().expect("socket pair should be created");
        let server_thread = thread::spawn(move || {
            writeln!(server, "{{\"QMP\":{{\"version\":{{}}}}}}").expect("greeting should be sent");
            let mut reader = BufReader::new(server.try_clone().expect("server should clone"));
            let mut command = String::new();
            reader
                .read_line(&mut command)
                .expect("capabilities should be read");
            assert!(command.contains("qmp_capabilities"));
            writeln!(server, "{{\"return\":{{}}}}").expect("capabilities response should be sent");
            command.clear();
            reader
                .read_line(&mut command)
                .expect("status query should be read");
            assert!(command.contains("query-status"));
            writeln!(server, "{{\"event\":\"RESET\"}}").expect("event should be sent");
            writeln!(server, "{{\"return\":{{\"status\":\"running\"}}}}")
                .expect("status should be sent");
        });
        let mut qmp = QmpClient::from_stream(client).expect("QMP should negotiate");
        assert_eq!(qmp.status().expect("status should parse"), "running");
        server_thread.join().expect("server fixture should finish");
    }

    #[test]
    fn qmp_client_sends_pause_and_resume_commands() {
        let (client, mut server) = UnixStream::pair().expect("socket pair should be created");
        let server_thread = thread::spawn(move || {
            writeln!(server, "{{\"QMP\":{{\"version\":{{}}}}}}").expect("greeting should be sent");
            let mut reader = BufReader::new(server.try_clone().expect("server should clone"));
            let mut command = String::new();
            reader
                .read_line(&mut command)
                .expect("capabilities should be read");
            assert!(command.contains("qmp_capabilities"));
            writeln!(server, "{{\"return\":{{}}}}").expect("capabilities response should be sent");

            for expected in ["stop", "cont"] {
                command.clear();
                reader
                    .read_line(&mut command)
                    .expect("lifecycle command should be read");
                assert!(command.contains(&format!("\"execute\":\"{expected}\"")));
                writeln!(server, "{{\"return\":{{}}}}").expect("lifecycle response should be sent");
            }
        });
        let mut qmp = QmpClient::from_stream(client).expect("QMP should negotiate");
        qmp.pause().expect("pause should succeed");
        qmp.resume().expect("resume should succeed");
        server_thread.join().expect("server fixture should finish");
    }
}
