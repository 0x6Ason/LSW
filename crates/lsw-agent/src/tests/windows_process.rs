// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[cfg(windows)]
#[test]
fn windows_pipe_job_terminates_a_spawned_descendant() {
    let mut child = spawn_program(
        "cmd.exe",
        &["/D", "/Q", "/C", "ping.exe -n 30 127.0.0.1 >NUL"],
        None,
        &ProcessEnvironment::default(),
        false,
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

#[cfg(windows)]
#[test]
fn windows_conpty_retries_only_transient_console_driver_startup() {
    assert!(windows_conpty::should_retry_pseudo_console_create(
        0x8007_0003_u32 as i32,
        1
    ));
    assert!(windows_conpty::should_retry_pseudo_console_create(
        0x8007_0003_u32 as i32,
        19
    ));
    assert!(!windows_conpty::should_retry_pseudo_console_create(
        0x8007_0003_u32 as i32,
        20
    ));
    assert!(!windows_conpty::should_retry_pseudo_console_create(
        0x8007_0005_u32 as i32,
        1
    ));
    assert!(!windows_conpty::should_retry_pseudo_console_create(
        0x8007_0057_u32 as i32,
        1
    ));
}

#[cfg(windows)]
#[test]
fn windows_conpty_process_round_trips_input_and_output() {
    let request = StartRequest {
        kind: SessionKind::Shell,
        argv: vec!["cmd.exe".to_owned()],
        working_directory: None,
    };
    let mut process = windows_conpty::spawn_shell(
        &request,
        TerminalSize {
            columns: 80,
            rows: 25,
        },
    )
    .expect("ConPTY shell should start");
    process
        .input
        .write_all(b"echo LSW_CONPTY_DIRECT_OK & exit\r")
        .and_then(|()| process.input.flush())
        .expect("ConPTY input should be writable");
    let deadline = Instant::now() + Duration::from_secs(10);
    let exit_code = loop {
        if let Some(code) = windows_conpty::wait_for_process_timeout(&process.process, 100)
            .expect("ConPTY process wait should succeed")
        {
            break code;
        }
        assert!(
            Instant::now() < deadline,
            "ConPTY command did not exit after receiving input"
        );
    };
    process
        .job
        .terminate(SESSION_CANCEL_EXIT_CODE)
        .expect("ConPTY descendants should terminate");
    drop(process.console);
    let mut output = Vec::new();
    process
        .output
        .read_to_end(&mut output)
        .expect("ConPTY output should be readable to EOF");
    assert_eq!(exit_code, 0);
    assert!(String::from_utf8_lossy(&output).contains("LSW_CONPTY_DIRECT_OK"));
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
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_USER_ACCOUNT_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_USER_ACCOUNT_ROLE_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_POWER_HIBERNATE_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_MAINTENANCE_TRIM_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_MAINTENANCE_SHUTDOWN_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_WINDOWS_SUDO_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_GUI_LAUNCH_V1));
    assert!(capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_SESSION_CONTROL_V1));
    assert!(capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_SESSION_LEASE_V1));
}

#[cfg(windows)]
#[test]
fn windows_agent_advertises_native_os_operations() {
    let capabilities = agent_capabilities();
    for expected in [
        lsw_core::CAPABILITY_CONPTY_V1,
        lsw_core::CAPABILITY_TERMINAL_RESIZE_V1,
        lsw_core::CAPABILITY_POWER_HIBERNATE_V1,
        lsw_core::CAPABILITY_USER_ACCOUNT_V1,
        lsw_core::CAPABILITY_USER_ACCOUNT_ROLE_V1,
        lsw_core::CAPABILITY_MAINTENANCE_TRIM_V1,
        lsw_core::CAPABILITY_MAINTENANCE_SHUTDOWN_V1,
        lsw_core::CAPABILITY_WINDOWS_SUDO_V1,
        lsw_core::CAPABILITY_GUI_LAUNCH_V1,
        lsw_core::CAPABILITY_GUI_ICON_V1,
        lsw_core::CAPABILITY_DESKTOP_LIVE_SHARE_V1,
    ] {
        assert!(capabilities.iter().any(|capability| capability == expected));
    }
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_MAINTENANCE_HIBERNATE_V1));
}

#[test]
fn native_shutdown_arguments_do_not_force_open_applications() {
    assert_eq!(WINDOWS_SHUTDOWN_ARGUMENTS, ["/s", "/t", "0", "/d", "p:0:0"]);
    assert!(!WINDOWS_SHUTDOWN_ARGUMENTS
        .iter()
        .any(|argument| argument.eq_ignore_ascii_case("/f")));
}

pub(super) fn controlled_test_connection(
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

#[test]
fn network_agent_rejects_the_helper_only_hibernate_frame() {
    let (mut stream, done, active_sessions) = controlled_test_connection("9".repeat(64));
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::MaintenanceHibernate, Vec::new()),
    )
    .expect("helper-only frame should be sent");
    let response = read_frame(&mut stream).expect("protocol rejection should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("unsupported"));
    drop(stream);
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish")
        .expect_err("helper-only frame should fail")
        .contains("unsupported"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(windows)]
#[test]
fn windows_conpty_shell_sends_exit_after_normal_completion() {
    let token = "d".repeat(64);
    let (mut stream, done, active_sessions) = controlled_test_connection(token);
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("terminal read timeout should apply");
    write_frame(
        &mut stream,
        &Frame::new(
            FrameKind::SessionOptions,
            SessionOptions {
                cancel_on_disconnect: true,
            }
            .encode(),
        ),
    )
    .expect("session options should be sent");
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionLease, SessionLease::standard().encode()),
    )
    .expect("session lease should be sent");
    let request = TerminalStartRequest {
        size: TerminalSize::new(25, 80).expect("terminal size should be valid"),
        request: StartRequest {
            kind: SessionKind::Shell,
            argv: vec!["cmd.exe".to_owned()],
            working_directory: None,
        },
    };
    write_frame(
        &mut stream,
        &Frame::new(
            FrameKind::TerminalStart,
            request.encode().expect("terminal request should encode"),
        ),
    )
    .expect("terminal start should be sent");
    if let Err(error) = write_frame(
        &mut stream,
        &Frame::new(
            FrameKind::Stdin,
            b"echo LSW_CONPTY_EXIT_OK & exit\r".to_vec(),
        ),
    ) {
        panic!(
            "terminal input should be sent: {error}; server={:?}",
            done.recv_timeout(Duration::from_secs(2))
        );
    }

    let mut output = Vec::new();
    let exit_code = loop {
        let frame = read_frame(&mut stream).unwrap_or_else(|error| {
            panic!(
                "terminal completion frame should arrive: {error}; output={:?}; server={:?}",
                String::from_utf8_lossy(&output),
                done.try_recv()
            )
        });
        match frame.kind {
            FrameKind::Stdout => output.extend(frame.payload),
            FrameKind::Exit => {
                break lsw_core::decode_exit(&frame.payload).expect("exit should decode")
            }
            FrameKind::Error => panic!(
                "terminal returned an error: {}",
                String::from_utf8_lossy(&frame.payload)
            ),
            other => panic!("unexpected terminal frame {other:?}"),
        }
    };
    assert_eq!(exit_code, 0);
    assert!(String::from_utf8_lossy(&output).contains("LSW_CONPTY_EXIT_OK"));
    drop(stream);
    done.recv_timeout(Duration::from_secs(5))
        .expect("server session should finish")
        .expect("server session should succeed");
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}
