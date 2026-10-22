// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

mod backend;
mod capabilities;
mod customization;
mod daemon_protocol;
mod error;
mod fs_checks;
#[cfg(not(windows))]
mod image;
mod install_seed;
#[cfg(not(windows))]
mod iso_download;
mod manifest;
mod pe;
mod preparation;
mod profile;
mod profile_application;
mod qemu;
#[cfg(not(windows))]
mod samba;
mod store;
mod windows_media;
mod winpe_dism;
mod winpe_profile;

pub use backend::{
    AcceleratorCapabilities, GuestCpuTuning, HostPlatform, QemuBackend, VmAccelerator,
    KVM_ENLIGHTENED_CPU_MODEL,
};
pub use capabilities::HostCapabilities;
pub use customization::{
    AppxRemoval, CustomizationPlan, ProductUninstaller, RegistryHive, RegistryPolicy,
    ServicePolicy, ServiceStartup, PROFILE_MANIFEST_VERSION,
};
pub use daemon_protocol::{
    read_daemon_reply, read_daemon_request, write_daemon_reply, write_daemon_request, DaemonInfo,
    DaemonReply, DaemonRequest, DaemonResponse, InstanceStatus, InstanceSummary, LaunchPlanSummary,
    Liveness, StartedInstance, DAEMON_PROTOCOL_VERSION, MAX_DAEMON_REPLY_BYTES,
    MAX_DAEMON_REQUEST_BYTES,
};
pub use error::{LswError, Result};
#[cfg(not(windows))]
pub use image::{ImageManager, SealedImage};
pub use install_seed::{InstallSeedBuilder, InstallSeedOptions, InstallSeedPlan};
#[cfg(not(windows))]
pub use iso_download::{
    sha256_file, IsoDownloadEngine, IsoDownloadProgress, IsoDownloadProgressStage,
    IsoDownloadReport, IsoDownloader, MicrosoftIsoRequest, MicrosoftIsoResolver,
    ResolvedWindowsIso, SecretDownloadUrl,
};
pub use manifest::{
    control_port_for_instance, validate_windows_user_name, windows_path_component_is_safe,
    FolderShare, FolderShareMode, FolderShareTransport, IdlePolicy, InstanceManifest, InstanceSpec,
    InstanceState, NetworkMode, PortForward, WindowsUserRole, AGENT_CONTROL_PORT_END_EXCLUSIVE,
    AGENT_CONTROL_PORT_START, DEFAULT_HIBERNATE_TIMEOUT_SECONDS,
};
pub use pe::{
    PeAssessment, PeImage, PeImport, PeImportSymbol, PeKind, PeMachine, PeSection, PeSubsystem,
    PeSupportLevel,
};
pub use preparation::{PreparationPlan, PreparationStep, Provisioner};
pub use profile::{SecuritySettings, WindowsProfile};
pub use qemu::{
    CommandInvocation, CommandPlan, LaunchPhase, QemuPlanner, LIVE_SMB_CONFIG_FILE,
    LIVE_SMB_RUNTIME_DIRECTORY,
};
#[cfg(not(windows))]
pub use samba::prepare_live_share_runtime;
pub use store::{default_state_root, StateStore};
pub use windows_media::{WindowsEdition, WindowsMediaInspector};
pub use winpe_dism::{
    WinPeDismApplyPlan, WinPeDismApplyStage, WinPeDismBackend, WinPeDismPlan, WinPeDismProgress,
    WinPeDismRunResult, WinPeDismStage, WinPeDismVmPhase, WinPeDismVmPlan,
    WINPE_PREPARED_IMAGE_NAME, WINPE_TARGET_DISK_ID, WINPE_VM_TIMEOUT, WINPE_WORKSPACE_DISK_ID,
    WINPE_WORKSPACE_SIZE_GIB,
};

pub const AGENT_TOKEN_FILE: &str = "agent.token";
pub const MANIFEST_FILE: &str = "instance.lsw";
