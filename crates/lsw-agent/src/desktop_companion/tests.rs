// SPDX-License-Identifier: GPL-3.0-or-later

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    read_frame, write_frame, ClientHello, Frame, FrameKind, ProcessEnvironment, ServerHello,
    SessionKind, StartRequest, AGENT_PROTOCOL_VERSION,
};

use super::{
    aam_activation_is_eligible, close_ack_exit_code, initial_window_state_action,
    retain_recovered_window_after_setup_failure, ConnectionWorkerPermit, DesktopCompanionState,
    DetachedGuiRegistry, GuiSessionEndPolicy, GuiStartRequest, RecoverableWindowIdentity,
    SharedGuiRecoveryRegistry, MAX_CONNECTION_WORKERS, MAX_DETACHED_GUI_WINDOWS,
    MAX_GUI_WINDOW_SESSIONS,
};

#[derive(Debug)]
struct FakeWindow {
    identity: Option<(u32, u64)>,
}

impl RecoverableWindowIdentity for FakeWindow {
    fn current_identity(&self) -> Option<(u32, u64)> {
        self.identity
    }
}

fn gui_request(program: &str) -> GuiStartRequest {
    GuiStartRequest {
        user_name: "desktop-user".to_owned(),
        request: StartRequest {
            kind: SessionKind::Run,
            argv: vec![program.to_owned()],
            working_directory: None,
        },
        environment: ProcessEnvironment::new(Vec::new()).unwrap(),
        mount_live_share: false,
    }
}

#[test]
fn closed_window_ack_preserves_observed_code_without_failing_a_draining_launcher() {
    assert_eq!(close_ack_exit_code(Some(23)), 23);
    assert_eq!(close_ack_exit_code(None), 0);
}

#[test]
fn initially_maximized_guest_requests_an_idempotent_host_state() {
    assert_eq!(
        initial_window_state_action(true),
        Some(lsw_core::GuiWindowAction::Maximize)
    );
    assert_eq!(initial_window_state_action(false), None);
}

#[test]
fn aam_activation_is_limited_to_an_unmodified_single_program_request() {
    let request = gui_request("notepad.exe");
    assert!(aam_activation_is_eligible(&request));

    let mut extra_argument = request.clone();
    extra_argument.request.argv.push("note.txt".to_owned());
    assert!(!aam_activation_is_eligible(&extra_argument));

    let mut working_directory = request.clone();
    working_directory.request.working_directory = Some(r"C:\Temp".to_owned());
    assert!(!aam_activation_is_eligible(&working_directory));

    let mut environment = request;
    environment.environment =
        ProcessEnvironment::new(vec![("MODE".to_owned(), "test".to_owned())]).unwrap();
    assert!(!aam_activation_is_eligible(&environment));
}

#[test]
fn dirty_outer_close_cross_product_never_finishes_a_live_window() {
    for (explicit_close_requested, window_is_live, expected) in [
        (false, true, GuiSessionEndPolicy::Detach),
        (false, false, GuiSessionEndPolicy::NaturalGuestClose),
        (true, true, GuiSessionEndPolicy::Detach),
        (true, false, GuiSessionEndPolicy::ExplicitHostClose),
    ] {
        assert_eq!(
            super::gui_session_end_policy(explicit_close_requested, window_is_live),
            expected
        );
    }
}

#[test]
fn dirty_outer_close_retains_only_the_exact_request_for_recovery() {
    let request = gui_request("notepad.exe");
    let other = gui_request("mspaint.exe");
    let mut registry = DetachedGuiRegistry::default();
    assert_eq!(
        super::gui_session_end_policy(true, true),
        GuiSessionEndPolicy::Detach
    );
    registry
        .insert(
            request.clone(),
            FakeWindow {
                identity: Some((42, 73)),
            },
        )
        .unwrap();

    assert!(registry.take(&other).is_none());
    assert_eq!(
        registry.take(&request).unwrap().current_identity(),
        Some((42, 73))
    );
    assert!(registry.take(&request).is_none());
}

#[test]
fn failed_reattach_setup_restores_the_exact_pinned_window() {
    let request = gui_request("notepad.exe");
    let other = gui_request("mspaint.exe");
    let registry = SharedGuiRecoveryRegistry::<FakeWindow>::default();
    let (seed, recovered) = registry.claim(&request).unwrap();
    assert!(recovered.is_none());
    seed.retain(FakeWindow {
        identity: Some((42, 73)),
    })
    .unwrap();
    let (claim, recovered) = registry.claim(&request).unwrap();
    let recovered = recovered.unwrap();

    let error = retain_recovered_window_after_setup_failure(
        claim,
        recovered,
        "reattach setup failed".to_owned(),
    );
    assert_eq!(error.to_string(), "reattach setup failed");
    let (other_claim, other_window) = registry.claim(&other).unwrap();
    assert!(other_window.is_none());
    drop(other_claim);
    let (claim, recovered) = registry.claim(&request).unwrap();
    assert_eq!(recovered.unwrap().current_identity(), Some((42, 73)));
    drop(claim);
}

#[test]
fn detached_registry_reattaches_only_the_identical_request_once() {
    let request = gui_request("notepad.exe");
    let other = gui_request("mspaint.exe");
    let mut registry = DetachedGuiRegistry::default();
    registry
        .insert(
            request.clone(),
            FakeWindow {
                identity: Some((42, 73)),
            },
        )
        .unwrap();

    assert!(registry.take(&other).is_none());
    assert_eq!(registry.entries.len(), 1);
    assert_eq!(
        registry.take(&request).unwrap().current_identity(),
        Some((42, 73))
    );
    assert!(registry.take(&request).is_none());
}

#[test]
fn detached_registry_requires_every_gui_request_field_to_match() {
    let request = gui_request("notepad.exe");
    let mut registry = DetachedGuiRegistry::default();
    registry
        .insert(
            request.clone(),
            FakeWindow {
                identity: Some((42, 73)),
            },
        )
        .unwrap();

    let mut different_user = request.clone();
    different_user.user_name = "other-user".to_owned();
    assert!(registry.take(&different_user).is_none());

    let mut different_cwd = request.clone();
    different_cwd.request.working_directory = Some(r"C:\Temp".to_owned());
    assert!(registry.take(&different_cwd).is_none());

    let mut different_environment = request.clone();
    different_environment.environment =
        ProcessEnvironment::new(vec![("MODE".to_owned(), "test".to_owned())]).unwrap();
    assert!(registry.take(&different_environment).is_none());

    let mut different_mount = request.clone();
    different_mount.mount_live_share = true;
    assert!(registry.take(&different_mount).is_none());

    assert_eq!(registry.entries.len(), 1);
    assert_eq!(
        registry.take(&request).unwrap().current_identity(),
        Some((42, 73))
    );
}

#[test]
fn detached_registry_purges_changed_pid_or_hwnd_identity() {
    let request = gui_request("notepad.exe");
    let mut registry = DetachedGuiRegistry::default();
    registry
        .insert(
            request.clone(),
            FakeWindow {
                identity: Some((42, 73)),
            },
        )
        .unwrap();
    registry.entries[0].window.identity = Some((43, 73));
    assert!(registry.take(&request).is_none());
    assert!(registry.entries.is_empty());

    registry
        .insert(
            request.clone(),
            FakeWindow {
                identity: Some((42, 73)),
            },
        )
        .unwrap();
    registry.entries[0].window.identity = Some((42, 74));
    assert!(registry.take(&request).is_none());
    assert!(registry.entries.is_empty());
}

#[test]
fn detached_registry_is_bounded_and_rejects_duplicate_request_state() {
    let mut registry = DetachedGuiRegistry::default();
    let duplicate = gui_request("duplicate.exe");
    registry
        .insert(
            duplicate.clone(),
            FakeWindow {
                identity: Some((1, 1)),
            },
        )
        .unwrap();
    assert!(registry
        .insert(
            duplicate,
            FakeWindow {
                identity: Some((2, 2)),
            },
        )
        .is_err());

    for index in 1..MAX_DETACHED_GUI_WINDOWS {
        registry
            .insert(
                gui_request(&format!("program-{index}.exe")),
                FakeWindow {
                    identity: Some((u32::try_from(index).unwrap() + 10, index as u64 + 10)),
                },
            )
            .unwrap();
    }
    assert_eq!(registry.entries.len(), MAX_DETACHED_GUI_WINDOWS);
    assert!(registry.ensure_launch_capacity().is_err());
    assert!(registry
        .insert(
            gui_request("overflow.exe"),
            FakeWindow {
                identity: Some((999, 999)),
            },
        )
        .is_err());
}

#[test]
fn shared_registry_claim_is_exclusive_and_recovers_the_exact_window() {
    let request = gui_request("notepad.exe");
    let registry = SharedGuiRecoveryRegistry::<FakeWindow>::default();

    let (claim, recovered) = registry.claim(&request).unwrap();
    assert!(recovered.is_none());
    assert_eq!(registry.counts(), (1, 0));
    assert!(registry.claim(&request).is_err());

    claim
        .retain(FakeWindow {
            identity: Some((42, 73)),
        })
        .unwrap();
    assert_eq!(registry.counts(), (0, 1));

    let (reattached, recovered) = registry.claim(&request).unwrap();
    let recovered = recovered.expect("the exact detached HWND should be recovered");
    assert_eq!(recovered.current_identity(), Some((42, 73)));
    assert_eq!(registry.counts(), (1, 0));
    assert!(registry.claim(&request).is_err());

    reattached.retain(recovered).unwrap();
    assert_eq!(registry.counts(), (0, 1));
}

#[test]
fn shared_registry_bounds_active_and_detached_windows_together() {
    let registry = SharedGuiRecoveryRegistry::<FakeWindow>::default();
    let detached_count = MAX_GUI_WINDOW_SESSIONS / 2;
    for index in 0..detached_count {
        let request = gui_request(&format!("detached-{index}.exe"));
        let (claim, recovered) = registry.claim(&request).unwrap();
        assert!(recovered.is_none());
        claim
            .retain(FakeWindow {
                identity: Some((u32::try_from(index).unwrap() + 1, index as u64 + 1)),
            })
            .unwrap();
    }

    let mut active = Vec::new();
    for index in detached_count..MAX_GUI_WINDOW_SESSIONS {
        let request = gui_request(&format!("active-{index}.exe"));
        let (claim, recovered) = registry.claim(&request).unwrap();
        assert!(recovered.is_none());
        active.push(claim);
    }
    assert_eq!(registry.counts(), (detached_count, detached_count));
    assert!(registry.claim(&gui_request("overflow.exe")).is_err());

    drop(active.pop());
    let (replacement, recovered) = registry.claim(&gui_request("replacement.exe")).unwrap();
    assert!(recovered.is_none());
    active.push(replacement);
    assert_eq!(
        registry.counts().0 + registry.counts().1,
        MAX_GUI_WINDOW_SESSIONS
    );

    // Reattach is an atomic detached-to-active transition and therefore
    // remains possible even while the combined registry is full.
    let detached = gui_request("detached-0.exe");
    let (reattached, recovered) = registry.claim(&detached).unwrap();
    let recovered = recovered.expect("full registries must still permit exact recovery");
    assert!(registry.claim(&detached).is_err());
    assert_eq!(
        registry.counts().0 + registry.counts().1,
        MAX_GUI_WINDOW_SESSIONS
    );
    reattached.retain(recovered).unwrap();
}

#[test]
fn gui_session_claim_is_released_when_its_worker_panics() {
    let request = gui_request("notepad.exe");
    let registry = SharedGuiRecoveryRegistry::<FakeWindow>::default();
    let worker_registry = registry.clone();
    let worker_request = request.clone();
    let worker = thread::spawn(move || {
        let (_claim, recovered) = worker_registry.claim(&worker_request).unwrap();
        assert!(recovered.is_none());
        panic!("deliberate GUI worker panic");
    });
    assert!(worker.join().is_err());
    assert_eq!(registry.counts(), (0, 0));

    let (claim, recovered) = registry.claim(&request).unwrap();
    assert!(recovered.is_none());
    drop(claim);
    assert_eq!(registry.counts(), (0, 0));
}

#[test]
fn connection_worker_permits_are_hard_bounded_and_raii_released() {
    let active = Arc::new(AtomicUsize::new(0));
    let permits = (0..MAX_CONNECTION_WORKERS)
        .map(|_| {
            ConnectionWorkerPermit::try_acquire(&active)
                .expect("every worker below the hard limit should be admitted")
        })
        .collect::<Vec<_>>();
    assert_eq!(active.load(Ordering::Acquire), MAX_CONNECTION_WORKERS);
    assert!(ConnectionWorkerPermit::try_acquire(&active).is_none());

    drop(permits);
    assert_eq!(active.load(Ordering::Acquire), 0);
}

fn authenticate_desktop_connection(stream: &mut TcpStream, token: &str) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write_frame(
        stream,
        &Frame::new(
            FrameKind::Hello,
            ClientHello {
                version: AGENT_PROTOCOL_VERSION,
                token: token.to_owned(),
            }
            .encode()
            .unwrap(),
        ),
    )
    .unwrap();
    let hello = read_frame(stream).expect("the worker should answer without another session");
    assert_eq!(hello.kind, FrameKind::HelloOk);
    ServerHello::decode(&hello.payload).unwrap();
}

#[test]
fn two_desktop_connections_do_not_block_each_other() {
    let token = "a".repeat(64);
    let state = Arc::new(DesktopCompanionState::new(
        token.clone(),
        "b".repeat(64),
        "desktop-user".to_owned(),
    ));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server_state = Arc::clone(&state);
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (stream, peer) = listener.accept().unwrap();
            assert!(peer.ip().is_loopback());
            assert!(super::dispatch_connection_worker(stream, Arc::clone(&server_state)).unwrap());
        }
    });

    let mut blocked = TcpStream::connect(address).unwrap();
    authenticate_desktop_connection(&mut blocked, &token);
    // The first worker remains blocked waiting for its request frame.
    let mut independent = TcpStream::connect(address).unwrap();
    authenticate_desktop_connection(&mut independent, &token);
    server.join().unwrap();
    assert_eq!(state.active_worker_count(), 2);

    for stream in [&mut independent, &mut blocked] {
        write_frame(stream, &Frame::new(FrameKind::Ping, Vec::new())).unwrap();
        assert_eq!(read_frame(stream).unwrap().kind, FrameKind::Error);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.active_worker_count() != 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(state.active_worker_count(), 0);
}
