// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;

use crate::{HostCapabilities, LswError, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostPlatform {
    Linux,
    MacOs,
    Windows,
    Other,
}

impl HostPlatform {
    pub const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }

    pub const fn native_accelerator(self) -> Option<VmAccelerator> {
        match self {
            Self::Linux => Some(VmAccelerator::Kvm),
            Self::MacOs => Some(VmAccelerator::Hvf),
            Self::Windows => Some(VmAccelerator::Whpx),
            Self::Other => None,
        }
    }
}

impl fmt::Display for HostPlatform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::Windows => "windows",
            Self::Other => "other",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmAccelerator {
    Kvm,
    Hvf,
    Whpx,
    Tcg,
}

impl VmAccelerator {
    pub const fn capability_name(self) -> &'static str {
        match self {
            Self::Kvm => "KVM acceleration",
            Self::Hvf => "Hypervisor.framework acceleration",
            Self::Whpx => "Windows Hypervisor Platform acceleration",
            Self::Tcg => "QEMU TCG acceleration",
        }
    }
}

impl fmt::Display for VmAccelerator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Kvm => "kvm",
            Self::Hvf => "hvf",
            Self::Whpx => "whpx",
            Self::Tcg => "tcg",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AcceleratorCapabilities {
    kvm: bool,
    hvf: bool,
    whpx: bool,
}

impl AcceleratorCapabilities {
    pub const fn none() -> Self {
        Self {
            kvm: false,
            hvf: false,
            whpx: false,
        }
    }

    pub const fn with_available(mut self, accelerator: VmAccelerator) -> Self {
        match accelerator {
            VmAccelerator::Kvm => self.kvm = true,
            VmAccelerator::Hvf => self.hvf = true,
            VmAccelerator::Whpx => self.whpx = true,
            // TCG is QEMU's portable fallback rather than a host hypervisor
            // capability, so it is always selectable.
            VmAccelerator::Tcg => {}
        }
        self
    }

    pub const fn supports(self, accelerator: VmAccelerator) -> bool {
        match accelerator {
            VmAccelerator::Kvm => self.kvm,
            VmAccelerator::Hvf => self.hvf,
            VmAccelerator::Whpx => self.whpx,
            VmAccelerator::Tcg => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QemuBackend {
    platform: HostPlatform,
    accelerator: VmAccelerator,
}

impl QemuBackend {
    pub fn select(capabilities: &HostCapabilities) -> Self {
        let native = capabilities.platform.native_accelerator();
        let accelerator = native
            .filter(|accelerator| capabilities.accelerators.supports(*accelerator))
            .unwrap_or(VmAccelerator::Tcg);
        Self {
            platform: capabilities.platform,
            accelerator,
        }
    }

    pub fn require(capabilities: &HostCapabilities, accelerator: VmAccelerator) -> Result<Self> {
        validate_platform_accelerator(capabilities.platform, accelerator)?;
        if !capabilities.accelerators.supports(accelerator) {
            return Err(LswError::MissingCapabilities(vec![
                accelerator.capability_name()
            ]));
        }
        Ok(Self {
            platform: capabilities.platform,
            accelerator,
        })
    }

    pub const fn platform(self) -> HostPlatform {
        self.platform
    }

    pub const fn accelerator(self) -> VmAccelerator {
        self.accelerator
    }

    pub(crate) const fn acceleration_arguments(self) -> &'static [&'static str] {
        match self.accelerator {
            // Keep the beta's established KVM spelling for stable dry-run output.
            VmAccelerator::Kvm => &["-enable-kvm", "-cpu", "host"],
            VmAccelerator::Hvf => &["-accel", "hvf", "-cpu", "host"],
            VmAccelerator::Whpx => &["-accel", "whpx", "-cpu", "max"],
            VmAccelerator::Tcg => &["-accel", "tcg,thread=multi", "-cpu", "max"],
        }
    }

    pub(crate) fn fallback_note(self) -> Option<String> {
        if self.accelerator != VmAccelerator::Tcg {
            return None;
        }
        Some(match self.platform.native_accelerator() {
            Some(VmAccelerator::Kvm) => {
                "KVM is unavailable; QEMU will fall back to TCG and run much more slowly".to_owned()
            }
            Some(accelerator) => format!(
                "{accelerator} is unavailable; QEMU will fall back to TCG and run much more slowly"
            ),
            None => {
                "no native accelerator is defined for this host; QEMU will use slow TCG".to_owned()
            }
        })
    }

    pub(crate) fn validate(self, capabilities: &HostCapabilities) -> Result<()> {
        if self.platform != capabilities.platform {
            return Err(LswError::InvalidValue {
                field: "virtualization backend",
                reason: format!(
                    "selection targets {} but detected host is {}",
                    self.platform, capabilities.platform
                ),
            });
        }
        validate_platform_accelerator(self.platform, self.accelerator)?;
        if !capabilities.accelerators.supports(self.accelerator) {
            return Err(LswError::MissingCapabilities(vec![self
                .accelerator
                .capability_name()]));
        }
        Ok(())
    }
}

fn validate_platform_accelerator(platform: HostPlatform, accelerator: VmAccelerator) -> Result<()> {
    if accelerator == VmAccelerator::Tcg
        || platform
            .native_accelerator()
            .is_some_and(|native| native == accelerator)
    {
        return Ok(());
    }
    Err(LswError::InvalidValue {
        field: "virtualization accelerator",
        reason: format!("{accelerator} is not a native accelerator for {platform}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(
        platform: HostPlatform,
        accelerators: AcceleratorCapabilities,
    ) -> HostCapabilities {
        let mut capabilities = HostCapabilities::unavailable(platform);
        capabilities.accelerators = accelerators;
        capabilities
    }

    #[test]
    fn auto_selection_uses_each_platforms_native_accelerator() {
        for (platform, accelerator) in [
            (HostPlatform::Linux, VmAccelerator::Kvm),
            (HostPlatform::MacOs, VmAccelerator::Hvf),
            (HostPlatform::Windows, VmAccelerator::Whpx),
        ] {
            let capabilities = capabilities(
                platform,
                AcceleratorCapabilities::none().with_available(accelerator),
            );
            assert_eq!(
                QemuBackend::select(&capabilities).accelerator(),
                accelerator
            );
        }
    }

    #[test]
    fn auto_selection_falls_back_to_tcg_without_native_acceleration() {
        let capabilities = capabilities(HostPlatform::Linux, AcceleratorCapabilities::none());
        let backend = QemuBackend::select(&capabilities);
        assert_eq!(backend.accelerator(), VmAccelerator::Tcg);
        assert!(backend
            .fallback_note()
            .expect("TCG should explain its fallback")
            .contains("KVM is unavailable"));
    }

    #[test]
    fn explicit_selection_rejects_an_unavailable_accelerator() {
        let capabilities = capabilities(HostPlatform::MacOs, AcceleratorCapabilities::none());
        assert!(matches!(
            QemuBackend::require(&capabilities, VmAccelerator::Hvf),
            Err(LswError::MissingCapabilities(_))
        ));
        assert_eq!(
            QemuBackend::require(&capabilities, VmAccelerator::Tcg)
                .expect("TCG should always be selectable")
                .accelerator(),
            VmAccelerator::Tcg
        );
    }

    #[test]
    fn explicit_selection_rejects_an_accelerator_for_another_platform() {
        let capabilities = capabilities(
            HostPlatform::Linux,
            AcceleratorCapabilities::none().with_available(VmAccelerator::Hvf),
        );
        assert!(matches!(
            QemuBackend::require(&capabilities, VmAccelerator::Hvf),
            Err(LswError::InvalidValue {
                field: "virtualization accelerator",
                ..
            })
        ));
    }

    #[test]
    fn accelerator_arguments_preserve_kvm_and_tcg_beta_behavior() {
        let kvm = capabilities(
            HostPlatform::Linux,
            AcceleratorCapabilities::none().with_available(VmAccelerator::Kvm),
        );
        assert_eq!(
            QemuBackend::select(&kvm).acceleration_arguments(),
            ["-enable-kvm", "-cpu", "host"]
        );

        let tcg = capabilities(HostPlatform::Linux, AcceleratorCapabilities::none());
        assert_eq!(
            QemuBackend::select(&tcg).acceleration_arguments(),
            ["-accel", "tcg,thread=multi", "-cpu", "max"]
        );
    }

    #[test]
    fn future_native_accelerators_have_explicit_qemu_arguments() {
        for (platform, accelerator, expected) in [
            (
                HostPlatform::MacOs,
                VmAccelerator::Hvf,
                ["-accel", "hvf", "-cpu", "host"],
            ),
            (
                HostPlatform::Windows,
                VmAccelerator::Whpx,
                ["-accel", "whpx", "-cpu", "max"],
            ),
        ] {
            let capabilities = capabilities(
                platform,
                AcceleratorCapabilities::none().with_available(accelerator),
            );
            assert_eq!(
                QemuBackend::select(&capabilities).acceleration_arguments(),
                expected
            );
        }
    }
}
