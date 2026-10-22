// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    FolderShare, FolderShareMode, FolderShareTransport, InstanceSpec, NetworkMode, WindowsProfile,
};

use super::*;

fn test_manifest(profile: WindowsProfile) -> (InstanceManifest, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock must be valid")
        .as_nanos();
    let iso = std::env::temp_dir().join(format!("lsw-qemu-test-{nonce}.iso"));
    fs::write(&iso, b"test media").expect("temporary ISO should be writable");
    let manifest = InstanceManifest::new(InstanceSpec {
        name: "win-dev".to_owned(),
        source_iso: iso.clone(),
        profile,
        cpus: 4,
        memory_mib: 8192,
        disk_gib: 64,
        network: NetworkMode::Offline,
        port_forwards: Vec::new(),
        license_accepted: true,
        allow_unsupported_requirements: false,
    })
    .expect("manifest should be valid");
    (manifest, iso)
}

fn headless_capabilities() -> HostCapabilities {
    HostCapabilities::unavailable(crate::HostPlatform::Linux)
}

#[test]
fn planner_uses_windows_inbox_install_devices() {
    let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    let planner = QemuPlanner::new(headless_capabilities());
    let plan = planner
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Install)
        .expect("plan should be built");
    let command = plan.display_command();
    assert!(command.contains("nvme,drive=system"));
    assert!(command.contains("discard=unmap,detect-zeroes=unmap"));
    assert!(command.contains("virtio-balloon-pci,id=balloon0"));
    assert!(command.contains("e1000e,netdev=net0"));
    assert!(command.contains("-device VGA"));
    assert!(command.contains("-boot once=d,menu=on"));
    assert!(command.contains("-accel tcg,thread=multi"));
    assert_eq!(plan.backend.accelerator(), crate::VmAccelerator::Tcg);
    assert!(command.contains("restrict=on"));
    assert_eq!(plan.helper_commands.len(), 1);
    assert!(plan.helper_commands[0]
        .display_command()
        .contains("swtpm socket --tpm2"));
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn planner_uses_an_explicit_validated_accelerator_selection() {
    let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    let mut capabilities = headless_capabilities();
    capabilities.accelerators =
        crate::AcceleratorCapabilities::none().with_available(crate::VmAccelerator::Kvm);
    let backend = QemuBackend::require(&capabilities, crate::VmAccelerator::Kvm)
        .expect("advertised KVM should be selectable");
    let planner = QemuPlanner::with_backend(capabilities, backend)
        .expect("the backend should match its capabilities");
    let plan = planner
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
        .expect("plan should be built");
    assert_eq!(planner.backend(), backend);
    assert_eq!(plan.backend, backend);
    let command = plan.display_command();
    assert!(command.contains("-enable-kvm -cpu host,migratable=off,hv-relaxed,"));
    assert!(command.contains(",hv-stimer-direct,"));
    assert!(!command.contains("-accel tcg"));
    assert!(plan
        .notes
        .iter()
        .any(|note| note.contains("Hyper-V enlightenments")));
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn baseline_cpu_tuning_keeps_plain_host_passthrough() {
    let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    let mut capabilities = headless_capabilities();
    capabilities.accelerators =
        crate::AcceleratorCapabilities::none().with_available(crate::VmAccelerator::Kvm);
    let planner = QemuPlanner::new(capabilities).with_cpu_tuning(crate::GuestCpuTuning::Baseline);
    assert_eq!(planner.cpu_tuning(), crate::GuestCpuTuning::Baseline);
    let plan = planner
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
        .expect("plan should be built");
    let command = plan.display_command();
    assert!(command.contains("-enable-kvm -cpu host -rtc"));
    assert!(!command.contains("hv-"));
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn planner_presents_vcpus_as_cores_of_one_socket() {
    let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    let plan = QemuPlanner::new(headless_capabilities())
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
        .expect("plan should be built");
    let command = plan.display_command();
    assert!(command.contains("-smp 4,sockets=1,cores=4,threads=1"));
    assert!(command.contains("-rtc base=utc,driftfix=slew"));
    // TCG never advertises Hyper-V features it cannot accelerate.
    assert!(command.contains("-accel tcg,thread=multi -cpu max"));
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn run_identity_uses_the_windows_inbox_ide_path() {
    let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock must be valid")
        .as_nanos();
    let instance_dir = std::env::temp_dir().join(format!("lsw-qemu-identity-{nonce}"));
    fs::create_dir_all(instance_dir.join("identity-seed"))
        .expect("identity directory should be created");
    let plan = QemuPlanner::new(headless_capabilities())
        .plan(&manifest, &instance_dir, LaunchPhase::Run)
        .expect("plan should be built");
    let command = plan.display_command();
    assert!(command.contains("id=lsw-identity,snapshot=on"));
    assert!(command.contains("-device ide-hd,drive=lsw-identity"));
    assert!(!command.contains("usb-storage,drive=lsw-identity"));
    fs::remove_dir_all(instance_dir).expect("temporary instance should be removable");
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn secure_profile_does_not_claim_test_driver_support() {
    let (manifest, iso) = test_manifest(WindowsProfile::Secure);
    let planner = QemuPlanner::new(headless_capabilities());
    let plan = planner
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
        .expect("plan should be built");
    assert!(plan.notes.iter().any(|note| note.contains("driverless")));
    assert!(!plan
        .notes
        .iter()
        .any(|note| note.contains("test-signed guest")));
    let command = plan.display_command();
    assert!(command.contains("smm=on"));
    assert!(command.contains("cfi.pflash01,property=secure,value=on"));
    assert!(plan
        .missing_capabilities
        .contains(&"Secure Boot OVMF code firmware"));
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn nat_network_publishes_requested_tcp_ports_on_loopback() {
    let (mut manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    manifest.spec.network = NetworkMode::Nat;
    manifest.spec.port_forwards = vec![
        crate::PortForward::new(8080, 80).expect("ports should be valid"),
        crate::PortForward::new(8443, 443).expect("ports should be valid"),
    ];
    let plan = QemuPlanner::new(headless_capabilities())
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
        .expect("plan should be built");
    let command = plan.display_command();
    assert!(command.contains("hostfwd=tcp:127.0.0.1:8080-:80"));
    assert!(command.contains("hostfwd=tcp:127.0.0.1:8443-:443"));
    assert!(!command.contains("0.0.0.0:8080"));
    assert!(plan
        .notes
        .iter()
        .any(|note| note.contains("127.0.0.1:8080")));
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn live_share_uses_only_the_approved_qemu_smb_root() {
    let (mut manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock must be valid")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("lsw-live-share-{nonce},root"));
    fs::create_dir(&root).expect("share root should be created");
    manifest.folder_shares.push(FolderShare {
        name: "linux".to_owned(),
        host_path: root.clone(),
        guest_path: "L:\\".to_owned(),
        mode: FolderShareMode::ReadWrite,
        transport: FolderShareTransport::LiveSmb,
    });
    let mut capabilities = headless_capabilities();
    capabilities.smbd = Some(PathBuf::from("/usr/sbin/smbd"));
    let plan = QemuPlanner::new(capabilities)
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
        .expect("live-share plan should be built");
    let command = plan.display_command();
    assert!(command.contains("guestfwd=tcp:10.0.2.4:139-cmd:"));
    assert!(command.contains("guestfwd=tcp:10.0.2.4:445-cmd:"));
    assert!(command.contains("/state/win-dev/run/live-smb/smb.conf"));
    assert!(!command.contains(",smb="));
    assert!(plan
        .notes
        .iter()
        .any(|note| { note.contains("private authenticated QEMU SMB") && note.contains("root") }));
    assert!(!plan
        .missing_capabilities
        .contains(&"smbd (Samba user-mode server)"));
    let blocked = QemuPlanner::new(headless_capabilities())
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
        .expect("missing smbd should be reported in the plan");
    assert!(blocked
        .missing_capabilities
        .contains(&"smbd (Samba user-mode server)"));
    fs::remove_dir(root).expect("share root should be removable");
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

/// Every value that follows `option` in the argument vector, in order.
fn option_values<'a>(arguments: &'a [OsString], option: &str) -> Vec<&'a str> {
    arguments
        .windows(2)
        .filter(|pair| pair[0] == option)
        .map(|pair| pair[1].to_str().expect("QEMU arguments are UTF-8"))
        .collect()
}

#[test]
fn run_phase_arguments_have_the_documented_device_groups_in_order() {
    let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    let plan = QemuPlanner::new(headless_capabilities())
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Run)
        .expect("plan should be built");
    let arguments = &plan.arguments;

    assert_eq!(option_values(arguments, "-name"), ["win-dev"]);
    assert_eq!(option_values(arguments, "-machine"), ["q35,usb=on"]);
    assert_eq!(
        option_values(arguments, "-smp"),
        ["4,sockets=1,cores=4,threads=1"]
    );
    assert_eq!(option_values(arguments, "-m"), ["8192M"]);
    assert_eq!(option_values(arguments, "-cpu"), ["max"]);
    assert_eq!(
        option_values(arguments, "-drive"),
        [
            "if=pflash,format=raw,readonly=on,file=/path/to/OVMF_CODE.fd",
            "if=pflash,format=raw,file=/state/win-dev/OVMF_VARS.fd",
            "file=/state/win-dev/disk.qcow2,if=none,id=system,format=qcow2,discard=unmap,detect-zeroes=unmap",
        ]
    );
    assert_eq!(
        option_values(arguments, "-device"),
        [
            "nvme,drive=system,serial=lsw-system",
            "e1000e,netdev=net0",
            "qemu-xhci",
            "usb-kbd",
            "usb-tablet",
            "virtio-rng-pci",
            "virtio-balloon-pci,id=balloon0",
            "VGA",
            "tpm-tis,tpmdev=tpm0",
        ]
    );
    assert_eq!(option_values(arguments, "-boot"), ["order=c,menu=on"]);
    assert_eq!(
        option_values(arguments, "-qmp"),
        ["unix:/state/win-dev/run/qmp.sock,server=on,wait=off"]
    );
    assert_eq!(
        option_values(arguments, "-vnc"),
        ["unix:/state/win-dev/run/recovery-vnc.sock"]
    );
    assert!(arguments.iter().any(|argument| argument == "-nodefaults"));
    for silenced in ["-monitor", "-display", "-serial"] {
        assert_eq!(option_values(arguments, silenced), ["none"]);
    }
    // The network device must follow its netdev, and the TPM device its
    // chardev, or QEMU rejects the command line.
    let position = |option: &str| {
        arguments
            .iter()
            .position(|argument| argument == option)
            .expect("option is present")
    };
    assert!(position("-netdev") < position("-boot"));
    assert!(position("-chardev") < position("-tpmdev"));
    assert!(position("-tpmdev") < position("-qmp"));
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn install_phase_adds_the_installer_media_and_boots_it_once() {
    let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    let plan = QemuPlanner::new(headless_capabilities())
        .plan(&manifest, Path::new("/state/win-dev"), LaunchPhase::Install)
        .expect("plan should be built");
    let drives = option_values(&plan.arguments, "-drive");
    assert_eq!(drives.len(), 4);
    assert_eq!(
        drives[3],
        format!("media=cdrom,readonly=on,file={}", iso.display())
    );
    assert_eq!(option_values(&plan.arguments, "-boot"), ["once=d,menu=on"]);
    assert!(!plan
        .arguments
        .iter()
        .any(|argument| argument.to_string_lossy().contains("lsw-identity")));
    fs::remove_file(iso).expect("temporary ISO should be removable");
}

#[test]
fn a_symlinked_seed_is_rejected_only_in_the_phase_that_attaches_it() {
    let (manifest, iso) = test_manifest(WindowsProfile::Vanilla);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock must be valid")
        .as_nanos();
    let instance_dir = std::env::temp_dir().join(format!("lsw-qemu-seed-{nonce}"));
    fs::create_dir_all(&instance_dir).expect("instance directory");
    fs::write(instance_dir.join("seed"), b"not a directory").expect("bogus seed");
    fs::write(instance_dir.join("identity-seed"), b"not a directory").expect("bogus identity");
    let planner = QemuPlanner::new(headless_capabilities());

    assert!(matches!(
        planner.plan(&manifest, &instance_dir, LaunchPhase::Install),
        Err(crate::LswError::InvalidValue {
            field: "install seed",
            ..
        })
    ));
    assert!(matches!(
        planner.plan(&manifest, &instance_dir, LaunchPhase::Run),
        Err(crate::LswError::InvalidValue {
            field: "boot identity seed",
            ..
        })
    ));
    fs::remove_file(instance_dir.join("seed")).expect("bogus seed removed");
    assert!(planner
        .plan(&manifest, &instance_dir, LaunchPhase::Install)
        .is_ok());

    fs::remove_dir_all(&instance_dir).expect("instance directory removed");
    fs::remove_file(iso).expect("temporary ISO should be removable");
}
