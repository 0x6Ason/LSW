// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::{LswError, Result, WindowsProfile};

pub const PROFILE_MANIFEST_VERSION: u32 = 2;
const MAX_PROFILE_MANIFEST_BYTES: usize = 64 * 1024;
const VANILLA_PROFILE: &str = include_str!("../profiles/vanilla-v2.json");
const SLIM_PROFILE: &str = include_str!("../profiles/slim-v2.json");

const REQUIRED_DEVELOPMENT_COMPONENTS: &[&str] = &[
    "Windows component store (WinSxS)",
    "Windows Update and servicing stack",
    "Windows Installer (MSI) and MSIX/AppX deployment",
    "PowerShell, Windows Terminal, and ConPTY dependencies",
    "Microsoft Store, App Installer (winget), and WebView2 runtime",
    "Windows Management Instrumentation (WMI)",
    ".NET Framework optional-feature support",
    "Microsoft Defender",
    "UAC and secure desktop",
    "hibernation support",
    "Windows Recovery Environment",
    "Windows SDK and Visual Studio Build Tools prerequisites",
    "Explorer, file dialogs, keyboard, IME, accessibility, clipboard, audio, and notifications",
    "Calculator, Notepad, Paint, Snipping Tool, and Photos",
    "Microsoft Edge",
    "SMB client and Windows networking",
];

const PROTECTED_APPX_NAMES: &[&str] = &[
    "Microsoft.DesktopAppInstaller",
    "Microsoft.Paint",
    "Microsoft.ScreenSketch",
    "Microsoft.Windows.Photos",
    "Microsoft.WindowsCalculator",
    "Microsoft.WindowsNotepad",
    "Microsoft.WindowsStore",
    "Microsoft.WindowsTerminal",
];

const PROTECTED_SERVICES: &[&str] = &[
    "AppXSvc",
    "BITS",
    "ClipSVC",
    "DPS",
    "EventLog",
    "InstallService",
    "msiserver",
    "ProfSvc",
    "Schedule",
    "StateRepository",
    "TrustedInstaller",
    "UsoSvc",
    "WaaSMedicSvc",
    "WinDefend",
    "Winmgmt",
    "wuauserv",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclarativeProfile {
    schema_version: u32,
    revision: String,
    name: String,
    experimental: bool,
    keeps_servicing: bool,
    compact_os: bool,
    appx_removals: Vec<AppxRemoval>,
    optional_feature_removals: Vec<String>,
    product_uninstallers: Vec<ProductUninstaller>,
    service_policies: Vec<ServicePolicy>,
    registry_policies: Vec<RegistryPolicy>,
    preserve_components: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppxRemoval {
    pub display_name: String,
    pub package_family_names: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum ProductUninstaller {
    OneDrive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceStartup {
    Disabled,
    Demand,
}

impl ServiceStartup {
    pub fn powershell_name(self) -> &'static str {
        match self {
            Self::Disabled => "Disabled",
            Self::Demand => "Manual",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServicePolicy {
    pub name: String,
    pub startup: ServiceStartup,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RegistryHive {
    Machine,
    DefaultUser,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RegistryPolicy {
    pub hive: RegistryHive,
    pub path: String,
    pub name: String,
    pub value: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomizationPlan {
    pub schema_version: u32,
    pub revision: String,
    pub profile: WindowsProfile,
    pub experimental: bool,
    pub appx_removals: Vec<AppxRemoval>,
    pub optional_feature_removals: Vec<String>,
    pub product_uninstallers: Vec<ProductUninstaller>,
    pub service_policies: Vec<ServicePolicy>,
    pub registry_policies: Vec<RegistryPolicy>,
    pub preserve_components: Vec<String>,
    pub compact_os: bool,
    pub warnings: Vec<String>,
}

impl CustomizationPlan {
    pub fn for_profile(profile: WindowsProfile) -> Result<Self> {
        let (manifest, expected_name, expected_revision) = match profile {
            WindowsProfile::Vanilla | WindowsProfile::Secure => {
                (VANILLA_PROFILE, "vanilla", "vanilla-v2")
            }
            WindowsProfile::Slim | WindowsProfile::Ephemeral => (SLIM_PROFILE, "slim", "slim-v2"),
        };
        let mut plan = Self::from_json(profile, expected_name, expected_revision, manifest)?;
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

    fn from_json(
        profile: WindowsProfile,
        expected_name: &str,
        expected_revision: &str,
        json: &str,
    ) -> Result<Self> {
        if json.len() > MAX_PROFILE_MANIFEST_BYTES {
            return Err(invalid_profile("manifest exceeds 64 KiB"));
        }
        let manifest: DeclarativeProfile =
            serde_json::from_str(json).map_err(|_| invalid_profile("manifest is invalid JSON"))?;
        validate_manifest(&manifest, expected_name, expected_revision)?;
        Ok(Self {
            schema_version: manifest.schema_version,
            revision: manifest.revision,
            profile,
            experimental: manifest.experimental,
            appx_removals: manifest.appx_removals,
            optional_feature_removals: manifest.optional_feature_removals,
            product_uninstallers: manifest.product_uninstallers,
            service_policies: manifest.service_policies,
            registry_policies: manifest.registry_policies,
            preserve_components: manifest.preserve_components,
            compact_os: manifest.compact_os,
            warnings: manifest.warnings,
        })
    }
}

fn validate_manifest(
    manifest: &DeclarativeProfile,
    expected_name: &str,
    expected_revision: &str,
) -> Result<()> {
    if manifest.schema_version != PROFILE_MANIFEST_VERSION {
        return Err(invalid_profile(&format!(
            "unsupported schema version {}; expected {PROFILE_MANIFEST_VERSION}",
            manifest.schema_version
        )));
    }
    if manifest.name != expected_name || manifest.revision != expected_revision {
        return Err(invalid_profile(
            "profile name or revision does not match its selector",
        ));
    }
    if manifest.experimental {
        return Err(invalid_profile(
            "experimental profiles are not enabled in this release line",
        ));
    }
    if !manifest.keeps_servicing {
        return Err(invalid_profile(
            "current profiles must preserve Windows servicing",
        ));
    }
    validate_preservation_contract(manifest)?;
    validate_appx_removals(&manifest.appx_removals)?;
    validate_optional_features(&manifest.optional_feature_removals)?;
    validate_services(&manifest.service_policies)?;
    validate_registry_policies(&manifest.registry_policies)?;

    let mut uninstallers = BTreeSet::new();
    for uninstaller in &manifest.product_uninstallers {
        if !uninstallers.insert(*uninstaller) {
            return Err(invalid_profile("product uninstallers must be unique"));
        }
    }

    if expected_name == "vanilla"
        && (manifest.compact_os
            || !manifest.appx_removals.is_empty()
            || !manifest.optional_feature_removals.is_empty()
            || !manifest.product_uninstallers.is_empty()
            || !manifest.service_policies.is_empty()
            || !manifest.registry_policies.is_empty())
    {
        return Err(invalid_profile("vanilla must not mutate the Windows image"));
    }
    Ok(())
}

fn validate_preservation_contract(manifest: &DeclarativeProfile) -> Result<()> {
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
    Ok(())
}

fn validate_appx_removals(removals: &[AppxRemoval]) -> Result<()> {
    let mut display_names = BTreeSet::new();
    let mut family_names = BTreeSet::new();
    for removal in removals {
        validate_identifier(&removal.display_name, "AppX display name")?;
        if PROTECTED_APPX_NAMES
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&removal.display_name))
        {
            return Err(invalid_profile("a protected development AppX was targeted"));
        }
        if !display_names.insert(removal.display_name.to_ascii_lowercase()) {
            return Err(invalid_profile("AppX display names must be unique"));
        }
        if removal.package_family_names.is_empty() {
            return Err(invalid_profile(
                "each AppX removal requires an exact package-family identity",
            ));
        }
        for family in &removal.package_family_names {
            validate_identifier(family, "AppX package family")?;
            if !family_names.insert(family.to_ascii_lowercase()) {
                return Err(invalid_profile("AppX package families must be unique"));
            }
        }
    }
    Ok(())
}

fn validate_optional_features(features: &[String]) -> Result<()> {
    let mut unique = BTreeSet::new();
    for feature in features {
        validate_identifier(feature, "optional feature")?;
        if feature != "Recall" {
            return Err(invalid_profile(
                "only the separately removable Recall feature is approved",
            ));
        }
        if !unique.insert(feature.to_ascii_lowercase()) {
            return Err(invalid_profile("optional features must be unique"));
        }
    }
    Ok(())
}

fn validate_services(services: &[ServicePolicy]) -> Result<()> {
    let mut unique = BTreeSet::new();
    for service in services {
        validate_identifier(&service.name, "service name")?;
        if PROTECTED_SERVICES
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&service.name))
        {
            return Err(invalid_profile(
                "a protected development service was targeted",
            ));
        }
        if service.name.eq_ignore_ascii_case("DoSvc") && service.startup != ServiceStartup::Demand {
            return Err(invalid_profile(
                "Delivery Optimization must remain demand-start capable",
            ));
        }
        if !unique.insert(service.name.to_ascii_lowercase()) {
            return Err(invalid_profile("service policies must be unique"));
        }
    }
    Ok(())
}

fn validate_registry_policies(policies: &[RegistryPolicy]) -> Result<()> {
    let mut unique = BTreeSet::new();
    for policy in policies {
        validate_registry_path(&policy.path)?;
        validate_registry_value_name(&policy.name)?;
        let identity =
            format!("{:?}\\{}\\{}", policy.hive, policy.path, policy.name).to_ascii_lowercase();
        if !unique.insert(identity) {
            return Err(invalid_profile("registry policies must be unique"));
        }
    }
    Ok(())
}

fn validate_identifier(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(invalid_profile(&format!(
            "{field} may contain only ASCII letters, digits, dot, dash, or underscore"
        )));
    }
    Ok(())
}

fn validate_registry_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('\\')
        || value.ends_with('\\')
        || value.contains("\\\\")
        || value.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'\\' | b' ' | b'.' | b'-' | b'_'))
        })
    {
        return Err(invalid_profile("registry policy path is invalid"));
    }
    Ok(())
}

fn validate_registry_value_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'.' | b'-' | b'_'))
        })
    {
        return Err(invalid_profile("registry policy value name is invalid"));
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
    fn slim_profile_is_typed_aggressive_and_preserves_development() {
        let plan = CustomizationPlan::for_profile(WindowsProfile::Slim)
            .expect("embedded slim profile should validate");
        assert_eq!(plan.schema_version, PROFILE_MANIFEST_VERSION);
        assert_eq!(plan.revision, "slim-v2");
        assert!(!plan.experimental);
        assert!(plan.appx_removals.len() >= 40);
        assert_eq!(plan.optional_feature_removals, ["Recall"]);
        assert_eq!(plan.product_uninstallers, [ProductUninstaller::OneDrive]);
        assert!(plan.service_policies.len() >= 10);
        assert!(plan.registry_policies.len() >= 20);
        for required in [
            "servicing",
            "Defender",
            "Store",
            "winget",
            "WebView2",
            "WMI",
            "UAC",
            "hibernation",
            "Recovery",
            "Explorer",
            "Notepad",
            "SMB",
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
    fn vanilla_profile_is_a_zero_mutation_contract() {
        let plan = CustomizationPlan::for_profile(WindowsProfile::Vanilla)
            .expect("embedded vanilla profile should validate");
        assert_eq!(plan.revision, "vanilla-v2");
        assert!(plan.appx_removals.is_empty());
        assert!(plan.optional_feature_removals.is_empty());
        assert!(plan.product_uninstallers.is_empty());
        assert!(plan.service_policies.is_empty());
        assert!(plan.registry_policies.is_empty());
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
        assert!(CustomizationPlan::from_json(
            WindowsProfile::Vanilla,
            "vanilla",
            "vanilla-v2",
            &unknown
        )
        .is_err());

        let mut missing: serde_json::Value =
            serde_json::from_str(SLIM_PROFILE).expect("embedded profile should be valid JSON");
        missing["preserve_components"]
            .as_array_mut()
            .expect("preservation contract should be an array")
            .retain(|component| component.as_str() != Some("Microsoft Defender"));
        let missing = serde_json::to_string(&missing).expect("profile should serialize");
        assert!(
            CustomizationPlan::from_json(WindowsProfile::Slim, "slim", "slim-v2", &missing)
                .is_err()
        );
    }

    #[test]
    fn protected_components_cannot_be_removed_or_disabled() {
        let mut profile: serde_json::Value =
            serde_json::from_str(SLIM_PROFILE).expect("embedded profile should be valid JSON");
        profile["appx_removals"]
            .as_array_mut()
            .expect("removals should be an array")
            .push(serde_json::json!({
                "display_name": "Microsoft.WindowsStore",
                "package_family_names": ["Microsoft.WindowsStore_8wekyb3d8bbwe"]
            }));
        let profile = serde_json::to_string(&profile).expect("profile should serialize");
        assert!(
            CustomizationPlan::from_json(WindowsProfile::Slim, "slim", "slim-v2", &profile)
                .is_err()
        );

        let mut profile: serde_json::Value =
            serde_json::from_str(SLIM_PROFILE).expect("embedded profile should be valid JSON");
        profile["service_policies"]
            .as_array_mut()
            .expect("services should be an array")
            .push(serde_json::json!({"name": "wuauserv", "startup": "disabled"}));
        let profile = serde_json::to_string(&profile).expect("profile should serialize");
        assert!(
            CustomizationPlan::from_json(WindowsProfile::Slim, "slim", "slim-v2", &profile)
                .is_err()
        );
    }
}
