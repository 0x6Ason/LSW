// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lsw_core::{
    decode_file_length, encode_file_length, read_frame, write_frame, FileGetRequest,
    FilePutRequest, Frame, FrameKind,
};

use super::{agent_error, AgentClient};

const STDIN_CHUNK_BYTES: usize = 32 * 1024;

impl AgentClient {
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
