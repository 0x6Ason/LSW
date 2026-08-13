// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
pub enum LswError {
    Io(io::Error),
    InvalidInstanceName(String),
    InvalidValue {
        field: &'static str,
        reason: String,
    },
    InvalidManifest(String),
    Protocol(String),
    MissingCapabilities(Vec<&'static str>),
    ExternalCommandFailed {
        program: PathBuf,
        status: Option<i32>,
    },
    InstanceAlreadyExists(PathBuf),
    InstanceNotFound(String),
}

impl fmt::Display for LswError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::InvalidInstanceName(name) => write!(
                formatter,
                "invalid instance name {name:?}; use 1-63 lowercase letters, digits, or hyphens"
            ),
            Self::InvalidValue { field, reason } => {
                write!(formatter, "invalid {field}: {reason}")
            }
            Self::InvalidManifest(reason) => {
                write!(formatter, "invalid instance manifest: {reason}")
            }
            Self::Protocol(reason) => write!(formatter, "protocol error: {reason}"),
            Self::MissingCapabilities(capabilities) => {
                write!(
                    formatter,
                    "missing host capabilities: {}",
                    capabilities.join(", ")
                )
            }
            Self::ExternalCommandFailed { program, status } => match status {
                Some(code) => write!(
                    formatter,
                    "external command {} exited with status {code}",
                    program.display()
                ),
                None => write!(
                    formatter,
                    "external command {} was terminated by a signal",
                    program.display()
                ),
            },
            Self::InstanceAlreadyExists(path) => {
                write!(formatter, "instance already exists at {}", path.display())
            }
            Self::InstanceNotFound(name) => write!(formatter, "instance {name:?} was not found"),
        }
    }
}

impl std::error::Error for LswError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for LswError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, LswError>;
