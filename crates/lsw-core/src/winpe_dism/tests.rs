use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use super::runtime::read_dism_progress;
use super::*;
#[cfg(unix)]
use crate::CommandInvocation;
use crate::{
    HostCapabilities, InstallSeedBuilder, InstallSeedOptions, InstanceSpec, NetworkMode,
    PreparationStep, WindowsProfile,
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
    assert!(script.contains("call :remove_feature_if_present \"Recall\""));
    assert!(script.contains("/Disable-Feature /FeatureName:%LSW_FEATURE% /Remove"));
    assert!(plan.compact_during_setup);
    assert!(plan.stages.iter().any(|stage| matches!(
        stage,
        WinPeDismStage::RemoveProvisionedAppx { display_name }
            if *display_name == "Clipchamp.Clipchamp"
    )));
    assert!(plan.stages.iter().any(|stage| matches!(
        stage,
        WinPeDismStage::RemoveOptionalFeature { feature_name }
            if *feature_name == "Recall"
    )));
    assert!(plan.stages.contains(&WinPeDismStage::VerifyProfile));

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
    assert!(!plan.script().contains("call :remove_appx_if_present \""));
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
    assert!(script.contains(OFFLINE_PROFILE_MARKER_NAME));
    assert!(script.contains("LSW-OFFLINE-PROFILE-APPLIED slim-v2"));

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
    let prepare_seed =
        WinPeDismBackend::plan(WindowsProfile::Slim, 6, &root).expect("prepare seed should plan");
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
    let apply = WinPeDismBackend::plan_vm(capabilities, &manifest, &root, WinPeDismVmPhase::Apply)
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
    let prepare_seed =
        WinPeDismBackend::plan(WindowsProfile::Slim, 6, &root).expect("prepare seed should plan");
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
    let result = WinPeDismBackend::run_vm_with_progress(&plan, Duration::from_secs(5), |event| {
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

    fs::write(&log, b"LSW-DISM-STAGE inventory-appx\r\n").expect("fixture log should be replaced");
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
    fs::write(root.join("disk.qcow2"), b"empty target").expect("target fixture should be written");
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
