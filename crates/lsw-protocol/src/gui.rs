// SPDX-License-Identifier: GPL-3.0-or-later

use crate::codec::{push_string, Decoder};
use crate::{
    ProcessEnvironment, ProtocolError as LswError, Result, SessionKind, StartRequest,
    MAX_GUI_DAMAGE_DIMENSION, MAX_GUI_FRAME_BYTES, MAX_GUI_WINDOW_DIMENSION,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuiStartRequest {
    pub user_name: String,
    pub request: StartRequest,
    pub environment: ProcessEnvironment,
    pub mount_live_share: bool,
}

impl GuiStartRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        crate::validate_windows_user_name(&self.user_name)?;
        validate_gui_start(&self.request, &self.environment)?;
        let request = self.request.encode()?;
        let environment = self.environment.encode()?;
        let request_length = u32::try_from(request.len())
            .map_err(|_| LswError::Protocol("GUI start request is too long".to_owned()))?;
        let mut payload = Vec::new();
        push_string(&mut payload, &self.user_name)?;
        payload.push(u8::from(self.mount_live_share));
        payload.extend_from_slice(&request_length.to_be_bytes());
        payload.extend_from_slice(&request);
        payload.extend_from_slice(&environment);
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let user_name = decoder.string()?;
        let mount_live_share = match decoder.u8()? {
            0 => false,
            1 => true,
            _ => {
                return Err(LswError::Protocol(
                    "GUI live-share flag must be zero or one".to_owned(),
                ))
            }
        };
        let request_length = usize::try_from(decoder.u32()?)
            .map_err(|_| LswError::Protocol("invalid GUI start length".to_owned()))?;
        let request = StartRequest::decode(decoder.take(request_length)?)?;
        let environment = ProcessEnvironment::decode(decoder.remaining)?;
        decoder.remaining = &[];
        decoder.finish()?;
        crate::validate_windows_user_name(&user_name)?;
        validate_gui_start(&request, &environment)?;
        Ok(Self {
            user_name,
            request,
            environment,
            mount_live_share,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuiWindowReady {
    pub process_id: u32,
    pub window_id: u64,
    pub width: u32,
    pub height: u32,
    pub title: String,
}

impl GuiWindowReady {
    pub fn encode(&self) -> Result<Vec<u8>> {
        validate_gui_dimensions(self.width, self.height)?;
        if self.process_id == 0 {
            return Err(LswError::Protocol(
                "GUI process identifier must be non-zero".to_owned(),
            ));
        }
        if self.window_id == 0 {
            return Err(LswError::Protocol(
                "GUI window identifier must be non-zero".to_owned(),
            ));
        }
        let mut payload = Vec::new();
        payload.extend_from_slice(&self.process_id.to_be_bytes());
        payload.extend_from_slice(&self.window_id.to_be_bytes());
        payload.extend_from_slice(&self.width.to_be_bytes());
        payload.extend_from_slice(&self.height.to_be_bytes());
        push_string(&mut payload, &self.title)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let ready = Self {
            process_id: decoder.u32()?,
            window_id: decoder.u64()?,
            width: decoder.u32()?,
            height: decoder.u32()?,
            title: decoder.string()?,
        };
        decoder.finish()?;
        ready.encode()?;
        Ok(ready)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuiWindowDamage {
    pub sequence: u64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl GuiWindowDamage {
    pub fn encode(&self) -> Result<Vec<u8>> {
        validate_gui_damage(self)?;
        let mut payload = Vec::with_capacity(24 + self.bgra.len());
        payload.extend_from_slice(&self.sequence.to_be_bytes());
        payload.extend_from_slice(&self.x.to_be_bytes());
        payload.extend_from_slice(&self.y.to_be_bytes());
        payload.extend_from_slice(&self.width.to_be_bytes());
        payload.extend_from_slice(&self.height.to_be_bytes());
        payload.extend_from_slice(&self.bgra);
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let sequence = decoder.u64()?;
        let x = decoder.u32()?;
        let y = decoder.u32()?;
        let width = decoder.u32()?;
        let height = decoder.u32()?;
        let bgra = decoder.remaining.to_vec();
        decoder.remaining = &[];
        decoder.finish()?;
        let damage = Self {
            sequence,
            x,
            y,
            width,
            height,
            bgra,
        };
        validate_gui_damage(&damage)?;
        Ok(damage)
    }
}

fn validate_gui_damage(damage: &GuiWindowDamage) -> Result<()> {
    if damage.width == 0
        || damage.height == 0
        || damage.width > MAX_GUI_DAMAGE_DIMENSION
        || damage.height > MAX_GUI_DAMAGE_DIMENSION
    {
        return Err(LswError::Protocol(format!(
            "GUI damage dimensions must be between 1 and {MAX_GUI_DAMAGE_DIMENSION}"
        )));
    }
    let right = damage
        .x
        .checked_add(damage.width)
        .ok_or_else(|| LswError::Protocol("GUI damage x coordinate overflowed".to_owned()))?;
    let bottom = damage
        .y
        .checked_add(damage.height)
        .ok_or_else(|| LswError::Protocol("GUI damage y coordinate overflowed".to_owned()))?;
    if right > MAX_GUI_WINDOW_DIMENSION || bottom > MAX_GUI_WINDOW_DIMENSION {
        return Err(LswError::Protocol(
            "GUI damage lies outside the supported window bounds".to_owned(),
        ));
    }
    let expected = usize::try_from(damage.width)
        .ok()
        .and_then(|width| {
            usize::try_from(damage.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| LswError::Protocol("GUI damage byte length overflowed".to_owned()))?;
    if damage.bgra.len() != expected {
        return Err(LswError::Protocol(format!(
            "GUI damage contains {} bytes; expected {expected}",
            damage.bgra.len()
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuiPointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuiWindowAction {
    Move,
    Minimize,
    Maximize,
    Close,
    ResizeTopLeft,
    ResizeTop,
    ResizeTopRight,
    ResizeRight,
    ResizeBottomRight,
    ResizeBottom,
    ResizeBottomLeft,
    ResizeLeft,
    Restore,
}

impl GuiWindowAction {
    pub fn encode(self) -> Vec<u8> {
        vec![match self {
            Self::Move => 1,
            Self::Minimize => 2,
            Self::Maximize => 3,
            Self::Close => 4,
            Self::ResizeTopLeft => 5,
            Self::ResizeTop => 6,
            Self::ResizeTopRight => 7,
            Self::ResizeRight => 8,
            Self::ResizeBottomRight => 9,
            Self::ResizeBottom => 10,
            Self::ResizeBottomLeft => 11,
            Self::ResizeLeft => 12,
            Self::Restore => 13,
        }]
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        match payload {
            [1] => Ok(Self::Move),
            [2] => Ok(Self::Minimize),
            [3] => Ok(Self::Maximize),
            [4] => Ok(Self::Close),
            [5] => Ok(Self::ResizeTopLeft),
            [6] => Ok(Self::ResizeTop),
            [7] => Ok(Self::ResizeTopRight),
            [8] => Ok(Self::ResizeRight),
            [9] => Ok(Self::ResizeBottomRight),
            [10] => Ok(Self::ResizeBottom),
            [11] => Ok(Self::ResizeBottomLeft),
            [12] => Ok(Self::ResizeLeft),
            [13] => Ok(Self::Restore),
            _ => Err(LswError::Protocol(
                "GUI window action must contain one known action byte".to_owned(),
            )),
        }
    }

    pub fn is_drag(self) -> bool {
        matches!(
            self,
            Self::Move
                | Self::ResizeTopLeft
                | Self::ResizeTop
                | Self::ResizeTopRight
                | Self::ResizeRight
                | Self::ResizeBottomRight
                | Self::ResizeBottom
                | Self::ResizeBottomLeft
                | Self::ResizeLeft
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuiWindowDragHint {
    pub x: u32,
    pub y: u32,
    pub action: Option<GuiWindowAction>,
}

impl GuiWindowDragHint {
    pub fn encode(self) -> Result<Vec<u8>> {
        if self.x >= MAX_GUI_WINDOW_DIMENSION || self.y >= MAX_GUI_WINDOW_DIMENSION {
            return Err(LswError::Protocol(
                "GUI drag hint coordinates exceed the bounded window dimensions".to_owned(),
            ));
        }
        if self.action.is_some_and(|action| !action.is_drag()) {
            return Err(LswError::Protocol(
                "GUI drag hint may contain only move or resize actions".to_owned(),
            ));
        }
        let mut payload = Vec::with_capacity(9);
        payload.push(self.action.map(|action| action.encode()[0]).unwrap_or(0));
        payload.extend_from_slice(&self.x.to_le_bytes());
        payload.extend_from_slice(&self.y.to_le_bytes());
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let [action, x0, x1, x2, x3, y0, y1, y2, y3] = payload else {
            return Err(LswError::Protocol(
                "GUI drag hint must contain one action byte and two coordinates".to_owned(),
            ));
        };
        let action = if *action == 0 {
            None
        } else {
            let action = GuiWindowAction::decode(&[*action])?;
            if !action.is_drag() {
                return Err(LswError::Protocol(
                    "GUI drag hint may contain only move or resize actions".to_owned(),
                ));
            }
            Some(action)
        };
        let hint = Self {
            x: u32::from_le_bytes([*x0, *x1, *x2, *x3]),
            y: u32::from_le_bytes([*y0, *y1, *y2, *y3]),
            action,
        };
        if hint.x >= MAX_GUI_WINDOW_DIMENSION || hint.y >= MAX_GUI_WINDOW_DIMENSION {
            return Err(LswError::Protocol(
                "GUI drag hint coordinates exceed the bounded window dimensions".to_owned(),
            ));
        }
        Ok(hint)
    }
}

impl GuiPointerButton {
    fn encode(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 3,
            Self::Back => 4,
            Self::Forward => 5,
        }
    }

    fn decode(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Left),
            2 => Ok(Self::Right),
            3 => Ok(Self::Middle),
            4 => Ok(Self::Back),
            5 => Ok(Self::Forward),
            _ => Err(LswError::Protocol(format!(
                "unknown GUI pointer button {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuiInputEvent {
    Focus {
        focused: bool,
    },
    Key {
        virtual_key: u16,
        scan_code: u16,
        pressed: bool,
        extended: bool,
    },
    PointerMove {
        x: u32,
        y: u32,
    },
    PointerButton {
        button: GuiPointerButton,
        pressed: bool,
        x: u32,
        y: u32,
    },
    PointerWheel {
        delta: i16,
        horizontal: bool,
        x: u32,
        y: u32,
    },
}

impl GuiInputEvent {
    pub fn encode(self) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        match self {
            Self::Focus { focused } => {
                payload.extend_from_slice(&[1, u8::from(focused)]);
            }
            Self::Key {
                virtual_key,
                scan_code,
                pressed,
                extended,
            } => {
                if virtual_key == 0 || virtual_key > u16::from(u8::MAX) {
                    return Err(LswError::Protocol(
                        "GUI virtual key must be in the Windows byte-sized VK range".to_owned(),
                    ));
                }
                payload.push(2);
                payload.extend_from_slice(&virtual_key.to_be_bytes());
                payload.extend_from_slice(&scan_code.to_be_bytes());
                payload.push(u8::from(pressed));
                payload.push(u8::from(extended));
            }
            Self::PointerMove { x, y } => {
                validate_gui_coordinate(x, y)?;
                payload.push(3);
                payload.extend_from_slice(&x.to_be_bytes());
                payload.extend_from_slice(&y.to_be_bytes());
            }
            Self::PointerButton {
                button,
                pressed,
                x,
                y,
            } => {
                validate_gui_coordinate(x, y)?;
                payload.extend_from_slice(&[4, button.encode(), u8::from(pressed)]);
                payload.extend_from_slice(&x.to_be_bytes());
                payload.extend_from_slice(&y.to_be_bytes());
            }
            Self::PointerWheel {
                delta,
                horizontal,
                x,
                y,
            } => {
                if delta == 0 {
                    return Err(LswError::Protocol(
                        "GUI pointer wheel delta must be non-zero".to_owned(),
                    ));
                }
                validate_gui_coordinate(x, y)?;
                payload.push(5);
                payload.extend_from_slice(&delta.to_be_bytes());
                payload.push(u8::from(horizontal));
                payload.extend_from_slice(&x.to_be_bytes());
                payload.extend_from_slice(&y.to_be_bytes());
            }
        }
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let event = match decoder.u8()? {
            1 => Self::Focus {
                focused: decode_bool(decoder.u8()?, "GUI focus")?,
            },
            2 => Self::Key {
                virtual_key: decoder.u16()?,
                scan_code: decoder.u16()?,
                pressed: decode_bool(decoder.u8()?, "GUI key pressed")?,
                extended: decode_bool(decoder.u8()?, "GUI key extended")?,
            },
            3 => Self::PointerMove {
                x: decoder.u32()?,
                y: decoder.u32()?,
            },
            4 => Self::PointerButton {
                button: GuiPointerButton::decode(decoder.u8()?)?,
                pressed: decode_bool(decoder.u8()?, "GUI pointer button pressed")?,
                x: decoder.u32()?,
                y: decoder.u32()?,
            },
            5 => Self::PointerWheel {
                delta: decoder.i16()?,
                horizontal: decode_bool(decoder.u8()?, "GUI pointer wheel direction")?,
                x: decoder.u32()?,
                y: decoder.u32()?,
            },
            kind => {
                return Err(LswError::Protocol(format!(
                    "unknown GUI input event kind {kind}"
                )))
            }
        };
        decoder.finish()?;
        event.encode()?;
        Ok(event)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuiWindowResize {
    pub width: u32,
    pub height: u32,
}

impl GuiWindowResize {
    pub fn encode(self) -> Result<Vec<u8>> {
        validate_gui_dimensions(self.width, self.height)?;
        let mut payload = self.width.to_be_bytes().to_vec();
        payload.extend_from_slice(&self.height.to_be_bytes());
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let resize = Self {
            width: decoder.u32()?,
            height: decoder.u32()?,
        };
        decoder.finish()?;
        validate_gui_dimensions(resize.width, resize.height)?;
        Ok(resize)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuiWindowClosed {
    pub exit_code: i32,
}

impl GuiWindowClosed {
    pub fn encode(self) -> [u8; 4] {
        self.exit_code.to_be_bytes()
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let bytes: [u8; 4] = payload.try_into().map_err(|_| {
            LswError::Protocol("GUI closed payload must contain four bytes".to_owned())
        })?;
        Ok(Self {
            exit_code: i32::from_be_bytes(bytes),
        })
    }
}

fn validate_gui_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0
        || height == 0
        || width > MAX_GUI_WINDOW_DIMENSION
        || height > MAX_GUI_WINDOW_DIMENSION
    {
        return Err(LswError::Protocol(format!(
            "GUI window dimensions must be between 1 and {MAX_GUI_WINDOW_DIMENSION}"
        )));
    }
    let bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| LswError::Protocol("GUI frame byte length overflowed".to_owned()))?;
    if bytes > MAX_GUI_FRAME_BYTES {
        return Err(LswError::Protocol(format!(
            "GUI frame exceeds the {MAX_GUI_FRAME_BYTES} byte memory limit"
        )));
    }
    Ok(())
}

fn validate_gui_coordinate(x: u32, y: u32) -> Result<()> {
    if x >= MAX_GUI_WINDOW_DIMENSION || y >= MAX_GUI_WINDOW_DIMENSION {
        return Err(LswError::Protocol(
            "GUI pointer coordinate exceeds the supported window bounds".to_owned(),
        ));
    }
    Ok(())
}

fn decode_bool(value: u8, field: &str) -> Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(LswError::Protocol(format!(
            "{field} flag must be zero or one"
        ))),
    }
}

fn validate_gui_start(request: &StartRequest, environment: &ProcessEnvironment) -> Result<()> {
    if request.kind != SessionKind::Run {
        return Err(LswError::Protocol(
            "GUI launch requires a run request".to_owned(),
        ));
    }
    let program = request
        .argv
        .first()
        .ok_or_else(|| LswError::Protocol("GUI launch requires an executable".to_owned()))?;
    if !program.to_ascii_lowercase().ends_with(".exe") {
        return Err(LswError::Protocol(
            "GUI launch program must end in .exe".to_owned(),
        ));
    }
    const RESERVED: [&str; 4] = [
        "LSW_DESKTOP_TOKEN",
        "LSW_LIVE_SHARE_TOKEN",
        "LSW_DESKTOP_USER",
        "LSW_ICON_SOURCE",
    ];
    if environment.variables.iter().any(|(name, _)| {
        RESERVED
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
    }) {
        return Err(LswError::Protocol(
            "GUI environment must not replace reserved LSW integration variables".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuiIconRequest {
    pub user_name: String,
    pub program: String,
}

impl GuiIconRequest {
    pub fn encode(&self) -> Result<Vec<u8>> {
        crate::validate_windows_user_name(&self.user_name)?;
        if self.program.is_empty() || !self.program.to_ascii_lowercase().ends_with(".exe") {
            return Err(LswError::Protocol(
                "GUI icon program must be a non-empty .exe path".to_owned(),
            ));
        }
        let mut payload = Vec::new();
        push_string(&mut payload, &self.user_name)?;
        push_string(&mut payload, &self.program)?;
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut decoder = Decoder::new(payload);
        let request = Self {
            user_name: decoder.string()?,
            program: decoder.string()?,
        };
        decoder.finish()?;
        request.encode()?;
        Ok(request)
    }
}
