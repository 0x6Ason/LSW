// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeSet;
use std::time::Duration;

use crate::codec::{push_string, push_strings, Decoder};
use crate::{
    ProtocolError as LswError, Result, DEFAULT_SESSION_LEASE_TIMEOUT_MILLIS, MAX_ARGUMENTS,
    MAX_SESSION_LEASE_TIMEOUT_MILLIS, MAX_TERMINAL_DIMENSION, MIN_SESSION_LEASE_TIMEOUT_MILLIS,
};

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopUserRequest {
    pub user_name: String,
}

impl DesktopUserRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        crate::validate_windows_user_name(&self.user_name)?;
        let mut payload = Vec::new();
        push_string(&mut payload, &self.user_name)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let user_name = decoder.string()?;
        decoder.finish()?;
        crate::validate_windows_user_name(&user_name)?;
        Ok(Self { user_name })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopLiveShareRequest {
    pub user_name: String,
    pub enable: bool,
}

impl DesktopLiveShareRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        crate::validate_windows_user_name(&self.user_name)?;
        let mut payload = Vec::new();
        push_string(&mut payload, &self.user_name)?;
        payload.push(u8::from(self.enable));
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let user_name = decoder.string()?;
        let enable = match decoder.u8()? {
            0 => false,
            1 => true,
            _ => {
                return Err(LswError::Protocol(
                    "desktop live-share flag must be zero or one".to_owned(),
                ))
            }
        };
        decoder.finish()?;
        crate::validate_windows_user_name(&user_name)?;
        Ok(Self { user_name, enable })
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
