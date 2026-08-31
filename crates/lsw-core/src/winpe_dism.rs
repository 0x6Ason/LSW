// SPDX-License-Identifier: GPL-3.0-or-later

//! Network-isolated Windows PE orchestration for real DISM servicing.
//!
//! The prepare VM owns virtual Disk 0 and produces a slim WIM. The apply VM
//! keeps that workspace as Disk 0 and may wipe only the LSW-owned target at
//! Disk 1. Plans encode this topology explicitly so callers cannot substitute a
//! host block device or silently change the destructive target.

#![deny(missing_docs)]

mod control_media;
mod io;
mod runtime;
mod vm;

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use self::io::{set_private_directory_permissions, set_private_file_permissions};
use crate::install_seed::OFFLINE_PROFILE_MARKER_NAME;
use crate::winpe_profile::prepare_script;
use crate::{
    CommandInvocation, CustomizationPlan, InstanceManifest, LswError, PreparationPlan, QemuBackend,
    Result, WindowsProfile,
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
///
/// Microsoft DISM can spend more than two hours finalizing a max-compressed
/// export on slower storage while both guest CPUs remain busy. Keep a hard
/// bound, but do not abort that healthy work before it can finish.
pub const WINPE_VM_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

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
    /// Enumerate optional features before bounded removal.
    InventoryOptionalFeatures,
    /// Remove one allowlisted provisioned AppX package when present.
    RemoveProvisionedAppx {
        /// Exact allowlisted package display-name pattern passed to DISM.
        display_name: String,
    },
    /// Remove one allowlisted optional feature and its payload when present.
    RemoveOptionalFeature {
        /// Exact optional-feature name passed to DISM.
        feature_name: String,
    },
    /// Re-inventory the mounted image and fail if an exact target survived.
    VerifyProfile,
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
            Self::InventoryOptionalFeatures => {
                "inventory optional features with Windows DISM".to_owned()
            }
            Self::RemoveProvisionedAppx { display_name } => {
                format!("remove provisioned AppX package {display_name} when present")
            }
            Self::RemoveOptionalFeature { feature_name } => {
                format!("remove optional feature payload {feature_name} when present")
            }
            Self::VerifyProfile => {
                "verify exact AppX and optional-feature targets are absent".to_owned()
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
        validate_appx_patterns(
            &customization
                .appx_removals
                .iter()
                .map(|removal| removal.display_name.clone())
                .collect::<Vec<_>>(),
        )?;
        let mut stages = vec![
            WinPeDismStage::InitializeWorkspace,
            WinPeDismStage::LocateInstallImage,
            WinPeDismStage::ExportEdition {
                index: edition_index,
            },
            WinPeDismStage::MountOfflineImage,
            WinPeDismStage::InventoryProvisionedAppx,
            WinPeDismStage::InventoryOptionalFeatures,
        ];
        stages.extend(customization.appx_removals.iter().map(|removal| {
            WinPeDismStage::RemoveProvisionedAppx {
                display_name: removal.display_name.clone(),
            }
        }));
        stages.extend(
            customization
                .optional_feature_removals
                .iter()
                .map(|feature_name| WinPeDismStage::RemoveOptionalFeature {
                    feature_name: feature_name.clone(),
                }),
        );
        stages.push(WinPeDismStage::VerifyProfile);
        stages.push(WinPeDismStage::CommitPreparedImage);

        let mut generated = BTreeMap::new();
        generated.insert(
            PathBuf::from("lsw/workspace.diskpart"),
            workspace_diskpart().into_bytes(),
        );
        generated.insert(
            PathBuf::from(WINPE_SCRIPT_FILE),
            prepare_script(
                edition_index,
                &customization,
                false,
                WINPE_WORKSPACE_DRIVE,
                WINPE_PREPARED_IMAGE_NAME,
                OFFLINE_PROFILE_MARKER_NAME,
            )
            .into_bytes(),
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
            prepare_script(
                edition_index,
                &customization,
                true,
                WINPE_WORKSPACE_DRIVE,
                WINPE_PREPARED_IMAGE_NAME,
                OFFLINE_PROFILE_MARKER_NAME,
            )
            .into_bytes(),
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

#[cfg(test)]
mod tests;
