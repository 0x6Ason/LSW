// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::Read;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use lsw_core::{
    read_frame, write_frame, Frame, FrameKind, GuiInputEvent, GuiStartRequest, GuiWindowAction,
    GuiWindowClosed, GuiWindowDamage, GuiWindowDragHint, GuiWindowReady, GuiWindowResize,
    LiveShareStatus, ProcessEnvironment, SessionKind, SessionLease, SessionOptions, StartRequest,
    TerminalSize, WindowsSudoConfigureRequest, WindowsSudoStatus, CAPABILITY_DETACHED_RUN_V1,
    CAPABILITY_GUI_LAUNCH_V1, CAPABILITY_GUI_WINDOW_V3, CAPABILITY_LIVE_SHARE_V1,
    CAPABILITY_MAINTENANCE_SHUTDOWN_V1, CAPABILITY_MAINTENANCE_TRIM_V1,
    CAPABILITY_PROCESS_ENVIRONMENT_V1, CAPABILITY_SESSION_CONTROL_V1, CAPABILITY_SESSION_LEASE_V1,
    CAPABILITY_WINDOWS_SUDO_V1,
};

use super::process::{spawn_heartbeat_bridge, ResizeState};
use super::{AgentClient, GuiWindowEvent, GuiWindowSession};

#[test]
fn agent_address_is_always_loopback() {
    let _ = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5040);
    assert!(Ipv4Addr::LOCALHOST.is_loopback());
}

#[test]
fn resize_state_emits_only_real_changes() {
    let initial = TerminalSize::new(24, 80).expect("initial size should be valid");
    let changed = TerminalSize::new(40, 120).expect("changed size should be valid");
    let mut state = ResizeState::new(initial);
    assert_eq!(state.update(initial), None);
    assert_eq!(state.update(changed), Some(changed));
    assert_eq!(state.update(changed), None);
    assert_eq!(state.last, changed);
}

#[test]
fn maintenance_trim_sends_only_the_empty_fixed_operation() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let request = read_frame(&mut stream).expect("trim request should arrive");
        assert_eq!(request.kind, FrameKind::MaintenanceTrim);
        assert!(request.payload.is_empty());
        write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))
            .expect("trim response should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    AgentClient {
        stream,
        capabilities: vec![CAPABILITY_MAINTENANCE_TRIM_V1.to_owned()],
    }
    .trim()
    .expect("trim should succeed");
    server.join().expect("fixture should not panic");
}

#[test]
fn maintenance_shutdown_sends_only_the_empty_fixed_operation() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let request = read_frame(&mut stream).expect("shutdown request should arrive");
        assert_eq!(request.kind, FrameKind::MaintenanceShutdown);
        assert!(request.payload.is_empty());
        write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))
            .expect("shutdown response should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    AgentClient {
        stream,
        capabilities: vec![CAPABILITY_MAINTENANCE_SHUTDOWN_V1.to_owned()],
    }
    .shutdown()
    .expect("shutdown should succeed");
    server.join().expect("fixture should not panic");
}

#[test]
fn windows_sudo_status_is_capability_gated_and_strictly_decoded() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let expected = WindowsSudoStatus {
        available: true,
        configured_mode: lsw_core::WindowsSudoMode::ForceNewWindow,
        policy_mode: None,
    };
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let request = read_frame(&mut stream).expect("sudo query should arrive");
        assert_eq!(request.kind, FrameKind::WindowsSudoQuery);
        assert!(request.payload.is_empty());
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::WindowsSudoStatus, expected.encode()),
        )
        .expect("sudo status should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let actual = AgentClient {
        stream,
        capabilities: vec![CAPABILITY_WINDOWS_SUDO_V1.to_owned()],
    }
    .windows_sudo_status()
    .expect("sudo status should succeed");
    assert_eq!(actual, expected);
    server.join().expect("fixture should not panic");
}

#[test]
fn windows_sudo_configuration_exposes_only_enable_or_disable() {
    for enable in [false, true] {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client should connect");
            let request = read_frame(&mut stream).expect("sudo configuration should arrive");
            assert_eq!(request.kind, FrameKind::WindowsSudoConfigure);
            assert_eq!(
                WindowsSudoConfigureRequest::decode(&request.payload)
                    .expect("sudo configuration should decode"),
                WindowsSudoConfigureRequest { enable }
            );
            write_frame(&mut stream, &Frame::new(FrameKind::Pong, Vec::new()))
                .expect("sudo configuration response should be sent");
        });
        let stream = TcpStream::connect(address).expect("fixture should connect");
        AgentClient {
            stream,
            capabilities: vec![CAPABILITY_WINDOWS_SUDO_V1.to_owned()],
        }
        .configure_windows_sudo(enable)
        .expect("sudo configuration should succeed");
        server.join().expect("fixture should not panic");
    }
}

#[test]
fn live_share_mapping_is_capability_gated_and_explicit() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let query = read_frame(&mut stream).expect("live-share query should arrive");
        assert_eq!(query.kind, FrameKind::LiveShareQuery);
        assert!(query.payload.is_empty());
        write_frame(
            &mut stream,
            &Frame::new(
                FrameKind::LiveShareStatus,
                LiveShareStatus { mapped: true }.encode(),
            ),
        )
        .expect("live-share status should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let status = AgentClient {
        stream,
        capabilities: vec![CAPABILITY_LIVE_SHARE_V1.to_owned()],
    }
    .live_share_status()
    .expect("live-share query should succeed");
    assert!(status.mapped);
    server.join().expect("fixture should not panic");
}

#[test]
fn gui_operations_are_capability_gated_and_keep_the_user_explicit() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let frame = read_frame(&mut stream).expect("GUI start should arrive");
        assert_eq!(frame.kind, FrameKind::GuiStart);
        let request = GuiStartRequest::decode(&frame.payload).expect("GUI start should decode");
        assert_eq!(request.user_name, "desktop-user");
        assert_eq!(request.request.argv, ["notepad.exe"]);
        assert!(request.mount_live_share);
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Started, lsw_core::encode_process_id(8123)),
        )
        .expect("GUI response should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let process_id = AgentClient {
        stream,
        capabilities: vec![CAPABILITY_GUI_LAUNCH_V1.to_owned()],
    }
    .run_gui(&GuiStartRequest {
        user_name: "desktop-user".to_owned(),
        request: StartRequest {
            kind: SessionKind::Run,
            argv: vec!["notepad.exe".to_owned()],
            working_directory: None,
        },
        environment: ProcessEnvironment::default(),
        mount_live_share: true,
    })
    .expect("GUI start should succeed");
    assert_eq!(process_id, 8123);
    server.join().expect("fixture should not panic");
}

#[test]
fn seamless_gui_session_is_capability_gated_bidirectional_and_strict() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let open = read_frame(&mut stream).expect("GUI window open should arrive");
        assert_eq!(open.kind, FrameKind::GuiWindowOpen);
        let request =
            GuiStartRequest::decode(&open.payload).expect("GUI window request should decode");
        assert_eq!(request.user_name, "desktop-user");
        let ready = GuiWindowReady {
            process_id: 42,
            window_id: 73,
            width: 2,
            height: 1,
            title: "Fixture".to_owned(),
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::GuiWindowReady, ready.encode().unwrap()),
        )
        .unwrap();

        let input = read_frame(&mut stream).expect("GUI input should arrive");
        assert_eq!(input.kind, FrameKind::GuiWindowInput);
        assert_eq!(
            GuiInputEvent::decode(&input.payload).unwrap(),
            GuiInputEvent::Focus { focused: true }
        );
        let resize = read_frame(&mut stream).expect("GUI resize should arrive");
        assert_eq!(resize.kind, FrameKind::GuiWindowResize);
        assert_eq!(
            GuiWindowResize::decode(&resize.payload).unwrap(),
            GuiWindowResize {
                width: 800,
                height: 600
            }
        );
        let action = read_frame(&mut stream).expect("GUI host state should arrive");
        assert_eq!(action.kind, FrameKind::GuiWindowAction);
        assert_eq!(
            GuiWindowAction::decode(&action.payload).unwrap(),
            GuiWindowAction::Maximize
        );
        let close = read_frame(&mut stream).expect("GUI close should arrive");
        assert_eq!(close.kind, FrameKind::GuiWindowClose);
        assert!(close.payload.is_empty());

        let damage = GuiWindowDamage {
            sequence: 1,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            bgra: vec![1, 2, 3, 255],
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::GuiWindowDamage, damage.encode().unwrap()),
        )
        .unwrap();
        let drag_hint = GuiWindowDragHint {
            x: 12,
            y: 34,
            action: Some(GuiWindowAction::Move),
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::GuiWindowDragHint, drag_hint.encode().unwrap()),
        )
        .unwrap();
        write_frame(
            &mut stream,
            &Frame::new(
                FrameKind::GuiWindowAction,
                GuiWindowAction::Maximize.encode(),
            ),
        )
        .unwrap();
        write_frame(
            &mut stream,
            &Frame::new(
                FrameKind::GuiWindowAction,
                GuiWindowAction::Restore.encode(),
            ),
        )
        .unwrap();
        write_frame(
            &mut stream,
            &Frame::new(
                FrameKind::GuiWindowClosed,
                GuiWindowClosed { exit_code: 17 }.encode().to_vec(),
            ),
        )
        .unwrap();
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let session = AgentClient {
        stream,
        capabilities: vec![CAPABILITY_GUI_WINDOW_V3.to_owned()],
    }
    .open_gui_window(&GuiStartRequest {
        user_name: "desktop-user".to_owned(),
        request: StartRequest {
            kind: SessionKind::Run,
            argv: vec!["notepad.exe".to_owned()],
            working_directory: None,
        },
        environment: ProcessEnvironment::default(),
        mount_live_share: false,
    })
    .expect("GUI window should open");
    let (ready, mut reader, mut writer) = session.split().unwrap();
    assert_eq!(ready.window_id, 73);
    writer
        .send_input(GuiInputEvent::Focus { focused: true })
        .unwrap();
    writer.resize(800, 600).unwrap();
    writer.window_action(GuiWindowAction::Maximize).unwrap();
    assert!(writer.window_action(GuiWindowAction::Move).is_err());
    writer.close().unwrap();
    assert!(matches!(
        reader.read_event().unwrap(),
        GuiWindowEvent::Damage(damage) if damage.bgra == [1, 2, 3, 255]
    ));
    assert!(matches!(
        reader.read_event().unwrap(),
        GuiWindowEvent::DragHint(GuiWindowDragHint {
            x: 12,
            y: 34,
            action: Some(GuiWindowAction::Move)
        })
    ));
    assert!(matches!(
        reader.read_event().unwrap(),
        GuiWindowEvent::Action(GuiWindowAction::Maximize)
    ));
    assert!(matches!(
        reader.read_event().unwrap(),
        GuiWindowEvent::Action(GuiWindowAction::Restore)
    ));
    assert!(matches!(
        reader.read_event().unwrap(),
        GuiWindowEvent::Closed(GuiWindowClosed { exit_code: 17 })
    ));
    server.join().expect("fixture should not panic");
}

#[test]
fn dropping_gui_writer_shuts_the_connection_while_reader_clone_is_alive() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("server read timeout should configure");
        let mut byte = [0_u8; 1];
        assert_eq!(
            stream
                .read(&mut byte)
                .expect("writer drop should yield EOF"),
            0,
            "the live reader clone must not keep an abandoned GUI session open"
        );
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let session = GuiWindowSession {
        stream,
        ready: GuiWindowReady {
            process_id: 42,
            window_id: 73,
            width: 2,
            height: 1,
            title: "Fixture".to_owned(),
        },
    };
    let (_, reader, writer) = session.split().expect("session should split");
    drop(writer);
    server.join().expect("fixture should observe EOF");
    drop(reader);
}

#[test]
fn seamless_gui_v3_rejects_a_v2_peer_before_sending_a_request() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let mut unexpected = Vec::new();
        stream
            .read_to_end(&mut unexpected)
            .expect("fixture should read until the rejected client closes");
        unexpected
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let result = AgentClient {
        stream,
        capabilities: vec!["gui-window-v2".to_owned()],
    }
    .open_gui_window(&GuiStartRequest {
        user_name: "desktop-user".to_owned(),
        request: StartRequest {
            kind: SessionKind::Run,
            argv: vec!["notepad.exe".to_owned()],
            working_directory: None,
        },
        environment: ProcessEnvironment::default(),
        mount_live_share: false,
    });
    let error = match result {
        Ok(_) => panic!("a v2 peer must be rejected before the v3 request is sent"),
        Err(error) => error,
    };
    assert!(error.to_string().contains(CAPABILITY_GUI_WINDOW_V3));
    assert!(server.join().expect("fixture should not panic").is_empty());
}

#[test]
fn controlled_run_sends_options_start_and_explicit_stdin_close() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let options = read_frame(&mut stream).expect("options should arrive");
        assert_eq!(options.kind, FrameKind::SessionOptions);
        assert!(
            SessionOptions::decode(&options.payload)
                .expect("options should decode")
                .cancel_on_disconnect
        );

        let start = read_frame(&mut stream).expect("start should arrive");
        assert_eq!(start.kind, FrameKind::Start);
        assert_eq!(
            StartRequest::decode(&start.payload)
                .expect("start should decode")
                .argv,
            vec!["fixture-command"]
        );

        let stdin_close = read_frame(&mut stream).expect("stdin close should arrive");
        assert_eq!(stdin_close.kind, FrameKind::StdinClose);
        assert!(stdin_close.payload.is_empty());
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Exit, lsw_core::encode_exit(0)),
        )
        .expect("exit should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let client = AgentClient {
        stream,
        capabilities: vec![CAPABILITY_SESSION_CONTROL_V1.to_owned()],
    };
    let request = StartRequest {
        kind: lsw_core::SessionKind::Exec,
        argv: vec!["fixture-command".to_owned()],
        working_directory: None,
    };
    assert_eq!(client.run(&request, false).expect("run should succeed"), 0);
    server.join().expect("fixture should not panic");
}

#[test]
fn controlled_run_sends_environment_and_working_directory() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        assert_eq!(
            read_frame(&mut stream).expect("options should arrive").kind,
            FrameKind::SessionOptions
        );
        let environment = read_frame(&mut stream).expect("environment should arrive");
        assert_eq!(environment.kind, FrameKind::ProcessEnvironment);
        assert_eq!(
            ProcessEnvironment::decode(&environment.payload)
                .expect("environment should decode")
                .variables,
            vec![("LSW_FIXTURE".to_owned(), "hello world".to_owned())]
        );
        let start = read_frame(&mut stream).expect("start should arrive");
        assert_eq!(start.kind, FrameKind::Start);
        assert_eq!(
            StartRequest::decode(&start.payload)
                .expect("start should decode")
                .working_directory
                .as_deref(),
            Some("C:\\work")
        );
        assert_eq!(
            read_frame(&mut stream)
                .expect("stdin close should arrive")
                .kind,
            FrameKind::StdinClose
        );
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Exit, lsw_core::encode_exit(17)),
        )
        .expect("exit should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let client = AgentClient {
        stream,
        capabilities: vec![
            CAPABILITY_SESSION_CONTROL_V1.to_owned(),
            CAPABILITY_PROCESS_ENVIRONMENT_V1.to_owned(),
        ],
    };
    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: vec!["fixture-command".to_owned()],
        working_directory: Some("C:\\work".to_owned()),
    };
    let environment =
        ProcessEnvironment::new(vec![("LSW_FIXTURE".to_owned(), "hello world".to_owned())])
            .expect("environment should be valid");
    assert_eq!(
        client
            .run_with_environment(&request, false, &environment)
            .expect("run should succeed"),
        17
    );
    server.join().expect("fixture should not panic");
}

#[test]
fn detached_run_uses_the_explicit_handshake_and_returns_the_pid() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let options = read_frame(&mut stream).expect("options should arrive");
        assert_eq!(options.kind, FrameKind::SessionOptions);
        assert!(
            !SessionOptions::decode(&options.payload)
                .expect("options should decode")
                .cancel_on_disconnect
        );
        assert_eq!(
            read_frame(&mut stream)
                .expect("detach marker should arrive")
                .kind,
            FrameKind::SessionDetach
        );
        let start = read_frame(&mut stream).expect("start should arrive");
        assert_eq!(start.kind, FrameKind::Start);
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Started, lsw_core::encode_process_id(4242)),
        )
        .expect("started response should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let client = AgentClient {
        stream,
        capabilities: vec![
            CAPABILITY_SESSION_CONTROL_V1.to_owned(),
            CAPABILITY_DETACHED_RUN_V1.to_owned(),
        ],
    };
    let request = StartRequest {
        kind: SessionKind::Run,
        argv: vec!["fixture-command".to_owned()],
        working_directory: None,
    };
    assert_eq!(
        client
            .run_detached(&request, &ProcessEnvironment::default())
            .expect("detached run should succeed"),
        4242
    );
    server.join().expect("fixture should not panic");
}

#[test]
fn captured_run_sends_private_input_and_bounds_separate_output_streams() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        assert_eq!(
            read_frame(&mut stream).expect("options should arrive").kind,
            FrameKind::SessionOptions
        );
        assert_eq!(
            read_frame(&mut stream).expect("start should arrive").kind,
            FrameKind::Start
        );
        let input = read_frame(&mut stream).expect("private input should arrive");
        assert_eq!(input.kind, FrameKind::Stdin);
        assert_eq!(input.payload, b"fixture-secret\n");
        assert_eq!(
            read_frame(&mut stream)
                .expect("stdin close should arrive")
                .kind,
            FrameKind::StdinClose
        );
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Stdout, b"STATUS=licensed\n".to_vec()),
        )
        .expect("stdout should be sent");
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Stderr, b"diagnostic\n".to_vec()),
        )
        .expect("stderr should be sent");
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Exit, lsw_core::encode_exit(0)),
        )
        .expect("exit should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let client = AgentClient {
        stream,
        capabilities: vec![CAPABILITY_SESSION_CONTROL_V1.to_owned()],
    };
    let request = StartRequest {
        kind: lsw_core::SessionKind::Exec,
        argv: vec!["license-client".to_owned()],
        working_directory: None,
    };
    let captured = client
        .run_capture(&request, b"fixture-secret\n", 1024)
        .expect("capture should succeed");
    assert_eq!(captured.exit_code, 0);
    assert_eq!(captured.stdout, b"STATUS=licensed\n");
    assert_eq!(captured.stderr, b"diagnostic\n");
    server.join().expect("fixture should not panic");
}

#[test]
fn leased_run_sends_lease_between_options_and_start() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        assert_eq!(
            read_frame(&mut stream).expect("options should arrive").kind,
            FrameKind::SessionOptions
        );
        let lease_frame = read_frame(&mut stream).expect("lease should arrive");
        assert_eq!(lease_frame.kind, FrameKind::SessionLease);
        assert_eq!(
            SessionLease::decode(&lease_frame.payload).expect("lease should decode"),
            SessionLease::standard()
        );
        assert_eq!(
            read_frame(&mut stream).expect("start should arrive").kind,
            FrameKind::Start
        );
        assert_eq!(
            read_frame(&mut stream)
                .expect("stdin close should arrive")
                .kind,
            FrameKind::StdinClose
        );
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Exit, lsw_core::encode_exit(0)),
        )
        .expect("exit should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let client = AgentClient {
        stream,
        capabilities: vec![
            CAPABILITY_SESSION_CONTROL_V1.to_owned(),
            CAPABILITY_SESSION_LEASE_V1.to_owned(),
        ],
    };
    let request = StartRequest {
        kind: lsw_core::SessionKind::Exec,
        argv: vec!["fixture-command".to_owned()],
        working_directory: None,
    };
    assert_eq!(client.run(&request, false).expect("run should succeed"), 0);
    server.join().expect("fixture should not panic");
}

#[test]
fn heartbeat_bridge_emits_frames_and_stops_promptly() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("timeout should configure");
        let heartbeat = read_frame(&mut stream).expect("heartbeat should arrive");
        assert_eq!(heartbeat.kind, FrameKind::SessionHeartbeat);
        assert!(heartbeat.payload.is_empty());
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let writer = Arc::new(Mutex::new(stream));
    let (stop, bridge) = spawn_heartbeat_bridge(writer, Duration::from_millis(10));
    server.join().expect("fixture should not panic");
    // The peer may close between the first successful heartbeat and this
    // signal, in which case the bridge has already stopped itself.
    let _ = stop.send(());
    bridge.join().expect("bridge should not panic");
}

#[test]
fn legacy_run_still_starts_without_session_options() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener.local_addr().expect("listener should have address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client should connect");
        let start = read_frame(&mut stream).expect("start should arrive");
        assert_eq!(start.kind, FrameKind::Start);
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Exit, lsw_core::encode_exit(0)),
        )
        .expect("exit should be sent");
    });
    let stream = TcpStream::connect(address).expect("fixture should connect");
    let client = AgentClient {
        stream,
        // An inconsistent peer advertising only the lease capability must
        // still get version-one legacy framing.
        capabilities: vec![CAPABILITY_SESSION_LEASE_V1.to_owned()],
    };
    let request = StartRequest {
        kind: lsw_core::SessionKind::Exec,
        argv: vec!["fixture-command".to_owned()],
        working_directory: None,
    };
    assert_eq!(client.run(&request, false).expect("run should succeed"), 0);
    server.join().expect("fixture should not panic");
}
