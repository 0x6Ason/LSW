// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;
use std::str::FromStr;

use crate::{LswError, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowsProfile {
    Vanilla,
    Slim,
    // Retained so existing beta manifests keep their runtime semantics. New
    // beta.5 instances expose only vanilla and slim.
    #[doc(hidden)]
    Ephemeral,
    #[doc(hidden)]
    Secure,
}

impl WindowsProfile {
    pub(crate) fn parse_manifest(value: &str) -> Result<Self> {
        match value {
            "standard" => Ok(Self::Vanilla),
            "ephemeral" => Ok(Self::Ephemeral),
            "secure" => Ok(Self::Secure),
            _ => value.parse(),
        }
    }

    pub fn security(self) -> SecuritySettings {
        match self {
            Self::Secure => SecuritySettings {
                uefi: true,
                secure_boot: true,
                vtpm: true,
                test_signing_allowed: false,
                custom_driver_allowed: false,
            },
            Self::Vanilla | Self::Slim | Self::Ephemeral => SecuritySettings {
                uefi: true,
                secure_boot: false,
                vtpm: true,
                test_signing_allowed: true,
                custom_driver_allowed: true,
            },
        }
    }

    pub fn keeps_servicing(self) -> bool {
        true
    }

    pub fn default_disk_gib(self) -> u32 {
        64
    }
}

impl fmt::Display for WindowsProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Vanilla => "vanilla",
            Self::Slim => "slim",
            Self::Ephemeral => "ephemeral",
            Self::Secure => "secure",
        };
        formatter.write_str(value)
    }
}

impl FromStr for WindowsProfile {
    type Err = LswError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "vanilla" => Ok(Self::Vanilla),
            "slim" => Ok(Self::Slim),
            _ => Err(LswError::InvalidValue {
                field: "profile",
                reason: format!("unknown profile {value:?}; beta.5 supports vanilla or slim"),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecuritySettings {
    pub uefi: bool,
    pub secure_boot: bool,
    pub vtpm: bool,
    pub test_signing_allowed: bool,
    pub custom_driver_allowed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_secure_profile_never_enables_test_signing() {
        let security = WindowsProfile::Secure.security();
        assert!(security.secure_boot);
        assert!(!security.test_signing_allowed);
        assert!(!security.custom_driver_allowed);
    }

    #[test]
    fn every_beta_profile_preserves_servicing() {
        assert!(WindowsProfile::Vanilla.keeps_servicing());
        assert!(WindowsProfile::Slim.keeps_servicing());
    }

    #[test]
    fn standard_manifest_alias_migrates_to_vanilla() {
        assert_eq!(
            WindowsProfile::parse_manifest("standard").expect("alias should parse"),
            WindowsProfile::Vanilla
        );
        assert!("standard".parse::<WindowsProfile>().is_err());
        assert!("ephemeral".parse::<WindowsProfile>().is_err());
        assert_eq!(WindowsProfile::Vanilla.to_string(), "vanilla");
    }
}
