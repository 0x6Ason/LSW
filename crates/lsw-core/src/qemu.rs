// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    FolderShareTransport, GuestCpuTuning, HostCapabilities, InstanceManifest, NetworkMode,
    QemuBackend, Result,
};
use lsw_protocol::AGENT_GUEST_PORT;

pub const LIVE_SMB_RUNTIME_DIRECTORY: &str = "live-smb";
pub const LIVE_SMB_CONFIG_FILE: &str = "smb.conf";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
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
    cpu_tuning: GuestCpuTuning,
}

impl QemuPlanner {
    pub fn new(capabilities: HostCapabilities) -> Self {
        let backend = QemuBackend::select(&capabilities);
        Self {
            capabilities,
            backend,
            cpu_tuning: GuestCpuTuning::default(),
        }
    }

    pub fn with_backend(capabilities: HostCapabilities, backend: QemuBackend) -> Result<Self> {
        backend.validate(&capabilities)?;
        Ok(Self {
            capabilities,
            backend,
            cpu_tuning: GuestCpuTuning::default(),
        })
    }

    /// Overrides the guest CPU tuning; see [`GuestCpuTuning`].
    #[must_use]
    pub const fn with_cpu_tuning(mut self, cpu_tuning: GuestCpuTuning) -> Self {
        self.cpu_tuning = cpu_tuning;
        self
    }

    pub const fn backend(&self) -> QemuBackend {
        self.backend
    }

    pub const fn cpu_tuning(&self) -> GuestCpuTuning {
        self.cpu_tuning
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
        let program = self
            .capabilities
            .qemu_system
            .clone()
            .unwrap_or_else(|| PathBuf::from("qemu-system-x86_64"))
            .into_os_string();

        // Argument order is part of the dry-run contract `lsw plan` prints,
        // so each device group is added in the order it has always appeared.
        let mut launch = LaunchArguments::new(self, manifest, instance_dir, phase);
        launch.machine_and_cpu();
        launch.firmware();
        launch.system_disk();
        let live_share = launch.network()?;
        launch.input_and_platform_devices();
        launch.boot_media();
        launch.install_seed()?;
        launch.identity_seed()?;
        launch.vtpm();
        launch.control_sockets();
        launch.profile_notes();
        let missing_capabilities = launch.missing_capabilities(live_share);

        Ok(CommandPlan {
            backend: self.backend,
            program,
            arguments: launch.arguments,
            helper_commands: launch.helper_commands,
            notes: launch.notes,
            missing_capabilities,
        })
    }
}

/// Accumulates one QEMU invocation, one device group at a time.
struct LaunchArguments<'a> {
    planner: &'a QemuPlanner,
    manifest: &'a InstanceManifest,
    instance_dir: &'a Path,
    phase: LaunchPhase,
    security: crate::SecuritySettings,
    arguments: Vec<OsString>,
    helper_commands: Vec<CommandInvocation>,
    notes: Vec<String>,
}

impl<'a> LaunchArguments<'a> {
    fn new(
        planner: &'a QemuPlanner,
        manifest: &'a InstanceManifest,
        instance_dir: &'a Path,
        phase: LaunchPhase,
    ) -> Self {
        Self {
            planner,
            manifest,
            instance_dir,
            phase,
            security: manifest.spec.profile.security(),
            arguments: Vec::new(),
            helper_commands: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn push(&mut self, option: &str, value: impl Into<OsString>) {
        push_pair(&mut self.arguments, option, value);
    }

    fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// Machine type, vCPU topology, memory, accelerator, CPU model, and RTC.
    fn machine_and_cpu(&mut self) {
        let backend = self.planner.backend;
        self.push("-name", self.manifest.spec.name.as_str());
        self.push(
            "-machine",
            if self.security.secure_boot {
                "q35,usb=on,smm=on"
            } else {
                "q35,usb=on"
            },
        );
        // Windows licenses a socket count (Home: 1, Pro: 2) and schedules per
        // core, so present every vCPU as a core of one socket instead of
        // leaving the topology to QEMU's version-dependent default.
        let cpus = self.manifest.spec.cpus;
        self.push("-smp", format!("{cpus},sockets=1,cores={cpus},threads=1"));
        self.push("-m", format!("{}M", self.manifest.spec.memory_mib));
        self.arguments
            .extend(backend.acceleration_arguments().iter().map(OsString::from));
        self.push("-cpu", backend.cpu_model(self.planner.cpu_tuning));
        // The unattend installs Windows with a UTC clock; slew the emulated RTC
        // instead of letting it jump when the host is busy.
        self.push("-rtc", "base=utc,driftfix=slew");
        if let Some(note) = backend.fallback_note() {
            self.note(note);
        }
        if backend.accelerator() == crate::VmAccelerator::Kvm {
            self.note(match self.planner.cpu_tuning {
                GuestCpuTuning::Enlightened => {
                    "guest CPU: host passthrough with Hyper-V enlightenments; set LSW_GUEST_CPU_TUNING=baseline if QEMU rejects an hv-* feature"
                }
                GuestCpuTuning::Baseline => {
                    "guest CPU: baseline host passthrough without Hyper-V enlightenments"
                }
            });
        }
    }

    /// OVMF code and the instance's private variable store.
    fn firmware(&mut self) {
        let firmware_code = self
            .planner
            .capabilities
            .firmware_code(self.manifest.spec.profile)
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/path/to/OVMF_CODE.fd"));
        let firmware_vars = self.instance_dir.join("OVMF_VARS.fd");
        self.push(
            "-drive",
            format!(
                "if=pflash,format=raw,readonly=on,file={}",
                qemu_path(&firmware_code)
            ),
        );
        self.push(
            "-drive",
            format!("if=pflash,format=raw,file={}", qemu_path(&firmware_vars)),
        );
        if self.security.secure_boot {
            self.push("-global", "driver=cfi.pflash01,property=secure,value=on");
        }
    }

    /// The persistent system disk, or the per-run overlay of an ephemeral
    /// instance, behind the Windows-inbox NVMe driver.
    fn system_disk(&mut self) {
        let disk = if self.phase == LaunchPhase::Run
            && self.manifest.spec.profile == crate::WindowsProfile::Ephemeral
        {
            self.instance_dir.join("run/ephemeral.qcow2")
        } else {
            self.instance_dir.join("disk.qcow2")
        };
        self.push(
            "-drive",
            format!(
                "file={},if=none,id=system,format=qcow2,discard=unmap,detect-zeroes=unmap",
                qemu_path(&disk)
            ),
        );
        // NVMe and e1000e are recognized by stock Windows installation media.
        // VirtIO can be selected later after its signed guest drivers are present.
        self.push("-device", "nvme,drive=system,serial=lsw-system");
    }

    /// User-mode NAT with the loopback agent forward, published ports, and
    /// the private SMB export of a live folder share. Returns whether a live
    /// share is attached.
    fn network(&mut self) -> Result<bool> {
        let manifest = self.manifest;
        let restrict = match manifest.spec.network {
            NetworkMode::Nat => "off",
            NetworkMode::Offline => "on",
        };
        let mut network = format!(
            "user,id=net0,restrict={restrict},hostfwd=tcp:127.0.0.1:{}-:{}",
            manifest.control_port, AGENT_GUEST_PORT
        );
        for forward in &manifest.spec.port_forwards {
            network.push_str(&format!(
                ",hostfwd=tcp:127.0.0.1:{}-:{}",
                forward.host_port, forward.guest_port
            ));
        }
        let live_share = if self.phase == LaunchPhase::Run {
            manifest
                .folder_shares
                .iter()
                .find(|share| share.transport == FolderShareTransport::LiveSmb)
        } else {
            None
        };
        if let Some(share) = live_share {
            let root = canonical_live_share_root(&share.host_path)?;
            let smbd = self
                .planner
                .capabilities
                .smbd
                .as_deref()
                .unwrap_or_else(|| Path::new("smbd"));
            let command = live_smb_guest_forward_command(smbd, self.instance_dir);
            for port in [139, 445] {
                network.push_str(&format!(
                    ",guestfwd=tcp:10.0.2.4:{port}-cmd:{}",
                    qemu_option_text(&command)
                ));
            }
            self.note(format!(
                "live folder {:?}: private authenticated QEMU SMB exports {} read-write at \\\\10.0.2.4\\qemu",
                share.name,
                root.display()
            ));
        }
        self.push("-netdev", network);
        self.push("-device", "e1000e,netdev=net0");
        self.note(match manifest.spec.network {
            NetworkMode::Nat if manifest.spec.port_forwards.is_empty() => {
                "network policy: user-mode NAT permits guest egress and exposes only the agent port on host loopback"
            }
            NetworkMode::Nat => {
                "network policy: user-mode NAT permits guest egress; the agent and explicitly published TCP ports bind only to host loopback"
            }
            NetworkMode::Offline => {
                "network policy: offline blocks guest egress while retaining the host-only agent forward"
            }
        });
        for forward in &manifest.spec.port_forwards {
            self.note(format!(
                "published TCP port: 127.0.0.1:{} forwards to guest port {}",
                forward.host_port, forward.guest_port
            ));
        }
        Ok(live_share.is_some())
    }

    /// USB input, entropy, balloon, and the driverless display adapter.
    fn input_and_platform_devices(&mut self) {
        for device in [
            "qemu-xhci",
            "usb-kbd",
            "usb-tablet",
            "virtio-rng-pci",
            "virtio-balloon-pci,id=balloon0",
            // -nodefaults removes QEMU's implicit display adapter. Standard VGA
            // is available to Windows Setup without an additional guest driver.
            "VGA",
        ] {
            self.push("-device", device);
        }
    }

    /// The installer ISO on first boot only; the system disk afterwards.
    fn boot_media(&mut self) {
        if self.phase == LaunchPhase::Install {
            self.push(
                "-drive",
                format!(
                    "media=cdrom,readonly=on,file={}",
                    qemu_path(&self.manifest.spec.source_iso)
                ),
            );
            // Boot the installer only once. Rebooting an unattended guest back
            // into the ISO could otherwise wipe and reinstall its virtual disk.
            self.push("-boot", "once=d,menu=on");
        } else {
            self.push("-boot", "order=c,menu=on");
        }
    }

    /// The transient installation payload, attached read-only as removable
    /// USB media during the install phase.
    fn install_seed(&mut self) -> Result<()> {
        if self.phase != LaunchPhase::Install {
            return Ok(());
        }
        let seed = self.instance_dir.join("seed");
        if !real_directory_exists(&seed, "install seed")? {
            return Ok(());
        }
        self.push(
            "-drive",
            format!(
                "file=fat:ro:{},format=raw,if=none,id=lsw-seed,snapshot=on",
                qemu_path(&seed)
            ),
        );
        self.push("-device", "usb-storage,drive=lsw-seed,removable=on");
        self.note("the local installation seed is attached read-only as removable media");
        Ok(())
    }

    /// The clone identity volume a running guest's agent consumes, attached
    /// on the inbox IDE path during the run phase.
    fn identity_seed(&mut self) -> Result<()> {
        let identity_seed = self.instance_dir.join("identity-seed");
        if self.phase != LaunchPhase::Run {
            // The install phase never attaches it; only an unreadable path is
            // worth reporting there.
            match fs::symlink_metadata(&identity_seed) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(());
        }
        if !real_directory_exists(&identity_seed, "boot identity seed")? {
            return Ok(());
        }
        self.push(
            "-drive",
            format!(
                "file=fat:ro:{},format=raw,if=none,id=lsw-identity,snapshot=on",
                qemu_path(&identity_seed)
            ),
        );
        self.push("-device", "ide-hd,drive=lsw-identity");
        self.note(
            "the boot identity uses an inbox IDE path, is attached read-only, and is consumed by the guest agent",
        );
        Ok(())
    }

    /// The swtpm helper and the TPM device bound to its socket.
    fn vtpm(&mut self) {
        if !self.security.vtpm {
            return;
        }
        let socket = self.instance_dir.join("run/swtpm.sock");
        let swtpm_program = self
            .planner
            .capabilities
            .swtpm
            .clone()
            .unwrap_or_else(|| PathBuf::from("swtpm"));
        self.helper_commands.push(CommandInvocation {
            program: swtpm_program.into_os_string(),
            arguments: vec![
                "socket".into(),
                "--tpm2".into(),
                "--tpmstate".into(),
                format!("dir={}", self.instance_dir.join("swtpm-state").display()).into(),
                "--ctrl".into(),
                format!("type=unixio,path={}", socket.display()).into(),
                "--log".into(),
                format!("file={}", self.instance_dir.join("swtpm.log").display()).into(),
                "--terminate".into(),
            ],
        });
        self.push(
            "-chardev",
            format!("socket,id=chrtpm,path={}", qemu_path(&socket)),
        );
        self.push("-tpmdev", "emulator,id=tpm0,chardev=chrtpm");
        self.push("-device", "tpm-tis,tpmdev=tpm0");
    }

    /// QMP, the PID file, no default devices, and the private recovery VNC.
    fn control_sockets(&mut self) {
        let qmp_socket = self.instance_dir.join("run/qmp.sock");
        self.push(
            "-qmp",
            format!("unix:{},server=on,wait=off", qemu_path(&qmp_socket)),
        );
        self.push(
            "-pidfile",
            self.instance_dir.join("run/qemu.pid").as_os_str(),
        );
        self.arguments.push("-nodefaults".into());
        self.push("-monitor", "none");
        self.push("-display", "none");
        self.push("-serial", "none");
        self.push(
            "-vnc",
            format!(
                "unix:{}",
                qemu_path(&self.instance_dir.join("run/recovery-vnc.sock"))
            ),
        );
    }

    fn profile_notes(&mut self) {
        if self.security.secure_boot {
            self.note(
                "secure profile: prepare an OVMF variable store enrolled with production keys",
            );
            self.note("secure profile uses the driverless Windows capture path");
        } else {
            self.note(
                "seamless profile: guest Secure Boot is disabled; host Secure Boot is unchanged",
            );
            self.note("test-signed guest drivers are optional; the initial capture path remains driverless");
        }
        self.note("the VNC Unix socket is reserved for installation and recovery; it is not the seamless application transport");
        self.note("start each helper command before starting QEMU");
        if self.manifest.spec.profile == crate::WindowsProfile::Ephemeral {
            self.note("ephemeral profile preserves the base disk and discards its per-run overlay when the guest stops");
        }
    }

    /// Host tools and prepared instance files the launch still needs.
    fn missing_capabilities(&mut self, live_share: bool) -> Vec<&'static str> {
        let mut missing = self
            .planner
            .capabilities
            .missing_for_profile_launch(self.manifest.spec.profile);
        if live_share && self.planner.capabilities.smbd.is_none() {
            missing.push("smbd (Samba user-mode server)");
        }
        if !self.instance_dir.join("disk.qcow2").is_file() {
            missing.push("prepared system disk");
        }
        if self.phase == LaunchPhase::Run
            && self.manifest.spec.profile == crate::WindowsProfile::Ephemeral
            && !self.instance_dir.join("run/ephemeral.qcow2").is_file()
        {
            missing.push("prepared ephemeral overlay");
        }
        if !self.instance_dir.join("OVMF_VARS.fd").is_file() {
            missing.push("prepared OVMF variable store");
        }
        if !self.instance_dir.join("run").is_dir()
            || !self.instance_dir.join("swtpm-state").is_dir()
        {
            missing.push("prepared runtime directories");
        }
        if missing
            .iter()
            .any(|missing| missing.starts_with("prepared "))
        {
            self.note("run `lsw prepare NAME --execute` before launching the guest");
        }
        missing
    }
}

/// Whether `path` is a real directory; a missing path is `false`, and a
/// symlink or non-directory is an error naming `field`.
fn real_directory_exists(path: &Path, field: &'static str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(true)
        }
        Ok(_) => Err(crate::LswError::InvalidValue {
            field,
            reason: format!("{} must be a real directory", path.display()),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn canonical_live_share_root(path: &Path) -> Result<PathBuf> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(crate::LswError::InvalidValue {
                    field: "live folder share root",
                    reason: format!("{} crosses a symbolic link", path.display()),
                })
            }
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let canonical = fs::canonicalize(path)?;
    let metadata = fs::symlink_metadata(&canonical)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(crate::LswError::InvalidValue {
            field: "live folder share root",
            reason: format!("{} must be a real directory", path.display()),
        });
    }
    Ok(canonical)
}

fn live_smb_guest_forward_command(smbd: &Path, instance_dir: &Path) -> String {
    let runtime = instance_dir.join("run").join(LIVE_SMB_RUNTIME_DIRECTORY);
    let config = runtime.join(LIVE_SMB_CONFIG_FILE);
    [
        smbd.as_os_str(),
        OsStr::new("-l"),
        runtime.as_os_str(),
        OsStr::new("-s"),
        config.as_os_str(),
    ]
    .into_iter()
    .map(shell_quote)
    .collect::<Vec<_>>()
    .join(" ")
}

fn qemu_option_text(value: &str) -> String {
    value.replace(',', ",,")
}

fn display_command(program: &OsStr, arguments: &[OsString]) -> String {
    std::iter::once(program)
        .chain(arguments.iter().map(OsString::as_os_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn push_pair(
    arguments: &mut Vec<OsString>,
    option: impl Into<OsString>,
    value: impl Into<OsString>,
) {
    arguments.push(option.into());
    arguments.push(value.into());
}

/// Escapes a path for a QEMU option value, where `,` is the separator.
pub(crate) fn qemu_path(path: &Path) -> String {
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
mod tests;
