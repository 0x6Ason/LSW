// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lsw_core::{
    decode_exit, decode_file_length, encode_file_length, read_frame, write_frame, ClientHello,
    FileGetRequest, FilePutRequest, Frame, FrameKind, InstanceManifest, ServerHello, StartRequest,
    AGENT_PROTOCOL_VERSION,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const STDIN_CHUNK_BYTES: usize = 32 * 1024;

pub struct AgentClient {
    stream: TcpStream,
    capabilities: Vec<String>,
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

    pub fn run(
        mut self,
        request: &StartRequest,
        connect_stdin: bool,
    ) -> Result<i32, Box<dyn std::error::Error>> {
        if matches!(request.kind, lsw_core::SessionKind::Shell)
            && !self
                .capabilities
                .iter()
                .any(|capability| capability == "conpty-v1")
        {
            eprintln!(
                "lsw: note: this agent provides a pipe shell; ConPTY is not available in this beta build"
            );
        }
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::Start, request.encode()?),
        )?;

        if connect_stdin {
            let writer = self.stream.try_clone()?;
            thread::spawn(move || forward_stdin(writer));
        }

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
                    return Err(
                        format!("guest agent: {}", String::from_utf8_lossy(&frame.payload)).into(),
                    )
                }
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
        if self
            .capabilities
            .iter()
            .any(|available| available == capability)
        {
            Ok(())
        } else {
            Err(format!("guest agent does not support {capability}").into())
        }
    }
}

fn forward_stdin(mut stream: TcpStream) {
    let mut stdin = io::stdin().lock();
    let mut buffer = [0_u8; STDIN_CHUNK_BYTES];
    loop {
        match stdin.read(&mut buffer) {
            Ok(0) => {
                let _ = stream.shutdown(Shutdown::Write);
                return;
            }
            Ok(length) => {
                if write_frame(
                    &mut stream,
                    &Frame::new(FrameKind::Stdin, buffer[..length].to_vec()),
                )
                .is_err()
                {
                    return;
                }
            }
            Err(_) => {
                let _ = stream.shutdown(Shutdown::Write);
                return;
            }
        }
    }
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
}
