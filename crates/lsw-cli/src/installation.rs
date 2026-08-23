// SPDX-License-Identifier: GPL-3.0-or-later

//! One-shot Windows installation orchestration.
//!
//! This module owns the state transition from official media to a bootable
//! instance. It marks failed installs, removes only validated transient paths,
//! and never treats host devices as installation targets.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    HostCapabilities, InstallSeedBuilder, InstanceManifest, InstanceSpec, InstanceState,
    IsoDownloadEngine, IsoDownloader, LaunchPhase, LswError, MicrosoftIsoRequest,
    MicrosoftIsoResolver, Provisioner, SessionKind, StartRequest, StateStore, WinPeDismBackend,
    WinPeDismVmPhase, WindowsEdition, WindowsMediaInspector, WindowsProfile, WINPE_VM_TIMEOUT,
};

use crate::agent_client::AgentClient;

use super::{
    absolute_path, fix_host_dependencies, launch_installation_viewer, resolve_name,
    show_activation_notice_once, start_named_instance, InstallArguments,
};

pub(super) fn install_instance(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = InstallArguments::parse(arguments)?;
    if parsed.without_agent && parsed.seed.agent_binary.is_some() {
        return Err("--agent and --without-agent cannot be used together".into());
    }
    if parsed.seed.unattended_image_index.is_some() && parsed.edition.is_some() {
        return Err("--edition and --unattended-index cannot be used together".into());
    }

    if parsed.iso.is_some() && parsed.language_option_seen {
        return Err(
            "--language is used only with Microsoft ISO download; omit it with --iso".into(),
        );
    }
    if let Some(iso) = parsed.iso.clone() {
        return install_new_instance(store, parsed, Some(iso));
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
        return install_new_instance(store, parsed, None);
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
    match fs::symlink_metadata(&seed) {
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
    start_named_instance(store, &name, LaunchPhase::Install)?;
    if !parsed.no_viewer {
        launch_installation_viewer(store, &name)?;
    }
    if verify_agent {
        wait_for_unattended_setup(store, &name, Duration::from_secs(60 * 60))?;
        show_activation_notice_once(store, &name);
        println!("Environment verified. Run `lsw` to enter PowerShell.");
    } else {
        println!("Windows setup is running without a verifiable LSW agent.");
    }
    Ok(())
}

fn install_new_instance(
    store: &StateStore,
    parsed: InstallArguments,
    supplied_iso: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = parsed
        .requested
        .as_deref()
        .ok_or("usage: lsw install NAME [--iso PATH] [--edition EDITION] [OPTIONS]")?;
    let capabilities = ensure_install_dependencies(parsed.profile, true, !parsed.no_viewer)?;
    let iso = match supplied_iso {
        Some(iso) => absolute_path(&iso)?,
        None => download_official_windows_iso(store, &capabilities, &parsed.language)?,
    };

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
        port_forwards: parsed.port_forwards,
        license_accepted: true,
        allow_unsupported_requirements: parsed.allow_unsupported_requirements,
    };
    let manifest = InstanceManifest::new(spec)?;
    let instance_dir = store.create(&manifest)?;
    println!("Created instance {name:?} using {}.", edition.name);
    println!(
        "You are responsible for the license, product key, and activation of the Windows media you supplied."
    );

    let provisioner = Provisioner::new(capabilities.clone());
    let preparation = provisioner.plan(&manifest, &instance_dir)?;
    provisioner.apply(&preparation)?;
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
        &capabilities,
        &manifest,
        &instance_dir,
        &edition,
        &seed_plan.destination,
        &options.locale,
        seed_plan.setup_account_password_value(),
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

    start_named_instance(store, name, LaunchPhase::Run)?;
    if !parsed.no_viewer {
        launch_installation_viewer(store, name)?;
    }
    if !parsed.without_agent {
        wait_for_unattended_setup(store, name, Duration::from_secs(15 * 60))?;
        show_activation_notice_once(store, name);
        println!("Environment verified. Run `lsw` to enter PowerShell.");
    } else {
        println!("Windows is running without the LSW agent; shell verification was skipped.");
    }
    if store.default_name()?.is_none() {
        store.set_default(name)?;
    }
    Ok(())
}

fn download_official_windows_iso(
    store: &StateStore,
    capabilities: &HostCapabilities,
    language: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    println!("Resolving the current official Windows 11 x64 ISO from Microsoft...");
    let request = MicrosoftIsoRequest {
        language: language.to_owned(),
    };
    let resolver = MicrosoftIsoResolver::new();
    let resolved = resolver.resolve(&request)?;
    println!(
        "Resolved {} {} with Microsoft SHA-256 {}.",
        resolved.language, resolved.architecture, resolved.expected_sha256
    );

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
    println!(
        "Downloading with {} (maximum four Microsoft CDN connections)...",
        match downloader.engine() {
            IsoDownloadEngine::Aria2 => "aria2c",
            IsoDownloadEngine::Native => "the native resumable downloader",
        }
    );
    let report = downloader.download_resolved(&resolver, &request, resolved, &destination)?;
    println!(
        "Verified {} bytes at {} (SHA-256 {}).",
        report.bytes,
        report.destination.display(),
        report.sha256
    );
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

fn run_winpe_preinstallation(
    capabilities: &HostCapabilities,
    manifest: &InstanceManifest,
    instance_dir: &Path,
    edition: &WindowsEdition,
    install_seed: &Path,
    locale: &str,
    setup_account_password_value: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "Preparing {} with official WinPE DISM (network disabled)...",
        edition.name
    );
    let prepare = WinPeDismBackend::plan_with_guest_setup(
        manifest,
        edition.index,
        instance_dir,
        install_seed,
        locale,
        setup_account_password_value,
    )?;
    WinPeDismBackend::write_seed(&prepare)?;
    let prepare_vm = WinPeDismBackend::plan_vm(
        capabilities.clone(),
        manifest,
        instance_dir,
        WinPeDismVmPhase::Prepare,
    )?;
    let prepare_result = WinPeDismBackend::run_vm(&prepare_vm, WINPE_VM_TIMEOUT)?;
    println!(
        "WinPE prepare phase completed in {} seconds.",
        prepare_result.elapsed.as_secs()
    );

    println!("Applying the prepared image to the LSW-owned qcow2 disk...");
    let apply = WinPeDismBackend::plan_apply(manifest, instance_dir)?;
    WinPeDismBackend::write_apply_seed(&apply)?;
    let apply_vm = WinPeDismBackend::plan_vm(
        capabilities.clone(),
        manifest,
        instance_dir,
        WinPeDismVmPhase::Apply,
    )?;
    let apply_result = WinPeDismBackend::run_vm(&apply_vm, WINPE_VM_TIMEOUT)?;
    println!(
        "WinPE apply phase completed in {} seconds.",
        apply_result.elapsed.as_secs()
    );
    Ok(())
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
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Waiting for unattended Windows setup and the boot-time LSW agent...");
    let token = store.read_agent_token(name)?;
    let deadline = Instant::now() + timeout;
    let marker_probe = StartRequest {
        kind: SessionKind::Exec,
        argv: vec![
            "powershell.exe".to_owned(),
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            r#"$Marker='C:\ProgramData\LSW\setup-complete.marker'; if (-not (Test-Path -LiteralPath $Marker -PathType Leaf)) { exit 23 }; if ([System.IO.File]::ReadAllText($Marker).Trim() -cne 'LSW-SETUP-COMPLETE') { exit 24 }; foreach ($Path in @('C:\Windows\Panther\unattend.xml', 'C:\Windows\Panther\Unattend\unattend.xml', 'C:\Windows\Setup\Scripts\SetupComplete.cmd', 'C:\ProgramData\LSW\setup')) { if (Test-Path -LiteralPath $Path) { exit 25 } }; if ($null -ne (Get-LocalUser -Name 'LSWSetup' -ErrorAction SilentlyContinue)) { exit 26 }; $Winlogon=Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'; if ([string]$Winlogon.AutoAdminLogon -eq '1') { exit 27 }; $StoredPassword=$Winlogon.PSObject.Properties['DefaultPassword']; if ($null -ne $StoredPassword -and -not [string]::IsNullOrEmpty([string]$StoredPassword.Value)) { exit 28 }"#.to_owned(),
        ],
        working_directory: None,
    };
    loop {
        let manifest = store.load(name)?;
        let setup_complete = AgentClient::connect(&manifest, &token)
            .and_then(|client| client.run_capture(&marker_probe, &[], 1024))
            .is_ok_and(|process| process.exit_code == 0);
        if setup_complete {
            return Ok(());
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

fn find_windows_agent() -> Option<PathBuf> {
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
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use lsw_core::NetworkMode;

    use super::*;

    #[test]
    fn completed_winpe_install_transitions_to_stopped_before_normal_run() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = env::temp_dir().join(format!("lsw-install-transition-{nonce}"));
        fs::create_dir(&root).expect("fixture root should be created");
        let iso = root.join("windows.iso");
        fs::write(&iso, b"fixture media").expect("fixture ISO should be written");
        let manifest = InstanceManifest::new(InstanceSpec {
            name: format!("winpe-transition-{}", std::process::id()),
            source_iso: iso,
            profile: WindowsProfile::Slim,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("fixture manifest should be valid");
        let store = StateStore::new(root.join("state"));
        store.create(&manifest).expect("instance should be stored");

        mark_winpe_install_complete(&store, &manifest)
            .expect("completed WinPE install should become runnable");
        assert_eq!(
            store
                .load(&manifest.spec.name)
                .expect("updated instance should load")
                .state,
            InstanceState::Stopped
        );

        let mut unexpected = manifest.clone();
        unexpected.state = InstanceState::Running;
        assert!(mark_winpe_install_complete(&store, &unexpected).is_err());
        fs::remove_dir_all(root).expect("fixture should be removed");
    }
}
