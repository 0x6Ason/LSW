// SPDX-License-Identifier: GPL-3.0-or-later

use crate::WindowsProfile;

const SLIM_APPX_PATTERNS: &[&str] = &[
    "Clipchamp.Clipchamp",
    "Microsoft.BingNews",
    "Microsoft.BingWeather",
    "Microsoft.GamingApp",
    "Microsoft.GetHelp",
    "Microsoft.Getstarted",
    "Microsoft.MicrosoftSolitaireCollection",
    "Microsoft.People",
    "Microsoft.PowerAutomateDesktop",
    "Microsoft.Todos",
    "Microsoft.WindowsAlarms",
    "Microsoft.WindowsFeedbackHub",
    "Microsoft.WindowsMaps",
    "Microsoft.WindowsSoundRecorder",
    "Microsoft.YourPhone",
    "Microsoft.ZuneMusic",
    "MSTeams",
];

const DEVELOPMENT_COMPONENTS: &[&str] = &[
    "Windows component store (WinSxS)",
    "Windows Update and servicing stack",
    "Windows Installer (MSI) and MSIX/AppX deployment",
    "PowerShell and Windows Terminal dependencies",
    ".NET Framework optional-feature support",
    "Microsoft Defender",
    "Windows Recovery Environment",
    "Windows SDK and Visual Studio Build Tools prerequisites",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomizationPlan {
    pub profile: WindowsProfile,
    pub remove_provisioned_appx_patterns: &'static [&'static str],
    pub preserve_components: &'static [&'static str],
    pub compact_os: bool,
    pub warnings: &'static [&'static str],
}

impl CustomizationPlan {
    pub fn for_profile(profile: WindowsProfile) -> Self {
        match profile {
            WindowsProfile::Standard | WindowsProfile::Secure => Self {
                profile,
                remove_provisioned_appx_patterns: &[],
                preserve_components: DEVELOPMENT_COMPONENTS,
                compact_os: false,
                warnings: &[],
            },
            WindowsProfile::Slim => Self {
                profile,
                remove_provisioned_appx_patterns: SLIM_APPX_PATTERNS,
                preserve_components: DEVELOPMENT_COMPONENTS,
                compact_os: true,
                warnings: &[
                    "package names vary by Windows build; resolve installed packages dynamically before invoking DISM",
                    "the recipe must run locally against user-supplied media and must not publish the resulting image",
                ],
            },
            WindowsProfile::Ephemeral => Self {
                profile,
                remove_provisioned_appx_patterns: SLIM_APPX_PATTERNS,
                preserve_components: DEVELOPMENT_COMPONENTS,
                compact_os: true,
                warnings: &[
                    "ephemeral uses the same conservative package recipe as slim",
                    "runtime writes are discarded with the per-run qcow2 overlay",
                    "the component store and Windows servicing remain intact",
                ],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slim_profile_keeps_servicing_and_defender() {
        let plan = CustomizationPlan::for_profile(WindowsProfile::Slim);
        assert!(plan
            .preserve_components
            .iter()
            .any(|component| component.contains("servicing")));
        assert!(plan
            .preserve_components
            .iter()
            .any(|component| component.contains("Defender")));
    }

    #[test]
    fn standard_profile_does_not_remove_packages() {
        let plan = CustomizationPlan::for_profile(WindowsProfile::Standard);
        assert!(plan.remove_provisioned_appx_patterns.is_empty());
    }
}
