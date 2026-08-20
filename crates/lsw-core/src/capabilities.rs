// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::path::PathBuf;

use crate::{AcceleratorCapabilities, HostPlatform, VmAccelerator, WindowsProfile};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCapabilities {
    pub platform: HostPlatform,
    pub accelerators: AcceleratorCapabilities,
    pub qemu_system: Option<PathBuf>,
    pub qemu_img: Option<PathBuf>,
    pub aria2c: Option<PathBuf>,
    pub swtpm: Option<PathBuf>,
    pub wimlib_imagex: Option<PathBuf>,
    pub xorriso: Option<PathBuf>,
    pub remote_viewer: Option<PathBuf>,
    pub ovmf_code: Option<PathBuf>,
    pub ovmf_vars: Option<PathBuf>,
    pub ovmf_secure_code: Option<PathBuf>,
    pub ovmf_secure_vars: Option<PathBuf>,
}

impl HostCapabilities {
    pub fn detect() -> Self {
        let platform = HostPlatform::current();
        let mut accelerators = AcceleratorCapabilities::none();
        if platform == HostPlatform::Linux
            && std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok()
        {
            accelerators = accelerators.with_available(VmAccelerator::Kvm);
        }
        Self {
            platform,
            accelerators,
            qemu_system: command_in_path("qemu-system-x86_64"),
            qemu_img: command_in_path("qemu-img"),
            aria2c: command_in_path("aria2c"),
            swtpm: command_in_path("swtpm"),
            wimlib_imagex: command_in_path("wimlib-imagex"),
            xorriso: command_in_path("xorriso"),
            remote_viewer: command_in_path("remote-viewer"),
            ovmf_code: configured_or_first_existing(
                "LSW_OVMF_CODE",
                &[
                    "/usr/share/OVMF/OVMF_CODE_4M.fd",
                    "/usr/share/OVMF/OVMF_CODE.fd",
                    "/usr/share/edk2/x64/OVMF_CODE.fd",
                    "/usr/share/edk2/ovmf/OVMF_CODE.fd",
                    "/usr/share/qemu/edk2-x86_64-code.fd",
                ],
            ),
            ovmf_vars: configured_or_first_existing(
                "LSW_OVMF_VARS",
                &[
                    "/usr/share/OVMF/OVMF_VARS_4M.fd",
                    "/usr/share/OVMF/OVMF_VARS.fd",
                    "/usr/share/edk2/x64/OVMF_VARS.fd",
                    "/usr/share/edk2/ovmf/OVMF_VARS.fd",
                    "/usr/share/qemu/edk2-i386-vars.fd",
                ],
            ),
            ovmf_secure_code: configured_or_first_existing(
                "LSW_OVMF_SECURE_CODE",
                &[
                    "/usr/share/OVMF/OVMF_CODE_4M.ms.fd",
                    "/usr/share/OVMF/OVMF_CODE.ms.fd",
                    "/usr/share/edk2/ovmf/OVMF_CODE.secboot.fd",
                ],
            ),
            ovmf_secure_vars: configured_or_first_existing(
                "LSW_OVMF_SECURE_VARS",
                &[
                    "/usr/share/OVMF/OVMF_VARS_4M.ms.fd",
                    "/usr/share/OVMF/OVMF_VARS.ms.fd",
                    "/usr/share/edk2/ovmf/OVMF_VARS.secboot.fd",
                ],
            ),
        }
    }

    pub fn unavailable(platform: HostPlatform) -> Self {
        Self {
            platform,
            accelerators: AcceleratorCapabilities::none(),
            qemu_system: None,
            qemu_img: None,
            aria2c: None,
            swtpm: None,
            wimlib_imagex: None,
            xorriso: None,
            remote_viewer: None,
            ovmf_code: None,
            ovmf_vars: None,
            ovmf_secure_code: None,
            ovmf_secure_vars: None,
        }
    }

    pub fn missing_for_launch(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.qemu_system.is_none() {
            missing.push("qemu-system-x86_64");
        }
        if self.swtpm.is_none() {
            missing.push("swtpm");
        }
        if self.ovmf_code.is_none() {
            missing.push("OVMF code firmware");
        }
        missing
    }

    pub fn missing_for_preparation(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.qemu_img.is_none() {
            missing.push("qemu-img");
        }
        if self.ovmf_vars.is_none() {
            missing.push("OVMF variable template");
        }
        missing
    }

    pub fn missing_for_install_workflow(&self) -> Vec<&'static str> {
        let mut missing = self.missing_for_launch();
        missing.extend(self.missing_for_preparation());
        if self.wimlib_imagex.is_none() {
            missing.push("wimlib-imagex");
        }
        if self.xorriso.is_none() {
            missing.push("xorriso");
        }
        if self.remote_viewer.is_none() {
            missing.push("remote-viewer");
        }
        missing.sort_unstable();
        missing.dedup();
        missing
    }

    pub fn firmware_code(&self, profile: WindowsProfile) -> Option<&PathBuf> {
        if profile.security().secure_boot {
            self.ovmf_secure_code.as_ref()
        } else {
            self.ovmf_code.as_ref()
        }
    }

    pub fn firmware_vars(&self, profile: WindowsProfile) -> Option<&PathBuf> {
        if profile.security().secure_boot {
            self.ovmf_secure_vars.as_ref()
        } else {
            self.ovmf_vars.as_ref()
        }
    }

    pub fn missing_for_profile_launch(&self, profile: WindowsProfile) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.qemu_system.is_none() {
            missing.push("qemu-system-x86_64");
        }
        if self.swtpm.is_none() {
            missing.push("swtpm");
        }
        if self.firmware_code(profile).is_none() {
            missing.push(if profile.security().secure_boot {
                "Secure Boot OVMF code firmware"
            } else {
                "OVMF code firmware"
            });
        }
        missing
    }

    pub fn missing_for_profile_preparation(&self, profile: WindowsProfile) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.qemu_img.is_none() {
            missing.push("qemu-img");
        }
        if self.firmware_vars(profile).is_none() {
            missing.push(if profile.security().secure_boot {
                "enrolled Secure Boot OVMF variable template"
            } else {
                "OVMF variable template"
            });
        }
        missing
    }
}

fn command_in_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(command))
        .find(|candidate| candidate.is_file())
}

fn configured_or_first_existing(variable: &str, candidates: &[&str]) -> Option<PathBuf> {
    if let Some(configured) = env::var_os(variable).map(PathBuf::from) {
        if configured.is_file() {
            return Some(configured);
        }
    }
    candidates
        .iter()
        .map(|candidate| PathBuf::from(*candidate))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_capabilities_are_reported_without_panicking() {
        let capabilities = HostCapabilities::unavailable(HostPlatform::Linux);
        assert_eq!(capabilities.missing_for_launch().len(), 3);
        assert_eq!(capabilities.missing_for_preparation().len(), 2);
    }
}
