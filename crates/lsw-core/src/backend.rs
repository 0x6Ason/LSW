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

/// How aggressively the guest CPU model is tuned for a Windows guest.
///
/// Windows polls its hypervisor through the Hyper-V paravirtual interface and
/// falls back to expensive emulated timers, APIC accesses, and IPIs when that
/// interface is absent. KVM implements the same interface, so exposing it is the
/// single largest Windows-on-KVM performance win and is what libvirt does by
/// default for Windows guests. `Baseline` keeps the plain passthrough model for
/// kernels or QEMU builds that reject an enlightenment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GuestCpuTuning {
    #[default]
    Enlightened,
    Baseline,
}

impl GuestCpuTuning {
    /// Parses the `LSW_GUEST_CPU_TUNING` setting.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "enlightened" | "default" => Ok(Self::Enlightened),
            "baseline" | "compat" => Ok(Self::Baseline),
            other => Err(LswError::InvalidValue {
                field: "guest CPU tuning",
                reason: format!("{other:?} is not one of enlightened or baseline"),
            }),
        }
    }
}

impl fmt::Display for GuestCpuTuning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Enlightened => "enlightened",
            Self::Baseline => "baseline",
        })
    }
}

/// Host passthrough plus the Hyper-V enlightenments that every KVM kernel since
/// Linux 5.0 and every QEMU since 4.0 implement.
///
/// - `migratable=off` exposes the invariant TSC so Windows uses it as a stable
///   clock source instead of emulated timers. LSW never live-migrates a guest.
/// - `hv-relaxed`, `hv-vapic`, `hv-spinlocks` remove timer-check, APIC, and
///   spinlock exits on the hot path.
/// - `hv-vpindex`, `hv-runtime`, `hv-time`, `hv-synic`, `hv-stimer`,
///   `hv-stimer-direct` give Windows paravirtual clocks and timers.
/// - `hv-reset`, `hv-frequencies`, `hv-reenlightenment` keep the TSC-based clock
///   correct across guest resets and host frequency changes.
/// - `hv-tlbflush`, `hv-ipi` turn multi-CPU TLB shootdowns and IPIs into single
///   hypercalls, which matters most for the `-smp` counts a developer VM uses.
///
/// Intel-only `hv-evmcs` and the newer `hv-avic` are deliberately omitted so
/// one model works on both vendors and on older stable distributions.
pub const KVM_ENLIGHTENED_CPU_MODEL: &str = "host,migratable=off,\
hv-relaxed,hv-vapic,hv-spinlocks=0x1fff,hv-vpindex,hv-runtime,hv-time,hv-synic,\
hv-stimer,hv-stimer-direct,hv-reset,hv-frequencies,hv-reenlightenment,hv-tlbflush,hv-ipi";

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
            VmAccelerator::Kvm => &["-enable-kvm"],
            VmAccelerator::Hvf => &["-accel", "hvf"],
            VmAccelerator::Whpx => &["-accel", "whpx"],
            VmAccelerator::Tcg => &["-accel", "tcg,thread=multi"],
        }
    }

    /// The `-cpu` model for this backend.
    ///
    /// Under KVM the host CPU is passed through with the Hyper-V enlightenments
    /// Windows expects from its own hypervisor; see [`GuestCpuTuning`].
    pub(crate) const fn cpu_model(self, tuning: GuestCpuTuning) -> &'static str {
        match (self.accelerator, tuning) {
            (VmAccelerator::Kvm, GuestCpuTuning::Enlightened) => KVM_ENLIGHTENED_CPU_MODEL,
            (VmAccelerator::Kvm | VmAccelerator::Hvf, GuestCpuTuning::Baseline)
            | (VmAccelerator::Hvf, GuestCpuTuning::Enlightened) => "host",
            (VmAccelerator::Whpx | VmAccelerator::Tcg, _) => "max",
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
        let kvm_backend = QemuBackend::select(&kvm);
        assert_eq!(kvm_backend.acceleration_arguments(), ["-enable-kvm"]);
        assert_eq!(
            kvm_backend.cpu_model(GuestCpuTuning::Enlightened),
            KVM_ENLIGHTENED_CPU_MODEL
        );
        assert_eq!(kvm_backend.cpu_model(GuestCpuTuning::Baseline), "host");

        let tcg = capabilities(HostPlatform::Linux, AcceleratorCapabilities::none());
        let tcg_backend = QemuBackend::select(&tcg);
        assert_eq!(
            tcg_backend.acceleration_arguments(),
            ["-accel", "tcg,thread=multi"]
        );
        for tuning in [GuestCpuTuning::Enlightened, GuestCpuTuning::Baseline] {
            assert_eq!(tcg_backend.cpu_model(tuning), "max");
        }
    }

    #[test]
    fn enlightened_model_is_a_single_cpu_option_with_satisfied_dependencies() {
        assert!(!KVM_ENLIGHTENED_CPU_MODEL.contains(char::is_whitespace));
        assert!(KVM_ENLIGHTENED_CPU_MODEL.starts_with("host,migratable=off,"));
        let features: Vec<&str> = KVM_ENLIGHTENED_CPU_MODEL.split(',').collect();
        // QEMU rejects a model whose Hyper-V features lack their prerequisites.
        for (feature, requirement) in [
            ("hv-synic", "hv-vpindex"),
            ("hv-stimer", "hv-synic"),
            ("hv-stimer", "hv-time"),
            ("hv-stimer-direct", "hv-stimer"),
            ("hv-tlbflush", "hv-vpindex"),
            ("hv-ipi", "hv-vpindex"),
        ] {
            assert!(features.contains(&feature), "{feature} is missing");
            assert!(
                features.contains(&requirement),
                "{feature} requires {requirement}"
            );
        }
        assert!(!features.contains(&"hv-evmcs"), "hv-evmcs is Intel only");
        assert!(!features.contains(&"hv-passthrough"));
    }

    #[test]
    fn cpu_tuning_parses_documented_spellings_only() {
        assert_eq!(
            GuestCpuTuning::parse(" Enlightened ").expect("documented spelling"),
            GuestCpuTuning::Enlightened
        );
        assert_eq!(
            GuestCpuTuning::parse("baseline").expect("documented spelling"),
            GuestCpuTuning::Baseline
        );
        assert!(matches!(
            GuestCpuTuning::parse("fast"),
            Err(LswError::InvalidValue {
                field: "guest CPU tuning",
                ..
            })
        ));
        assert_eq!(GuestCpuTuning::default(), GuestCpuTuning::Enlightened);
    }

    #[test]
    fn future_native_accelerators_have_explicit_qemu_arguments() {
        for (platform, accelerator, expected_accel, expected_cpu) in [
            (
                HostPlatform::MacOs,
                VmAccelerator::Hvf,
                ["-accel", "hvf"],
                "host",
            ),
            (
                HostPlatform::Windows,
                VmAccelerator::Whpx,
                ["-accel", "whpx"],
                "max",
            ),
        ] {
            let capabilities = capabilities(
                platform,
                AcceleratorCapabilities::none().with_available(accelerator),
            );
            let backend = QemuBackend::select(&capabilities);
            assert_eq!(backend.acceleration_arguments(), expected_accel);
            assert_eq!(backend.cpu_model(GuestCpuTuning::Enlightened), expected_cpu);
        }
    }
}
