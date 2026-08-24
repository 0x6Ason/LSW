// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::time::Duration;

use crate::{LswError, Result};

pub const AGENT_PROTOCOL_VERSION: u16 = 1;
pub const CAPABILITY_CONPTY_V1: &str = "conpty-v1";
pub const CAPABILITY_SESSION_CONTROL_V1: &str = "session-control-v1";
pub const CAPABILITY_SESSION_LEASE_V1: &str = "session-lease-v1";
pub const CAPABILITY_PROCESS_ENVIRONMENT_V1: &str = "process-environment-v1";
pub const CAPABILITY_DETACHED_RUN_V1: &str = "detached-run-v1";
pub const CAPABILITY_SESSION_SIGNAL_V1: &str = "session-signal-v1";
pub const CAPABILITY_TERMINAL_RESIZE_V1: &str = "terminal-resize-v1";
pub const CAPABILITY_POWER_HIBERNATE_V1: &str = "power-hibernate-v1";
pub const CAPABILITY_USER_ACCOUNT_V1: &str = "user-account-v1";
pub const CAPABILITY_MAINTENANCE_TRIM_V1: &str = "maintenance-trim-v1";
pub const SESSION_CANCEL_EXIT_CODE: i32 = 130;
pub const DEFAULT_SESSION_LEASE_TIMEOUT_MILLIS: u32 = 120_000;
pub const MIN_SESSION_LEASE_TIMEOUT_MILLIS: u32 = 1_000;
pub const MAX_SESSION_LEASE_TIMEOUT_MILLIS: u32 = 300_000;
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
    SessionLease = 24,
    SessionHeartbeat = 25,
    ProcessEnvironment = 26,
    SessionDetach = 27,
    Started = 28,
    SessionSignal = 29,
    PowerHibernate = 30,
    UserCreate = 31,
    MaintenanceTrim = 32,
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
            24 => Ok(Self::SessionLease),
            25 => Ok(Self::SessionHeartbeat),
            26 => Ok(Self::ProcessEnvironment),
            27 => Ok(Self::SessionDetach),
            28 => Ok(Self::Started),
            29 => Ok(Self::SessionSignal),
            30 => Ok(Self::PowerHibernate),
            31 => Ok(Self::UserCreate),
            32 => Ok(Self::MaintenanceTrim),
            _ => Err(LswError::Protocol(format!("unknown frame kind {value}"))),
        }
    }
}

pub struct UserCreateRequest {
    pub user_name: String,
    pub password: Vec<u8>,
    pub administrator: bool,
}

impl std::fmt::Debug for UserCreateRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UserCreateRequest")
            .field("user_name", &self.user_name)
            .field("password", &"[REDACTED]")
            .field("administrator", &self.administrator)
            .finish()
    }
}

impl Drop for UserCreateRequest {
    fn drop(&mut self) {
        self.password.fill(0);
    }
}

impl UserCreateRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        crate::validate_windows_user_name(&self.user_name)?;
        validate_password(&self.password)?;
        let mut payload = Vec::new();
        push_string(&mut payload, &self.user_name)?;
        let length = u32::try_from(self.password.len())
            .map_err(|_| LswError::Protocol("password is too long".to_owned()))?;
        payload.extend_from_slice(&length.to_be_bytes());
        payload.extend_from_slice(&self.password);
        payload.push(u8::from(self.administrator));
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let user_name = decoder.string()?;
        let length = usize::try_from(decoder.u32()?)
            .map_err(|_| LswError::Protocol("invalid password length".to_owned()))?;
        if length > 1024 {
            return Err(LswError::Protocol(
                "password exceeds the 1024 byte protocol limit".to_owned(),
            ));
        }
        let mut password = decoder.take(length)?.to_vec();
        let flag = match decoder.u8() {
            Ok(flag) => flag,
            Err(error) => {
                password.fill(0);
                return Err(error);
            }
        };
        let administrator = match flag {
            0 => false,
            1 => true,
            _ => {
                password.fill(0);
                return Err(LswError::Protocol(
                    "administrator flag must be zero or one".to_owned(),
                ));
            }
        };
        if let Err(error) = decoder.finish() {
            password.fill(0);
            return Err(error);
        }
        let request = Self {
            user_name,
            password,
            administrator,
        };
        crate::validate_windows_user_name(&request.user_name)?;
        validate_password(&request.password)?;
        Ok(request)
    }
}

fn validate_password(password: &[u8]) -> Result<()> {
    if password.is_empty() {
        return Err(LswError::Protocol("password must not be empty".to_owned()));
    }
    if password.len() > 1024 {
        return Err(LswError::Protocol(
            "password exceeds the 1024 byte protocol limit".to_owned(),
        ));
    }
    let password = std::str::from_utf8(password)
        .map_err(|_| LswError::Protocol("password must be valid UTF-8".to_owned()))?;
    if password.contains('\0') || password.encode_utf16().count() > 256 {
        return Err(LswError::Protocol(
            "password must contain at most 256 UTF-16 code units and no NUL".to_owned(),
        ));
    }
    Ok(())
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessEnvironment {
    pub variables: Vec<(String, String)>,
}

impl ProcessEnvironment {
    pub fn new(variables: Vec<(String, String)>) -> Result<Self> {
        validate_environment(&variables)?;
        Ok(Self { variables })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        validate_environment(&self.variables)?;
        let mut flattened = Vec::with_capacity(self.variables.len() * 2);
        for (name, value) in &self.variables {
            flattened.push(name.clone());
            flattened.push(value.clone());
        }
        let mut payload = Vec::new();
        push_strings(&mut payload, &flattened)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let flattened = decoder.strings()?;
        decoder.finish()?;
        if flattened.len() % 2 != 0 {
            return Err(LswError::Protocol(
                "process environment must contain name/value pairs".to_owned(),
            ));
        }
        let variables = flattened
            .chunks_exact(2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();
        Self::new(variables)
    }

    pub fn is_empty(&self) -> bool {
        self.variables.is_empty()
    }
}

fn validate_environment(variables: &[(String, String)]) -> Result<()> {
    if variables.len() > MAX_ARGUMENTS / 2 {
        return Err(LswError::Protocol(format!(
            "more than {} environment variables",
            MAX_ARGUMENTS / 2
        )));
    }
    let mut names = BTreeSet::new();
    for (name, value) in variables {
        if name.is_empty() || name.contains(['=', '\0']) {
            return Err(LswError::Protocol(
                "environment variable names must be non-empty and contain neither '=' nor NUL"
                    .to_owned(),
            ));
        }
        if value.contains('\0') {
            return Err(LswError::Protocol(
                "environment variable values must not contain NUL".to_owned(),
            ));
        }
        if !names.insert(name.to_uppercase()) {
            return Err(LswError::Protocol(format!(
                "environment variable {name:?} was supplied more than once"
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSignal {
    Interrupt,
    Terminate,
}

impl SessionSignal {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }

    pub const fn encode(self) -> [u8; 1] {
        [match self {
            Self::Interrupt => 1,
            Self::Terminate => 2,
        }]
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        match payload {
            [1] => Ok(Self::Interrupt),
            [2] => Ok(Self::Terminate),
            [value] => Err(LswError::Protocol(format!(
                "unknown session signal {value}"
            ))),
            _ => Err(LswError::Protocol(
                "session signal payload must contain one byte".to_owned(),
            )),
        }
    }
}

pub fn encode_process_id(process_id: u32) -> [u8; 4] {
    process_id.to_be_bytes()
}

pub fn decode_process_id(payload: &[u8]) -> Result<u32> {
    let bytes: [u8; 4] = payload
        .try_into()
        .map_err(|_| LswError::Protocol("process ID must contain four bytes".to_owned()))?;
    Ok(u32::from_be_bytes(bytes))
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

/// Opts one controlled process session into a bounded client heartbeat lease.
///
/// The timeout is negotiated per session so future clients can choose a more
/// conservative value without changing protocol version one. The agent caps
/// it to keep every opted-in half-open session bounded. Extremely short values
/// are rejected to avoid timer churn and sessions that cannot survive ordinary
/// scheduler jitter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLease {
    timeout_millis: u32,
}

impl SessionLease {
    pub fn new(timeout_millis: u32) -> Result<Self> {
        if !(MIN_SESSION_LEASE_TIMEOUT_MILLIS..=MAX_SESSION_LEASE_TIMEOUT_MILLIS)
            .contains(&timeout_millis)
        {
            return Err(LswError::Protocol(format!(
                "session lease timeout must be between {MIN_SESSION_LEASE_TIMEOUT_MILLIS} and {MAX_SESSION_LEASE_TIMEOUT_MILLIS} milliseconds"
            )));
        }
        Ok(Self { timeout_millis })
    }

    pub fn standard() -> Self {
        Self {
            timeout_millis: DEFAULT_SESSION_LEASE_TIMEOUT_MILLIS,
        }
    }

    pub const fn timeout_millis(self) -> u32 {
        self.timeout_millis
    }

    /// Four heartbeat opportunities per lease balance prompt cleanup with
    /// tolerance for a temporarily busy VM.
    pub fn heartbeat_interval(self) -> Duration {
        Duration::from_millis(u64::from((self.timeout_millis / 4).max(1)))
    }

    pub fn encode(self) -> Vec<u8> {
        self.timeout_millis.to_be_bytes().to_vec()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let bytes: [u8; 4] = payload.try_into().map_err(|_| {
            LswError::Protocol("session lease payload must contain four bytes".to_owned())
        })?;
        Self::new(u32::from_be_bytes(bytes))
    }
}

impl Default for SessionLease {
    fn default() -> Self {
        Self::standard()
    }
}

/// Clock-independent lease state. Callers supply elapsed monotonic
/// milliseconds, which keeps expiry behavior deterministic in tests and avoids
/// tying the wire protocol to a platform clock implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLeaseState {
    timeout_millis: u64,
    deadline_millis: u64,
}

impl SessionLeaseState {
    pub fn new(lease: SessionLease, now_millis: u64) -> Self {
        let timeout_millis = u64::from(lease.timeout_millis());
        Self {
            timeout_millis,
            deadline_millis: now_millis.saturating_add(timeout_millis),
        }
    }

    pub const fn deadline_millis(self) -> u64 {
        self.deadline_millis
    }

    pub fn is_expired(self, now_millis: u64) -> bool {
        now_millis >= self.deadline_millis
    }

    /// Extends a live lease. A heartbeat observed at or after the deadline
    /// cannot resurrect an already expired session.
    pub fn observe_heartbeat(&mut self, now_millis: u64) -> bool {
        if self.is_expired(now_millis) {
            return false;
        }
        self.deadline_millis = now_millis.saturating_add(self.timeout_millis);
        true
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
            argv: vec!["pwsh".to_owned(), "résumé.ps1".to_owned()],
            working_directory: Some("C:\\données".to_owned()),
        };
        assert_eq!(
            StartRequest::decode(&request.encode().expect("request should encode"))
                .expect("request should decode"),
            request
        );
    }

    #[test]
    fn process_environment_is_bounded_unambiguous_and_case_insensitive() {
        let environment = ProcessEnvironment::new(vec![
            ("LSW_MODE".to_owned(), "development".to_owned()),
            ("UNICODE".to_owned(), "données".to_owned()),
        ])
        .expect("environment should validate");
        assert_eq!(
            ProcessEnvironment::decode(&environment.encode().expect("environment should encode"))
                .expect("environment should decode"),
            environment
        );
        assert!(ProcessEnvironment::new(vec![("".to_owned(), "x".to_owned())]).is_err());
        assert!(ProcessEnvironment::new(vec![
            ("Path".to_owned(), "one".to_owned()),
            ("PATH".to_owned(), "two".to_owned()),
        ])
        .is_err());
        assert!(ProcessEnvironment::decode(&[0, 1, 0, 0, 0, 1, b'X']).is_err());
    }

    #[test]
    fn signal_and_process_id_payloads_are_exact() {
        for signal in [SessionSignal::Interrupt, SessionSignal::Terminate] {
            assert_eq!(
                SessionSignal::decode(&signal.encode()).expect("signal should decode"),
                signal
            );
        }
        assert_eq!(SessionSignal::Interrupt.exit_code(), 130);
        assert_eq!(SessionSignal::Terminate.exit_code(), 143);
        assert!(SessionSignal::decode(&[3]).is_err());
        assert_eq!(
            decode_process_id(&encode_process_id(u32::MAX)).expect("PID should decode"),
            u32::MAX
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
    fn user_creation_is_bounded_and_redacts_debug_output() {
        let request = UserCreateRequest {
            user_name: "desktop-user".to_owned(),
            password: "S3cure password!".as_bytes().to_vec(),
            administrator: false,
        };
        let encoded = request.encode().expect("user request should encode");
        let decoded = UserCreateRequest::decode(&encoded).expect("user request should decode");
        assert_eq!(decoded.user_name, "desktop-user");
        assert_eq!(decoded.password, b"S3cure password!");
        assert!(!decoded.administrator);
        assert!(!format!("{request:?}").contains("S3cure"));
        assert_eq!(
            FrameKind::try_from(31).expect("user-create kind should decode"),
            FrameKind::UserCreate
        );

        let mut invalid_flag = encoded;
        *invalid_flag.last_mut().expect("encoded flag exists") = 2;
        assert!(UserCreateRequest::decode(&invalid_flag).is_err());
        assert!(UserCreateRequest {
            user_name: "desktop-user".to_owned(),
            password: Vec::new(),
            administrator: false,
        }
        .encode()
        .is_err());
    }

    #[test]
    fn maintenance_trim_has_an_append_only_empty_request_kind() {
        assert_eq!(
            FrameKind::try_from(32).expect("maintenance-trim kind should decode"),
            FrameKind::MaintenanceTrim
        );
        assert_eq!(FrameKind::MaintenanceTrim as u8, 32);
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
    fn session_lease_is_append_only_bounded_and_strictly_encoded() {
        assert_eq!(
            FrameKind::try_from(24).expect("lease kind should decode"),
            FrameKind::SessionLease
        );
        assert_eq!(
            FrameKind::try_from(25).expect("heartbeat kind should decode"),
            FrameKind::SessionHeartbeat
        );

        let lease = SessionLease::standard();
        assert_eq!(
            SessionLease::decode(&lease.encode()).expect("lease should decode"),
            lease
        );
        assert_eq!(lease.heartbeat_interval(), Duration::from_secs(30));
        assert!(SessionLease::decode(&[]).is_err());
        assert!(SessionLease::decode(&[0, 0, 0, 0]).is_err());
        assert!(SessionLease::new(MIN_SESSION_LEASE_TIMEOUT_MILLIS - 1).is_err());
        assert!(SessionLease::new(MIN_SESSION_LEASE_TIMEOUT_MILLIS).is_ok());
        assert!(SessionLease::new(MAX_SESSION_LEASE_TIMEOUT_MILLIS).is_ok());
        assert!(SessionLease::new(MAX_SESSION_LEASE_TIMEOUT_MILLIS + 1).is_err());
    }

    #[test]
    fn session_lease_state_uses_monotonic_input_and_never_resurrects() {
        let lease = SessionLease::new(MIN_SESSION_LEASE_TIMEOUT_MILLIS)
            .expect("minimum lease should be valid");
        let mut state = SessionLeaseState::new(lease, 1_000);
        assert_eq!(state.deadline_millis(), 2_000);
        assert!(!state.is_expired(1_999));
        assert!(state.observe_heartbeat(1_500));
        assert_eq!(state.deadline_millis(), 2_500);
        assert!(!state.is_expired(2_499));
        assert!(state.is_expired(2_500));
        assert!(!state.observe_heartbeat(2_500));
        assert_eq!(state.deadline_millis(), 2_500);

        let saturated = SessionLeaseState::new(lease, u64::MAX - 10);
        assert_eq!(saturated.deadline_millis(), u64::MAX);
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
