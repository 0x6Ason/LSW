// SPDX-License-Identifier: GPL-3.0-or-later

//! Network-isolated Windows PE orchestration for real DISM servicing.
//!
//! The prepare VM owns virtual Disk 0 and produces a slim WIM. The apply VM
//! keeps that workspace as Disk 0 and may wipe only the LSW-owned target at
//! Disk 1. Plans encode this topology explicitly so callers cannot substitute a
//! host block device or silently change the destructive target.

#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::install_seed::OFFLINE_APPX_MARKER_NAME;
use crate::{
    CommandInvocation, CustomizationPlan, HostCapabilities, InstanceManifest, LswError,
    PreparationPlan, PreparationStep, QemuBackend, Result, WindowsProfile,
};

/// Disk number assigned to the private temporary WinPE workspace.
pub const WINPE_WORKSPACE_DISK_ID: u32 = 0;
/// Capacity of the private temporary workspace disk.
pub const WINPE_WORKSPACE_SIZE_GIB: u32 = 32;
/// Stable filename of the WIM produced by the prepare phase.
pub const WINPE_PREPARED_IMAGE_NAME: &str = "lsw-prepared.wim";
/// Disk number assigned to the LSW-owned instance target during apply.
pub const WINPE_TARGET_DISK_ID: u32 = 1;
/// Maximum wall-clock duration of either WinPE microVM phase.
pub const WINPE_VM_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

const WINPE_WORKSPACE_DRIVE: &str = "W:";
const WINPE_SCRIPT_FILE: &str = "lsw/winpe-dism.cmd";
const WINPE_APPLY_SCRIPT_FILE: &str = "lsw/apply-image.cmd";
const WINPE_STATUS_TAG: &str = "lsw-status.tag";
const WINPE_STARTNET: &[u8] = include_bytes!("../assets/winpe-startnet.cmd");
const WINPE_SHELL: &[u8] = include_bytes!("../assets/winpeshl.ini");
const MAX_SEED_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STATUS_LOG_BYTES: u64 = 1024 * 1024;
const MAX_DISM_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MIN_APPLIED_DISK_BYTES: u64 = 512 * 1024 * 1024;
const SETUP_ACCOUNT_NAME: &str = "LSWSetup";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// One of the two isolated WinPE microVM phases.
pub enum WinPeDismVmPhase {
    /// Service an exported edition and produce the prepared WIM.
    Prepare,
    /// Apply the prepared WIM to the instance disk and configure UEFI boot.
    Apply,
}

impl WinPeDismVmPhase {
    fn seed_directory(self) -> &'static str {
        match self {
            Self::Prepare => "winpe-seed",
            Self::Apply => "winpe-apply-seed",
        }
    }

    fn completion_marker(self) -> &'static str {
        match self {
            Self::Prepare => "LSW-WINPE-DISM complete",
            Self::Apply => "LSW-WINPE-DISM apply-complete",
        }
    }

    fn failure_marker(self) -> &'static str {
        match self {
            Self::Prepare => "LSW-WINPE-DISM failed",
            Self::Apply => "LSW-WINPE-DISM apply-failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Fully resolved QEMU and host-preparation plan for one WinPE phase.
pub struct WinPeDismVmPlan {
    /// Phase implemented by this plan.
    pub phase: WinPeDismVmPhase,
    /// Validated QEMU backend and accelerator selection.
    pub backend: QemuBackend,
    /// QEMU command with fixed disk ordering and disabled networking.
    pub invocation: CommandInvocation,
    /// Host files and private disks that must exist before launch.
    pub host_preparation: PreparationPlan,
    /// Host-backed phase status log containing bounded completion markers.
    pub status_log: PathBuf,
    /// Host-backed DISM output used for live percentage reporting and diagnostics.
    pub dism_log: PathBuf,
    /// Serial firmware and WinPE diagnostic log for this phase.
    pub serial_log: PathBuf,
    /// QEMU diagnostic log for this phase.
    pub qemu_log: PathBuf,
    /// Private QMP socket used for lifecycle control and diagnostics.
    pub qmp_socket: PathBuf,
    /// Capabilities missing from the current host.
    pub missing_capabilities: Vec<&'static str>,
    /// Human-readable security and topology notes.
    pub notes: Vec<String>,
    target_disk: Option<PathBuf>,
    qemu_img: PathBuf,
    control_media: WinPeControlMediaPlan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WinPeControlMediaPlan {
    source_iso: PathBuf,
    root: PathBuf,
    destination: PathBuf,
    seven_zip: PathBuf,
    wimlib_imagex: PathBuf,
    xorriso: PathBuf,
}

impl WinPeDismVmPlan {
    /// Returns a shell-escaped display form of the QEMU invocation.
    pub fn display_command(&self) -> String {
        self.invocation.display_command()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Observed result of a successfully completed WinPE phase.
pub struct WinPeDismRunResult {
    /// Phase that completed.
    pub phase: WinPeDismVmPhase,
    /// Elapsed wall-clock duration.
    pub elapsed: Duration,
    /// Bounded private-volume status lines retained for diagnostics.
    pub status_events: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Live progress observed from a network-isolated WinPE phase.
pub struct WinPeDismProgress {
    /// Phase producing this progress event.
    pub phase: WinPeDismVmPhase,
    /// Stable machine-readable stage emitted by the WinPE script.
    pub stage: String,
    /// DISM percentage for the current stage when DISM reports one.
    pub percent: Option<u8>,
    /// Wall-clock time elapsed since the WinPE child was launched.
    pub elapsed: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Declarative DISM operation in the prepare phase.
pub enum WinPeDismStage {
    /// Partition and format the private workspace disk.
    InitializeWorkspace,
    /// Locate `install.wim` or `install.esd` on the official ISO.
    LocateInstallImage,
    /// Export the selected Windows edition with DISM.
    ExportEdition {
        /// One-based WIM image index selected from official media metadata.
        index: u32,
    },
    /// Mount the exported WIM for offline servicing.
    MountOfflineImage,
    /// Enumerate provisioned AppX packages before bounded removal.
    InventoryProvisionedAppx,
    /// Remove one allowlisted provisioned AppX package when present.
    RemoveProvisionedAppx {
        /// Exact allowlisted package display-name pattern passed to DISM.
        display_name: String,
    },
    /// Stage the offline unattend file and private guest-agent setup payload.
    StageGuestSetup,
    /// Commit and integrity-check the prepared WIM.
    CommitPreparedImage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Declarative operation in the target-disk apply phase.
pub enum WinPeDismApplyStage {
    /// Locate the prepared WIM on private Disk 0.
    LocatePreparedImage,
    /// Partition and format only the LSW-owned target at Disk 1.
    InitializeTarget,
    /// Apply the prepared WIM without coupling installation to CompactOS.
    ApplyPreparedImage,
    /// Create UEFI boot files with BCDBoot.
    ConfigureUefiBoot,
}

impl WinPeDismApplyStage {
    /// Describes the stage without exposing generated script contents.
    pub fn describe(&self) -> String {
        match self {
            Self::LocatePreparedImage => {
                format!("locate {WINPE_PREPARED_IMAGE_NAME} on the private workspace")
            }
            Self::InitializeTarget => format!(
                "initialize LSW-owned virtual Disk {WINPE_TARGET_DISK_ID} with EFI, MSR, and Windows partitions"
            ),
            Self::ApplyPreparedImage => "apply the prepared image to the target".to_owned(),
            Self::ConfigureUefiBoot => {
                "create the target UEFI boot files with BCDBoot".to_owned()
            }
        }
    }
}

impl WinPeDismStage {
    /// Describes the stage without exposing generated script contents.
    pub fn describe(&self) -> String {
        match self {
            Self::InitializeWorkspace => format!(
                "initialize virtual Disk {WINPE_WORKSPACE_DISK_ID} as the private WinPE workspace"
            ),
            Self::LocateInstallImage => {
                "locate sources/install.wim or sources/install.esd on the official ISO".to_owned()
            }
            Self::ExportEdition { index } => format!(
                "export Windows image index {index} to {WINPE_PREPARED_IMAGE_NAME} with DISM"
            ),
            Self::MountOfflineImage => {
                "mount the exported image with Windows DISM inside WinPE".to_owned()
            }
            Self::InventoryProvisionedAppx => {
                "inventory provisioned AppX packages with Windows DISM".to_owned()
            }
            Self::RemoveProvisionedAppx { display_name } => {
                format!("remove provisioned AppX package {display_name} when present")
            }
            Self::StageGuestSetup => {
                "stage the offline unattend file and private guest-agent setup payload in the prepared WIM"
                    .to_owned()
            }
            Self::CommitPreparedImage => {
                "commit and integrity-check the prepared WIM with Windows DISM".to_owned()
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Generated prepare-phase seed and its private workspace contract.
pub struct WinPeDismPlan {
    /// Profile used to derive the bounded DISM customization.
    pub profile: WindowsProfile,
    /// One-based edition index exported from the official install image.
    pub edition_index: u32,
    /// New seed directory written atomically by [`WinPeDismBackend::write_seed`].
    pub destination: PathBuf,
    /// Private qcow2 workspace owned by this instance.
    pub workspace_disk: PathBuf,
    /// Required workspace capacity in GiB.
    pub workspace_size_gib: u32,
    /// Stable prepared-WIM filename inside the workspace.
    pub prepared_image_name: &'static str,
    /// Whether Windows setup should enable CompactOS after the reliable apply phase.
    pub compact_during_setup: bool,
    /// Whether the prepared WIM contains guest-agent setup files.
    pub includes_agent: bool,
    /// Ordered prepare-phase operations encoded in the script.
    pub stages: Vec<WinPeDismStage>,
    /// Relative files generated in the seed directory.
    pub files: Vec<PathBuf>,
    generated: BTreeMap<PathBuf, Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Generated apply-phase seed and fixed target-disk contract.
pub struct WinPeDismApplyPlan {
    /// Profile whose prepared WIM will be applied.
    pub profile: WindowsProfile,
    /// New apply-seed directory written atomically by the backend.
    pub destination: PathBuf,
    /// Existing private workspace containing the prepared WIM.
    pub workspace_disk: PathBuf,
    /// LSW-owned instance qcow2 that may be initialized.
    pub target_disk: PathBuf,
    /// Fixed virtual disk number expected by the generated DiskPart script.
    pub target_disk_id: u32,
    /// Ordered apply-phase operations encoded in the script.
    pub stages: Vec<WinPeDismApplyStage>,
    /// Relative files generated in the apply seed.
    pub files: Vec<PathBuf>,
    generated: BTreeMap<PathBuf, Vec<u8>>,
}

impl WinPeDismApplyPlan {
    /// Returns a human-readable plan including its destructive-disk warning.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = self
            .stages
            .iter()
            .map(WinPeDismApplyStage::describe)
            .collect::<Vec<_>>();
        lines.extend(
            self.files
                .iter()
                .map(|file| format!("write {}", self.destination.join(file).display())),
        );
        lines.push(format!(
            "warning: the apply job wipes only LSW-owned virtual Disk {}; the workspace must remain Disk 0 and no host disk may be attached",
            self.target_disk_id
        ));
        lines
    }

    /// Returns the generated ASCII apply script.
    pub fn script(&self) -> &str {
        std::str::from_utf8(
            self.generated
                .get(Path::new(WINPE_APPLY_SCRIPT_FILE))
                .expect("WinPE DISM apply plans always contain their script"),
        )
        .expect("the generated WinPE apply script is ASCII")
    }
}

impl WinPeDismPlan {
    /// Returns a human-readable plan including its workspace-disk warning.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "require a blank private {} GiB workspace disk at {}",
            self.workspace_size_gib,
            self.workspace_disk.display()
        )];
        lines.extend(self.stages.iter().map(WinPeDismStage::describe));
        lines.extend(
            self.files
                .iter()
                .map(|file| format!("write {}", self.destination.join(file).display())),
        );
        lines.push(format!(
            "warning: the WinPE job wipes only LSW-owned virtual Disk {WINPE_WORKSPACE_DISK_ID}; the QEMU integration must not attach a host disk"
        ));
        lines
    }

    /// Returns the generated ASCII prepare script.
    pub fn script(&self) -> &str {
        // Construction always inserts this fixed path.
        std::str::from_utf8(
            self.generated
                .get(Path::new(WINPE_SCRIPT_FILE))
                .expect("WinPE DISM plans always contain their script"),
        )
        .expect("the generated WinPE script is ASCII")
    }
}

/// Plans, writes, and runs the two-phase WinPE DISM workflow.
pub struct WinPeDismBackend;

impl WinPeDismBackend {
    /// Builds a prepare-phase plan without writing files or launching QEMU.
    pub fn plan(
        profile: WindowsProfile,
        edition_index: u32,
        instance_dir: &Path,
    ) -> Result<WinPeDismPlan> {
        if edition_index == 0 {
            return Err(LswError::InvalidValue {
                field: "image index",
                reason: "must be at least 1".to_owned(),
            });
        }
        require_real_directory(instance_dir)?;

        let destination = instance_dir.join("winpe-seed");
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(LswError::InvalidValue {
                field: "WinPE DISM seed",
                reason: format!(
                    "{} already exists; LSW will not replace it automatically",
                    destination.display()
                ),
            });
        }

        let customization = CustomizationPlan::for_profile(profile)?;
        validate_appx_patterns(&customization.remove_provisioned_appx_patterns)?;
        let mut stages = vec![
            WinPeDismStage::InitializeWorkspace,
            WinPeDismStage::LocateInstallImage,
            WinPeDismStage::ExportEdition {
                index: edition_index,
            },
            WinPeDismStage::MountOfflineImage,
            WinPeDismStage::InventoryProvisionedAppx,
        ];
        stages.extend(
            customization
                .remove_provisioned_appx_patterns
                .iter()
                .map(|display_name| WinPeDismStage::RemoveProvisionedAppx {
                    display_name: display_name.clone(),
                }),
        );
        stages.push(WinPeDismStage::CommitPreparedImage);

        let mut generated = BTreeMap::new();
        generated.insert(
            PathBuf::from("lsw/workspace.diskpart"),
            workspace_diskpart().into_bytes(),
        );
        generated.insert(
            PathBuf::from(WINPE_SCRIPT_FILE),
            winpe_script(edition_index, &customization, false).into_bytes(),
        );
        generated.insert(
            PathBuf::from("README.txt"),
            seed_readme(profile, edition_index, &customization).into_bytes(),
        );
        let files = generated.keys().cloned().collect();

        Ok(WinPeDismPlan {
            profile,
            edition_index,
            destination,
            workspace_disk: instance_dir.join("run/winpe-workspace.qcow2"),
            workspace_size_gib: WINPE_WORKSPACE_SIZE_GIB,
            prepared_image_name: WINPE_PREPARED_IMAGE_NAME,
            compact_during_setup: customization.compact_os,
            includes_agent: false,
            stages,
            files,
            generated,
        })
    }

    /// Builds the one-shot prepare plan with guest setup committed into the WIM.
    pub fn plan_with_guest_setup(
        manifest: &InstanceManifest,
        edition_index: u32,
        instance_dir: &Path,
        install_seed: &Path,
        locale: &str,
        setup_account_password_value: &str,
    ) -> Result<WinPeDismPlan> {
        manifest.spec.validate()?;
        require_real_directory(install_seed)?;
        validate_locale(locale)?;
        validate_unattend_password_value(setup_account_password_value)?;
        let mut plan = Self::plan(manifest.spec.profile, edition_index, instance_dir)?;
        let customization = CustomizationPlan::for_profile(manifest.spec.profile)?;

        plan.generated.insert(
            PathBuf::from("lsw/offline-unattend.xml"),
            offline_unattend(manifest, locale, setup_account_password_value).into_bytes(),
        );
        plan.includes_agent = copy_guest_setup_payload(&mut plan.generated, install_seed)?;
        plan.generated.insert(
            PathBuf::from(WINPE_SCRIPT_FILE),
            winpe_script(edition_index, &customization, true).into_bytes(),
        );
        let commit = plan
            .stages
            .iter()
            .position(|stage| matches!(stage, WinPeDismStage::CommitPreparedImage))
            .expect("prepare plans always contain a commit stage");
        plan.stages.insert(commit, WinPeDismStage::StageGuestSetup);
        plan.files = plan.generated.keys().cloned().collect();
        Ok(plan)
    }

    /// Builds an apply-phase plan with Disk 1 as the only destructive target.
    pub fn plan_apply(
        manifest: &InstanceManifest,
        instance_dir: &Path,
    ) -> Result<WinPeDismApplyPlan> {
        manifest.spec.validate()?;
        require_real_directory(instance_dir)?;

        let destination = instance_dir.join("winpe-apply-seed");
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(LswError::InvalidValue {
                field: "WinPE DISM apply seed",
                reason: format!(
                    "{} already exists; LSW will not replace it automatically",
                    destination.display()
                ),
            });
        }

        let workspace_disk = instance_dir.join("run/winpe-workspace.qcow2");
        let target_disk = instance_dir.join("disk.qcow2");
        let mut generated = BTreeMap::new();
        generated.insert(
            PathBuf::from("lsw/target.diskpart"),
            target_diskpart().into_bytes(),
        );
        generated.insert(
            PathBuf::from(WINPE_APPLY_SCRIPT_FILE),
            apply_script().into_bytes(),
        );
        generated.insert(
            PathBuf::from("README.txt"),
            apply_seed_readme(manifest.spec.profile).into_bytes(),
        );

        let files = generated.keys().cloned().collect();
        Ok(WinPeDismApplyPlan {
            profile: manifest.spec.profile,
            destination,
            workspace_disk,
            target_disk,
            target_disk_id: WINPE_TARGET_DISK_ID,
            stages: vec![
                WinPeDismApplyStage::LocatePreparedImage,
                WinPeDismApplyStage::InitializeTarget,
                WinPeDismApplyStage::ApplyPreparedImage,
                WinPeDismApplyStage::ConfigureUefiBoot,
            ],
            files,
            generated,
        })
    }

    /// Builds the network-disabled QEMU plan for one WinPE phase.
    pub fn plan_vm(
        capabilities: HostCapabilities,
        manifest: &InstanceManifest,
        instance_dir: &Path,
        phase: WinPeDismVmPhase,
    ) -> Result<WinPeDismVmPlan> {
        manifest.spec.validate_for_create()?;
        require_real_directory(instance_dir)?;
        require_real_directory(&instance_dir.join("run"))?;
        require_real_directory(&instance_dir.join(phase.seed_directory()))?;

        let backend = QemuBackend::select(&capabilities);
        let program = capabilities
            .qemu_system
            .clone()
            .unwrap_or_else(|| PathBuf::from("qemu-system-x86_64"));
        let firmware_code = capabilities
            .firmware_code(manifest.spec.profile)
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/path/to/OVMF_CODE.fd"));
        let firmware_vars_source = capabilities
            .firmware_vars(manifest.spec.profile)
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/path/to/OVMF_VARS.fd"));
        let phase_name = match phase {
            WinPeDismVmPhase::Prepare => "prepare",
            WinPeDismVmPhase::Apply => "apply",
        };
        let firmware_vars = instance_dir.join(format!("run/winpe-{phase_name}-OVMF_VARS.fd"));
        let workspace_disk = instance_dir.join("run/winpe-workspace.qcow2");
        let target_disk = instance_dir.join("disk.qcow2");
        let status_directory = instance_dir.join(format!("run/winpe-{phase_name}-status"));
        let status_log = status_directory.join("status.log");
        let dism_log = status_directory.join("dism.log");
        let serial_log = instance_dir.join(format!("run/winpe-{phase_name}-serial.log"));
        let qemu_log = instance_dir.join(format!("winpe-{phase_name}-qemu.log"));
        let qmp_socket = instance_dir.join(format!("run/winpe-{phase_name}-qmp.sock"));
        let control_media_root = instance_dir.join("run/winpe-control-root");
        let control_media_iso = instance_dir.join("run/winpe-control.iso");
        let mut steps = Vec::new();
        let mut missing_capabilities = Vec::new();

        plan_new_firmware_vars(
            &firmware_vars_source,
            &firmware_vars,
            &mut steps,
            &mut missing_capabilities,
            capabilities.firmware_vars(manifest.spec.profile).is_some(),
        )?;
        match phase {
            WinPeDismVmPhase::Prepare => {
                if path_is_missing(&workspace_disk, "WinPE workspace disk")? {
                    let qemu_img = capabilities
                        .qemu_img
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("qemu-img"));
                    steps.push(PreparationStep::CreateDisk {
                        program: qemu_img,
                        destination: workspace_disk.clone(),
                        size_gib: WINPE_WORKSPACE_SIZE_GIB,
                    });
                    if capabilities.qemu_img.is_none() {
                        missing_capabilities.push("qemu-img");
                    }
                } else {
                    require_regular_file(&workspace_disk, "WinPE workspace disk")?;
                }
            }
            WinPeDismVmPhase::Apply => {
                require_regular_file(&workspace_disk, "prepared WinPE workspace disk")?;
                require_regular_file(&target_disk, "target system disk")?;
            }
        }

        if capabilities.qemu_system.is_none() {
            missing_capabilities.push("qemu-system-x86_64");
        }
        if capabilities.firmware_code(manifest.spec.profile).is_none() {
            missing_capabilities.push("OVMF code firmware");
        }
        if capabilities.seven_zip.is_none() {
            missing_capabilities.push("7z (UDF-capable ISO extractor)");
        }
        if capabilities.wimlib_imagex.is_none() {
            missing_capabilities.push("wimlib-imagex");
        }
        if capabilities.xorriso.is_none() {
            missing_capabilities.push("xorriso");
        }
        if capabilities.qemu_img.is_none() {
            missing_capabilities.push("qemu-img");
        }
        missing_capabilities.sort_unstable();
        missing_capabilities.dedup();

        let mut arguments = Vec::new();
        push_pair(
            &mut arguments,
            "-name",
            format!("lsw-{}-winpe-{phase_name}", manifest.spec.name),
        );
        push_pair(&mut arguments, "-machine", "q35,usb=on");
        push_pair(&mut arguments, "-smp", manifest.spec.cpus.to_string());
        push_pair(
            &mut arguments,
            "-m",
            format!("{}M", manifest.spec.memory_mib),
        );
        arguments.extend(backend.acceleration_arguments().iter().map(OsString::from));
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
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "file={},if=none,id=workspace,format=qcow2,discard=unmap",
                qemu_path(&workspace_disk)
            ),
        );
        push_pair(
            &mut arguments,
            "-device",
            "nvme,drive=workspace,serial=lsw-winpe-workspace,addr=0x4",
        );
        if phase == WinPeDismVmPhase::Apply {
            push_pair(
                &mut arguments,
                "-drive",
                format!(
                    "file={},if=none,id=target,format=qcow2,discard=unmap",
                    qemu_path(&target_disk)
                ),
            );
            push_pair(
                &mut arguments,
                "-device",
                "nvme,drive=target,serial=lsw-system,addr=0x5",
            );
        }
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "media=cdrom,readonly=on,file={}",
                qemu_path(&control_media_iso)
            ),
        );
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "media=cdrom,readonly=on,file={}",
                qemu_path(&manifest.spec.source_iso)
            ),
        );
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "file=fat:ro:{},format=raw,if=none,id=lsw-winpe-seed,snapshot=on",
                qemu_path(&instance_dir.join(phase.seed_directory()))
            ),
        );
        push_pair(&mut arguments, "-device", "qemu-xhci");
        push_pair(
            &mut arguments,
            "-device",
            "usb-storage,drive=lsw-winpe-seed,removable=on",
        );
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "file=fat:rw:{},format=raw,if=none,id=lsw-winpe-status",
                qemu_path(&status_directory)
            ),
        );
        push_pair(
            &mut arguments,
            "-device",
            "usb-storage,drive=lsw-winpe-status,removable=on",
        );
        push_pair(&mut arguments, "-device", "VGA");
        push_pair(&mut arguments, "-boot", "once=d,menu=off");
        push_pair(&mut arguments, "-nic", "none");
        push_pair(
            &mut arguments,
            "-serial",
            format!("file:{}", qemu_path(&serial_log)),
        );
        push_pair(
            &mut arguments,
            "-qmp",
            format!("unix:{},server=on,wait=off", qemu_path(&qmp_socket)),
        );
        arguments.push("-nodefaults".into());
        arguments.push("-no-reboot".into());
        push_pair(&mut arguments, "-monitor", "none");
        push_pair(&mut arguments, "-display", "none");

        let mut notes = vec![match phase {
            WinPeDismVmPhase::Prepare => {
                "preparation VM attaches only the private workspace as writable Disk 0; the target disk is not attached"
                    .to_owned()
            }
            WinPeDismVmPhase::Apply => {
                "apply VM attaches the prepared workspace as Disk 0 and the LSW target qcow2 as Disk 1"
                    .to_owned()
            }
        }];
        notes.push("preparation networking is disabled".to_owned());
        notes.push(
            "WinPE boots from an ephemeral control ISO derived from the supplied Microsoft media"
                .to_owned(),
        );
        notes.push(format!(
            "successful completion requires private status-volume marker {:?}",
            phase.completion_marker()
        ));
        if let Some(note) = backend.fallback_note() {
            notes.push(note);
        }

        Ok(WinPeDismVmPlan {
            phase,
            backend,
            invocation: CommandInvocation {
                program: program.into_os_string(),
                arguments,
            },
            host_preparation: PreparationPlan {
                steps,
                missing_capabilities: missing_capabilities.clone(),
            },
            status_log,
            dism_log,
            serial_log,
            qemu_log,
            qmp_socket,
            missing_capabilities,
            notes,
            target_disk: (phase == WinPeDismVmPhase::Apply).then_some(target_disk),
            qemu_img: capabilities
                .qemu_img
                .clone()
                .unwrap_or_else(|| PathBuf::from("qemu-img")),
            control_media: WinPeControlMediaPlan {
                source_iso: manifest.spec.source_iso.clone(),
                root: control_media_root,
                destination: control_media_iso,
                seven_zip: capabilities
                    .seven_zip
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("7z")),
                wimlib_imagex: capabilities
                    .wimlib_imagex
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("wimlib-imagex")),
                xorriso: capabilities
                    .xorriso
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("xorriso")),
            },
        })
    }

    /// Runs a WinPE phase and requires its exact private-volume completion marker.
    pub fn run_vm(plan: &WinPeDismVmPlan, timeout: Duration) -> Result<WinPeDismRunResult> {
        Self::run_vm_with_progress(plan, timeout, |_| {})
    }

    /// Runs a WinPE phase while reporting trusted guest stages and DISM percentages.
    pub fn run_vm_with_progress<F>(
        plan: &WinPeDismVmPlan,
        timeout: Duration,
        mut on_progress: F,
    ) -> Result<WinPeDismRunResult>
    where
        F: FnMut(&WinPeDismProgress),
    {
        if timeout.is_zero() {
            return Err(LswError::InvalidValue {
                field: "WinPE timeout",
                reason: "must be greater than zero".to_owned(),
            });
        }
        if !plan.missing_capabilities.is_empty() {
            return Err(LswError::MissingCapabilities(
                plan.missing_capabilities.clone(),
            ));
        }
        crate::Provisioner::new(HostCapabilities::unavailable(plan.backend.platform()))
            .apply(&plan.host_preparation)?;
        prepare_status_volume(&plan.status_log, &plan.dism_log)?;
        let uses_qmp = plan
            .invocation
            .arguments
            .iter()
            .any(|argument| argument.to_string_lossy() == "-qmp");
        if uses_qmp {
            prepare_control_media(&plan.control_media)?;
        }
        prepare_output_file(&plan.serial_log)?;
        let stdout = prepare_output_file(&plan.qemu_log)?;
        let stderr = stdout.try_clone()?;
        let started = Instant::now();
        let mut child = Command::new(&plan.invocation.program)
            .args(&plan.invocation.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()?;

        let mut observed_status_events = 0_usize;
        let mut current_stage = Some("starting-winpe".to_owned());
        let mut last_percent = None;
        on_progress(&WinPeDismProgress {
            phase: plan.phase,
            stage: "starting-winpe".to_owned(),
            percent: None,
            elapsed: started.elapsed(),
        });

        let status = loop {
            report_winpe_progress(
                plan,
                started,
                &mut observed_status_events,
                &mut current_stage,
                &mut last_percent,
                &mut on_progress,
            )?;
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(LswError::InvalidValue {
                    field: "WinPE DISM run",
                    reason: format!(
                        "{:?} phase exceeded {} seconds; inspect {}, {}, and {}",
                        plan.phase,
                        timeout.as_secs(),
                        plan.status_log.display(),
                        plan.serial_log.display(),
                        plan.qemu_log.display()
                    ),
                });
            }
            // The terminal renderer updates once per second. Matching that
            // cadence avoids repeatedly rereading a growing guest DISM log.
            thread::sleep(Duration::from_secs(1));
        };
        report_winpe_progress(
            plan,
            started,
            &mut observed_status_events,
            &mut current_stage,
            &mut last_percent,
            &mut on_progress,
        )?;
        if !status.success() {
            return Err(LswError::ExternalCommandFailed {
                program: PathBuf::from(&plan.invocation.program),
                status: status.code(),
            });
        }

        let status_events = read_status_events(&plan.status_log)?;
        if status_events
            .iter()
            .any(|event| event.contains(plan.phase.failure_marker()))
        {
            return Err(LswError::InvalidValue {
                field: "WinPE DISM run",
                reason: format!(
                    "{:?} phase reported failure; inspect {} and {}",
                    plan.phase,
                    plan.status_log.display(),
                    plan.qemu_log.display()
                ),
            });
        }
        if !status_events
            .iter()
            .any(|event| event.contains(plan.phase.completion_marker()))
        {
            return Err(LswError::InvalidValue {
                field: "WinPE DISM run",
                reason: format!(
                    "{:?} phase exited without completion marker {:?}; inspect {}",
                    plan.phase,
                    plan.phase.completion_marker(),
                    plan.status_log.display()
                ),
            });
        }
        if let Some(target_disk) = &plan.target_disk {
            validate_applied_target(&plan.qemu_img, target_disk)?;
        }
        Ok(WinPeDismRunResult {
            phase: plan.phase,
            elapsed: started.elapsed(),
            status_events,
        })
    }

    /// Atomically writes a new prepare seed and refuses replacement.
    pub fn write_seed(plan: &WinPeDismPlan) -> Result<()> {
        write_generated_seed(&plan.destination, &plan.generated, "WinPE DISM seed")
    }

    /// Atomically writes a new apply seed and refuses replacement.
    pub fn write_apply_seed(plan: &WinPeDismApplyPlan) -> Result<()> {
        write_generated_seed(&plan.destination, &plan.generated, "WinPE DISM apply seed")
    }
}

fn write_generated_seed(
    destination: &Path,
    generated: &BTreeMap<PathBuf, Vec<u8>>,
    field: &'static str,
) -> Result<()> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(LswError::InvalidValue {
            field,
            reason: format!("refusing to replace existing {}", destination.display()),
        });
    }
    let parent = destination.parent().ok_or_else(|| LswError::InvalidValue {
        field,
        reason: "destination has no parent directory".to_owned(),
    })?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("winpe-seed");
    let staging = parent.join(format!("{name}.tmp-{}", std::process::id()));
    if fs::symlink_metadata(&staging).is_ok() {
        return Err(LswError::InvalidValue {
            field,
            reason: format!("staging path {} already exists", staging.display()),
        });
    }
    fs::create_dir(&staging)?;
    set_private_directory_permissions(&staging)?;

    let result = (|| {
        for (relative, contents) in generated {
            let destination = staging.join(relative);
            if let Some(directory) = destination.parent() {
                fs::create_dir_all(directory)?;
                set_private_directory_permissions(directory)?;
            }
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&destination)?;
            set_private_file_permissions(&destination)?;
            file.write_all(contents)?;
            file.sync_all()?;
        }
        fs::rename(&staging, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn plan_new_firmware_vars(
    source: &Path,
    destination: &Path,
    steps: &mut Vec<PreparationStep>,
    missing_capabilities: &mut Vec<&'static str>,
    source_available: bool,
) -> Result<()> {
    if path_is_missing(destination, "WinPE firmware variable store")? {
        steps.push(PreparationStep::CopyFirmwareVariables {
            source: source.to_owned(),
            destination: destination.to_owned(),
        });
        if !source_available {
            missing_capabilities.push("OVMF variable template");
        }
    } else {
        require_regular_file(destination, "WinPE firmware variable store")?;
    }
    Ok(())
}

fn path_is_missing(path: &Path, field: &'static str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(LswError::InvalidValue {
            field,
            reason: format!("{} must not be a symbolic link", path.display()),
        }),
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn require_regular_file(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a regular file", path.display()),
        })
    }
}

fn prepare_output_file(path: &Path) -> Result<fs::File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            return Err(LswError::InvalidValue {
                field: "WinPE log",
                reason: format!("{} is not a regular file", path.display()),
            })
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    set_private_file_permissions(path)?;
    Ok(file)
}

fn read_status_events(path: &Path) -> Result<Vec<String>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(LswError::InvalidValue {
            field: "WinPE status log",
            reason: format!("{} is not a regular file", path.display()),
        });
    }
    if metadata.len() > MAX_STATUS_LOG_BYTES {
        return Err(LswError::InvalidValue {
            field: "WinPE status log",
            reason: format!("{} exceeds 1 MiB", path.display()),
        });
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(path)?
        .take(MAX_STATUS_LOG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(|line| line.trim_matches(|character: char| character.is_control()))
        .filter(|line| line.contains("LSW-WINPE-DISM "))
        .map(str::to_owned)
        .collect())
}

fn report_winpe_progress<F>(
    plan: &WinPeDismVmPlan,
    started: Instant,
    observed_status_events: &mut usize,
    current_stage: &mut Option<String>,
    last_percent: &mut Option<u8>,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(&WinPeDismProgress),
{
    let mut emitted = false;
    let events = read_status_events(&plan.status_log)?;
    if events.len() < *observed_status_events {
        *observed_status_events = 0;
    }
    for event in events.iter().skip(*observed_status_events) {
        let Some(stage) = event.strip_prefix("LSW-WINPE-DISM ") else {
            continue;
        };
        let stage = stage.trim().to_owned();
        *current_stage = Some(stage.clone());
        *last_percent = None;
        on_progress(&WinPeDismProgress {
            phase: plan.phase,
            stage,
            percent: None,
            elapsed: started.elapsed(),
        });
        emitted = true;
    }
    *observed_status_events = events.len();

    if let Some((dism_stage, percent)) = read_dism_progress(&plan.dism_log)? {
        if current_stage.as_deref() == Some(dism_stage.as_str())
            && last_percent.as_ref() != Some(&percent)
        {
            *last_percent = Some(percent);
            on_progress(&WinPeDismProgress {
                phase: plan.phase,
                stage: dism_stage,
                percent: Some(percent),
                elapsed: started.elapsed(),
            });
            emitted = true;
        }
    }
    if !emitted {
        if let Some(stage) = current_stage.as_ref() {
            on_progress(&WinPeDismProgress {
                phase: plan.phase,
                stage: stage.clone(),
                percent: *last_percent,
                elapsed: started.elapsed(),
            });
        }
    }
    Ok(())
}

fn read_dism_progress(path: &Path) -> Result<Option<(String, u8)>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(LswError::InvalidValue {
            field: "WinPE DISM log",
            reason: format!("{} is not a regular file", path.display()),
        });
    }
    if metadata.len() > MAX_DISM_LOG_BYTES {
        return Err(LswError::InvalidValue {
            field: "WinPE DISM log",
            reason: format!("{} exceeds 8 MiB", path.display()),
        });
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(path)?
        .take(MAX_DISM_LOG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let log = String::from_utf8_lossy(&bytes);
    let Some(stage_offset) = log.rfind("LSW-DISM-STAGE ") else {
        return Ok(None);
    };
    let stage_and_output = &log[stage_offset + "LSW-DISM-STAGE ".len()..];
    let Some(stage_end) = stage_and_output.find(['\r', '\n']) else {
        return Ok(None);
    };
    let stage = stage_and_output[..stage_end].trim();
    if stage.is_empty() {
        return Ok(None);
    }
    let output = &stage_and_output[stage_end..];
    let Some(command_offset) = output.find("LSW-DISM-COMMAND ") else {
        return Ok(None);
    };
    let command_output = &output[command_offset + "LSW-DISM-COMMAND ".len()..];
    let percent = command_output
        .match_indices('%')
        .filter_map(|(offset, _)| parse_percentage_before(&command_output[..offset]))
        .next_back();
    Ok(percent.map(|percent| (stage.to_owned(), percent)))
}

fn parse_percentage_before(value: &str) -> Option<u8> {
    let candidate = value
        .chars()
        .rev()
        .skip_while(|character| character.is_ascii_whitespace())
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let percent = candidate.parse::<f32>().ok()?;
    (percent.is_finite() && (0.0..=100.0).contains(&percent)).then_some(percent.floor() as u8)
}

fn validate_applied_target(qemu_img: &Path, target_disk: &Path) -> Result<()> {
    require_regular_file(target_disk, "applied target disk")?;
    let length = fs::metadata(target_disk)?.len();
    if length < MIN_APPLIED_DISK_BYTES {
        return Err(LswError::InvalidValue {
            field: "WinPE DISM apply",
            reason: format!(
                "{} is only {length} bytes after apply; refusing a false completion marker",
                target_disk.display()
            ),
        });
    }
    run_control_command(
        qemu_img,
        &["check".into(), target_disk.as_os_str().to_owned()],
        None,
    )
}

fn prepare_status_volume(status_log: &Path, dism_log: &Path) -> Result<()> {
    let directory = status_log.parent().ok_or_else(|| LswError::InvalidValue {
        field: "WinPE status volume",
        reason: "status log has no parent directory".to_owned(),
    })?;
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(LswError::InvalidValue {
                field: "WinPE status volume",
                reason: format!("{} is not a real directory", directory.display()),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(directory)?;
            set_private_directory_permissions(directory)?;
        }
        Err(error) => return Err(error.into()),
    }

    let tag = directory.join(WINPE_STATUS_TAG);
    if path_is_missing(&tag, "WinPE status tag")? {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tag)?;
        set_private_file_permissions(&tag)?;
        file.write_all(b"LSW private WinPE status volume\r\n")?;
        file.sync_all()?;
    } else {
        require_regular_file(&tag, "WinPE status tag")?;
    }
    prepare_output_file(status_log)?;
    prepare_output_file(dism_log)?;
    Ok(())
}

fn prepare_control_media(plan: &WinPeControlMediaPlan) -> Result<()> {
    match fs::symlink_metadata(&plan.destination) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            return Ok(())
        }
        Ok(_) => {
            return Err(LswError::InvalidValue {
                field: "WinPE control ISO",
                reason: format!("{} is not a regular file", plan.destination.display()),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    require_regular_file(&plan.source_iso, "Windows source ISO")?;
    if fs::symlink_metadata(&plan.root).is_ok() {
        return Err(LswError::InvalidValue {
            field: "WinPE control media staging",
            reason: format!("{} already exists", plan.root.display()),
        });
    }
    fs::create_dir(&plan.root)?;
    set_private_directory_permissions(&plan.root)?;

    let temporary = plan
        .destination
        .with_extension(format!("iso.tmp-{}", std::process::id()));
    if fs::symlink_metadata(&temporary).is_ok() {
        let _ = fs::remove_dir_all(&plan.root);
        return Err(LswError::InvalidValue {
            field: "WinPE control ISO",
            reason: format!("temporary path {} already exists", temporary.display()),
        });
    }

    let result = (|| {
        run_control_command(
            &plan.seven_zip,
            &[
                "x".into(),
                "-y".into(),
                "-bd".into(),
                "-bso0".into(),
                "-bsp0".into(),
                format!("-o{}", plan.root.display()).into(),
                plan.source_iso.as_os_str().to_owned(),
                "boot/*".into(),
                "efi/*".into(),
                "sources/boot.wim".into(),
                "bootmgr".into(),
                "bootmgr.efi".into(),
            ],
            None,
        )?;
        let boot_wim = plan.root.join("sources/boot.wim");
        let bios_boot = plan.root.join("boot/etfsboot.com");
        let uefi_boot = plan.root.join("efi/microsoft/boot/efisys_noprompt.bin");
        for (path, field) in [
            (&boot_wim, "Windows PE boot.wim"),
            (&bios_boot, "Windows BIOS boot image"),
            (&uefi_boot, "Windows UEFI no-prompt boot image"),
        ] {
            require_regular_file(path, field)?;
        }

        let startnet = plan.root.join("lsw-startnet.cmd");
        let shell = plan.root.join("lsw-winpeshl.ini");
        write_private_new_file(&startnet, WINPE_STARTNET)?;
        write_private_new_file(&shell, WINPE_SHELL)?;
        for (source, destination) in [
            ("lsw-startnet.cmd", "/Windows/System32/startnet.cmd"),
            ("lsw-winpeshl.ini", "/Windows/System32/winpeshl.ini"),
        ] {
            run_control_command(
                &plan.wimlib_imagex,
                &[
                    "update".into(),
                    boot_wim.as_os_str().to_owned(),
                    "2".into(),
                    "--check".into(),
                    format!("--command=add {source} {destination}").into(),
                ],
                Some(&plan.root),
            )?;
        }
        fs::remove_file(startnet)?;
        fs::remove_file(shell)?;

        run_control_command(
            &plan.xorriso,
            &[
                "-as".into(),
                "mkisofs".into(),
                "-iso-level".into(),
                "3".into(),
                "-full-iso9660-filenames".into(),
                "-volid".into(),
                "LSW_WINPE".into(),
                "-eltorito-boot".into(),
                "boot/etfsboot.com".into(),
                "-no-emul-boot".into(),
                "-boot-load-size".into(),
                "8".into(),
                "-eltorito-alt-boot".into(),
                "-e".into(),
                "efi/microsoft/boot/efisys_noprompt.bin".into(),
                "-no-emul-boot".into(),
                "-output".into(),
                temporary.as_os_str().to_owned(),
                ".".into(),
            ],
            Some(&plan.root),
        )?;
        require_regular_file(&temporary, "temporary WinPE control ISO")?;
        set_private_file_permissions(&temporary)?;
        fs::rename(&temporary, &plan.destination)?;
        Ok(())
    })();

    let _ = fs::remove_dir_all(&plan.root);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn run_control_command(program: &Path, arguments: &[OsString], cwd: Option<&Path>) -> Result<()> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(LswError::ExternalCommandFailed {
            program: program.to_owned(),
            status: status.code(),
        })
    }
}

fn write_private_new_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    set_private_file_permissions(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
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

fn workspace_diskpart() -> String {
    format!(
        "select disk {WINPE_WORKSPACE_DISK_ID}\r\nclean\r\nconvert gpt\r\ncreate partition primary\r\nformat fs=ntfs quick label=LSW-WORK\r\nassign letter={}\r\nexit\r\n",
        WINPE_WORKSPACE_DRIVE.trim_end_matches(':')
    )
}

fn target_diskpart() -> String {
    format!(
        "select disk {WINPE_TARGET_DISK_ID}\r\nclean\r\nconvert gpt\r\ncreate partition efi size=260\r\nformat fs=fat32 quick label=System\r\nassign letter=S\r\ncreate partition msr size=16\r\ncreate partition primary\r\nformat fs=ntfs quick label=Windows\r\nassign letter=T\r\nexit\r\n"
    )
}

fn apply_script() -> String {
    format!(
        r#"@echo off
setlocal EnableExtensions EnableDelayedExpansion
set "LSW_SEED=%~d0"
set "LSW_LOG="
set "LSW_IMAGE="
set "LSW_DISM=%SystemRoot%\System32\dism.exe"
set "LSW_STATUS="
for %%D in (C D E F G H I J K L M N O P Q R S T U V W X Y Z) do if exist "%%D:\lsw-status.tag" set "LSW_STATUS=%%D:"
if not defined LSW_STATUS (
    wpeutil.exe shutdown
    exit /b 1
)
set "LSW_LOG=%LSW_STATUS%\dism.log"

for %%D in (C D E F G H I J K L M N O P Q R S U V W X Y Z) do (
    if not defined LSW_IMAGE if exist "%%D:\{WINPE_PREPARED_IMAGE_NAME}" (
        set "LSW_IMAGE=%%D:\{WINPE_PREPARED_IMAGE_NAME}"
    )
)
if not defined LSW_IMAGE goto :fail
if /i "%LSW_IMAGE:~0,2%"=="%LSW_SEED%" goto :fail
if not exist "%LSW_IMAGE%" goto :fail

call :status initialize-target
diskpart.exe /s "%LSW_SEED%\lsw\target.diskpart" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail
if not exist "T:\" goto :fail
if not exist "S:\" goto :fail

call :status apply-image
call :run "%LSW_DISM%" /English /Apply-Image /ImageFile:"%LSW_IMAGE%" /Index:1 /ApplyDir:T:\ /CheckIntegrity
if errorlevel 1 goto :fail
if not exist "T:\Windows\System32\bcdboot.exe" goto :fail

call :status configure-boot
call :run "T:\Windows\System32\bcdboot.exe" T:\Windows /s S: /f UEFI
if errorlevel 1 goto :fail

call :retain_log
call :status apply-complete
call :flush_status
wpeutil.exe shutdown
exit /b 0

:run
>>"%LSW_LOG%" echo LSW-DISM-COMMAND %*
%* >>"%LSW_LOG%" 2>&1
set "LSW_EXIT=!errorlevel!"
if not "!LSW_EXIT!"=="0" >>"%LSW_LOG%" echo command failed with exit code !LSW_EXIT!
exit /b !LSW_EXIT!

:fail
call :retain_log
call :status apply-failed
call :flush_status
if defined LSW_LOG >>"%LSW_LOG%" echo WinPE apply failed
wpeutil.exe shutdown
exit /b 1

:retain_log
exit /b 0

:flush_status
if exist "%SystemRoot%\System32\timeout.exe" (
    timeout.exe /t 2 /nobreak >nul 2>&1
) else (
    ping.exe -n 3 127.0.0.1 >nul 2>&1
)
exit /b 0

:status
>>"%LSW_STATUS%\status.log" echo LSW-WINPE-DISM %*
if defined LSW_LOG >>"%LSW_LOG%" echo LSW-DISM-STAGE %*
exit /b 0
"#
    )
}

fn offline_unattend(
    manifest: &InstanceManifest,
    locale: &str,
    setup_account_password_value: &str,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="specialize">
    <component name="Microsoft-Windows-Deployment" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <RunSynchronous>
        <RunSynchronousCommand wcm:action="add">
          <Order>1</Order><Description>Install LSW guest agent service</Description>
          <Path>powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "C:\ProgramData\LSW\setup\install-agent.ps1"</Path>
        </RunSynchronousCommand>
      </RunSynchronous>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <ComputerName>{computer_name}</ComputerName>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <InputLocale>{locale}</InputLocale><SystemLocale>{locale}</SystemLocale><UILanguage>{locale}</UILanguage><UserLocale>{locale}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <RegisteredOrganization>LSW</RegisteredOrganization>
      <RegisteredOwner>LSW User</RegisteredOwner>
      <TimeZone>UTC</TimeZone>
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Password><Value>{setup_account_password_value}</Value><PlainText>false</PlainText></Password>
            <Description>Temporary account removed when unattended setup completes</Description>
            <DisplayName>LSW Setup</DisplayName><Group>Users</Group><Name>{setup_account_name}</Name>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
    </component>
  </settings>
</unattend>
"#,
        locale = xml_escape(locale),
        computer_name = windows_computer_name(&manifest.spec.name),
        setup_account_name = SETUP_ACCOUNT_NAME,
        setup_account_password_value = xml_escape(setup_account_password_value),
    )
}

fn validate_unattend_password_value(value: &str) -> Result<()> {
    let padding = value.bytes().rev().take_while(|byte| *byte == b'=').count();
    if (16..=512).contains(&value.len())
        && value.len() % 4 == 0
        && padding <= 2
        && value[..value.len() - padding]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
    {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "setup account password value",
            reason: "must be a bounded base64 value produced by the install seed".to_owned(),
        })
    }
}

fn winpe_script(
    edition_index: u32,
    customization: &CustomizationPlan,
    stage_guest_setup: bool,
) -> String {
    let removal_patterns = customization
        .remove_provisioned_appx_patterns
        .iter()
        .map(|pattern| {
            format!("call :remove_if_present \"{pattern}\"\r\nif errorlevel 1 goto :fail_mounted")
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    let removal = if removal_patterns.is_empty() {
        "call :status no-appx-removals".to_owned()
    } else {
        removal_patterns
    };
    let guest_setup = if stage_guest_setup {
        format!(
            r#"call :status stage-guest-setup
mkdir "%LSW_MOUNT%\ProgramData\LSW\setup" "%LSW_MOUNT%\Windows\Panther" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
xcopy.exe "%LSW_SEED%\payload\lsw\*" "%LSW_MOUNT%\ProgramData\LSW\setup\" /E /H /K /Y /I >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
>"%LSW_MOUNT%\ProgramData\LSW\setup\{OFFLINE_APPX_MARKER_NAME}" echo LSW-OFFLINE-APPX-APPLIED
if errorlevel 1 goto :fail_mounted
copy /Y "%LSW_SEED%\lsw\offline-unattend.xml" "%LSW_MOUNT%\Windows\Panther\unattend.xml" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
icacls.exe "%LSW_MOUNT%\ProgramData\LSW\setup" /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
"#
        )
    } else {
        String::new()
    };

    format!(
        r#"@echo off
setlocal EnableExtensions EnableDelayedExpansion
set "LSW_SEED=%~d0"
set "LSW_WORK={WINPE_WORKSPACE_DRIVE}"
set "LSW_MOUNT={WINPE_WORKSPACE_DRIVE}\mount"
set "LSW_SCRATCH={WINPE_WORKSPACE_DRIVE}\scratch"
set "LSW_LOG="
set "LSW_PACKAGES={WINPE_WORKSPACE_DRIVE}\logs\provisioned-appx.txt"
set "LSW_IMAGE={WINPE_WORKSPACE_DRIVE}\{WINPE_PREPARED_IMAGE_NAME}"
set "LSW_DISM=%SystemRoot%\System32\dism.exe"
set "LSW_STATUS="
for %%D in (C D E F G H I J K L M N O P Q R S T U V W X Y Z) do if exist "%%D:\lsw-status.tag" set "LSW_STATUS=%%D:"
if not defined LSW_STATUS (
    wpeutil.exe shutdown
    exit /b 1
)
set "LSW_LOG=%LSW_STATUS%\dism.log"

call :status initialize-workspace
diskpart.exe /s "%LSW_SEED%\lsw\workspace.diskpart" > X:\lsw-workspace.log 2>&1
if errorlevel 1 goto :fail
mkdir "%LSW_MOUNT%" "%LSW_SCRATCH%" "{WINPE_WORKSPACE_DRIVE}\logs" >nul 2>&1
if errorlevel 1 goto :fail

set "LSW_SOURCE="
for %%D in (C D E F G H I J K L M N O P Q R S T U V X Y Z) do (
    if not defined LSW_SOURCE if exist "%%D:\sources\install.wim" set "LSW_SOURCE=%%D:\sources\install.wim"
    if not defined LSW_SOURCE if exist "%%D:\sources\install.esd" set "LSW_SOURCE=%%D:\sources\install.esd"
)
if not defined LSW_SOURCE (
    >>"%LSW_LOG%" echo official media has no sources\install.wim or sources\install.esd
    goto :fail
)

call :status export-image
call :run "%LSW_DISM%" /English /Export-Image /SourceImageFile:"%LSW_SOURCE%" /SourceIndex:{edition_index} /DestinationImageFile:"%LSW_IMAGE%" /Compress:max /ScratchDir:"%LSW_SCRATCH%" /CheckIntegrity
if errorlevel 1 goto :fail

call :status mount-image
call :run "%LSW_DISM%" /English /Mount-Image /ImageFile:"%LSW_IMAGE%" /Index:1 /MountDir:"%LSW_MOUNT%" /ScratchDir:"%LSW_SCRATCH%" /CheckIntegrity
if errorlevel 1 goto :fail_mounted

call :status inventory-appx
"%LSW_DISM%" /English /Image:"%LSW_MOUNT%" /Get-ProvisionedAppxPackages >"%LSW_PACKAGES%" 2>>"%LSW_LOG%"
if errorlevel 1 goto :fail_mounted
{removal}

{guest_setup}
call :status commit-image
call :run "%LSW_DISM%" /English /Unmount-Image /MountDir:"%LSW_MOUNT%" /Commit /CheckIntegrity
if errorlevel 1 goto :fail_mounted
call :retain_log
call :status complete
call :flush_status
wpeutil.exe shutdown
exit /b 0

:remove_if_present
set "LSW_DISPLAY_NAME=%~1"
for /f "tokens=2 delims=:" %%P in ('findstr.exe /b /c:"PackageName :" "%LSW_PACKAGES%"') do (
    set "LSW_PACKAGE=%%P"
    for /f "tokens=*" %%Q in ("!LSW_PACKAGE!") do set "LSW_PACKAGE=%%Q"
    echo(!LSW_PACKAGE!| findstr.exe /i /b /l /c:"!LSW_DISPLAY_NAME!_" >nul
    if not errorlevel 1 (
        call :status remove-appx !LSW_DISPLAY_NAME!
        call :run "%LSW_DISM%" /English /Image:"%LSW_MOUNT%" /Remove-ProvisionedAppxPackage /PackageName:!LSW_PACKAGE!
        if errorlevel 1 exit /b 1
    )
)
exit /b 0

:run
>>"%LSW_LOG%" echo LSW-DISM-COMMAND %*
%* >>"%LSW_LOG%" 2>&1
set "LSW_EXIT=!errorlevel!"
if not "!LSW_EXIT!"=="0" >>"%LSW_LOG%" echo command failed with exit code !LSW_EXIT!
exit /b !LSW_EXIT!

:fail_mounted
call :status discard-image
"%LSW_DISM%" /English /Unmount-Image /MountDir:"%LSW_MOUNT%" /Discard >>"%LSW_LOG%" 2>&1

:fail
call :retain_log
call :status failed
call :flush_status
wpeutil.exe shutdown
exit /b 1

:retain_log
exit /b 0

:flush_status
if exist "%SystemRoot%\System32\timeout.exe" (
    timeout.exe /t 2 /nobreak >nul 2>&1
) else (
    ping.exe -n 3 127.0.0.1 >nul 2>&1
)
exit /b 0

:status
>>"%LSW_STATUS%\status.log" echo LSW-WINPE-DISM %*
if defined LSW_LOG >>"%LSW_LOG%" echo LSW-DISM-STAGE %*
exit /b 0
"#
    )
}

fn seed_readme(
    profile: WindowsProfile,
    edition_index: u32,
    customization: &CustomizationPlan,
) -> String {
    format!(
        "LSW WinPE DISM preparation seed\r\n\r\nProfile: {profile}\r\nEdition index: {edition_index}\r\nOutput: {WINPE_WORKSPACE_DRIVE}\\{WINPE_PREPARED_IMAGE_NAME}\r\n\r\nThis seed contains no Windows image, product key, activation data, or Microsoft binary. The one-shot plan may add the private per-instance agent token and setup payload.\r\nIt must be booted only in LSW's isolated preparation VM with a blank LSW-owned virtual Disk {WINPE_WORKSPACE_DISK_ID}.\r\nThe script uses dism.exe from the official Windows ISO's WinPE environment. Linux wimlib is not used for Windows package, AppX, or feature servicing.\r\nCompactOS during Windows setup: {}\r\n",
        if customization.compact_os { "yes" } else { "no" }
    )
}

fn apply_seed_readme(profile: WindowsProfile) -> String {
    format!(
        "LSW WinPE DISM apply seed\r\n\r\nProfile: {profile}\r\nInput: {WINPE_PREPARED_IMAGE_NAME}\r\nTarget: virtual Disk {WINPE_TARGET_DISK_ID}\r\nCompact-on-apply: no\r\n\r\nCompactOS, when requested by the profile, runs during Windows setup after the target image is safely applied.\r\nThis seed contains no Windows image, product key, activation data, or agent token.\r\nIt must be booted only with the LSW workspace as virtual Disk 0 and a new LSW-owned target qcow2 as virtual Disk {WINPE_TARGET_DISK_ID}. Never attach a host block device.\r\n"
    )
}

fn copy_guest_setup_payload(
    generated: &mut BTreeMap<PathBuf, Vec<u8>>,
    install_seed: &Path,
) -> Result<bool> {
    let mut includes_agent = false;
    for relative in [
        "lsw/agent.token",
        "lsw/instance.txt",
        "lsw/install-agent.ps1",
        "lsw/license-helper.ps1",
        "lsw/apply-profile.ps1",
        "lsw/lsw-agent.exe",
    ] {
        let source = install_seed.join(relative);
        match read_seed_payload(&source)? {
            Some(contents) => {
                if relative.ends_with("lsw-agent.exe") {
                    includes_agent = true;
                }
                generated.insert(PathBuf::from("payload").join(relative), contents);
            }
            None if relative.ends_with("lsw-agent.exe") => {}
            None => {
                return Err(LswError::InvalidValue {
                    field: "install seed",
                    reason: format!("{} is missing", source.display()),
                })
            }
        }
    }
    Ok(includes_agent)
}

fn read_seed_payload(path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(LswError::InvalidValue {
            field: "install seed payload",
            reason: format!("{} is not a regular file", path.display()),
        });
    }
    if metadata.len() > MAX_SEED_PAYLOAD_BYTES {
        return Err(LswError::InvalidValue {
            field: "install seed payload",
            reason: format!("{} exceeds 64 MiB", path.display()),
        });
    }
    Ok(Some(fs::read(path)?))
}

fn validate_locale(locale: &str) -> Result<()> {
    if (2..=20).contains(&locale.len())
        && locale
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && locale.bytes().any(|byte| byte == b'-')
    {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "locale",
            reason: "must look like en-US or zh-HK".to_owned(),
        })
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn windows_computer_name(instance: &str) -> String {
    let mut name = format!("LSW-{}", instance.to_ascii_uppercase())
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(15)
        .collect::<String>();
    while name.ends_with('-') {
        name.pop();
    }
    name
}

fn validate_appx_patterns(patterns: &[String]) -> Result<()> {
    if patterns.iter().all(|pattern| {
        !pattern.is_empty()
            && pattern.len() <= 128
            && pattern
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    }) {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "AppX removal pattern",
            reason: "must contain only ASCII letters, digits, dot, dash, or underscore".to_owned(),
        })
    }
}

fn require_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "instance directory",
            reason: format!("{} is not a real directory", path.display()),
        })
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::{
        InstallSeedBuilder, InstallSeedOptions, InstanceSpec, NetworkMode, WindowsProfile,
    };

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let fixture_id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "lsw-winpe-dism-test-{}-{nonce}-{fixture_id}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("run")).expect("fixture should be created");
        root
    }

    fn manifest(root: &Path, profile: WindowsProfile) -> InstanceManifest {
        let iso = root.join("windows.iso");
        fs::write(&iso, b"media").expect("ISO fixture should be written");
        InstanceManifest::new(InstanceSpec {
            name: "win-dev".to_owned(),
            source_iso: iso,
            profile,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid")
    }

    fn install_seed(root: &Path, manifest: &InstanceManifest) -> (PathBuf, String) {
        let agent = root.join("lsw-agent.exe");
        fs::write(&agent, b"MZfixture agent").expect("agent fixture should be written");
        let options = InstallSeedOptions {
            unattended_image_name: Some("Windows 11 Pro".to_owned()),
            agent_binary: Some(agent),
            ..InstallSeedOptions::default()
        };
        let plan = InstallSeedBuilder::plan(manifest, root, &"a".repeat(64), &options)
            .expect("install seed should be planned");
        let setup_account_password_value = plan.setup_account_password_value().to_owned();
        InstallSeedBuilder::apply(&plan).expect("install seed should be written");
        (root.join("seed"), setup_account_password_value)
    }

    fn vm_capabilities(root: &Path) -> HostCapabilities {
        let mut capabilities = HostCapabilities::unavailable(crate::HostPlatform::Linux);
        let qemu = root.join("qemu-system-x86_64");
        let qemu_img = root.join("qemu-img");
        let seven_zip = root.join("7z");
        let wimlib = root.join("wimlib-imagex");
        let xorriso = root.join("xorriso");
        let code = root.join("OVMF_CODE.fd");
        let vars = root.join("OVMF_VARS.fd");
        for path in [
            &qemu, &qemu_img, &seven_zip, &wimlib, &xorriso, &code, &vars,
        ] {
            fs::write(path, b"fixture").expect("capability fixture should be written");
        }
        capabilities.qemu_system = Some(qemu);
        capabilities.qemu_img = Some(qemu_img);
        capabilities.seven_zip = Some(seven_zip);
        capabilities.wimlib_imagex = Some(wimlib);
        capabilities.xorriso = Some(xorriso);
        capabilities.ovmf_code = Some(code);
        capabilities.ovmf_vars = Some(vars);
        capabilities
    }

    #[test]
    fn slim_plan_uses_only_windows_dism_for_offline_servicing() {
        let root = fixture();
        let plan = WinPeDismBackend::plan(WindowsProfile::Slim, 6, &root)
            .expect("slim plan should be generated");
        let script = plan.script();

        assert!(script.contains("dism.exe"));
        assert!(script.contains("/English /Export-Image"));
        assert!(script.contains("/Compress:max /ScratchDir:\"%LSW_SCRATCH%\" /CheckIntegrity"));
        assert!(script.contains("/SourceIndex:6"));
        assert!(script.contains("/Mount-Image"));
        assert!(script.contains("/ScratchDir:\"%LSW_SCRATCH%\" /CheckIntegrity"));
        assert!(script.contains("/Get-ProvisionedAppxPackages"));
        assert!(script.contains("/Remove-ProvisionedAppxPackage"));
        assert!(script.contains("/Unmount-Image"));
        assert!(script.contains("/Commit /CheckIntegrity"));
        assert!(script.contains("call :status complete\ncall :flush_status"));
        assert!(script.contains("timeout.exe /t 2 /nobreak"));
        assert!(!script.contains("wimlib"));
        assert!(!script.contains("powershell"));
        assert!(!script.contains("/Remove-Package"));
        assert!(!script.contains("/Disable-Feature"));
        assert!(plan.compact_during_setup);
        assert!(plan.stages.iter().any(|stage| matches!(
            stage,
            WinPeDismStage::RemoveProvisionedAppx { display_name }
                if *display_name == "Clipchamp.Clipchamp"
        )));

        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn stock_profile_exports_without_removing_packages() {
        let root = fixture();
        let plan = WinPeDismBackend::plan(WindowsProfile::Vanilla, 1, &root)
            .expect("vanilla plan should be generated");
        assert!(!plan.compact_during_setup);
        assert!(!plan
            .stages
            .iter()
            .any(|stage| matches!(stage, WinPeDismStage::RemoveProvisionedAppx { .. })));
        assert!(!plan.script().contains("call :remove_if_present \""));
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn seed_is_atomic_and_does_not_contain_license_secrets() {
        let root = fixture();
        let plan = WinPeDismBackend::plan(WindowsProfile::Slim, 6, &root)
            .expect("slim plan should be generated");
        WinPeDismBackend::write_seed(&plan).expect("seed should be written");

        assert!(!root.join("winpe-seed/Autounattend.xml").exists());
        assert!(root.join("winpe-seed/lsw/winpe-dism.cmd").is_file());
        assert!(plan.script().contains("lsw-status.tag"));
        assert!(plan.script().contains("status.log"));
        assert!(!plan.script().contains("ProductKey"));
        assert!(WinPeDismBackend::write_seed(&plan).is_err());

        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn zero_image_index_is_rejected() {
        let root = fixture();
        assert!(WinPeDismBackend::plan(WindowsProfile::Slim, 0, &root).is_err());
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn prepare_plan_stages_guest_setup_inside_the_wim() {
        let root = fixture();
        let manifest = manifest(&root, WindowsProfile::Slim);
        let (install_seed, setup_account_password_value) = install_seed(&root, &manifest);
        let plan = WinPeDismBackend::plan_with_guest_setup(
            &manifest,
            6,
            &root,
            &install_seed,
            "zh-HK",
            &setup_account_password_value,
        )
        .expect("prepare plan with guest setup should be generated");
        let script = plan.script();

        assert!(plan.includes_agent);
        assert!(script.contains("icacls.exe"));
        let stage = script
            .find("call :status stage-guest-setup")
            .expect("guest payload should be staged");
        let commit = script
            .find("call :status commit-image")
            .expect("prepared image should be committed");
        assert!(stage < commit);
        assert!(script.contains("%LSW_MOUNT%\\Windows\\Panther\\unattend.xml"));
        assert!(script.contains(OFFLINE_APPX_MARKER_NAME));
        assert!(script.contains("LSW-OFFLINE-APPX-APPLIED"));

        let unattend = String::from_utf8(
            plan.generated
                .get(Path::new("lsw/offline-unattend.xml"))
                .expect("offline unattend should exist")
                .clone(),
        )
        .expect("offline unattend should be UTF-8");
        assert!(!plan.generated.contains_key(Path::new("Autounattend.xml")));
        assert!(unattend.contains("<InputLocale>zh-HK</InputLocale>"));
        assert!(unattend.contains("<settings pass=\"specialize\">"));
        assert!(unattend.contains("<RunSynchronous>"));
        assert!(unattend.contains("C:\\ProgramData\\LSW\\setup\\install-agent.ps1"));
        assert!(!unattend.contains("FirstLogonCommands"));
        assert!(unattend.contains("<HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>"));
        assert!(unattend.contains("<HideOnlineAccountScreens>true</HideOnlineAccountScreens>"));
        assert!(unattend.contains("<ProtectYourPC>3</ProtectYourPC>"));
        assert!(unattend.contains("<Name>LSWSetup</Name>"));
        assert!(unattend.contains("<Group>Users</Group>"));
        assert!(unattend.contains("<PlainText>false</PlainText>"));
        assert!(unattend.contains(&setup_account_password_value));
        assert!(!unattend.contains("AutoLogon"));
        assert!(!unattend.contains("SkipMachineOOBE"));
        let computer_name = unattend
            .find("<ComputerName>")
            .expect("computer name should exist");
        let oobe = unattend
            .find("<settings pass=\"oobeSystem\">")
            .expect("OOBE pass should exist");
        assert!(computer_name < oobe);
        assert!(!unattend.contains("ProductKey"));
        assert!(plan
            .generated
            .contains_key(Path::new("payload/lsw/agent.token")));
        assert!(plan
            .generated
            .contains_key(Path::new("payload/lsw/license-helper.ps1")));
        assert!(plan
            .generated
            .contains_key(Path::new("payload/lsw/lsw-agent.exe")));

        WinPeDismBackend::write_seed(&plan).expect("prepare seed should be written");
        assert!(root.join("winpe-seed/payload/lsw/agent.token").is_file());
        assert!(WinPeDismBackend::write_seed(&plan).is_err());
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn apply_plan_wipes_only_the_second_virtual_disk() {
        let root = fixture();
        let manifest = manifest(&root, WindowsProfile::Slim);
        let plan =
            WinPeDismBackend::plan_apply(&manifest, &root).expect("apply plan should be generated");
        let script = plan.script();

        assert_eq!(plan.target_disk_id, 1);
        assert!(script.contains("/Apply-Image"));
        assert!(!script.contains("/Compact:on"));
        assert!(script.contains("/ApplyDir:T:\\"));
        assert!(script.contains("bcdboot.exe"));
        assert!(script.contains("/s S: /f UEFI"));
        assert!(script.contains("call :status apply-complete\ncall :flush_status"));
        assert!(!script.contains("stage-guest-setup"));
        assert!(!script.contains("select disk"));

        let diskpart = String::from_utf8(
            plan.generated
                .get(Path::new("lsw/target.diskpart"))
                .expect("target diskpart script should exist")
                .clone(),
        )
        .expect("diskpart script should be UTF-8");
        assert!(diskpart.starts_with("select disk 1\r\nclean\r\n"));
        assert!(!diskpart.contains("select disk 0"));

        WinPeDismBackend::write_apply_seed(&plan).expect("apply seed should be written");
        assert!(!root
            .join("winpe-apply-seed/payload/lsw/agent.token")
            .exists());
        assert!(WinPeDismBackend::write_apply_seed(&plan).is_err());
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn every_apply_defers_compact_os_until_windows_setup() {
        let root = fixture();
        let manifest = manifest(&root, WindowsProfile::Vanilla);
        let plan =
            WinPeDismBackend::plan_apply(&manifest, &root).expect("apply plan should be generated");
        assert!(!plan.script().contains("/Compact:on"));
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn vm_plans_keep_prepare_and_apply_disk_topologies_separate() {
        let root = fixture();
        let manifest = manifest(&root, WindowsProfile::Slim);
        let prepare_seed = WinPeDismBackend::plan(WindowsProfile::Slim, 6, &root)
            .expect("prepare seed should plan");
        WinPeDismBackend::write_seed(&prepare_seed).expect("prepare seed should be written");
        let capabilities = vm_capabilities(&root);

        let prepare = WinPeDismBackend::plan_vm(
            capabilities.clone(),
            &manifest,
            &root,
            WinPeDismVmPhase::Prepare,
        )
        .expect("prepare VM should plan");
        let prepare_command = prepare.display_command();
        assert!(prepare_command.contains("id=workspace"));
        assert!(prepare_command.contains("serial=lsw-winpe-workspace"));
        assert!(!prepare_command.contains("id=target"));
        assert!(!prepare_command.contains("disk.qcow2"));
        assert!(prepare_command.contains("-nic none"));
        assert!(prepare_command.contains("winpe-seed"));
        assert!(prepare_command.contains("winpe-control.iso"));
        assert!(prepare_command.contains("fat:rw:"));
        assert_eq!(
            prepare.dism_log,
            root.join("run/winpe-prepare-status/dism.log")
        );
        assert!(!prepare_command.contains("tpm-tis"));
        assert!(prepare
            .host_preparation
            .steps
            .iter()
            .any(|step| matches!(step, PreparationStep::CreateDisk { size_gib: 32, .. })));

        fs::write(root.join("run/winpe-workspace.qcow2"), b"workspace")
            .expect("workspace fixture should be written");
        fs::write(root.join("disk.qcow2"), b"target").expect("target fixture should be written");
        let apply_seed =
            WinPeDismBackend::plan_apply(&manifest, &root).expect("apply seed should plan");
        WinPeDismBackend::write_apply_seed(&apply_seed).expect("apply seed should be written");
        let apply =
            WinPeDismBackend::plan_vm(capabilities, &manifest, &root, WinPeDismVmPhase::Apply)
                .expect("apply VM should plan");
        let apply_command = apply.display_command();
        let workspace = apply_command
            .find("id=workspace")
            .expect("workspace disk should be present");
        let target = apply_command
            .find("id=target")
            .expect("target disk should be present");
        assert!(workspace < target);
        assert!(apply_command.contains("serial=lsw-system"));
        assert!(apply_command.contains("winpe-apply-seed"));
        assert!(apply_command.contains("-nic none"));

        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn vm_runner_requires_the_phase_completion_marker() {
        let root = fixture();
        let manifest = manifest(&root, WindowsProfile::Slim);
        let prepare_seed = WinPeDismBackend::plan(WindowsProfile::Slim, 6, &root)
            .expect("prepare seed should plan");
        WinPeDismBackend::write_seed(&prepare_seed).expect("prepare seed should be written");
        let capabilities = vm_capabilities(&root);
        fs::write(root.join("run/winpe-workspace.qcow2"), b"workspace")
            .expect("workspace fixture should be written");
        fs::write(root.join("run/winpe-prepare-OVMF_VARS.fd"), b"vars")
            .expect("firmware fixture should be written");
        let mut plan =
            WinPeDismBackend::plan_vm(capabilities, &manifest, &root, WinPeDismVmPhase::Prepare)
                .expect("prepare VM should plan");
        plan.host_preparation.steps.clear();
        plan.invocation = CommandInvocation {
            program: "sh".into(),
            arguments: vec![
                "-c".into(),
                format!(
                    "printf '%s\\n' 'LSW-WINPE-DISM complete' > '{}'",
                    plan.status_log.display()
                )
                .into(),
            ],
        };
        let mut progress = Vec::new();
        let result =
            WinPeDismBackend::run_vm_with_progress(&plan, Duration::from_secs(5), |event| {
                progress.push(event.clone())
            })
            .expect("completion marker should succeed");
        assert_eq!(result.phase, WinPeDismVmPhase::Prepare);
        assert_eq!(result.status_events, vec!["LSW-WINPE-DISM complete"]);
        assert!(progress.iter().any(|event| event.stage == "starting-winpe"));
        assert!(progress.iter().any(|event| event.stage == "complete"));

        plan.invocation.arguments = vec!["-c".into(), ":".into()];
        assert!(WinPeDismBackend::run_vm(&plan, Duration::from_secs(5)).is_err());
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn dism_progress_parser_uses_only_the_latest_stage() {
        let root = fixture();
        let log = root.join("dism.log");
        fs::write(
            &log,
            b"LSW-DISM-STAGE export-image\r\nLSW-DISM-COMMAND dism /Export-Image\r\n[===== 100.0% =====]\r\nLSW-DISM-STAGE mount-image\r\nLSW-DISM-COMMAND dism /Mount-Image\r\n[===== 42.5% =====]\r\n",
        )
        .expect("fixture log should be written");
        assert_eq!(
            read_dism_progress(&log).expect("progress should parse"),
            Some(("mount-image".to_owned(), 42))
        );

        fs::write(&log, b"LSW-DISM-STAGE inventory-appx\r\n")
            .expect("fixture log should be replaced");
        assert_eq!(
            read_dism_progress(&log).expect("stage without DISM should parse"),
            None
        );
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn apply_runner_rejects_a_completion_marker_for_an_empty_target() {
        let root = fixture();
        let manifest = manifest(&root, WindowsProfile::Slim);
        fs::write(root.join("run/winpe-workspace.qcow2"), b"workspace")
            .expect("workspace fixture should be written");
        fs::write(root.join("disk.qcow2"), b"empty target")
            .expect("target fixture should be written");
        let apply_seed =
            WinPeDismBackend::plan_apply(&manifest, &root).expect("apply seed should plan");
        WinPeDismBackend::write_apply_seed(&apply_seed).expect("apply seed should be written");
        let mut plan = WinPeDismBackend::plan_vm(
            vm_capabilities(&root),
            &manifest,
            &root,
            WinPeDismVmPhase::Apply,
        )
        .expect("apply VM should plan");
        plan.host_preparation.steps.clear();
        plan.invocation = CommandInvocation {
            program: "sh".into(),
            arguments: vec![
                "-c".into(),
                format!(
                    "printf '%s\\n' 'LSW-WINPE-DISM apply-complete' > '{}'",
                    plan.status_log.display()
                )
                .into(),
            ],
        };

        let error = WinPeDismBackend::run_vm(&plan, Duration::from_secs(5))
            .expect_err("an empty target must fail closed despite the guest marker");
        assert!(error.to_string().contains("false completion marker"));
        fs::remove_dir_all(root).expect("fixture should be removed");
    }
}
