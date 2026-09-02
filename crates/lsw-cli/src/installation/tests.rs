// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::{SystemTime, UNIX_EPOCH};

use lsw_core::NetworkMode;

use super::*;

#[test]
fn safe_install_name_is_used_only_for_an_empty_unconfigured_store() {
    assert!(should_use_safe_install_name(None, false, false, true));
    assert!(!should_use_safe_install_name(
        Some("work"),
        false,
        false,
        true
    ));
    assert!(!should_use_safe_install_name(None, true, false, true));
    assert!(!should_use_safe_install_name(None, false, true, true));
    assert!(!should_use_safe_install_name(None, false, false, false));
}

#[test]
fn setup_and_winpe_stages_are_presented_without_invented_progress() {
    assert_eq!(windows_setup_stage("waiting-for-oobe"), "oobeSystem");
    assert_eq!(
        windows_setup_stage("cleanup"),
        "removing temporary setup state"
    );

    let stage = winpe_progress_event(
        &WinPeDismProgress {
            phase: WinPeDismVmPhase::Apply,
            stage: "apply-image".to_owned(),
            percent: None,
            elapsed: Duration::from_secs(1),
        },
        6,
        8,
    );
    assert_eq!(stage.detail, "applying Windows to target disk");
    assert_eq!(stage.completed, None);

    let measured = winpe_progress_event(
        &WinPeDismProgress {
            phase: WinPeDismVmPhase::Apply,
            stage: "apply-image".to_owned(),
            percent: Some(73),
            elapsed: Duration::from_secs(2),
        },
        6,
        8,
    );
    assert_eq!((measured.completed, measured.total), (Some(73), Some(100)));
}

#[test]
fn unattended_setup_wait_covers_slow_default_kvm_first_boot() {
    assert!(UNATTENDED_SETUP_TIMEOUT >= Duration::from_secs(30 * 60));
}

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
