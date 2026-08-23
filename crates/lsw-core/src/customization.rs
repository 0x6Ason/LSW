// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::{LswError, Result, WindowsProfile};

pub const PROFILE_MANIFEST_VERSION: u32 = 1;
const MAX_PROFILE_MANIFEST_BYTES: usize = 64 * 1024;
const VANILLA_PROFILE: &str = include_str!("../profiles/vanilla-v1.json");
const SLIM_PROFILE: &str = include_str!("../profiles/slim-v1.json");

const REQUIRED_DEVELOPMENT_COMPONENTS: &[&str] = &[
    "Windows component store (WinSxS)",
    "Windows Update and servicing stack",
    "Windows Installer (MSI) and MSIX/AppX deployment",
    "PowerShell, Windows Terminal, and ConPTY dependencies",
    "Microsoft Store, App Installer (winget), and WebView2 runtime",
    "Windows Management Instrumentation (WMI)",
    ".NET Framework optional-feature support",
    "Microsoft Defender",
    "hibernation support",
    "Windows Recovery Environment",
    "Windows SDK and Visual Studio Build Tools prerequisites",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclarativeProfile {
    schema_version: u32,
    name: String,
    experimental: bool,
    keeps_servicing: bool,
    compact_os: bool,
    remove_provisioned_appx_patterns: Vec<String>,
    preserve_components: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomizationPlan {
    pub schema_version: u32,
    pub profile: WindowsProfile,
    pub experimental: bool,
    pub remove_provisioned_appx_patterns: Vec<String>,
    pub preserve_components: Vec<String>,
    pub compact_os: bool,
    pub warnings: Vec<String>,
}

impl CustomizationPlan {
    pub fn for_profile(profile: WindowsProfile) -> Result<Self> {
        let (manifest, expected_name) = match profile {
            WindowsProfile::Vanilla | WindowsProfile::Secure => (VANILLA_PROFILE, "vanilla"),
            WindowsProfile::Slim | WindowsProfile::Ephemeral => (SLIM_PROFILE, "slim"),
        };
        let mut plan = Self::from_json(profile, expected_name, manifest)?;
        if profile == WindowsProfile::Ephemeral {
            plan.warnings.push(
                "legacy ephemeral manifest: runtime writes use a disposable qcow2 overlay"
                    .to_owned(),
            );
        } else if profile == WindowsProfile::Secure {
            plan.warnings.push(
                "legacy secure manifest: requires production-key-enrolled OVMF variables"
                    .to_owned(),
            );
        }
        Ok(plan)
    }

    fn from_json(profile: WindowsProfile, expected_name: &str, json: &str) -> Result<Self> {
        if json.len() > MAX_PROFILE_MANIFEST_BYTES {
            return Err(invalid_profile("manifest exceeds 64 KiB"));
        }
        let manifest: DeclarativeProfile =
            serde_json::from_str(json).map_err(|_| invalid_profile("manifest is invalid JSON"))?;
        validate_manifest(&manifest, expected_name)?;
        Ok(Self {
            schema_version: manifest.schema_version,
            profile,
            experimental: manifest.experimental,
            remove_provisioned_appx_patterns: manifest.remove_provisioned_appx_patterns,
            preserve_components: manifest.preserve_components,
            compact_os: manifest.compact_os,
            warnings: manifest.warnings,
        })
    }
}

fn validate_manifest(manifest: &DeclarativeProfile, expected_name: &str) -> Result<()> {
    if manifest.schema_version != PROFILE_MANIFEST_VERSION {
        return Err(invalid_profile(&format!(
            "unsupported schema version {}; expected {PROFILE_MANIFEST_VERSION}",
            manifest.schema_version
        )));
    }
    if manifest.name != expected_name {
        return Err(invalid_profile("profile name does not match its selector"));
    }
    if manifest.experimental {
        return Err(invalid_profile(
            "experimental profiles are not enabled in beta.7",
        ));
    }
    if !manifest.keeps_servicing {
        return Err(invalid_profile(
            "beta.7 profiles must preserve Windows servicing",
        ));
    }

    let preserved = manifest
        .preserve_components
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for required in REQUIRED_DEVELOPMENT_COMPONENTS {
        if !preserved.contains(required) {
            return Err(invalid_profile(&format!(
                "required preservation contract is missing {required:?}"
            )));
        }
    }

    let mut patterns = BTreeSet::new();
    for pattern in &manifest.remove_provisioned_appx_patterns {
        if pattern.is_empty()
            || pattern.len() > 128
            || !pattern
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(invalid_profile(
                "AppX patterns may contain only ASCII letters, digits, dot, dash, or underscore",
            ));
        }
        if !patterns.insert(pattern) {
            return Err(invalid_profile("AppX removal patterns must be unique"));
        }
    }
    if expected_name == "vanilla"
        && (!manifest.remove_provisioned_appx_patterns.is_empty() || manifest.compact_os)
    {
        return Err(invalid_profile(
            "vanilla must not remove AppX packages or enable CompactOS",
        ));
    }
    Ok(())
}

fn invalid_profile(reason: &str) -> LswError {
    LswError::InvalidValue {
        field: "Windows profile manifest",
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slim_profile_keeps_the_complete_development_contract() {
        let plan = CustomizationPlan::for_profile(WindowsProfile::Slim)
            .expect("embedded slim profile should validate");
        assert_eq!(plan.schema_version, PROFILE_MANIFEST_VERSION);
        assert!(!plan.experimental);
        for required in [
            "servicing",
            "Defender",
            "Store",
            "winget",
            "WebView2",
            "WMI",
            "hibernation",
            "Recovery",
        ] {
            assert!(
                plan.preserve_components
                    .iter()
                    .any(|component| component.contains(required)),
                "slim must explicitly preserve {required}"
            );
        }
    }

    #[test]
    fn vanilla_profile_does_not_remove_packages() {
        let plan = CustomizationPlan::for_profile(WindowsProfile::Vanilla)
            .expect("embedded vanilla profile should validate");
        assert!(plan.remove_provisioned_appx_patterns.is_empty());
        assert!(!plan.compact_os);
    }

    #[test]
    fn manifest_rejects_unknown_fields_and_missing_preservation_contracts() {
        let mut unknown: serde_json::Value =
            serde_json::from_str(VANILLA_PROFILE).expect("embedded profile should be valid JSON");
        unknown
            .as_object_mut()
            .expect("profile should be an object")
            .insert(
                "command".to_owned(),
                serde_json::Value::String("Remove-WindowsPackage".to_owned()),
            );
        let unknown = serde_json::to_string(&unknown).expect("profile should serialize");
        assert!(
            CustomizationPlan::from_json(WindowsProfile::Vanilla, "vanilla", &unknown).is_err()
        );

        let mut missing: serde_json::Value =
            serde_json::from_str(SLIM_PROFILE).expect("embedded profile should be valid JSON");
        missing["preserve_components"]
            .as_array_mut()
            .expect("preservation contract should be an array")
            .retain(|component| component.as_str() != Some("Microsoft Defender"));
        let missing = serde_json::to_string(&missing).expect("profile should serialize");
        assert!(CustomizationPlan::from_json(WindowsProfile::Slim, "slim", &missing).is_err());
    }
}
