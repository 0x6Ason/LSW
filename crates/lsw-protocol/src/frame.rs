// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{Read, Write};

use crate::{ProtocolError as LswError, Result, MAX_FRAME_BYTES};

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
    MaintenanceHibernate = 33,
    UserSetRole = 34,
    WindowsSudoQuery = 35,
    WindowsSudoConfigure = 36,
    WindowsSudoStatus = 37,
    LiveShareQuery = 38,
    LiveShareConfigure = 39,
    LiveShareStatus = 40,
    MaintenanceShutdown = 41,
    DesktopCompanionStart = 42,
    GuiStart = 43,
    GuiIcon = 44,
    GuiIconData = 45,
    DesktopLiveShareConfigure = 46,
    GuiWindowOpen = 47,
    GuiWindowReady = 48,
    GuiWindowDamage = 49,
    GuiWindowInput = 50,
    GuiWindowResize = 51,
    GuiWindowClose = 52,
    GuiWindowClosed = 53,
    GuiWindowAction = 54,
    GuiWindowDragHint = 55,
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
            33 => Ok(Self::MaintenanceHibernate),
            34 => Ok(Self::UserSetRole),
            35 => Ok(Self::WindowsSudoQuery),
            36 => Ok(Self::WindowsSudoConfigure),
            37 => Ok(Self::WindowsSudoStatus),
            38 => Ok(Self::LiveShareQuery),
            39 => Ok(Self::LiveShareConfigure),
            40 => Ok(Self::LiveShareStatus),
            41 => Ok(Self::MaintenanceShutdown),
            42 => Ok(Self::DesktopCompanionStart),
            43 => Ok(Self::GuiStart),
            44 => Ok(Self::GuiIcon),
            45 => Ok(Self::GuiIconData),
            46 => Ok(Self::DesktopLiveShareConfigure),
            47 => Ok(Self::GuiWindowOpen),
            48 => Ok(Self::GuiWindowReady),
            49 => Ok(Self::GuiWindowDamage),
            50 => Ok(Self::GuiWindowInput),
            51 => Ok(Self::GuiWindowResize),
            52 => Ok(Self::GuiWindowClose),
            53 => Ok(Self::GuiWindowClosed),
            54 => Ok(Self::GuiWindowAction),
            55 => Ok(Self::GuiWindowDragHint),
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
