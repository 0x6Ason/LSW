// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

static ISO_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

fn temporary_iso() -> PathBuf {
    // Wall-clock resolution can be coarse under emulation, while the test
    // harness may create several ISO fixtures concurrently.
    let fixture_id = ISO_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "lsw-manifest-test-{}-{}-{fixture_id}.iso",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be valid")
            .as_nanos()
    ));
    fs::write(&path, b"test media").expect("temporary ISO should be writable");
    path
}

fn remove_v5_fields(encoded: String, version: u32) -> String {
    let mut legacy = encoded
        .lines()
        .filter(|line| {
            ![
                "hibernate_timeout_seconds=",
                "idle_policy=",
                "memory_min_mib=",
                "state_changed_unix_seconds=",
                "base_image_key=",
                "default_user=",
                "default_user_role=",
                "scoped_live_share_credential=",
                "share_count=",
                "share.",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    legacy[0] = format!("version={version}");
    format!("{}\n", legacy.join("\n"))
}

#[test]
fn manifest_round_trip_is_stable() {
    let iso = temporary_iso();
    let mut manifest = InstanceManifest::new(InstanceSpec {
        name: "win-dev".to_owned(),
        source_iso: iso.clone(),
        profile: WindowsProfile::Slim,
        cpus: 4,
        memory_mib: 8192,
        disk_gib: 96,
        network: NetworkMode::Nat,
        port_forwards: vec![PortForward::new(8080, 80).expect("ports should be valid")],
        license_accepted: true,
        allow_unsupported_requirements: false,
    })
    .expect("manifest should be valid");
    manifest.hibernate_timeout_seconds = 240;
    manifest.idle_policy = IdlePolicy::PauseHibernate;
    manifest.memory_min_mib = 1024;
    manifest.base_image_key = Some("a".repeat(64));
    manifest.default_user = Some("desktop-user".to_owned());
    manifest.default_user_role = Some(WindowsUserRole::Administrator);
    manifest.folder_shares.push(FolderShare {
        name: "source".to_owned(),
        host_path: PathBuf::from(if cfg!(windows) {
            "C:\\srv\\source"
        } else {
            "/srv/source"
        }),
        guest_path: "C:\\Users\\desktop-user\\source".to_owned(),
        mode: FolderShareMode::ReadWrite,
        transport: FolderShareTransport::Mirror,
    });

    let encoded = manifest.encode().expect("manifest should encode");
    let decoded = InstanceManifest::decode(&encoded).expect("manifest should decode");
    assert_eq!(manifest, decoded);

    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn instance_name_cannot_escape_state_directory() {
    assert!(validate_instance_name("../escape").is_err());
    assert!(validate_instance_name("Uppercase").is_err());
    assert!(validate_instance_name("win-dev").is_ok());
}

#[test]
fn folder_share_roots_are_absolute_safe_and_non_overlapping() {
    let (host_source, host_other, host_nested, host_filesystem_root) = if cfg!(windows) {
        (
            "C:\\srv\\source",
            "C:\\srv\\other",
            "C:\\srv\\source\\nested",
            "C:\\",
        )
    } else {
        ("/srv/source", "/srv/other", "/srv/source/nested", "/")
    };
    let valid = FolderShare {
        name: "source".to_owned(),
        host_path: PathBuf::from(host_source),
        guest_path: "C:\\src".to_owned(),
        mode: FolderShareMode::ReadWrite,
        transport: FolderShareTransport::Mirror,
    };
    valid.validate().expect("ordinary roots should be valid");
    for invalid_guest in ["C:\\", "C:\\src\\", "C:\\src:stream", "C:\\CON"] {
        let mut invalid = valid.clone();
        invalid.guest_path = invalid_guest.to_owned();
        assert!(invalid.validate().is_err(), "accepted {invalid_guest:?}");
    }
    let mut host_root = valid.clone();
    host_root.host_path = PathBuf::from(host_filesystem_root);
    assert!(host_root.validate().is_err());

    let iso = temporary_iso();
    let mut manifest = InstanceManifest::new(InstanceSpec {
        name: "share-overlap".to_owned(),
        source_iso: iso.clone(),
        profile: WindowsProfile::Vanilla,
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 64,
        network: NetworkMode::Nat,
        port_forwards: Vec::new(),
        license_accepted: true,
        allow_unsupported_requirements: false,
    })
    .expect("fixture manifest should be valid");
    manifest.folder_shares.push(valid.clone());
    manifest.folder_shares.push(FolderShare {
        name: "nested".to_owned(),
        host_path: PathBuf::from(host_other),
        guest_path: "c:/SRC/nested".to_owned(),
        mode: FolderShareMode::ReadOnly,
        transport: FolderShareTransport::Mirror,
    });
    assert!(validate_runtime_fields(&manifest).is_err());
    manifest.folder_shares[1].guest_path = "D:\\other".to_owned();
    manifest.folder_shares[1].host_path = PathBuf::from(host_nested);
    assert!(validate_runtime_fields(&manifest).is_err());
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn live_folder_share_has_one_fixed_driverless_mount_boundary() {
    let mut live = FolderShare {
        name: "linux".to_owned(),
        host_path: PathBuf::from(if cfg!(windows) {
            "C:\\Users\\developer\\LSW"
        } else {
            "/home/developer/LSW"
        }),
        guest_path: "L:\\".to_owned(),
        mode: FolderShareMode::ReadWrite,
        transport: FolderShareTransport::LiveSmb,
    };
    live.validate().expect("fixed live mount should validate");
    live.mode = FolderShareMode::ReadOnly;
    assert!(live.validate().is_err());
    live.mode = FolderShareMode::ReadWrite;
    live.guest_path = "M:\\".to_owned();
    assert!(live.validate().is_err());
}

#[test]
fn official_requirements_need_explicit_override() {
    let iso = temporary_iso();
    let spec = InstanceSpec {
        name: "tiny-test".to_owned(),
        source_iso: iso.clone(),
        profile: WindowsProfile::Ephemeral,
        cpus: 2,
        memory_mib: 2048,
        disk_gib: 32,
        network: NetworkMode::Offline,
        port_forwards: Vec::new(),
        license_accepted: true,
        allow_unsupported_requirements: false,
    };
    assert!(spec.validate().is_err());

    let unsupported = InstanceSpec {
        allow_unsupported_requirements: true,
        ..spec
    };
    assert!(unsupported.validate().is_ok());
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn version_one_manifests_migrate_to_offline_networking() {
    let iso = temporary_iso();
    let manifest = InstanceManifest::new(InstanceSpec {
        name: "old-instance".to_owned(),
        source_iso: iso.clone(),
        profile: WindowsProfile::Vanilla,
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 64,
        network: NetworkMode::Nat,
        port_forwards: Vec::new(),
        license_accepted: true,
        allow_unsupported_requirements: false,
    })
    .expect("manifest should be valid");
    let legacy = remove_v5_fields(manifest.encode().expect("manifest should encode"), 1)
        .replace("profile=vanilla\n", "profile=standard\n")
        .replace("network=nat\n", "")
        .replace("port_forwards=\n", "")
        .replace("idle_timeout_seconds=600\n", "");
    let migrated = InstanceManifest::decode(&legacy).expect("v1 manifest should migrate");
    assert_eq!(migrated.version, MANIFEST_VERSION);
    assert_eq!(migrated.spec.profile, WindowsProfile::Vanilla);
    assert_eq!(migrated.spec.network, NetworkMode::Offline);
    assert!(migrated.spec.port_forwards.is_empty());
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn version_two_manifests_migrate_without_published_ports() {
    let iso = temporary_iso();
    let manifest = InstanceManifest::new(InstanceSpec {
        name: "version-two".to_owned(),
        source_iso: iso.clone(),
        profile: WindowsProfile::Vanilla,
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 64,
        network: NetworkMode::Nat,
        port_forwards: Vec::new(),
        license_accepted: true,
        allow_unsupported_requirements: false,
    })
    .expect("manifest should be valid");
    let legacy = remove_v5_fields(manifest.encode().expect("manifest should encode"), 2)
        .replace("port_forwards=\n", "")
        .replace("idle_timeout_seconds=600\n", "");
    let migrated = InstanceManifest::decode(&legacy).expect("v2 manifest should migrate");
    assert_eq!(migrated.spec.network, NetworkMode::Nat);
    assert!(migrated.spec.port_forwards.is_empty());
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn version_three_manifests_receive_the_default_idle_timeout() {
    let iso = temporary_iso();
    let manifest = InstanceManifest::new(InstanceSpec {
        name: "version-three".to_owned(),
        source_iso: iso.clone(),
        profile: WindowsProfile::Vanilla,
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 64,
        network: NetworkMode::Nat,
        port_forwards: Vec::new(),
        license_accepted: true,
        allow_unsupported_requirements: false,
    })
    .expect("manifest should be valid");
    let legacy = remove_v5_fields(manifest.encode().expect("manifest should encode"), 3)
        .replace("idle_timeout_seconds=600\n", "");
    let migrated = InstanceManifest::decode(&legacy).expect("v3 manifest should migrate");
    assert_eq!(migrated.idle_timeout_seconds, DEFAULT_IDLE_TIMEOUT_SECONDS);
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn version_four_manifests_keep_beta7_features_opted_out() {
    let iso = temporary_iso();
    let manifest = InstanceManifest::new(InstanceSpec {
        name: "version-four".to_owned(),
        source_iso: iso.clone(),
        profile: WindowsProfile::Vanilla,
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 64,
        network: NetworkMode::Nat,
        port_forwards: Vec::new(),
        license_accepted: true,
        allow_unsupported_requirements: false,
    })
    .expect("manifest should be valid");
    let legacy = remove_v5_fields(manifest.encode().expect("manifest should encode"), 4);
    let migrated = InstanceManifest::decode(&legacy).expect("v4 manifest should migrate");
    assert_eq!(migrated.idle_policy, IdlePolicy::Off);
    assert_eq!(migrated.memory_min_mib, 2048);
    assert!(migrated.base_image_key.is_none());
    assert!(migrated.default_user.is_none());
    assert!(migrated.folder_shares.is_empty());
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn version_five_desktop_users_migrate_without_privilege_escalation() {
    let iso = temporary_iso();
    let mut manifest = InstanceManifest::new(InstanceSpec {
        name: "version-five".to_owned(),
        source_iso: iso.clone(),
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
    manifest.default_user = Some("desktop-user".to_owned());
    manifest.default_user_role = Some(WindowsUserRole::Standard);
    let legacy = manifest
        .encode()
        .expect("manifest should encode")
        .lines()
        .filter(|line| !line.starts_with("default_user_role=") && !line.contains(".transport="))
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("\n")
        .replacen("version=8", "version=5", 1);

    let migrated = InstanceManifest::decode(&legacy).expect("v5 manifest should migrate");
    assert_eq!(migrated.default_user.as_deref(), Some("desktop-user"));
    assert_eq!(migrated.default_user_role, Some(WindowsUserRole::Standard));
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn version_six_folder_shares_migrate_to_the_mirror_transport() {
    let iso = temporary_iso();
    let mut manifest = InstanceManifest::new(InstanceSpec {
        name: "version-six".to_owned(),
        source_iso: iso.clone(),
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
    manifest.folder_shares.push(FolderShare {
        name: "source".to_owned(),
        host_path: PathBuf::from(if cfg!(windows) {
            "C:\\srv\\source"
        } else {
            "/srv/source"
        }),
        guest_path: "C:\\source".to_owned(),
        mode: FolderShareMode::ReadWrite,
        transport: FolderShareTransport::Mirror,
    });
    let legacy = manifest
        .encode()
        .expect("manifest should encode")
        .lines()
        .filter(|line| !line.contains(".transport="))
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("\n")
        .replacen("version=8", "version=6", 1);
    let migrated = InstanceManifest::decode(&legacy).expect("v6 manifest should migrate");
    assert_eq!(
        migrated.folder_shares[0].transport,
        FolderShareTransport::Mirror
    );
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn version_seven_instances_keep_the_legacy_live_share_credential() {
    let iso = temporary_iso();
    let manifest = InstanceManifest::new(InstanceSpec {
        name: "version-seven".to_owned(),
        source_iso: iso.clone(),
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
    let legacy = manifest
        .encode()
        .expect("manifest should encode")
        .lines()
        .filter(|line| !line.starts_with("scoped_live_share_credential="))
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("\n")
        .replacen("version=8", "version=7", 1);

    let migrated = InstanceManifest::decode(&legacy).expect("v7 manifest should migrate");
    assert!(!migrated.scoped_live_share_credential);
    assert!(
        InstanceManifest::new(manifest.spec)
            .expect("new manifest should be valid")
            .scoped_live_share_credential
    );
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn published_ports_are_validated_before_launch() {
    let iso = temporary_iso();
    let base = InstanceSpec {
        name: "port-validation".to_owned(),
        source_iso: iso.clone(),
        profile: WindowsProfile::Vanilla,
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 64,
        network: NetworkMode::Nat,
        port_forwards: Vec::new(),
        license_accepted: true,
        allow_unsupported_requirements: false,
    };

    let duplicate = InstanceSpec {
        port_forwards: vec![
            PortForward::new(8080, 80).expect("ports should be valid"),
            PortForward::new(8080, 443).expect("ports should be valid"),
        ],
        ..base.clone()
    };
    assert!(duplicate.validate().is_err());

    let agent_collision = InstanceSpec {
        port_forwards: vec![
            PortForward::new(stable_control_port(&base.name), 80).expect("ports should be valid")
        ],
        ..base.clone()
    };
    assert!(agent_collision.validate().is_err());

    let offline = InstanceSpec {
        network: NetworkMode::Offline,
        port_forwards: vec![PortForward::new(8080, 80).expect("ports should be valid")],
        ..base
    };
    assert!(offline.validate().is_err());
    assert!("0:80".parse::<PortForward>().is_err());
    assert!("8080:0".parse::<PortForward>().is_err());
    assert!("8080".parse::<PortForward>().is_err());
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn manifest_cannot_redirect_agent_credentials_to_another_port() {
    let iso = temporary_iso();
    let manifest = InstanceManifest::new(InstanceSpec {
        name: "port-guard".to_owned(),
        source_iso: iso.clone(),
        profile: WindowsProfile::Vanilla,
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 64,
        network: NetworkMode::Nat,
        port_forwards: Vec::new(),
        license_accepted: true,
        allow_unsupported_requirements: false,
    })
    .expect("manifest should be valid");
    let encoded = manifest.encode().expect("manifest should encode");
    let tampered = encoded.replace(
        &format!("control_port={}\n", manifest.control_port),
        "control_port=22\n",
    );
    assert!(InstanceManifest::decode(&tampered).is_err());
    fs::remove_file(iso).expect("temporary ISO should be removable");
}
