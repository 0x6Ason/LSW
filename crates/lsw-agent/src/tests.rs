// SPDX-License-Identifier: GPL-3.0-or-later

//! Protocol, process-lifecycle, service, and configuration regression tests.
//!
//! Keeping these fixtures beside the agent but outside `main.rs` makes the
//! production control flow reviewable without weakening private-item access.

use super::*;

#[cfg(unix)]
#[test]
fn shell_fallback_reaches_a_known_program() {
    let request = StartRequest {
        kind: SessionKind::Shell,
        argv: vec!["definitely-not-an-lsw-shell".to_owned(), "sh".to_owned()],
        working_directory: None,
    };
    let mut child = spawn_request(&request).expect("sh fallback should start");
    child.kill().expect("fixture process should stop");
    assert!(
        child.tree.is_none(),
        "successful teardown must disarm owner"
    );
    child
        .kill()
        .expect("a repeated cleanup request must be a no-op");
    child.wait().expect("fixture process should be reaped");
    assert!(
        child.tree.is_none(),
        "wait and Drop must not re-signal PGID"
    );
    // Exercise Drop with an already-reaped leader and disarmed owner. It
    // must not call Child::kill on a potentially reused Unix PID.
    drop(child);
}

#[test]
fn token_parser_rejects_short_or_uppercase_secrets() {
    let root = std::env::temp_dir().join(format!("lsw-agent-token-{}", std::process::id()));
    fs::write(&root, "abcd").expect("fixture should be written");
    assert!(read_token(&root).is_err());
    fs::write(&root, "A".repeat(64)).expect("fixture should be updated");
    assert!(read_token(&root).is_err());
    fs::remove_file(root).expect("fixture should be removed");
}

#[test]
fn session_limit_is_bounded() {
    let arguments = vec![
        "--token-file".into(),
        "token.txt".into(),
        "--max-sessions".into(),
        "4".into(),
    ];
    let configuration = Configuration::parse(&arguments).expect("configuration should parse");
    assert_eq!(configuration.max_sessions, 4);

    let invalid = vec![
        "--token-file".into(),
        "token.txt".into(),
        "--max-sessions".into(),
        "0".into(),
    ];
    assert!(Configuration::parse(&invalid).is_err());
}

#[test]
fn service_mode_composes_with_existing_options() {
    let arguments = vec![
        "--service".into(),
        "--token-file".into(),
        "C:\\ProgramData\\LSW\\agent.token".into(),
        "--listen".into(),
        "127.0.0.1:55040".into(),
        "--max-sessions".into(),
        "8".into(),
    ];
    let configuration = Configuration::parse(&arguments).expect("configuration should parse");

    assert!(configuration.service);
    assert_eq!(configuration.service_kind, ServiceKind::Agent);
    assert_eq!(configuration.listen, "127.0.0.1:55040".parse().unwrap());
    assert_eq!(configuration.max_sessions, 8);
    assert_eq!(
        configuration.token_file,
        PathBuf::from("C:\\ProgramData\\LSW\\agent.token")
    );
}

#[test]
fn license_helper_is_loopback_only_and_uses_a_distinct_service_mode() {
    let configuration = Configuration::parse(&[
        "--license-helper".into(),
        "--token-file".into(),
        "agent.token".into(),
        "--listen".into(),
        "127.0.0.1:5041".into(),
    ])
    .expect("license helper configuration should parse");
    assert!(configuration.service);
    assert_eq!(configuration.service_kind, ServiceKind::LicenseHelper);
    assert!(configuration.listen.ip().is_loopback());

    assert!(Configuration::parse(&[
        "--license-helper".into(),
        "--token-file".into(),
        "agent.token".into(),
        "--listen".into(),
        "0.0.0.0:5041".into(),
    ])
    .is_err());
}

#[test]
fn product_key_shape_is_strict_without_recording_a_real_key() {
    assert!(valid_product_key(b"AAAAA-BBBBB-CCCCC-DDDDD-EEEEE"));
    assert!(!valid_product_key(b"AAAAA-BBBBB-CCCCC-DDDDD"));
    assert!(!valid_product_key(b"AAAAA_BBBBB_CCCCC_DDDDD_EEEEE"));
    assert!(!valid_product_key(b"aaaaa-bbbbb-ccccc-ddddd-eeeee"));
}

#[cfg(not(windows))]
#[test]
fn service_mode_fails_before_accessing_files_off_windows() {
    let error = run(vec![
        "--service".into(),
        "--token-file".into(),
        "a-file-that-must-not-be-read".into(),
    ])
    .expect_err("service mode must be Windows-only");

    assert_eq!(error.to_string(), "--service is only supported on Windows");
}

#[cfg(windows)]
#[test]
fn service_mode_requires_scm_before_accessing_files() {
    let error = run(vec![
        "--service".into(),
        "--token-file".into(),
        "a-file-that-must-not-be-read".into(),
    ])
    .expect_err("a normal test process must not connect to SCM as a service");

    assert!(
        error
            .to_string()
            .contains("--service must be started by SCM"),
        "unexpected SCM rejection: {error}"
    );
}

#[test]
fn shutdown_channel_wakes_or_disconnects_the_accept_wait() {
    let (sender, receiver) = mpsc::sync_channel(1);
    sender.send(()).expect("shutdown should be queued");
    assert!(wait_for_shutdown(&receiver, Duration::from_secs(1)));

    let (sender, receiver) = mpsc::sync_channel(1);
    drop(sender);
    assert!(shutdown_requested(&receiver));
}

#[test]
fn normal_exit_wins_a_cancel_race() {
    assert_eq!(cancel_session_end(false), SessionEnd::Normal);
    assert_eq!(cancel_session_end(true), SessionEnd::Cancelled);
}

#[test]
fn normal_exit_wins_a_lease_expiry_race() {
    assert_eq!(lease_session_end(false), SessionEnd::Normal);
    assert_eq!(lease_session_end(true), SessionEnd::LeaseExpired);
}

#[test]
fn windows_command_line_quotes_empty_and_space_containing_arguments() {
    assert_eq!(windows_command_line("", &[]), "\"\"");
    assert_eq!(
        windows_command_line("C:\\Program Files\\pwsh.exe", &["hello world"]),
        "\"C:\\Program Files\\pwsh.exe\" \"hello world\""
    );
}

#[test]
fn windows_command_line_quotes_quotes_and_trailing_backslashes() {
    assert_eq!(
        windows_command_line("tool.exe", &["a\"b"]),
        "tool.exe \"a\\\"b\""
    );
    assert_eq!(
        windows_command_line("tool.exe", &["C:\\path with space\\"]),
        "tool.exe \"C:\\path with space\\\\\""
    );
    assert_eq!(
        windows_command_line("tool.exe", &["plain\\"]),
        "tool.exe plain\\"
    );
}

#[cfg(windows)]
#[test]
fn windows_pipe_job_terminates_a_spawned_descendant() {
    let mut child = spawn_program(
        "cmd.exe",
        &["/D", "/Q", "/C", "ping.exe -n 30 127.0.0.1 >NUL"],
        None,
    )
    .expect("cmd should start inside a Job Object");
    let leader = child.process.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    // Observe kernel-owned Job membership instead of treating shell output
    // as readiness. This keeps the lifecycle assertion independent of
    // PowerShell cold-start and Start-Process scheduling delays.
    let descendant = loop {
        let process_ids = child
            .tree
            .as_ref()
            .expect("spawned process should retain its Job Object")
            .process_ids()
            .expect("Job Object process membership should be queryable");
        if let Some(process_id) = process_ids
            .into_iter()
            .find(|process_id| *process_id != leader)
        {
            break process_id;
        }
        assert!(
            Instant::now() < deadline,
            "cmd did not start its ping descendant within 10 seconds"
        );
        thread::sleep(Duration::from_millis(10));
    };

    let (_, terminated) = child
        .terminate()
        .expect("Job Object termination should succeed");
    assert!(terminated);
    assert!(
        process_tree::wait_for_process_id_exit(descendant, 2_000)
            .expect("descendant state should be queryable"),
        "the Job Object descendant was still running after session cancellation"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn windows_job_ffi_layout_matches_the_x64_sdk_abi() {
    assert_eq!(process_tree::ffi_layout_sizes(), (64, 144, 28));
}

#[cfg(windows)]
#[test]
fn windows_conpty_process_starts_inside_a_job() {
    let request = StartRequest {
        kind: SessionKind::Shell,
        argv: vec!["cmd.exe".to_owned()],
        working_directory: None,
    };
    let process = windows_conpty::spawn_shell(
        &request,
        TerminalSize {
            columns: 80,
            rows: 25,
        },
    )
    .expect("ConPTY shell should start suspended, join its Job, and resume");
    process
        .job
        .terminate(SESSION_CANCEL_EXIT_CODE)
        .expect("ConPTY Job termination should succeed");
    assert_eq!(
        windows_conpty::wait_for_process(&process.process)
            .expect("ConPTY process should become signalled"),
        SESSION_CANCEL_EXIT_CODE
    );
}

#[cfg(not(windows))]
#[test]
fn non_windows_agent_does_not_advertise_conpty() {
    let capabilities = agent_capabilities();
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_CONPTY_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_TERMINAL_RESIZE_V1));
    assert!(capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_SESSION_CONTROL_V1));
    assert!(capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_SESSION_LEASE_V1));
}

#[cfg(unix)]
fn controlled_test_connection(
    token: String,
) -> (TcpStream, Receiver<Result<(), String>>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let expected_token = token.clone();
    let active_sessions = Arc::new(AtomicUsize::new(1));
    let server_sessions = Arc::clone(&active_sessions);
    let (done_sender, done_receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = {
            let _slot = SessionSlot(server_sessions);
            let (stream, _) = listener.accept().expect("fixture should connect");
            handle_connection(stream, &expected_token).map_err(|error| error.to_string())
        };
        let _ = done_sender.send(result);
    });

    let mut stream = TcpStream::connect(address).expect("client should connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout should apply");
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token,
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
    )
    .expect("hello should be sent");
    let response = read_frame(&mut stream).expect("hello response should arrive");
    assert_eq!(response.kind, FrameKind::HelloOk);
    let hello = ServerHello::decode(&response.payload).expect("server hello should decode");
    assert!(hello
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY_SESSION_CONTROL_V1));
    assert!(hello
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY_SESSION_LEASE_V1));
    (stream, done_receiver, active_sessions)
}

#[cfg(unix)]
fn send_session_options(stream: &mut TcpStream) {
    let options = SessionOptions {
        cancel_on_disconnect: true,
    };
    write_frame(
        stream,
        &Frame::new(FrameKind::SessionOptions, options.encode()),
    )
    .expect("session options should be sent");
}

#[cfg(unix)]
fn send_session_lease(stream: &mut TcpStream, timeout_millis: u32) {
    let lease = SessionLease::new(timeout_millis).expect("test lease should be valid");
    write_frame(stream, &Frame::new(FrameKind::SessionLease, lease.encode()))
        .expect("session lease should be sent");
}

#[cfg(unix)]
fn send_exec(stream: &mut TcpStream, argv: &[&str]) {
    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
        working_directory: None,
    };
    write_frame(
        stream,
        &Frame::new(FrameKind::Start, request.encode().unwrap()),
    )
    .expect("start should be sent");
}

#[cfg(unix)]
fn send_waiting_descendant_tree(stream: &mut TcpStream) {
    // outer sh -> inner sh -> sleep. Every process inherits the session
    // output pipes; killing only the outer process would make bridge joins
    // and the session slot hang until sleep exits.
    send_exec(
        stream,
        &[
            "sh",
            "-c",
            "sh -c 'sleep 30 & printf tree-ready; wait' & wait",
        ],
    );
    let ready = read_frame(stream).expect("descendant readiness should arrive");
    assert_eq!(ready.kind, FrameKind::Stdout);
    assert_eq!(ready.payload, b"tree-ready");
}

#[cfg(unix)]
fn collect_process(stream: &mut TcpStream) -> (Vec<u8>, i32) {
    let mut stdout = Vec::new();
    loop {
        let frame = read_frame(stream).expect("process response should arrive");
        match frame.kind {
            FrameKind::Stdout => stdout.extend(frame.payload),
            FrameKind::Stderr => {}
            FrameKind::Exit => return (stdout, lsw_core::decode_exit(&frame.payload).unwrap()),
            other => panic!("unexpected process frame {other:?}"),
        }
    }
}

#[cfg(unix)]
fn assert_session_released(done: Receiver<Result<(), String>>, active_sessions: Arc<AtomicUsize>) {
    done.recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .expect("server session should succeed");
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn authenticated_cancel_terminates_a_controlled_process() {
    let (mut stream, done, active_sessions) = controlled_test_connection("d".repeat(64));
    send_session_options(&mut stream);
    send_waiting_descendant_tree(&mut stream);
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionCancel, Vec::new()),
    )
    .expect("cancel should be sent");

    assert_eq!(
        collect_process(&mut stream),
        (Vec::new(), SESSION_CANCEL_EXIT_CODE)
    );
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn session_control_is_unavailable_before_authentication() {
    let expected_token = "7".repeat(64);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("fixture should connect");
        handle_connection(stream, &expected_token).map_err(|error| error.to_string())
    });
    let mut stream = TcpStream::connect(address).expect("client should connect");
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token: "8".repeat(64),
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
    )
    .expect("hello should be sent");
    let response = read_frame(&mut stream).expect("authentication error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("authentication"));
    assert!(server
        .join()
        .expect("fixture should not panic")
        .expect_err("authentication should fail")
        .contains("authentication"));
}

#[cfg(unix)]
#[test]
fn controlled_disconnect_terminates_process_and_releases_slot() {
    let (mut stream, done, active_sessions) = controlled_test_connection("e".repeat(64));
    send_session_options(&mut stream);
    send_waiting_descendant_tree(&mut stream);
    drop(stream);

    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn leased_session_expires_and_releases_its_process_tree() {
    let (mut stream, done, active_sessions) = controlled_test_connection("4".repeat(64));
    send_session_options(&mut stream);
    send_session_lease(&mut stream, 1_000);
    send_waiting_descendant_tree(&mut stream);

    assert!(
        read_frame(&mut stream).is_err(),
        "lease expiry closes the half-open transport instead of risking a blocking error write"
    );
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("leased server session should finish promptly")
        .expect_err("lease expiry should fail the session")
        .contains("lease expired"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn lease_expiry_unblocks_output_backpressure_and_releases_slot() {
    let (mut stream, done, active_sessions) = controlled_test_connection("8".repeat(64));
    send_session_options(&mut stream);
    send_session_lease(&mut stream, 1_000);
    send_exec(&mut stream, &["sh", "-c", "exec yes lsw-lease-output"]);

    // Deliberately never read process output. The agent output bridge will
    // eventually block in TCP write while holding its shared writer lock.
    // Lease expiry must use socket shutdown, not that lock, to free it.
    assert!(done
        .recv_timeout(Duration::from_secs(3))
        .expect("backpressured leased session should finish promptly")
        .expect_err("lease expiry should fail the session")
        .contains("lease expired"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    drop(stream);
}

#[cfg(unix)]
#[test]
fn timely_heartbeats_keep_an_idle_leased_session_alive() {
    let (mut stream, done, active_sessions) = controlled_test_connection("5".repeat(64));
    send_session_options(&mut stream);
    send_session_lease(&mut stream, 2_000);
    send_waiting_descendant_tree(&mut stream);

    // The total duration exceeds one lease, while every individual gap is
    // comfortably below it. This proves idle-but-healthy sessions survive.
    for _ in 0..9 {
        thread::sleep(Duration::from_millis(250));
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::SessionHeartbeat, Vec::new()),
        )
        .expect("heartbeat should be sent");
    }
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionCancel, Vec::new()),
    )
    .expect("cancel should be sent");
    assert_eq!(
        collect_process(&mut stream),
        (Vec::new(), SESSION_CANCEL_EXIT_CODE)
    );
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn heartbeat_requires_a_leased_session_and_empty_payload() {
    for (token, with_lease, payload) in [("a", false, Vec::new()), ("b", true, vec![1])] {
        let (mut stream, done, active_sessions) = controlled_test_connection(token.repeat(64));
        send_session_options(&mut stream);
        if with_lease {
            send_session_lease(&mut stream, 5_000);
        }
        send_waiting_descendant_tree(&mut stream);
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::SessionHeartbeat, payload),
        )
        .expect("invalid heartbeat should be sent");

        let response = read_frame(&mut stream).expect("protocol error should arrive");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("requires a leased"));
        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .expect("server session should finish promptly")
            .is_err());
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }
}

#[cfg(unix)]
#[test]
fn lease_requires_options_and_can_appear_only_once_before_start() {
    let (mut legacy, legacy_done, legacy_sessions) = controlled_test_connection("c".repeat(64));
    send_session_lease(&mut legacy, 5_000);
    let response = read_frame(&mut legacy).expect("legacy lease should be rejected");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("unsupported request"));
    assert!(legacy_done
        .recv_timeout(Duration::from_secs(2))
        .expect("legacy request should finish promptly")
        .is_err());
    assert_eq!(legacy_sessions.load(Ordering::Acquire), 0);

    let (mut duplicate, duplicate_done, duplicate_sessions) =
        controlled_test_connection("d".repeat(64));
    send_session_options(&mut duplicate);
    send_session_lease(&mut duplicate, 5_000);
    send_session_lease(&mut duplicate, 5_000);
    let response = read_frame(&mut duplicate).expect("duplicate lease should be rejected");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("one SESSION_LEASE"));
    assert!(duplicate_done
        .recv_timeout(Duration::from_secs(2))
        .expect("duplicate request should finish promptly")
        .is_err());
    assert_eq!(duplicate_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn controlled_frames_reject_nonempty_cancel_payloads() {
    let (mut stream, done, active_sessions) = controlled_test_connection("9".repeat(64));
    send_session_options(&mut stream);
    send_waiting_descendant_tree(&mut stream);
    write_frame(&mut stream, &Frame::new(FrameKind::SessionCancel, [1]))
        .expect("malformed cancel should be sent");

    let response = read_frame(&mut stream).expect("protocol error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("empty payload"));
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .is_err());
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn normal_leader_exit_cleans_background_descendants_and_releases_slot() {
    let (mut stream, done, active_sessions) = controlled_test_connection("6".repeat(64));
    send_session_options(&mut stream);
    // The shell exits normally without waiting for this background sleep.
    // The sleep inherits stdout/stderr, so an agent that owns only the
    // leader blocks in its output bridge instead of sending EXIT.
    send_exec(&mut stream, &["sh", "-c", "sleep 30 & printf normal-tree"]);

    assert_eq!(collect_process(&mut stream), (b"normal-tree".to_vec(), 0));
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn session_options_only_prefix_process_start_requests() {
    for invalid_kind in [
        FrameKind::SessionOptions,
        FrameKind::Ping,
        FrameKind::FileGet,
        FrameKind::FilePut,
        FrameKind::SessionCancel,
        FrameKind::StdinClose,
    ] {
        let token_byte = match invalid_kind {
            FrameKind::SessionOptions => 'a',
            FrameKind::Ping => 'b',
            FrameKind::FileGet => 'c',
            FrameKind::FilePut => 'd',
            FrameKind::SessionCancel => 'e',
            FrameKind::StdinClose => 'f',
            _ => unreachable!(),
        };
        let (mut stream, done, active_sessions) =
            controlled_test_connection(token_byte.to_string().repeat(64));
        send_session_options(&mut stream);
        let payload = if invalid_kind == FrameKind::SessionOptions {
            SessionOptions {
                cancel_on_disconnect: true,
            }
            .encode()
        } else {
            Vec::new()
        };
        write_frame(&mut stream, &Frame::new(invalid_kind, payload))
            .expect("invalid controlled request should be sent");
        let response = read_frame(&mut stream).expect("protocol error should arrive");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("must be followed"));
        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .expect("server session should finish promptly")
            .is_err());
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }
}

#[cfg(unix)]
#[test]
fn unknown_session_option_flags_are_rejected_before_spawn() {
    let (mut stream, done, active_sessions) = controlled_test_connection("2".repeat(64));
    write_frame(&mut stream, &Frame::new(FrameKind::SessionOptions, [2]))
        .expect("unknown options should be sent");
    let response = read_frame(&mut stream).expect("protocol error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("unknown flags"));

    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .expect_err("unknown option flags should fail")
        .contains("unknown flags"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn legacy_control_frame_is_rejected_without_starting_a_process() {
    let (mut stream, done, active_sessions) = controlled_test_connection("3".repeat(64));
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionCancel, Vec::new()),
    )
    .expect("legacy cancel should be sent");
    let response = read_frame(&mut stream).expect("protocol error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("unsupported request"));
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .is_err());
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn controlled_stdin_close_delivers_eof_without_cancelling() {
    let (mut stream, done, active_sessions) = controlled_test_connection("f".repeat(64));
    send_session_options(&mut stream);
    send_exec(
        &mut stream,
        &["sh", "-c", "IFS= read -r value; printf controlled-eof"],
    );
    write_frame(&mut stream, &Frame::new(FrameKind::StdinClose, Vec::new()))
        .expect("stdin close should be sent");

    assert_eq!(
        collect_process(&mut stream),
        (b"controlled-eof".to_vec(), 0)
    );
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn controlled_child_stdin_failure_terminates_process_and_releases_slot() {
    let (mut stream, done, active_sessions) = controlled_test_connection("0".repeat(64));
    // macOS can defer delivery of the peer-side EPIPE while reaping the
    // exec'd shell. Keep the assertion bounded, but use the same five-second
    // allowance as the loopback E2E fixtures instead of the generic
    // two-second protocol timeout.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("stdin-failure timeout should apply");
    send_session_options(&mut stream);
    send_exec(
        &mut stream,
        &["sh", "-c", "exec 0<&-; printf ready; exec sleep 5"],
    );

    let ready = read_frame(&mut stream).expect("child readiness should arrive");
    assert_eq!(ready.kind, FrameKind::Stdout);
    assert_eq!(ready.payload, b"ready");
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Stdin, b"input after child closed stdin"),
    )
    .expect("stdin payload should be sent");

    let response = read_frame(&mut stream).expect("protocol error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("child stdin"));
    drop(stream);
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .expect_err("child stdin failure should fail the session")
        .contains("child stdin"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn legacy_half_close_remains_stdin_eof_not_cancellation() {
    let (mut stream, done, active_sessions) = controlled_test_connection("1".repeat(64));
    send_exec(
        &mut stream,
        &["sh", "-c", "IFS= read -r value; printf legacy-eof"],
    );
    stream
        .shutdown(Shutdown::Write)
        .expect("legacy write side should close");

    assert_eq!(collect_process(&mut stream), (b"legacy-eof".to_vec(), 0));
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn authenticated_loopback_exec_streams_output_and_exit_status() {
    let token = "a".repeat(64);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let expected_token = token.clone();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("fixture should connect");
        handle_connection(stream, &expected_token).expect("agent request should succeed");
    });

    let mut stream = TcpStream::connect(address).expect("client should connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout should apply");
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token,
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
    )
    .expect("hello should be sent");
    let response = read_frame(&mut stream).expect("hello response should arrive");
    assert_eq!(response.kind, FrameKind::HelloOk);
    ServerHello::decode(&response.payload).expect("server hello should decode");

    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf stdout-bytes; printf stderr-bytes >&2; exit 7".to_owned(),
        ],
        working_directory: None,
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Start, request.encode().unwrap()),
    )
    .expect("start should be sent");

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = loop {
        let frame = read_frame(&mut stream).expect("process frame should arrive");
        match frame.kind {
            FrameKind::Stdout => stdout.extend(frame.payload),
            FrameKind::Stderr => stderr.extend(frame.payload),
            FrameKind::Exit => break lsw_core::decode_exit(&frame.payload).unwrap(),
            other => panic!("unexpected process frame {other:?}"),
        }
    };
    assert_eq!(stdout, b"stdout-bytes");
    assert_eq!(stderr, b"stderr-bytes");
    assert_eq!(exit, 7);
    drop(stream);
    server.join().expect("agent fixture should finish");
}

#[cfg(unix)]
#[test]
fn authenticated_loopback_file_transfer_preserves_unicode_and_bytes() {
    fn connect(token: &str) -> (TcpStream, thread::JoinHandle<Result<(), String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let expected_token = token.to_owned();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("fixture should connect");
            handle_connection(stream, &expected_token).map_err(|error| error.to_string())
        });
        let mut stream = TcpStream::connect(address).expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout should apply");
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token: token.to_owned(),
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
        )
        .expect("hello should be sent");
        let response = read_frame(&mut stream).expect("hello response should arrive");
        assert_eq!(response.kind, FrameKind::HelloOk);
        ServerHello::decode(&response.payload).expect("server hello should decode");
        (stream, server)
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("lsw-agent-e2e-{nonce}"));
    fs::create_dir(&root).expect("fixture directory should be created");
    let destination = root.join("résumé-данные.bin");
    let destination_text = destination.to_string_lossy().into_owned();
    let contents = b"binary\0payload\xff\nUTF-8:\xf0\x9f\x9a\x80";
    let token = "b".repeat(64);

    let (mut upload, upload_server) = connect(&token);
    let put = FilePutRequest {
        destination: destination_text.clone(),
        length: contents.len() as u64,
    };
    write_frame(
        &mut upload,
        &Frame::new(FrameKind::FilePut, put.encode().unwrap()),
    )
    .expect("upload request should be sent");
    assert_eq!(
        read_frame(&mut upload)
            .expect("upload ready should arrive")
            .kind,
        FrameKind::Pong
    );
    for chunk in contents.chunks(7) {
        write_frame(
            &mut upload,
            &Frame::new(FrameKind::FileData, chunk.to_vec()),
        )
        .expect("upload data should be sent");
    }
    write_frame(
        &mut upload,
        &Frame::new(
            FrameKind::FileDone,
            encode_file_length(contents.len() as u64),
        ),
    )
    .expect("upload completion should be sent");
    let completion = read_frame(&mut upload).expect("upload completion should arrive");
    assert_eq!(completion.kind, FrameKind::FileDone);
    assert_eq!(
        decode_file_length(&completion.payload).unwrap(),
        contents.len() as u64
    );
    drop(upload);
    upload_server
        .join()
        .expect("upload fixture should finish")
        .expect("upload should succeed");
    assert_eq!(fs::read(&destination).unwrap(), contents);

    let (mut download, download_server) = connect(&token);
    let get = FileGetRequest {
        source: destination_text,
    };
    write_frame(
        &mut download,
        &Frame::new(FrameKind::FileGet, get.encode().unwrap()),
    )
    .expect("download request should be sent");
    let mut received = Vec::new();
    loop {
        let frame = read_frame(&mut download).expect("download frame should arrive");
        match frame.kind {
            FrameKind::FileData => received.extend(frame.payload),
            FrameKind::FileDone => {
                assert_eq!(
                    decode_file_length(&frame.payload).unwrap(),
                    received.len() as u64
                );
                break;
            }
            other => panic!("unexpected download frame {other:?}"),
        }
    }
    drop(download);
    download_server
        .join()
        .expect("download fixture should finish")
        .expect("download should succeed");
    assert_eq!(received, contents);

    fs::remove_dir_all(root).expect("fixture directory should be removable");
}

#[cfg(unix)]
#[test]
fn independent_authenticated_sessions_run_concurrently() {
    fn connect(address: SocketAddr, token: &str) -> TcpStream {
        let mut stream = TcpStream::connect(address).expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout should apply");
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token: token.to_owned(),
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
        )
        .expect("hello should be sent");
        assert_eq!(
            read_frame(&mut stream)
                .expect("hello response should arrive")
                .kind,
            FrameKind::HelloOk
        );
        stream
    }

    fn start(stream: &mut TcpStream, script: &str) {
        let request = StartRequest {
            kind: SessionKind::Exec,
            argv: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            working_directory: None,
        };
        write_frame(
            stream,
            &Frame::new(FrameKind::Start, request.encode().unwrap()),
        )
        .expect("start should be sent");
    }

    fn collect(stream: &mut TcpStream) -> (Vec<u8>, i32) {
        let mut stdout = Vec::new();
        loop {
            let frame = read_frame(stream).expect("process frame should arrive");
            match frame.kind {
                FrameKind::Stdout => stdout.extend(frame.payload),
                FrameKind::Stderr => {}
                FrameKind::Exit => return (stdout, lsw_core::decode_exit(&frame.payload).unwrap()),
                other => panic!("unexpected process frame {other:?}"),
            }
        }
    }

    let token = "c".repeat(64);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let expected_token = token.clone();
    let server = thread::spawn(move || {
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().expect("fixture should connect");
            let token = expected_token.clone();
            sessions.push(thread::spawn(move || {
                handle_connection(stream, &token).map_err(|error| error.to_string())
            }));
        }
        for session in sessions {
            session
                .join()
                .expect("session should not panic")
                .expect("session should succeed");
        }
    });

    let mut blocked = connect(address, &token);
    start(
        &mut blocked,
        "IFS= read -r value; printf 'first-%s' \"$value\"",
    );

    let mut independent = connect(address, &token);
    start(&mut independent, "printf second");
    assert_eq!(collect(&mut independent), (b"second".to_vec(), 0));

    write_frame(
        &mut blocked,
        &Frame::new(FrameKind::Stdin, b"ready\n".to_vec()),
    )
    .expect("blocked session input should be sent");
    assert_eq!(collect(&mut blocked), (b"first-ready".to_vec(), 0));
    drop(independent);
    drop(blocked);
    server.join().expect("server fixture should finish");
}
