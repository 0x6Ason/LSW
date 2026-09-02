// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[test]
fn exact_external_qemu_identity_requires_all_runtime_arguments() {
    let instance = Path::new("/state/instances/win");
    let exact = b"/usr/bin/qemu-system-x86_64\0-name\0win\0-qmp\0unix:/state/instances/win/run/qmp.sock,server=on,wait=off\0-pidfile\0/state/instances/win/run/qemu.pid\0";
    assert!(qemu_command_matches_instance(exact, instance, "win"));

    let wrong_name = b"/usr/bin/qemu-system-x86_64\0-name\0other\0-qmp\0unix:/state/instances/win/run/qmp.sock,server=on,wait=off\0-pidfile\0/state/instances/win/run/qemu.pid\0";
    assert!(!qemu_command_matches_instance(wrong_name, instance, "win"));
    let wrong_program = b"/usr/bin/sleep\0-name\0win\0-qmp\0unix:/state/instances/win/run/qmp.sock,server=on,wait=off\0-pidfile\0/state/instances/win/run/qemu.pid\0";
    assert!(!qemu_command_matches_instance(
        wrong_program,
        instance,
        "win"
    ));
}

#[test]
fn owned_qemu_is_live_without_consuming_its_single_qmp_endpoint() {
    assert_eq!(
        qemu_process_evidence(Path::new("/missing"), "win", true),
        QemuProcessEvidence::Live
    );
}

#[test]
fn missing_pidfile_scans_for_the_exact_external_qemu() {
    let root = std::env::temp_dir().join(format!(
        "lsw-supervisor-proc-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock should be after the Unix epoch")
            .as_nanos()
    ));
    let instance = root.join("instances/win");
    let proc_root = root.join("proc");
    let process = proc_root.join("1234");
    fs::create_dir_all(instance.join("run")).expect("instance fixture should be created");
    fs::create_dir_all(&process).expect("proc fixture should be created");
    let exact = format!(
        "/usr/bin/qemu-system-x86_64\0-name\0win\0-qmp\0unix:{}/run/qmp.sock,server=on,wait=off\0-pidfile\0{}/run/qemu.pid\0",
        instance.display(),
        instance.display()
    );
    fs::write(process.join("cmdline"), exact.as_bytes())
        .expect("exact process fixture should be written");
    assert_eq!(
        qemu_process_evidence_with_proc(&instance, "win", &proc_root),
        QemuProcessEvidence::Live
    );

    fs::write(process.join("cmdline"), b"/usr/bin/sleep\0")
        .expect("stale process fixture should be written");
    assert_eq!(
        qemu_process_evidence_with_proc(&instance, "win", &proc_root),
        QemuProcessEvidence::Gone
    );
    fs::remove_dir_all(root).expect("fixture should be removed");
}

#[test]
fn stale_socket_cleanup_refuses_regular_files() {
    let root = std::env::temp_dir().join(format!("lsw-supervisor-test-{}", std::process::id()));
    let run = root.join("run");
    fs::create_dir_all(&run).expect("fixture should be created");
    fs::write(run.join("qmp.sock"), b"do not remove").expect("fixture file should be written");
    assert!(cleanup_runtime_sockets(&root).is_err());
    assert_eq!(
        fs::read(run.join("qmp.sock")).expect("fixture should remain"),
        b"do not remove"
    );
    fs::remove_dir_all(root).expect("fixture should be removed");
}

#[test]
fn external_force_stop_requires_proof_that_an_active_vm_stopped() {
    for state in [
        InstanceState::Running,
        InstanceState::Installing,
        InstanceState::Suspended,
    ] {
        assert!(refuse_unproven_external_force_stop(true, false, false, state, "win").is_err());
        assert!(refuse_unproven_external_force_stop(true, true, false, state, "win").is_ok());
        assert!(refuse_unproven_external_force_stop(true, false, true, state, "win").is_ok());
    }
    assert!(
        refuse_unproven_external_force_stop(true, false, false, InstanceState::Stopped, "win")
            .is_ok()
    );
}

#[test]
fn requested_shutdown_wins_qemu_exit_status() {
    assert_eq!(
        state_after_qemu_exit(true, false, false),
        InstanceState::Stopped
    );
    assert_eq!(
        state_after_qemu_exit(false, false, true),
        InstanceState::Stopped
    );
    assert_eq!(
        state_after_qemu_exit(false, false, false),
        InstanceState::Failed
    );
    assert_eq!(
        state_after_qemu_exit(false, true, false),
        InstanceState::Hibernated
    );
}
