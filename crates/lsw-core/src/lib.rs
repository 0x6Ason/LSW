// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

mod agent_protocol;
mod backend;
mod capabilities;
mod customization;
mod error;
mod install_seed;
#[cfg(not(windows))]
mod iso_download;
mod manifest;
mod pe;
mod preparation;
mod profile;
mod qemu;
mod store;
mod windows_media;
mod winpe_dism;

pub use backend::{AcceleratorCapabilities, HostPlatform, QemuBackend, VmAccelerator};
pub use capabilities::HostCapabilities;
pub use customization::{CustomizationPlan, PROFILE_MANIFEST_VERSION};
pub use error::{LswError, Result};
pub use install_seed::{InstallSeedBuilder, InstallSeedOptions, InstallSeedPlan};
#[cfg(not(windows))]
pub use iso_download::{
    sha256_file, IsoDownloadEngine, IsoDownloadProgress, IsoDownloadProgressStage,
    IsoDownloadReport, IsoDownloader, MicrosoftIsoRequest, MicrosoftIsoResolver,
    ResolvedWindowsIso, SecretDownloadUrl,
};
pub use manifest::{
    control_port_for_instance, InstanceManifest, InstanceSpec, InstanceState, NetworkMode,
    PortForward, AGENT_CONTROL_PORT_END_EXCLUSIVE, AGENT_CONTROL_PORT_START,
};
pub use pe::{
    PeAssessment, PeImage, PeImport, PeImportSymbol, PeKind, PeMachine, PeSection, PeSubsystem,
    PeSupportLevel,
};
pub use preparation::{PreparationPlan, PreparationStep, Provisioner};
pub use profile::{SecuritySettings, WindowsProfile};
pub use qemu::{CommandInvocation, CommandPlan, LaunchPhase, QemuPlanner};
pub use store::StateStore;
pub use windows_media::{WindowsEdition, WindowsMediaInspector};
pub use winpe_dism::{
    WinPeDismApplyPlan, WinPeDismApplyStage, WinPeDismBackend, WinPeDismPlan, WinPeDismProgress,
    WinPeDismRunResult, WinPeDismStage, WinPeDismVmPhase, WinPeDismVmPlan,
    WINPE_PREPARED_IMAGE_NAME, WINPE_TARGET_DISK_ID, WINPE_VM_TIMEOUT, WINPE_WORKSPACE_DISK_ID,
    WINPE_WORKSPACE_SIZE_GIB,
};

// Windows reserves TCP 5040 for the Connected Devices Platform service on
// clean installations. Keep LSW outside both that system-service collision
// and Windows' default dynamic client-port range (49152-65535).
pub const AGENT_GUEST_PORT: u16 = 35040;
pub const LICENSE_HELPER_GUEST_PORT: u16 = 35041;
pub const AGENT_TOKEN_FILE: &str = "agent.token";
pub const DAEMON_PROTOCOL_VERSION: u16 = 2;
pub const MANIFEST_FILE: &str = "instance.lsw";
pub use agent_protocol::{
    constant_time_token_eq, decode_exit, decode_file_length, decode_process_id, decode_resize,
    encode_exit, encode_file_length, encode_process_id, encode_resize, read_frame, write_frame,
    ClientHello, FileGetRequest, FilePutRequest, Frame, FrameKind, ProcessEnvironment, ServerHello,
    SessionKind, SessionLease, SessionLeaseState, SessionOptions, SessionSignal, StartRequest,
    TerminalSize, TerminalStartRequest, AGENT_PROTOCOL_VERSION, CAPABILITY_CONPTY_V1,
    CAPABILITY_DETACHED_RUN_V1, CAPABILITY_PROCESS_ENVIRONMENT_V1, CAPABILITY_SESSION_CONTROL_V1,
    CAPABILITY_SESSION_LEASE_V1, CAPABILITY_SESSION_SIGNAL_V1, CAPABILITY_TERMINAL_RESIZE_V1,
    DEFAULT_SESSION_LEASE_TIMEOUT_MILLIS, MAX_FRAME_BYTES, MAX_SESSION_LEASE_TIMEOUT_MILLIS,
    MAX_TERMINAL_DIMENSION, MIN_SESSION_LEASE_TIMEOUT_MILLIS, SESSION_CANCEL_EXIT_CODE,
};
