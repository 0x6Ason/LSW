// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{ProtocolError as LswError, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveShareConfigureRequest {
    pub enable: bool,
}

impl LiveShareConfigureRequest {
    pub fn encode(self) -> Vec<u8> {
        vec![u8::from(self.enable)]
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        match payload {
            [0] => Ok(Self { enable: false }),
            [1] => Ok(Self { enable: true }),
            _ => Err(LswError::Protocol(
                "live-share configuration must be exactly zero or one".to_owned(),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveShareStatus {
    pub mapped: bool,
}

impl LiveShareStatus {
    pub fn encode(self) -> Vec<u8> {
        vec![u8::from(self.mapped)]
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        match payload {
            [0] => Ok(Self { mapped: false }),
            [1] => Ok(Self { mapped: true }),
            _ => Err(LswError::Protocol(
                "live-share status must be exactly zero or one".to_owned(),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum WindowsSudoMode {
    Disabled = 0,
    ForceNewWindow = 1,
    DisableInput = 2,
    Normal = 3,
}

impl WindowsSudoMode {
    fn decode(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Disabled),
            1 => Ok(Self::ForceNewWindow),
            2 => Ok(Self::DisableInput),
            3 => Ok(Self::Normal),
            _ => Err(LswError::Protocol(format!(
                "unknown Windows sudo mode {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowsSudoConfigureRequest {
    pub enable: bool,
}

impl WindowsSudoConfigureRequest {
    pub fn encode(self) -> Vec<u8> {
        vec![u8::from(self.enable)]
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        match payload {
            [0] => Ok(Self { enable: false }),
            [1] => Ok(Self { enable: true }),
            _ => Err(LswError::Protocol(
                "Windows sudo configuration must be exactly zero or one".to_owned(),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowsSudoStatus {
    pub available: bool,
    pub configured_mode: WindowsSudoMode,
    pub policy_mode: Option<WindowsSudoMode>,
}

impl WindowsSudoStatus {
    const NO_POLICY: u8 = u8::MAX;

    pub fn effective_mode(self) -> WindowsSudoMode {
        self.policy_mode.map_or(self.configured_mode, |maximum| {
            self.configured_mode.min(maximum)
        })
    }

    pub fn encode(self) -> Vec<u8> {
        vec![
            u8::from(self.available),
            self.configured_mode as u8,
            self.policy_mode.map_or(Self::NO_POLICY, |mode| mode as u8),
        ]
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let [available, configured_mode, policy_mode] = payload else {
            return Err(LswError::Protocol(
                "Windows sudo status must contain exactly three bytes".to_owned(),
            ));
        };
        let available = match available {
            0 => false,
            1 => true,
            _ => {
                return Err(LswError::Protocol(
                    "Windows sudo availability must be zero or one".to_owned(),
                ))
            }
        };
        let configured_mode = WindowsSudoMode::decode(*configured_mode)?;
        let policy_mode = if *policy_mode == Self::NO_POLICY {
            None
        } else {
            Some(WindowsSudoMode::decode(*policy_mode)?)
        };
        Ok(Self {
            available,
            configured_mode,
            policy_mode,
        })
    }
}
