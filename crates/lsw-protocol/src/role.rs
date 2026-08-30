// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;
use std::str::FromStr;

use crate::{ProtocolError, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowsUserRole {
    Standard,
    Administrator,
}

impl fmt::Display for WindowsUserRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Standard => "standard",
            Self::Administrator => "administrator",
        })
    }
}

impl FromStr for WindowsUserRole {
    type Err = ProtocolError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "standard" => Ok(Self::Standard),
            "administrator" => Ok(Self::Administrator),
            _ => Err(ProtocolError::Protocol(format!(
                "unknown Windows user role {value:?}; expected standard or administrator"
            ))),
        }
    }
}

pub fn validate_windows_user_name(name: &str) -> Result<()> {
    const FORBIDDEN: [char; 16] = [
        '"', '/', '\\', '[', ']', ':', ';', '|', '=', ',', '+', '*', '?', '<', '>', '@',
    ];
    let valid = !name.is_empty()
        && name.encode_utf16().count() <= 20
        && name != "."
        && name != ".."
        && !name.ends_with([' ', '.'])
        && !name
            .chars()
            .any(|character| character.is_control() || FORBIDDEN.contains(&character));
    if valid {
        Ok(())
    } else {
        Err(ProtocolError::Protocol(
            "Windows user name must be 1-20 UTF-16 code units and contain no account-name separators or trailing space/dot".to_owned(),
        ))
    }
}
