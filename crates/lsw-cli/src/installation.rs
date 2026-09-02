// SPDX-License-Identifier: GPL-3.0-or-later

//! One-shot Windows installation orchestration.
//!
//! This module owns the state transition from official media to a bootable
//! instance. It marks failed installs, removes only validated transient paths,
//! and never treats host devices as installation targets.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    HostCapabilities, ImageManager, InstallSeedBuilder, InstanceManifest, InstanceSpec,
    InstanceState, IsoDownloadEngine, IsoDownloadProgressStage, IsoDownloader, LaunchPhase,
    LswError, MicrosoftIsoRequest, MicrosoftIsoResolver, Provisioner, SessionKind, StartRequest,
    StateStore, WinPeDismBackend, WinPeDismProgress, WinPeDismVmPhase, WindowsEdition,
    WindowsMediaInspector, WindowsProfile, WINPE_VM_TIMEOUT,
};

use crate::progress::{ProgressEvent, ProgressRenderer};
use lsw_host::AgentClient;

use super::{
    absolute_path, fix_host_dependencies, launch_installation_viewer, resolve_name,
    resolve_port_forwards, show_activation_notice_once, start_named_instance, InstallArguments,
};

// Default-sized KVM guests can spend well over fifteen minutes in the first
// Windows 11 specialize/OOBE pass. This is a safety bound, not an estimate:
// the wait returns as soon as the agent and setup cleanup marker are ready.
const UNATTENDED_SETUP_TIMEOUT: Duration = Duration::from_secs(60 * 60);

pub(super) fn install_instance(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut parsed = InstallArguments::parse(arguments)?;
    if should_use_safe_install_name(
        parsed.requested.as_deref(),
        env::var_os("LSW_DEFAULT_INSTANCE").is_some(),
        store.default_name()?.is_some(),
        store.list()?.is_empty(),
    ) {
        parsed.requested = Some("windows".to_owned());
        println!("No instance name was supplied; using the safe default \"windows\".");
    }
    if parsed.without_agent && parsed.seed.agent_binary.is_some() {
        return Err("--agent and --without-agent cannot be used together".into());
    }
    if parsed.without_agent && !parsed.defer_user_setup {
        return Err("--without-agent requires --defer-user-setup because no agent can create the permanent Windows user".into());
    }
    if !parsed.defer_user_setup && (!io::stdin().is_terminal() || !io::stderr().is_terminal()) {
        return Err("noninteractive installation requires --defer-user-setup; run `lsw user setup --password-stdin` after installation".into());
    }
    if parsed.seed.unattended_image_index.is_some() && parsed.edition.is_some() {
        return Err("--edition and --unattended-index cannot be used together".into());
    }

    if parsed.iso.is_some() && parsed.language_option_seen {
        return Err(
            "--language is used only with Microsoft ISO download; omit it with --iso".into(),
        );
    }
    let mut progress = ProgressRenderer::new();
    if let Some(iso) = parsed.iso.clone() {
        return install_new_instance(store, parsed, Some(iso), &mut progress);
    }

    let create_from_microsoft = if let Some(name) = parsed.requested.as_deref() {
        match store.load(name) {
            Ok(_) => false,
            Err(LswError::InstanceNotFound(_)) => true,
            Err(error) => return Err(error.into()),
        }
    } else {
        false
    };
    if create_from_microsoft {
        return install_new_instance(store, parsed, None, &mut progress);
    }
    if parsed.create_option_seen {
        return Err(
            "--profile, --cpus, --memory, --disk, --network, and --publish require a new instance name"
                .into(),
        );
    }
    if parsed.language_option_seen {
        return Err("--language requires a new instance name without --iso".into());
    }

    let name = resolve_name(store, parsed.requested.as_deref())?;
    let manifest = store.load(&name)?;
    ensure_install_dependencies(
        manifest.spec.profile,
        parsed.edition.is_some(),
        !parsed.no_viewer,
    )?;
    let mut options = parsed.seed;
    if let Some(requested) = parsed.edition.as_deref() {
        let edition =
            select_windows_edition(store, &manifest.spec.source_iso, Some(requested), true)?
                .expect("a required edition selection must return an edition");
        options.unattended_image_name = Some(edition.name);
    }

    let instance_dir = store.instance_dir(&name)?;
    let seed = instance_dir.join("seed");
    let seed_metadata = fs::symlink_metadata(&seed);
    if manifest.state == InstanceState::Stopped
        && seed_metadata
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        if parsed.seed_option_seen || parsed.edition.is_some() {
            return Err(
                "installation options cannot be changed while resuming completed WinPE deployment"
                    .into(),
            );
        }
        return resume_winpe_installation(
            store,
            &name,
            parsed.without_agent,
            parsed.no_viewer,
            parsed.defer_user_setup,
            &mut progress,
        );
    }
    match seed_metadata {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            if parsed.seed_option_seen {
                return Err(format!(
                    "{} already exists; install seed options cannot be changed implicitly",
                    seed.display()
                )
                .into());
            }
            println!("Using existing installation seed at {}", seed.display());
        }
        Ok(_) => {
            return Err(format!("{} must be a real directory", seed.display()).into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if options.agent_binary.is_none() && !parsed.without_agent {
                options.agent_binary = find_windows_agent();
                if options.agent_binary.is_none() {
                    return Err(
                        "lsw-agent.exe was not found; pass --agent PATH, set LSW_WINDOWS_AGENT, or explicitly use --without-agent"
                            .into(),
                    );
                }
            }
            let token = store.read_agent_token(&name)?;
            let plan = InstallSeedBuilder::plan(&manifest, &instance_dir, &token, &options)?;
            for line in plan.describe() {
                println!("  {line}");
            }
            InstallSeedBuilder::apply(&plan)?;
            println!("Installation seed created at {}", seed.display());
        }
        Err(error) => return Err(error.into()),
    }
    let verify_agent = !parsed.without_agent && seed.join("lsw/lsw-agent.exe").is_file();
    progress.update(ProgressEvent::stage(1, 2, "Starting Windows", "first boot"));
    start_named_instance(store, &name, LaunchPhase::Install)?;
    if !parsed.no_viewer {
        launch_installation_viewer(store, &name)?;
    }
    if verify_agent {
        wait_for_unattended_setup(store, &name, UNATTENDED_SETUP_TIMEOUT, &mut progress, 2, 2)?;
        progress.finish();
        show_activation_notice_once(store, &name);
        crate::user_setup::after_install(store, &name, parsed.defer_user_setup)?;
        println!("Environment verified. Run `lsw` to enter PowerShell.");
    } else {
        progress.finish();
        println!("Windows setup is running without a verifiable LSW agent.");
    }
    Ok(())
}

fn should_use_safe_install_name(
    requested: Option<&str>,
    has_environment_default: bool,
    has_saved_default: bool,
    instance_store_is_empty: bool,
) -> bool {
    requested.is_none() && !has_environment_default && !has_saved_default && instance_store_is_empty
}

fn resume_winpe_installation(
    store: &StateStore,
    name: &str,
    without_agent: bool,
    no_viewer: bool,
    defer_user_setup: bool,
    progress: &mut ProgressRenderer,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Resuming Windows first boot and installation verification for {name:?}.");
    progress.update(ProgressEvent::stage(
        1,
        2,
        "Completing Windows setup",
        "first boot / specialize",
    ));
    start_named_instance(store, name, LaunchPhase::Run)?;
    if !no_viewer {
        launch_installation_viewer(store, name)?;
    }
    if without_agent {
        progress.finish();
        println!("Windows is running without the LSW agent; shell verification was skipped.");
    } else {
        wait_for_unattended_setup(store, name, UNATTENDED_SETUP_TIMEOUT, progress, 1, 2)?;
        progress.update(ProgressEvent::measured(
            2,
            2,
            "Verifying environment",
            "agent and setup cleanup",
            1,
            1,
        ));
        progress.finish();
        show_activation_notice_once(store, name);
        crate::user_setup::after_install(store, name, defer_user_setup)?;
        println!("Environment verified. Run `lsw` to enter PowerShell.");
    }
    if store.default_name()?.is_none() {
        store.set_default(name)?;
    }
    Ok(())
}

fn install_new_instance(
    store: &StateStore,
    parsed: InstallArguments,
    supplied_iso: Option<PathBuf>,
    progress: &mut ProgressRenderer,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = parsed
        .requested
        .as_deref()
        .ok_or("usage: lsw install NAME [--iso PATH] [--edition EDITION] [OPTIONS]")?;
    let license_accepted = confirm_windows_license(parsed.accept_windows_license)?;
    let capabilities = ensure_install_dependencies(parsed.profile, true, !parsed.no_viewer)?;
    let iso = match supplied_iso {
        Some(iso) => absolute_path(&iso)?,
        None => download_official_windows_iso(store, &capabilities, &parsed.language, progress)?,
    };

    progress.update(ProgressEvent::stage(
        3,
        8,
        "Inspecting Windows media",
        "selecting edition",
    ));

    let edition = if let Some(index) = parsed.seed.unattended_image_index {
        select_windows_edition_index(store, &iso, index)?
    } else {
        select_windows_edition(
            store,
            &iso,
            Some(parsed.edition.as_deref().unwrap_or("pro")),
            true,
        )?
        .expect("a new one-shot install must select an edition")
    };
    progress.finish();
    let mut options = parsed.seed;
    options.unattended_image_index = None;
    options.unattended_image_name = Some(edition.name.clone());
    if options.agent_binary.is_none() && !parsed.without_agent {
        options.agent_binary = find_windows_agent();
        if options.agent_binary.is_none() {
            return Err(
                "lsw-agent.exe was not found; pass --agent PATH, set LSW_WINDOWS_AGENT, or explicitly use --without-agent"
                    .into(),
            );
        }
    }

    let spec = InstanceSpec {
        name: name.to_owned(),
        source_iso: iso,
        profile: parsed.profile,
        cpus: parsed.cpus,
        memory_mib: parsed.memory_mib,
        disk_gib: parsed
            .disk_gib
            .unwrap_or_else(|| parsed.profile.default_disk_gib()),
        network: parsed.network,
        port_forwards: resolve_port_forwards(&parsed.port_forwards, name)?,
        license_accepted,
        allow_unsupported_requirements: parsed.allow_unsupported_requirements,
    };
    let manifest = InstanceManifest::new(spec)?;
    let instance_dir = store.create(&manifest)?;
    ImageManager::new(store, &capabilities).stage_instance_identity(name)?;
    println!("Created instance {name:?} using {}.", edition.name);
    println!(
        "You are responsible for the license, product key, and activation of the Windows media you supplied."
    );

    progress.update(ProgressEvent::stage(
        4,
        8,
        "Preparing instance storage",
        "disk, firmware, and vTPM",
    ));
    let provisioner = Provisioner::new(capabilities.clone());
    let preparation = provisioner.plan(&manifest, &instance_dir)?;
    provisioner.apply(&preparation)?;
    progress.finish();
    println!("Prepared disk, firmware variables, runtime directories, and vTPM state.");

    let token = store.read_agent_token(name)?;
    let seed_plan = InstallSeedBuilder::plan(&manifest, &instance_dir, &token, &options)?;
    for line in seed_plan.describe() {
        println!("  {line}");
    }
    InstallSeedBuilder::apply(&seed_plan)?;
    println!(
        "Installation seed created at {}",
        seed_plan.destination.display()
    );

    if let Err(error) = run_winpe_preinstallation(
        WinPePreinstallation {
            capabilities: &capabilities,
            manifest: &manifest,
            instance_dir: &instance_dir,
            edition: &edition,
            install_seed: &seed_plan.destination,
            locale: &options.locale,
            setup_account_password_value: seed_plan.setup_account_password_value(),
        },
        progress,
    ) {
        let mut failed = manifest.clone();
        failed.state = InstanceState::Failed;
        let _ = store.update(&failed);
        return Err(error);
    }
    cleanup_winpe_preinstallation(&instance_dir)?;
    println!("Prepared Windows image applied to the instance disk.");

    // The two WinPE phases install a bootable Windows image without going
    // through the daemon's legacy ISO-install phase. Publish that completed
    // transition before requesting a normal run; the daemon deliberately
    // rejects Run for a still-Configured instance.
    mark_winpe_install_complete(store, &manifest)?;

    progress.update(ProgressEvent::stage(
        7,
        8,
        "Completing Windows setup",
        "first boot / specialize",
    ));
    start_named_instance(store, name, LaunchPhase::Run)?;
    if !parsed.no_viewer {
        launch_installation_viewer(store, name)?;
    }
    if !parsed.without_agent {
        wait_for_unattended_setup(store, name, UNATTENDED_SETUP_TIMEOUT, progress, 7, 8)?;
        progress.update(ProgressEvent::measured(
            8,
            8,
            "Verifying environment",
            "agent and setup cleanup",
            1,
            1,
        ));
        progress.finish();
        show_activation_notice_once(store, name);
        crate::user_setup::after_install(store, name, parsed.defer_user_setup)?;
        println!("Environment verified. Run `lsw` to enter PowerShell.");
    } else {
        progress.finish();
        println!("Windows is running without the LSW agent; shell verification was skipped.");
    }
    if store.default_name()?.is_none() {
        store.set_default(name)?;
    }
    Ok(())
}

fn confirm_windows_license(accepted_by_option: bool) -> Result<bool, Box<dyn std::error::Error>> {
    if accepted_by_option {
        println!("Microsoft Windows license acceptance confirmed by --accept-windows-license.");
        println!("LSW is GPL-3.0-or-later; see LICENSE and THIRD_PARTY_NOTICES.md.");
        return Ok(true);
    }

    let stdin = io::stdin();
    let mut stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(
            "a new Windows installation requires explicit license acceptance; rerun with --accept-windows-license"
                .into(),
        );
    }

    writeln!(
        stderr,
        "Windows is proprietary software. Review and accept the Microsoft Software License Terms that apply to this media and your use."
    )?;
    writeln!(
        stderr,
        "Microsoft licensing documents: https://aka.ms/licensingdocs"
    )?;
    writeln!(
        stderr,
        "The terms supplied with your media or applicable retail/volume agreement control."
    )?;
    writeln!(
        stderr,
        "LSW does not grant a Windows license or activation entitlement."
    )?;
    writeln!(
        stderr,
        "LSW itself is GPL-3.0-or-later; see LICENSE and THIRD_PARTY_NOTICES.md."
    )?;
    write!(
        stderr,
        "Continue and set Windows Setup AcceptEula=true? [y/N] "
    )?;
    stderr.flush()?;

    let mut response = String::new();
    stdin.read_line(&mut response)?;
    if affirmative_license_response(&response) {
        Ok(true)
    } else {
        Err("Windows license terms were not accepted; no instance was created".into())
    }
}

pub(super) fn affirmative_license_response(response: &str) -> bool {
    response.trim().eq_ignore_ascii_case("y") || response.trim().eq_ignore_ascii_case("yes")
}

fn download_official_windows_iso(
    store: &StateStore,
    capabilities: &HostCapabilities,
    language: &str,
    progress: &mut ProgressRenderer,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    progress.update(ProgressEvent::stage(
        1,
        8,
        "Resolving Windows ISO",
        "Microsoft download service",
    ));
    let request = MicrosoftIsoRequest {
        language: language.to_owned(),
    };
    let resolver = MicrosoftIsoResolver::new();
    let resolved = resolver.resolve(&request)?;

    store.initialize()?;
    let cache = store.root().join("cache");
    ensure_private_directory(&cache)?;
    let iso_cache = cache.join("windows-iso");
    ensure_private_directory(&iso_cache)?;
    let destination = iso_cache.join(format!(
        "windows-11-{}.iso",
        resolved.expected_sha256.to_ascii_lowercase()
    ));
    let downloader = IsoDownloader::new(capabilities);
    let engine = downloader.engine();
    progress.update(ProgressEvent::stage(
        2,
        8,
        "Downloading Windows ISO",
        match engine {
            IsoDownloadEngine::Aria2 => "aria2c",
            IsoDownloadEngine::Native => "native resumable downloader",
        },
    ));
    let report = downloader.download_resolved_with_progress(
        &resolver,
        &request,
        resolved,
        &destination,
        |event| {
            let label = match event.stage {
                IsoDownloadProgressStage::Transferring => "Downloading Windows ISO",
                IsoDownloadProgressStage::Assembling => "Assembling Windows ISO",
                IsoDownloadProgressStage::Verifying => "Verifying Windows ISO",
            };
            let detail = match (event.stage, engine) {
                (IsoDownloadProgressStage::Transferring, IsoDownloadEngine::Aria2) => "aria2c",
                (IsoDownloadProgressStage::Transferring, IsoDownloadEngine::Native) => {
                    "native resumable downloader"
                }
                (IsoDownloadProgressStage::Assembling, _) => "resumable range files",
                (IsoDownloadProgressStage::Verifying, _) => "Microsoft SHA-256",
            };
            match (event.completed_bytes, event.total_bytes) {
                (Some(completed), Some(total)) => {
                    progress.update(ProgressEvent::measured(2, 8, label, "", completed, total))
                }
                _ => progress.update(ProgressEvent::stage(2, 8, label, detail)),
            }
        },
    )?;
    Ok(report.destination)
}

fn ensure_private_directory(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(format!("{} must be a real directory", path.display()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

struct WinPePreinstallation<'a> {
    capabilities: &'a HostCapabilities,
    manifest: &'a InstanceManifest,
    instance_dir: &'a Path,
    edition: &'a WindowsEdition,
    install_seed: &'a Path,
    locale: &'a str,
    setup_account_password_value: &'a str,
}

fn run_winpe_preinstallation(
    request: WinPePreinstallation<'_>,
    progress: &mut ProgressRenderer,
) -> Result<(), Box<dyn std::error::Error>> {
    let prepare = WinPeDismBackend::plan_with_guest_setup(
        request.manifest,
        request.edition.index,
        request.instance_dir,
        request.install_seed,
        request.locale,
        request.setup_account_password_value,
    )?;
    WinPeDismBackend::write_seed(&prepare)?;
    let prepare_vm = WinPeDismBackend::plan_vm(
        request.capabilities.clone(),
        request.manifest,
        request.instance_dir,
        WinPeDismVmPhase::Prepare,
    )?;
    WinPeDismBackend::run_vm_with_progress(&prepare_vm, WINPE_VM_TIMEOUT, |event| {
        progress.update(winpe_progress_event(event, 5, 8));
    })?;

    let apply = WinPeDismBackend::plan_apply(request.manifest, request.instance_dir)?;
    WinPeDismBackend::write_apply_seed(&apply)?;
    let apply_vm = WinPeDismBackend::plan_vm(
        request.capabilities.clone(),
        request.manifest,
        request.instance_dir,
        WinPeDismVmPhase::Apply,
    )?;
    WinPeDismBackend::run_vm_with_progress(&apply_vm, WINPE_VM_TIMEOUT, |event| {
        progress.update(winpe_progress_event(event, 6, 8));
    })?;
    progress.finish();
    Ok(())
}

fn winpe_progress_event(event: &WinPeDismProgress, step: u8, total_steps: u8) -> ProgressEvent {
    let label = match event.phase {
        WinPeDismVmPhase::Prepare => "Preparing Windows image",
        WinPeDismVmPhase::Apply => "Applying Windows image",
    };
    let detail = match event.stage.as_str() {
        "starting-winpe" => "starting isolated WinPE".to_owned(),
        "initialize-workspace" => "initializing workspace".to_owned(),
        "export-image" => "exporting selected edition".to_owned(),
        "mount-image" => "mounting offline image".to_owned(),
        "inventory-appx" => "inventorying provisioned applications".to_owned(),
        "no-appx-removals" => "preserving provisioned applications".to_owned(),
        "stage-guest-setup" => "staging unattended setup and agent".to_owned(),
        "commit-image" => "committing prepared image".to_owned(),
        "complete" => "prepared image complete".to_owned(),
        "initialize-target" => "partitioning target disk".to_owned(),
        "apply-image" => "applying Windows to target disk".to_owned(),
        "configure-boot" => "configuring UEFI boot".to_owned(),
        "apply-complete" => "Windows image applied".to_owned(),
        "discard-image" => "discarding failed image mount".to_owned(),
        "failed" | "apply-failed" => "WinPE reported failure".to_owned(),
        stage if stage.starts_with("remove-appx ") => {
            format!("removing {}", stage.trim_start_matches("remove-appx "))
        }
        stage => stage.replace('-', " "),
    };
    match event.percent {
        Some(percent) => {
            ProgressEvent::measured(step, total_steps, label, detail, u64::from(percent), 100)
        }
        None => ProgressEvent::stage(step, total_steps, label, detail),
    }
}

fn cleanup_winpe_preinstallation(instance_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for directory in [
        "seed",
        "winpe-seed",
        "winpe-apply-seed",
        "run/winpe-control-root",
    ] {
        remove_transient_path(&instance_dir.join(directory), true)?;
    }
    for file in [
        "run/winpe-workspace.qcow2",
        "run/winpe-prepare-OVMF_VARS.fd",
        "run/winpe-apply-OVMF_VARS.fd",
        "run/winpe-prepare-qmp.sock",
        "run/winpe-apply-qmp.sock",
        "run/winpe-control.iso",
    ] {
        remove_transient_path(&instance_dir.join(file), false)?;
    }
    Ok(())
}

fn mark_winpe_install_complete(
    store: &StateStore,
    manifest: &InstanceManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    if manifest.state != InstanceState::Configured {
        return Err(format!(
            "refusing to complete WinPE installation from unexpected state {}",
            manifest.state
        )
        .into());
    }
    let mut installed = manifest.clone();
    installed.state = InstanceState::Stopped;
    store.update(&installed)?;
    Ok(())
}

fn remove_transient_path(
    path: &Path,
    expect_directory: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || (expect_directory && !metadata.file_type().is_dir())
        || (!expect_directory && metadata.file_type().is_dir())
    {
        return Err(format!(
            "refusing to remove unexpected transient path {}",
            path.display()
        )
        .into());
    }
    if expect_directory {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn wait_for_unattended_setup(
    store: &StateStore,
    name: &str,
    timeout: Duration,
    progress: &mut ProgressRenderer,
    step: u8,
    total_steps: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let token = store.read_agent_token(name)?;
    let deadline = Instant::now() + timeout;
    let marker_probe = StartRequest {
        kind: SessionKind::Exec,
        argv: vec![
            "powershell.exe".to_owned(),
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            r#"$Progress='C:\ProgramData\LSW\setup-progress.marker'; if (Test-Path -LiteralPath $Progress -PathType Leaf) { [Console]::Out.Write([System.IO.File]::ReadAllText($Progress).Trim()) }; $Marker='C:\ProgramData\LSW\setup-complete.marker'; if (-not (Test-Path -LiteralPath $Marker -PathType Leaf)) { exit 23 }; if ([System.IO.File]::ReadAllText($Marker).Trim() -cne 'LSW-SETUP-COMPLETE') { exit 24 }; foreach ($Path in @('C:\Windows\Panther\unattend.xml', 'C:\Windows\Panther\Unattend\unattend.xml', 'C:\Windows\Setup\Scripts\SetupComplete.cmd', 'C:\ProgramData\LSW\setup')) { if (Test-Path -LiteralPath $Path) { exit 25 } }; foreach ($Name in @('LSWSetup', 'defaultuser0')) { if ($null -ne (Get-LocalUser -Name $Name -ErrorAction SilentlyContinue)) { exit 26 } }; $Winlogon=Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'; if ([string]$Winlogon.AutoAdminLogon -eq '1') { exit 27 }; $StoredPassword=$Winlogon.PSObject.Properties['DefaultPassword']; if ($null -ne $StoredPassword -and -not [string]::IsNullOrEmpty([string]$StoredPassword.Value)) { exit 28 }; $Oobe=Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE'; if ([int]$Oobe.LaunchUserOOBE -ne 0 -or $null -ne $Oobe.PSObject.Properties['DefaultAccountSAMName']) { exit 29 }; $Privacy=Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\OOBE'; if ([int]$Privacy.DisablePrivacyExperience -ne 1) { exit 30 }"#.to_owned(),
        ],
        working_directory: None,
    };
    loop {
        let manifest = store.load(name)?;
        match AgentClient::connect(&manifest, &token)
            .and_then(|client| client.run_capture(&marker_probe, &[], 1024))
        {
            Ok(process) => {
                let stage = String::from_utf8_lossy(&process.stdout);
                progress.update(ProgressEvent::stage(
                    step,
                    total_steps,
                    "Completing Windows setup",
                    windows_setup_stage(stage.trim()),
                ));
                if process.exit_code == 0 {
                    return Ok(());
                }
            }
            Err(_) => progress.update(ProgressEvent::stage(
                step,
                total_steps,
                "Completing Windows setup",
                "first boot / specialize",
            )),
        }
        if manifest.state == InstanceState::Failed {
            return Err(format!("instance {name:?} failed while waiting for its agent").into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for unattended Windows setup; inspect {} or open the display with `lsw view {name}`",
                store.instance_dir(name)?.join("qemu.log").display()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn windows_setup_stage(stage: &str) -> String {
    match stage {
        "installing-agent" => "installing LSW agent".to_owned(),
        "configuring-services" => "configuring Windows services".to_owned(),
        "applying-profile" => "applying LSW profile".to_owned(),
        "starting-agent" => "starting LSW agent".to_owned(),
        "waiting-for-oobe" => "oobeSystem".to_owned(),
        "cleanup" => "removing temporary setup state".to_owned(),
        "complete" => "Windows setup complete".to_owned(),
        "" => "specialize".to_owned(),
        unknown => format!(
            "Windows setup: {}",
            unknown
                .chars()
                .filter(|character| !character.is_control())
                .take(64)
                .collect::<String>()
        ),
    }
}

fn ensure_install_dependencies(
    profile: WindowsProfile,
    needs_media_tools: bool,
    needs_viewer: bool,
) -> Result<HostCapabilities, Box<dyn std::error::Error>> {
    let mut capabilities = HostCapabilities::detect();
    let mut missing = capabilities.missing_for_profile_launch(profile);
    missing.extend(capabilities.missing_for_profile_preparation(profile));
    if needs_media_tools && capabilities.wimlib_imagex.is_none() {
        missing.push("wimlib-imagex");
    }
    if needs_media_tools && capabilities.xorriso.is_none() {
        missing.push("xorriso");
    }
    if needs_media_tools && capabilities.seven_zip.is_none() {
        missing.push("7z (UDF-capable ISO extractor)");
    }
    if needs_viewer && capabilities.remote_viewer.is_none() {
        missing.push("remote-viewer");
    }
    missing.sort_unstable();
    missing.dedup();
    if !missing.is_empty() {
        println!("Missing host dependencies: {}", missing.join(", "));
        println!("Attempting the same package repair as `lsw doctor --fix`...");
        fix_host_dependencies(needs_viewer)?;
        capabilities = HostCapabilities::detect();
        let mut remaining = capabilities.missing_for_profile_launch(profile);
        remaining.extend(capabilities.missing_for_profile_preparation(profile));
        if needs_media_tools && capabilities.wimlib_imagex.is_none() {
            remaining.push("wimlib-imagex");
        }
        if needs_media_tools && capabilities.xorriso.is_none() {
            remaining.push("xorriso");
        }
        if needs_media_tools && capabilities.seven_zip.is_none() {
            remaining.push("7z (UDF-capable ISO extractor)");
        }
        if needs_viewer && capabilities.remote_viewer.is_none() {
            remaining.push("remote-viewer");
        }
        remaining.sort_unstable();
        remaining.dedup();
        if !remaining.is_empty() {
            return Err(format!(
                "required install dependencies remain unavailable: {}",
                remaining.join(", ")
            )
            .into());
        }
    }
    Ok(capabilities)
}

fn select_windows_edition(
    store: &StateStore,
    iso: &Path,
    requested: Option<&str>,
    require_selection: bool,
) -> Result<Option<WindowsEdition>, Box<dyn std::error::Error>> {
    if requested.is_none() && !require_selection {
        return Ok(None);
    }
    let editions = WindowsMediaInspector::new(HostCapabilities::detect())
        .inspect(iso, &store.root().join("run"))?;
    let selected = if let Some(requested) = requested {
        let matches = editions
            .iter()
            .filter(|edition| edition.matches(requested))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [edition] => Some((*edition).clone()),
            [] => {
                return Err(format!(
                    "edition {requested:?} is not present in {}; available editions: {}",
                    iso.display(),
                    editions
                        .iter()
                        .map(|edition| edition.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into())
            }
            _ => {
                return Err(format!(
                    "edition {requested:?} is ambiguous; use one of the full names shown below"
                )
                .into())
            }
        }
    } else if editions.len() == 1 {
        editions.first().cloned()
    } else {
        None
    };

    println!("Windows editions found in {}:", iso.display());
    for edition in &editions {
        let marker = if selected.as_ref() == Some(edition) {
            " (selected)"
        } else {
            ""
        };
        println!("  - {}{}", edition.name, marker);
    }
    if let Some(selected) = selected {
        Ok(Some(selected))
    } else {
        Err(
            "the ISO contains multiple editions; pass --edition NAME (for example, --edition pro)"
                .into(),
        )
    }
}

fn select_windows_edition_index(
    store: &StateStore,
    iso: &Path,
    requested_index: u32,
) -> Result<WindowsEdition, Box<dyn std::error::Error>> {
    let editions = WindowsMediaInspector::new(HostCapabilities::detect())
        .inspect(iso, &store.root().join("run"))?;
    let selected = editions
        .iter()
        .find(|edition| edition.index == requested_index)
        .cloned()
        .ok_or_else(|| {
            format!(
                "image index {requested_index} is not present in {}; available indices: {}",
                iso.display(),
                editions
                    .iter()
                    .map(|edition| format!("{} ({})", edition.index, edition.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    println!(
        "Selected Windows image index {} ({}).",
        selected.index, selected.name
    );
    Ok(selected)
}

pub(super) fn find_windows_agent() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("LSW_WINDOWS_AGENT") {
        return Some(PathBuf::from(configured));
    }
    let executable = env::current_exe().ok()?;
    let binary_directory = executable.parent()?;
    let candidates = [
        Some(binary_directory.join("lsw-agent.exe")),
        binary_directory
            .parent()
            .map(|prefix| prefix.join("libexec/lsw/lsw-agent.exe")),
    ];
    candidates.into_iter().flatten().find(|candidate| {
        fs::symlink_metadata(candidate).is_ok_and(|metadata| {
            metadata.file_type().is_file() && !metadata.file_type().is_symlink()
        })
    })
}

#[cfg(test)]
mod tests;
