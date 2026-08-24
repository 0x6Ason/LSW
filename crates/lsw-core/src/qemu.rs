// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{
    HostCapabilities, InstanceManifest, NetworkMode, QemuBackend, Result, AGENT_GUEST_PORT,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchPhase {
    Install,
    Run,
}

impl fmt::Display for LaunchPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Install => formatter.write_str("install"),
            Self::Run => formatter.write_str("run"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandInvocation {
    pub program: OsString,
    pub arguments: Vec<OsString>,
}

impl CommandInvocation {
    pub fn display_command(&self) -> String {
        display_command(&self.program, &self.arguments)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPlan {
    pub backend: QemuBackend,
    pub program: OsString,
    pub arguments: Vec<OsString>,
    pub helper_commands: Vec<CommandInvocation>,
    pub notes: Vec<String>,
    pub missing_capabilities: Vec<&'static str>,
}

impl CommandPlan {
    pub fn display_command(&self) -> String {
        display_command(&self.program, &self.arguments)
    }
}

#[derive(Clone, Debug)]
pub struct QemuPlanner {
    capabilities: HostCapabilities,
    backend: QemuBackend,
}

impl QemuPlanner {
    pub fn new(capabilities: HostCapabilities) -> Self {
        let backend = QemuBackend::select(&capabilities);
        Self {
            capabilities,
            backend,
        }
    }

    pub fn with_backend(capabilities: HostCapabilities, backend: QemuBackend) -> Result<Self> {
        backend.validate(&capabilities)?;
        Ok(Self {
            capabilities,
            backend,
        })
    }

    pub const fn backend(&self) -> QemuBackend {
        self.backend
    }

    pub fn plan(
        &self,
        manifest: &InstanceManifest,
        instance_dir: &Path,
        phase: LaunchPhase,
    ) -> Result<CommandPlan> {
        if phase == LaunchPhase::Install {
            manifest.spec.validate_for_create()?;
        } else {
            manifest.spec.validate()?;
        }
        let security = manifest.spec.profile.security();
        let program = self
            .capabilities
            .qemu_system
            .clone()
            .unwrap_or_else(|| PathBuf::from("qemu-system-x86_64"))
            .into_os_string();
        let mut arguments = Vec::new();
        let mut helper_commands = Vec::new();
        let mut notes = Vec::new();

        push_pair(&mut arguments, "-name", manifest.spec.name.as_str());
        push_pair(
            &mut arguments,
            "-machine",
            if security.secure_boot {
                "q35,usb=on,smm=on"
            } else {
                "q35,usb=on"
            },
        );
        push_pair(&mut arguments, "-smp", manifest.spec.cpus.to_string());
        push_pair(
            &mut arguments,
            "-m",
            format!("{}M", manifest.spec.memory_mib),
        );

        arguments.extend(
            self.backend
                .acceleration_arguments()
                .iter()
                .map(OsString::from),
        );
        if let Some(note) = self.backend.fallback_note() {
            notes.push(note);
        }

        let firmware_code = self
            .capabilities
            .firmware_code(manifest.spec.profile)
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/path/to/OVMF_CODE.fd"));
        let firmware_vars = instance_dir.join("OVMF_VARS.fd");
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "if=pflash,format=raw,readonly=on,file={}",
                qemu_path(&firmware_code)
            ),
        );
        push_pair(
            &mut arguments,
            "-drive",
            format!("if=pflash,format=raw,file={}", qemu_path(&firmware_vars)),
        );
        if security.secure_boot {
            push_pair(
                &mut arguments,
                "-global",
                "driver=cfi.pflash01,property=secure,value=on",
            );
        }

        let disk = if phase == LaunchPhase::Run
            && manifest.spec.profile == crate::WindowsProfile::Ephemeral
        {
            instance_dir.join("run/ephemeral.qcow2")
        } else {
            instance_dir.join("disk.qcow2")
        };
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "file={},if=none,id=system,format=qcow2,discard=unmap,detect-zeroes=unmap",
                qemu_path(&disk)
            ),
        );
        // NVMe and e1000e are recognized by stock Windows installation media.
        // VirtIO can be selected later after its signed guest drivers are present.
        push_pair(
            &mut arguments,
            "-device",
            "nvme,drive=system,serial=lsw-system",
        );
        let mut network = match manifest.spec.network {
            NetworkMode::Nat => format!(
                "user,id=net0,restrict=off,hostfwd=tcp:127.0.0.1:{}-:{}",
                manifest.control_port, AGENT_GUEST_PORT
            ),
            NetworkMode::Offline => format!(
                "user,id=net0,restrict=on,hostfwd=tcp:127.0.0.1:{}-:{}",
                manifest.control_port, AGENT_GUEST_PORT
            ),
        };
        for forward in &manifest.spec.port_forwards {
            network.push_str(&format!(
                ",hostfwd=tcp:127.0.0.1:{}-:{}",
                forward.host_port, forward.guest_port
            ));
        }
        push_pair(&mut arguments, "-netdev", network);
        push_pair(&mut arguments, "-device", "e1000e,netdev=net0");
        notes.push(match manifest.spec.network {
            NetworkMode::Nat if manifest.spec.port_forwards.is_empty() => {
                "network policy: user-mode NAT permits guest egress and exposes only the agent port on host loopback"
                    .to_owned()
            }
            NetworkMode::Nat => {
                "network policy: user-mode NAT permits guest egress; the agent and explicitly published TCP ports bind only to host loopback"
                    .to_owned()
            }
            NetworkMode::Offline => {
                "network policy: offline blocks guest egress while retaining the host-only agent forward"
                    .to_owned()
            }
        });
        for forward in &manifest.spec.port_forwards {
            notes.push(format!(
                "published TCP port: 127.0.0.1:{} forwards to guest port {}",
                forward.host_port, forward.guest_port
            ));
        }
        push_pair(&mut arguments, "-device", "qemu-xhci");
        push_pair(&mut arguments, "-device", "usb-kbd");
        push_pair(&mut arguments, "-device", "usb-tablet");
        push_pair(&mut arguments, "-device", "virtio-rng-pci");
        push_pair(&mut arguments, "-device", "virtio-balloon-pci,id=balloon0");
        // -nodefaults removes QEMU's implicit display adapter. Standard VGA is
        // available to Windows Setup without an additional guest driver.
        push_pair(&mut arguments, "-device", "VGA");

        if phase == LaunchPhase::Install {
            push_pair(
                &mut arguments,
                "-drive",
                format!(
                    "media=cdrom,readonly=on,file={}",
                    qemu_path(&manifest.spec.source_iso)
                ),
            );
            // Boot the installer only once. Rebooting an unattended guest back
            // into the ISO could otherwise wipe and reinstall its virtual disk.
            push_pair(&mut arguments, "-boot", "once=d,menu=on");
        } else {
            push_pair(&mut arguments, "-boot", "order=c,menu=on");
        }

        let seed = instance_dir.join("seed");
        match (phase, fs::symlink_metadata(&seed)) {
            (LaunchPhase::Install, Ok(metadata))
                if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
            {
                push_pair(
                    &mut arguments,
                    "-drive",
                    format!(
                        "file=fat:ro:{},format=raw,if=none,id=lsw-seed,snapshot=on",
                        qemu_path(&seed)
                    ),
                );
                push_pair(
                    &mut arguments,
                    "-device",
                    "usb-storage,drive=lsw-seed,removable=on",
                );
                notes.push(
                    "the local installation seed is attached read-only as removable media"
                        .to_owned(),
                );
            }
            (LaunchPhase::Install, Ok(_)) => {
                return Err(crate::LswError::InvalidValue {
                    field: "install seed",
                    reason: format!("{} must be a real directory", seed.display()),
                })
            }
            (LaunchPhase::Install, Err(error)) if error.kind() == io::ErrorKind::NotFound => {}
            (LaunchPhase::Install, Err(error)) => return Err(error.into()),
            (LaunchPhase::Run, _) => {}
        }

        let identity_seed = instance_dir.join("identity-seed");
        match (phase, fs::symlink_metadata(&identity_seed)) {
            (LaunchPhase::Run, Ok(metadata))
                if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
            {
                push_pair(
                    &mut arguments,
                    "-drive",
                    format!(
                        "file=fat:ro:{},format=raw,if=none,id=lsw-identity,snapshot=on",
                        qemu_path(&identity_seed)
                    ),
                );
                push_pair(&mut arguments, "-device", "ide-hd,drive=lsw-identity");
                notes.push(
                    "the boot identity uses an inbox IDE path, is attached read-only, and is consumed by the guest agent"
                        .to_owned(),
                );
            }
            (LaunchPhase::Run, Ok(_)) => {
                return Err(crate::LswError::InvalidValue {
                    field: "boot identity seed",
                    reason: format!("{} must be a real directory", identity_seed.display()),
                })
            }
            (_, Err(error)) if error.kind() == io::ErrorKind::NotFound => {}
            (_, Err(error)) => return Err(error.into()),
            (LaunchPhase::Install, Ok(_)) => {}
        }

        if security.vtpm {
            let socket = instance_dir.join("run/swtpm.sock");
            let swtpm_program = self
                .capabilities
                .swtpm
                .clone()
                .unwrap_or_else(|| PathBuf::from("swtpm"));
            helper_commands.push(CommandInvocation {
                program: swtpm_program.into_os_string(),
                arguments: vec![
                    "socket".into(),
                    "--tpm2".into(),
                    "--tpmstate".into(),
                    format!("dir={}", instance_dir.join("swtpm-state").display()).into(),
                    "--ctrl".into(),
                    format!("type=unixio,path={}", socket.display()).into(),
                    "--log".into(),
                    format!("file={}", instance_dir.join("swtpm.log").display()).into(),
                    "--terminate".into(),
                ],
            });
            push_pair(
                &mut arguments,
                "-chardev",
                format!("socket,id=chrtpm,path={}", qemu_path(&socket)),
            );
            push_pair(&mut arguments, "-tpmdev", "emulator,id=tpm0,chardev=chrtpm");
            push_pair(&mut arguments, "-device", "tpm-tis,tpmdev=tpm0");
        }

        let qmp_socket = instance_dir.join("run/qmp.sock");
        push_pair(
            &mut arguments,
            "-qmp",
            format!("unix:{},server=on,wait=off", qemu_path(&qmp_socket)),
        );
        push_pair(
            &mut arguments,
            "-pidfile",
            instance_dir.join("run/qemu.pid").as_os_str(),
        );
        arguments.push("-nodefaults".into());
        push_pair(&mut arguments, "-monitor", "none");
        push_pair(&mut arguments, "-display", "none");
        push_pair(&mut arguments, "-serial", "none");
        push_pair(
            &mut arguments,
            "-vnc",
            format!(
                "unix:{}",
                qemu_path(&instance_dir.join("run/recovery-vnc.sock"))
            ),
        );

        if security.secure_boot {
            notes.push(
                "secure profile: prepare an OVMF variable store enrolled with production keys"
                    .to_owned(),
            );
            notes.push("secure profile uses the driverless Windows capture path".to_owned());
        } else {
            notes.push(
                "seamless profile: guest Secure Boot is disabled; host Secure Boot is unchanged"
                    .to_owned(),
            );
            notes.push(
                "test-signed guest drivers are optional; the initial capture path remains driverless"
                    .to_owned(),
            );
        }
        notes.push(
            "the VNC Unix socket is reserved for installation and recovery; it is not the seamless application transport"
                .to_owned(),
        );
        notes.push("start each helper command before starting QEMU".to_owned());
        if manifest.spec.profile == crate::WindowsProfile::Ephemeral {
            notes.push(
                "ephemeral profile preserves the base disk and discards its per-run overlay when the guest stops"
                    .to_owned(),
            );
        }

        let mut missing_capabilities = self
            .capabilities
            .missing_for_profile_launch(manifest.spec.profile);
        if !instance_dir.join("disk.qcow2").is_file() {
            missing_capabilities.push("prepared system disk");
        }
        if phase == LaunchPhase::Run
            && manifest.spec.profile == crate::WindowsProfile::Ephemeral
            && !instance_dir.join("run/ephemeral.qcow2").is_file()
        {
            missing_capabilities.push("prepared ephemeral overlay");
        }
        if !instance_dir.join("OVMF_VARS.fd").is_file() {
            missing_capabilities.push("prepared OVMF variable store");
        }
        if !instance_dir.join("run").is_dir() || !instance_dir.join("swtpm-state").is_dir() {
            missing_capabilities.push("prepared runtime directories");
        }
        if missing_capabilities
            .iter()
            .any(|missing| missing.starts_with("prepared "))
        {
            notes.push("run `lsw prepare NAME --execute` before launching the guest".to_owned());
        }

        Ok(CommandPlan {
            backend: self.backend,
            program,
            arguments,
            helper_commands,
            notes,
            missing_capabilities,
        })
    }
}

fn display_command(program: &OsStr, arguments: &[OsString]) -> String {
    std::iter::once(program)
        .chain(arguments.iter().map(OsString::as_os_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_pair(
    arguments: &mut Vec<OsString>,
    option: impl Into<OsString>,
    value: impl Into<OsString>,
) {
    arguments.push(option.into());
    arguments.push(value.into());
}

fn qemu_path(path: &Path) -> String {
    path.to_string_lossy().replace(',', ",,")
}

fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=,@".contains(&byte))
    {
        return value.into_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{InstanceSpec, NetworkMode, WindowsProfile};

    use super::*;

    fn test_manifest(profile: WindowsProfile) -> (InstanceManifest, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be valid")
            .as_nanos();
        let iso = std::env::temp_dir().join(format!("lsw-qemu-test-{nonce}.iso"));
        fs::write(&iso, b"test media").expect("temporary ISO should be writable");
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "win-dev".to_owned(),
            source_iso: iso.clone(),
            profile,
            cpus: 4,
            memory_mib: 8192,
            disk_gib: 64,
            network: NetworkMode::Offline,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        (manifest, iso)
    }

    fn headless_capabilities() -> HostCapabilities {
        HostCapabilities::unavailable(crate::HostPlatform::Linux)
    }

    #[test]
    fn planner_uses_windows_inbox_install_devices() {
        let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
        let planner = QemuPlanner::new(headless_capabilities());
        let plan = planner
            .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Install)
            .expect("plan should be built");
        let command = plan.display_command();
        assert!(command.contains("nvme,drive=system"));
        assert!(command.contains("discard=unmap,detect-zeroes=unmap"));
        assert!(command.contains("virtio-balloon-pci,id=balloon0"));
        assert!(command.contains("e1000e,netdev=net0"));
        assert!(command.contains("-device VGA"));
        assert!(command.contains("-boot once=d,menu=on"));
        assert!(command.contains("-accel tcg,thread=multi"));
        assert_eq!(plan.backend.accelerator(), crate::VmAccelerator::Tcg);
        assert!(command.contains("restrict=on"));
        assert_eq!(plan.helper_commands.len(), 1);
        assert!(plan.helper_commands[0]
            .display_command()
            .contains("swtpm socket --tpm2"));
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn planner_uses_an_explicit_validated_accelerator_selection() {
        let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
        let mut capabilities = headless_capabilities();
        capabilities.accelerators =
            crate::AcceleratorCapabilities::none().with_available(crate::VmAccelerator::Kvm);
        let backend = QemuBackend::require(&capabilities, crate::VmAccelerator::Kvm)
            .expect("advertised KVM should be selectable");
        let planner = QemuPlanner::with_backend(capabilities, backend)
            .expect("the backend should match its capabilities");
        let plan = planner
            .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
            .expect("plan should be built");
        assert_eq!(planner.backend(), backend);
        assert_eq!(plan.backend, backend);
        assert!(plan.display_command().contains("-enable-kvm -cpu host"));
        assert!(!plan.display_command().contains("-accel tcg"));
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn run_identity_uses_the_windows_inbox_ide_path() {
        let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be valid")
            .as_nanos();
        let instance_dir = std::env::temp_dir().join(format!("lsw-qemu-identity-{nonce}"));
        fs::create_dir_all(instance_dir.join("identity-seed"))
            .expect("identity directory should be created");
        let plan = QemuPlanner::new(headless_capabilities())
            .plan(&manifest, &instance_dir, LaunchPhase::Run)
            .expect("plan should be built");
        let command = plan.display_command();
        assert!(command.contains("id=lsw-identity,snapshot=on"));
        assert!(command.contains("-device ide-hd,drive=lsw-identity"));
        assert!(!command.contains("usb-storage,drive=lsw-identity"));
        fs::remove_dir_all(instance_dir).expect("temporary instance should be removable");
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn secure_profile_does_not_claim_test_driver_support() {
        let (manifest, iso) = test_manifest(WindowsProfile::Secure);
        let planner = QemuPlanner::new(headless_capabilities());
        let plan = planner
            .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
            .expect("plan should be built");
        assert!(plan.notes.iter().any(|note| note.contains("driverless")));
        assert!(!plan
            .notes
            .iter()
            .any(|note| note.contains("test-signed guest")));
        let command = plan.display_command();
        assert!(command.contains("smm=on"));
        assert!(command.contains("cfi.pflash01,property=secure,value=on"));
        assert!(plan
            .missing_capabilities
            .contains(&"Secure Boot OVMF code firmware"));
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn nat_network_publishes_requested_tcp_ports_on_loopback() {
        let (mut manifest, iso) = test_manifest(WindowsProfile::Vanilla);
        manifest.spec.network = NetworkMode::Nat;
        manifest.spec.port_forwards = vec![
            crate::PortForward::new(8080, 80).expect("ports should be valid"),
            crate::PortForward::new(8443, 443).expect("ports should be valid"),
        ];
        let plan = QemuPlanner::new(headless_capabilities())
            .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
            .expect("plan should be built");
        let command = plan.display_command();
        assert!(command.contains("hostfwd=tcp:127.0.0.1:8080-:80"));
        assert!(command.contains("hostfwd=tcp:127.0.0.1:8443-:443"));
        assert!(!command.contains("0.0.0.0:8080"));
        assert!(plan
            .notes
            .iter()
            .any(|note| note.contains("127.0.0.1:8080")));
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }
}
