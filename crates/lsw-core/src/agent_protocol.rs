// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{Read, Write};

use crate::{LswError, Result};

pub const AGENT_PROTOCOL_VERSION: u16 = 1;
pub const CAPABILITY_CONPTY_V1: &str = "conpty-v1";
pub const CAPABILITY_SESSION_CONTROL_V1: &str = "session-control-v1";
pub const CAPABILITY_TERMINAL_RESIZE_V1: &str = "terminal-resize-v1";
pub const SESSION_CANCEL_EXIT_CODE: i32 = 130;
pub const MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;
pub const MAX_ARGUMENTS: usize = 1024;
pub const MAX_STRING_BYTES: usize = 1024 * 1024;
pub const MAX_TERMINAL_DIMENSION: u16 = i16::MAX as u16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FrameKind {
    Hello = 1,
    HelloOk = 2,
    Error = 3,
    Ping = 4,
    Pong = 5,
    Start = 10,
    Stdin = 11,
    Stdout = 12,
    Stderr = 13,
    Resize = 14,
    Exit = 15,
    TerminalStart = 16,
    SessionOptions = 17,
    StdinClose = 18,
    SessionCancel = 19,
    FilePut = 20,
    FileGet = 21,
    FileData = 22,
    FileDone = 23,
}

impl TryFrom<u8> for FrameKind {
    type Error = LswError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::HelloOk),
            3 => Ok(Self::Error),
            4 => Ok(Self::Ping),
            5 => Ok(Self::Pong),
            10 => Ok(Self::Start),
            11 => Ok(Self::Stdin),
            12 => Ok(Self::Stdout),
            13 => Ok(Self::Stderr),
            14 => Ok(Self::Resize),
            15 => Ok(Self::Exit),
            16 => Ok(Self::TerminalStart),
            17 => Ok(Self::SessionOptions),
            18 => Ok(Self::StdinClose),
            19 => Ok(Self::SessionCancel),
            20 => Ok(Self::FilePut),
            21 => Ok(Self::FileGet),
            22 => Ok(Self::FileData),
            23 => Ok(Self::FileDone),
            _ => Err(LswError::Protocol(format!("unknown frame kind {value}"))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: FrameKind, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            payload: payload.into(),
        }
    }
}

pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> Result<()> {
    let length = u32::try_from(frame.payload.len()).map_err(|_| {
        LswError::Protocol(format!("frame exceeds the {} byte limit", MAX_FRAME_BYTES))
    })?;
    if length > MAX_FRAME_BYTES {
        return Err(LswError::Protocol(format!(
            "frame exceeds the {} byte limit",
            MAX_FRAME_BYTES
        )));
    }
    writer.write_all(&[frame.kind as u8])?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&frame.payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame(reader: &mut impl Read) -> Result<Frame> {
    let mut header = [0_u8; 5];
    reader.read_exact(&mut header)?;
    let kind = FrameKind::try_from(header[0])?;
    let length = u32::from_be_bytes(header[1..5].try_into().expect("fixed header length"));
    if length > MAX_FRAME_BYTES {
        return Err(LswError::Protocol(format!(
            "peer frame exceeds the {} byte limit",
            MAX_FRAME_BYTES
        )));
    }
    let mut payload = vec![0; length as usize];
    reader.read_exact(&mut payload)?;
    Ok(Frame { kind, payload })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientHello {
    pub version: u16,
    pub token: String,
}

impl ClientHello {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = self.version.to_be_bytes().to_vec();
        push_string(&mut payload, &self.token)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let version = decoder.u16()?;
        let token = decoder.string()?;
        decoder.finish()?;
        Ok(Self { version, token })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerHello {
    pub version: u16,
    pub capabilities: Vec<String>,
}

impl ServerHello {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = self.version.to_be_bytes().to_vec();
        push_strings(&mut payload, &self.capabilities)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let version = decoder.u16()?;
        let capabilities = decoder.strings()?;
        decoder.finish()?;
        Ok(Self {
            version,
            capabilities,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionKind {
    Shell,
    Exec,
    Run,
}

impl SessionKind {
    fn encode(self) -> u8 {
        match self {
            Self::Shell => 1,
            Self::Exec => 2,
            Self::Run => 3,
        }
    }

    fn decode(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Shell),
            2 => Ok(Self::Exec),
            3 => Ok(Self::Run),
            _ => Err(LswError::Protocol(format!("unknown session kind {value}"))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartRequest {
    pub kind: SessionKind,
    pub argv: Vec<String>,
    pub working_directory: Option<String>,
}

/// Opts a single authenticated process session into capability-gated control
/// semantics without changing the version-one handshake or start payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    pub cancel_on_disconnect: bool,
}

impl SessionOptions {
    const CANCEL_ON_DISCONNECT: u8 = 1;

    pub fn encode(self) -> Vec<u8> {
        vec![if self.cancel_on_disconnect {
            Self::CANCEL_ON_DISCONNECT
        } else {
            0
        }]
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let flags = match payload {
            [flags] => *flags,
            _ => {
                return Err(LswError::Protocol(
                    "session options payload must contain one byte".to_owned(),
                ))
            }
        };
        if flags & !Self::CANCEL_ON_DISCONNECT != 0 {
            return Err(LswError::Protocol(format!(
                "session options contain unknown flags 0x{flags:02x}"
            )));
        }
        Ok(Self {
            cancel_on_disconnect: flags & Self::CANCEL_ON_DISCONNECT != 0,
        })
    }
}

impl StartRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = vec![self.kind.encode()];
        match &self.working_directory {
            Some(directory) => {
                payload.push(1);
                push_string(&mut payload, directory)?;
            }
            None => payload.push(0),
        }
        push_strings(&mut payload, &self.argv)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let kind = SessionKind::decode(decoder.u8()?)?;
        let working_directory = match decoder.u8()? {
            0 => None,
            1 => Some(decoder.string()?),
            other => {
                return Err(LswError::Protocol(format!(
                    "invalid working-directory flag {other}"
                )))
            }
        };
        let argv = decoder.strings()?;
        decoder.finish()?;
        if argv.is_empty() {
            return Err(LswError::Protocol(
                "a start request needs at least one command".to_owned(),
            ));
        }
        Ok(Self {
            kind,
            argv,
            working_directory,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub rows: u16,
    pub columns: u16,
}

impl TerminalSize {
    pub const DEFAULT: Self = Self {
        rows: 24,
        columns: 80,
    };

    pub fn new(rows: u16, columns: u16) -> Result<Self> {
        if rows == 0 || columns == 0 {
            return Err(LswError::Protocol(
                "terminal dimensions must be non-zero".to_owned(),
            ));
        }
        if rows > MAX_TERMINAL_DIMENSION || columns > MAX_TERMINAL_DIMENSION {
            return Err(LswError::Protocol(format!(
                "terminal dimensions must not exceed {MAX_TERMINAL_DIMENSION}"
            )));
        }
        Ok(Self { rows, columns })
    }

    pub fn encode(self) -> Vec<u8> {
        encode_resize(self.rows, self.columns)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let (rows, columns) = decode_resize(payload)?;
        Self::new(rows, columns)
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A capability-gated terminal start message.
///
/// Clients send this as `FrameKind::TerminalStart` only after the server has
/// advertised `conpty-v1`. Older agents therefore continue to receive the
/// original `Start` frame and do not need to understand this payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalStartRequest {
    pub size: TerminalSize,
    pub request: StartRequest,
}

impl TerminalStartRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = self.size.encode();
        payload.extend_from_slice(&self.request.encode()?);
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        if payload.len() < 4 {
            return Err(LswError::Protocol(
                "terminal start payload is truncated".to_owned(),
            ));
        }
        Ok(Self {
            size: TerminalSize::decode(&payload[..4])?,
            request: StartRequest::decode(&payload[4..])?,
        })
    }
}

pub fn encode_exit(code: i32) -> Vec<u8> {
    code.to_be_bytes().to_vec()
}

pub fn decode_exit(payload: &[u8]) -> Result<i32> {
    let bytes: [u8; 4] = payload
        .try_into()
        .map_err(|_| LswError::Protocol("exit payload must contain four bytes".to_owned()))?;
    Ok(i32::from_be_bytes(bytes))
}

pub fn encode_resize(rows: u16, columns: u16) -> Vec<u8> {
    [rows.to_be_bytes(), columns.to_be_bytes()].concat()
}

pub fn decode_resize(payload: &[u8]) -> Result<(u16, u16)> {
    if payload.len() != 4 {
        return Err(LswError::Protocol(
            "resize payload must contain four bytes".to_owned(),
        ));
    }
    Ok((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePutRequest {
    pub destination: String,
    pub length: u64,
}

impl FilePutRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = self.length.to_be_bytes().to_vec();
        push_string(&mut payload, &self.destination)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let length = decoder.u64()?;
        let destination = decoder.string()?;
        decoder.finish()?;
        if destination.is_empty() {
            return Err(LswError::Protocol(
                "file destination must not be empty".to_owned(),
            ));
        }
        Ok(Self {
            destination,
            length,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileGetRequest {
    pub source: String,
}

impl FileGetRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        push_string(&mut payload, &self.source)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let source = decoder.string()?;
        decoder.finish()?;
        if source.is_empty() {
            return Err(LswError::Protocol(
                "file source must not be empty".to_owned(),
            ));
        }
        Ok(Self { source })
    }
}

pub fn encode_file_length(length: u64) -> Vec<u8> {
    length.to_be_bytes().to_vec()
}

pub fn decode_file_length(payload: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = payload
        .try_into()
        .map_err(|_| LswError::Protocol("file length must contain eight bytes".to_owned()))?;
    Ok(u64::from_be_bytes(bytes))
}

pub fn constant_time_token_eq(left: &str, right: &str) -> bool {
    let mut difference = left.len() ^ right.len();
    let maximum = left.len().max(right.len());
    for index in 0..maximum {
        let left_byte = left.as_bytes().get(index).copied().unwrap_or_default();
        let right_byte = right.as_bytes().get(index).copied().unwrap_or_default();
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

fn push_strings(payload: &mut Vec<u8>, values: &[String]) -> Result<()> {
    let count = u16::try_from(values.len())
        .map_err(|_| LswError::Protocol("too many string values".to_owned()))?;
    if values.len() > MAX_ARGUMENTS {
        return Err(LswError::Protocol(format!(
            "more than {MAX_ARGUMENTS} string values"
        )));
    }
    payload.extend_from_slice(&count.to_be_bytes());
    for value in values {
        push_string(payload, value)?;
    }
    Ok(())
}

fn push_string(payload: &mut Vec<u8>, value: &str) -> Result<()> {
    if value.len() > MAX_STRING_BYTES {
        return Err(LswError::Protocol(format!(
            "string exceeds the {MAX_STRING_BYTES} byte limit"
        )));
    }
    let length = u32::try_from(value.len())
        .map_err(|_| LswError::Protocol("string length does not fit in u32".to_owned()))?;
    payload.extend_from_slice(&length.to_be_bytes());
    payload.extend_from_slice(value.as_bytes());
    Ok(())
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self { remaining: payload }
    }

    fn u8(&mut self) -> Result<u8> {
        let value = *self
            .remaining
            .first()
            .ok_or_else(|| LswError::Protocol("truncated u8 field".to_owned()))?;
        self.remaining = &self.remaining[1..];
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes(
            bytes.try_into().expect("fixed u32 field length"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes(
            bytes.try_into().expect("fixed u64 field length"),
        ))
    }

    fn string(&mut self) -> Result<String> {
        let length = usize::try_from(self.u32()?)
            .map_err(|_| LswError::Protocol("invalid string length".to_owned()))?;
        if length > MAX_STRING_BYTES {
            return Err(LswError::Protocol(format!(
                "string exceeds the {MAX_STRING_BYTES} byte limit"
            )));
        }
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| LswError::Protocol("string is not valid UTF-8".to_owned()))
    }

    fn strings(&mut self) -> Result<Vec<String>> {
        let count = usize::from(self.u16()?);
        if count > MAX_ARGUMENTS {
            return Err(LswError::Protocol(format!(
                "more than {MAX_ARGUMENTS} string values"
            )));
        }
        (0..count).map(|_| self.string()).collect()
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        if self.remaining.len() < length {
            return Err(LswError::Protocol("truncated payload".to_owned()));
        }
        let (value, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        Ok(value)
    }

    fn finish(self) -> Result<()> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(LswError::Protocol("payload has trailing bytes".to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_is_binary_safe() {
        let frame = Frame::new(FrameKind::Stdout, [0, 1, 2, 255]);
        let mut encoded = Vec::new();
        write_frame(&mut encoded, &frame).expect("frame should encode");
        assert_eq!(
            read_frame(&mut encoded.as_slice()).expect("frame should decode"),
            frame
        );
    }

    #[test]
    fn start_request_round_trip_preserves_unicode() {
        let request = StartRequest {
            kind: SessionKind::Exec,
            argv: vec!["pwsh".to_owned(), "香港.ps1".to_owned()],
            working_directory: Some("C:\\工作".to_owned()),
        };
        assert_eq!(
            StartRequest::decode(&request.encode().expect("request should encode"))
                .expect("request should decode"),
            request
        );
    }

    #[test]
    fn malformed_lengths_and_tokens_are_rejected() {
        assert!(read_frame(&mut [FrameKind::Stdout as u8, 255, 255, 255, 255].as_slice()).is_err());
        assert!(constant_time_token_eq("same", "same"));
        assert!(!constant_time_token_eq("same", "different"));
    }

    #[test]
    fn file_requests_are_length_delimited() {
        let put = FilePutRequest {
            destination: "C:\\src\\main.rs".to_owned(),
            length: u64::MAX - 1,
        };
        assert_eq!(
            FilePutRequest::decode(&put.encode().expect("put should encode"))
                .expect("put should decode"),
            put
        );
        let get = FileGetRequest {
            source: "C:\\out\\app.exe".to_owned(),
        };
        assert_eq!(
            FileGetRequest::decode(&get.encode().expect("get should encode"))
                .expect("get should decode"),
            get
        );
    }

    #[test]
    fn terminal_start_is_capability_gated_without_changing_start() {
        let request = StartRequest {
            kind: SessionKind::Shell,
            argv: vec!["pwsh.exe".to_owned(), "cmd.exe".to_owned()],
            working_directory: Some("C:\\src".to_owned()),
        };
        let terminal = TerminalStartRequest {
            size: TerminalSize::new(42, 132).expect("terminal size should be valid"),
            request,
        };
        assert_eq!(
            TerminalStartRequest::decode(&terminal.encode().expect("request should encode"))
                .expect("request should decode"),
            terminal
        );
        assert_eq!(
            FrameKind::try_from(16).expect("kind should decode"),
            FrameKind::TerminalStart
        );
    }

    #[test]
    fn session_control_is_append_only_and_strictly_encoded() {
        assert_eq!(
            FrameKind::try_from(17).expect("options kind should decode"),
            FrameKind::SessionOptions
        );
        assert_eq!(
            FrameKind::try_from(18).expect("stdin-close kind should decode"),
            FrameKind::StdinClose
        );
        assert_eq!(
            FrameKind::try_from(19).expect("cancel kind should decode"),
            FrameKind::SessionCancel
        );

        for cancel_on_disconnect in [false, true] {
            let options = SessionOptions {
                cancel_on_disconnect,
            };
            assert_eq!(
                SessionOptions::decode(&options.encode()).expect("options should decode"),
                options
            );
        }
        assert!(SessionOptions::decode(&[]).is_err());
        assert!(SessionOptions::decode(&[0, 0]).is_err());
        assert!(SessionOptions::decode(&[2]).is_err());
    }

    #[test]
    fn terminal_dimensions_fit_windows_coord() {
        assert!(TerminalSize::new(0, 80).is_err());
        assert!(TerminalSize::new(24, 0).is_err());
        assert!(TerminalSize::new(MAX_TERMINAL_DIMENSION, MAX_TERMINAL_DIMENSION).is_ok());
        assert!(TerminalSize::new(MAX_TERMINAL_DIMENSION + 1, 80).is_err());
        assert!(TerminalSize::decode(&[0, 24, 0, 80, 0]).is_err());
    }
}
