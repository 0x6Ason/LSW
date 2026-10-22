// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

const MAX_QMP_MESSAGE_BYTES: u64 = 256 * 1024;
const MAX_QMP_REQUEST_BYTES: usize = 4096;
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
        if !greeting.get("QMP").is_some_and(Value::is_object) {
            return Err("QMP socket returned an invalid greeting".into());
        }
        client.execute("qmp_capabilities")?;
        Ok(client)
    }

    pub fn status(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let response = self.execute("query-status")?;
        response
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned)
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

    pub fn balloon(&mut self, target_bytes: u64) -> Result<(), Box<dyn std::error::Error>> {
        self.execute_document(&serde_json::json!({
            "execute": "balloon",
            "arguments": { "value": target_bytes },
        }))?;
        Ok(())
    }

    pub fn quit(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.send(&serde_json::json!({ "execute": "quit" }))?;
        // QEMU is allowed to close the socket before sending the final response.
        match self.read_response() {
            Ok(_) => Ok(()),
            Err(error) if is_disconnect(error.as_ref()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Executes an argument-free command and returns its `return` value.
    fn execute(&mut self, command: &str) -> Result<Value, Box<dyn std::error::Error>> {
        if !command
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'-' | b'_'))
        {
            return Err("invalid QMP command name".into());
        }
        self.execute_document(&serde_json::json!({ "execute": command }))
    }

    fn execute_document(&mut self, document: &Value) -> Result<Value, Box<dyn std::error::Error>> {
        self.send(document)?;
        self.read_response()
    }

    fn send(&mut self, document: &Value) -> Result<(), Box<dyn std::error::Error>> {
        let encoded = serde_json::to_string(document)?;
        if encoded.len() > MAX_QMP_REQUEST_BYTES {
            return Err("QMP request document is too large".into());
        }
        writeln!(self.writer, "{encoded}")?;
        self.writer.flush()?;
        Ok(())
    }

    /// Reads messages until the command response arrives, skipping the
    /// asynchronous events QEMU may interleave.
    fn read_response(&mut self) -> Result<Value, Box<dyn std::error::Error>> {
        loop {
            let mut message = self.read_message()?;
            if let Some(error) = message.get("error") {
                return Err(format!("QMP command failed: {error}").into());
            }
            if let Some(value) = message.get_mut("return") {
                return Ok(value.take());
            }
            if message.get("event").is_none() {
                return Err(format!("unexpected QMP message: {message}").into());
            }
        }
    }

    fn read_message(&mut self) -> Result<Value, Box<dyn std::error::Error>> {
        let mut line = String::new();
        let bytes = (&mut self.reader)
            .take(MAX_QMP_MESSAGE_BYTES + 1)
            .read_line(&mut line)?;
        if bytes == 0 {
            return Err("QMP socket closed unexpectedly".into());
        }
        if bytes as u64 > MAX_QMP_MESSAGE_BYTES || !line.ends_with('\n') {
            return Err("QMP message was too large or unterminated".into());
        }
        let message: Value = serde_json::from_str(&line)
            .map_err(|error| format!("QMP message was not valid JSON: {error}"))?;
        if !message.is_object() {
            return Err("QMP message was not a JSON object".into());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::thread;

    fn negotiate(server: &mut UnixStream) -> BufReader<UnixStream> {
        writeln!(server, "{{\"QMP\":{{\"version\":{{}}}}}}").expect("greeting should be sent");
        let mut reader = BufReader::new(server.try_clone().expect("server should clone"));
        let mut command = String::new();
        reader
            .read_line(&mut command)
            .expect("capabilities should be read");
        assert_eq!(command, "{\"execute\":\"qmp_capabilities\"}\n");
        writeln!(server, "{{\"return\":{{}}}}").expect("capabilities response should be sent");
        reader
    }

    #[test]
    fn qmp_client_rejects_an_invalid_greeting() {
        let (client, mut server) = UnixStream::pair().expect("socket pair should be created");
        let server_thread = thread::spawn(move || {
            writeln!(server, "{{\"return\":{{}}}}").expect("message should be sent");
        });
        let Err(error) = QmpClient::from_stream(client) else {
            panic!("greeting must be a QMP object");
        };
        assert!(error.to_string().contains("invalid greeting"));
        server_thread.join().expect("server fixture should finish");
    }

    #[test]
    fn qmp_client_reports_command_errors_and_ignores_error_text_inside_events() {
        let (client, mut server) = UnixStream::pair().expect("socket pair should be created");
        let server_thread = thread::spawn(move || {
            let mut reader = negotiate(&mut server);
            let mut command = String::new();
            reader
                .read_line(&mut command)
                .expect("status query should be read");
            // An event whose payload mentions "error" and "return" must not be
            // mistaken for the command response.
            writeln!(
                server,
                "{{\"event\":\"BLOCK_JOB_COMPLETED\",\"data\":{{\"error\":\"x\",\"return\":1}}}}"
            )
            .expect("event should be sent");
            writeln!(
                server,
                "{{\"error\":{{\"class\":\"GenericError\",\"desc\":\"boom\"}}}}"
            )
            .expect("error should be sent");
        });
        let mut qmp = QmpClient::from_stream(client).expect("QMP should negotiate");
        let error = qmp.status().expect_err("QMP error must surface");
        assert!(error.to_string().contains("GenericError"));
        server_thread.join().expect("server fixture should finish");
    }

    #[test]
    fn qmp_status_decodes_json_escapes_and_rejects_missing_fields() {
        let (client, mut server) = UnixStream::pair().expect("socket pair should be created");
        let server_thread = thread::spawn(move || {
            let mut reader = negotiate(&mut server);
            let mut command = String::new();
            for response in [
                "{\"return\":{\"running\":false,\"status\":\"guest-\\u0070anicked\"}}",
                "{\"return\":{\"running\":true}}",
            ] {
                command.clear();
                reader
                    .read_line(&mut command)
                    .expect("status query should be read");
                writeln!(server, "{response}").expect("status should be sent");
            }
        });
        let mut qmp = QmpClient::from_stream(client).expect("QMP should negotiate");
        assert_eq!(qmp.status().expect("status should parse"), "guest-panicked");
        assert!(qmp
            .status()
            .expect_err("missing status must fail")
            .to_string()
            .contains("did not contain a status"));
        server_thread.join().expect("server fixture should finish");
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
            command.clear();
            reader
                .read_line(&mut command)
                .expect("balloon command should be read");
            assert!(command.contains("\"execute\":\"balloon\""));
            assert!(command.contains("\"value\":1073741824"));
            writeln!(server, "{{\"return\":{{}}}}").expect("balloon response should be sent");
        });
        let mut qmp = QmpClient::from_stream(client).expect("QMP should negotiate");
        qmp.pause().expect("pause should succeed");
        qmp.resume().expect("resume should succeed");
        qmp.balloon(1_073_741_824)
            .expect("balloon command should succeed");
        server_thread.join().expect("server fixture should finish");
    }
}
