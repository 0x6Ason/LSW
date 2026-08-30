// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use super::*;

#[test]
fn frame_round_trip_is_binary_safe() {
    let frame = Frame::new(FrameKind::Stdout, [0, 1, 2, 255]);
    let mut encoded = Vec::new();
    write_frame(&mut encoded, &frame).expect("frame should encode");
    assert_eq!(
        read_frame(&mut encoded.as_slice()).expect("frame should decode"),
        frame
    );
}

#[test]
fn start_request_round_trip_preserves_unicode() {
    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: vec!["pwsh".to_owned(), "résumé.ps1".to_owned()],
        working_directory: Some("C:\\données".to_owned()),
    };
    assert_eq!(
        StartRequest::decode(&request.encode().expect("request should encode"))
            .expect("request should decode"),
        request
    );
}

#[test]
fn process_environment_is_bounded_unambiguous_and_case_insensitive() {
    let environment = ProcessEnvironment::new(vec![
        ("LSW_MODE".to_owned(), "development".to_owned()),
        ("UNICODE".to_owned(), "données".to_owned()),
    ])
    .expect("environment should validate");
    assert_eq!(
        ProcessEnvironment::decode(&environment.encode().expect("environment should encode"))
            .expect("environment should decode"),
        environment
    );
    assert!(ProcessEnvironment::new(vec![("".to_owned(), "x".to_owned())]).is_err());
    assert!(ProcessEnvironment::new(vec![
        ("Path".to_owned(), "one".to_owned()),
        ("PATH".to_owned(), "two".to_owned()),
    ])
    .is_err());
    assert!(ProcessEnvironment::decode(&[0, 1, 0, 0, 0, 1, b'X']).is_err());
}

#[test]
fn signal_and_process_id_payloads_are_exact() {
    for signal in [SessionSignal::Interrupt, SessionSignal::Terminate] {
        assert_eq!(
            SessionSignal::decode(&signal.encode()).expect("signal should decode"),
            signal
        );
    }
    assert_eq!(SessionSignal::Interrupt.exit_code(), 130);
    assert_eq!(SessionSignal::Terminate.exit_code(), 143);
    assert!(SessionSignal::decode(&[3]).is_err());
    assert_eq!(
        decode_process_id(&encode_process_id(u32::MAX)).expect("PID should decode"),
        u32::MAX
    );
}

#[test]
fn malformed_lengths_and_tokens_are_rejected() {
    assert!(read_frame(&mut [FrameKind::Stdout as u8, 255, 255, 255, 255].as_slice()).is_err());
    assert!(constant_time_token_eq("same", "same"));
    assert!(!constant_time_token_eq("same", "different"));
}

#[test]
fn file_requests_are_length_delimited() {
    let put = FilePutRequest {
        destination: "C:\\src\\main.rs".to_owned(),
        length: u64::MAX - 1,
    };
    assert_eq!(
        FilePutRequest::decode(&put.encode().expect("put should encode"))
            .expect("put should decode"),
        put
    );
    let get = FileGetRequest {
        source: "C:\\out\\app.exe".to_owned(),
    };
    assert_eq!(
        FileGetRequest::decode(&get.encode().expect("get should encode"))
            .expect("get should decode"),
        get
    );
}

#[test]
fn user_creation_is_bounded_and_redacts_debug_output() {
    let request = UserCreateRequest {
        user_name: "desktop-user".to_owned(),
        password: "S3cure password!".as_bytes().to_vec(),
        administrator: false,
    };
    let encoded = request.encode().expect("user request should encode");
    let decoded = UserCreateRequest::decode(&encoded).expect("user request should decode");
    assert_eq!(decoded.user_name, "desktop-user");
    assert_eq!(decoded.password, b"S3cure password!");
    assert!(!decoded.administrator);
    assert!(!format!("{request:?}").contains("S3cure"));
    assert_eq!(
        FrameKind::try_from(31).expect("user-create kind should decode"),
        FrameKind::UserCreate
    );

    let mut invalid_flag = encoded;
    *invalid_flag.last_mut().expect("encoded flag exists") = 2;
    assert!(UserCreateRequest::decode(&invalid_flag).is_err());
    assert!(UserCreateRequest {
        user_name: "desktop-user".to_owned(),
        password: Vec::new(),
        administrator: false,
    }
    .encode()
    .is_err());
}

#[test]
fn user_role_changes_are_explicit_and_append_only() {
    let request = UserSetRoleRequest {
        user_name: "desktop-user".to_owned(),
        role: WindowsUserRole::Administrator,
    };
    let encoded = request.encode().expect("role request should encode");
    assert_eq!(
        UserSetRoleRequest::decode(&encoded).expect("role request should decode"),
        request
    );
    assert_eq!(
        FrameKind::try_from(34).expect("user-set-role kind should decode"),
        FrameKind::UserSetRole
    );
    assert_eq!(FrameKind::UserSetRole as u8, 34);

    let mut invalid_role = encoded;
    *invalid_role.last_mut().expect("encoded role exists") = 2;
    assert!(UserSetRoleRequest::decode(&invalid_role).is_err());
}

#[test]
fn maintenance_trim_has_an_append_only_empty_request_kind() {
    assert_eq!(
        FrameKind::try_from(32).expect("maintenance-trim kind should decode"),
        FrameKind::MaintenanceTrim
    );
    assert_eq!(FrameKind::MaintenanceTrim as u8, 32);
}

#[test]
fn maintenance_hibernate_has_an_append_only_empty_request_kind() {
    assert_eq!(
        FrameKind::try_from(33).expect("maintenance-hibernate kind should decode"),
        FrameKind::MaintenanceHibernate
    );
    assert_eq!(FrameKind::MaintenanceHibernate as u8, 33);
}

#[test]
fn maintenance_shutdown_has_an_append_only_empty_request_kind() {
    assert_eq!(
        FrameKind::try_from(41).expect("shutdown request kind should decode"),
        FrameKind::MaintenanceShutdown
    );
    assert_eq!(FrameKind::MaintenanceShutdown as u8, 41);
}

#[test]
fn desktop_protocol_is_append_only_bounded_and_round_trips() {
    assert_eq!(FrameKind::DesktopCompanionStart as u8, 42);
    assert_eq!(FrameKind::GuiStart as u8, 43);
    assert_eq!(FrameKind::GuiIcon as u8, 44);
    assert_eq!(FrameKind::GuiIconData as u8, 45);
    assert_eq!(FrameKind::DesktopLiveShareConfigure as u8, 46);
    for value in 42..=46 {
        assert!(FrameKind::try_from(value).is_ok());
    }

    let user = DesktopUserRequest {
        user_name: "desktop-user".to_owned(),
    };
    assert_eq!(
        DesktopUserRequest::decode(&user.encode().unwrap()).unwrap(),
        user
    );
    let live = DesktopLiveShareRequest {
        user_name: "desktop-user".to_owned(),
        enable: true,
    };
    assert_eq!(
        DesktopLiveShareRequest::decode(&live.encode().unwrap()).unwrap(),
        live
    );
    let gui = GuiStartRequest {
        user_name: "desktop-user".to_owned(),
        request: StartRequest {
            kind: SessionKind::Run,
            argv: vec!["notepad.exe".to_owned(), "L:\\note.txt".to_owned()],
            working_directory: Some("L:\\".to_owned()),
        },
        environment: ProcessEnvironment::new(vec![("MODE".to_owned(), "desktop".to_owned())])
            .unwrap(),
        mount_live_share: true,
    };
    assert_eq!(
        GuiStartRequest::decode(&gui.encode().unwrap()).unwrap(),
        gui
    );
    let icon = GuiIconRequest {
        user_name: "desktop-user".to_owned(),
        program: "notepad.exe".to_owned(),
    };
    assert_eq!(
        GuiIconRequest::decode(&icon.encode().unwrap()).unwrap(),
        icon
    );

    let mut invalid_live = live.encode().unwrap();
    *invalid_live.last_mut().unwrap() = 2;
    assert!(DesktopLiveShareRequest::decode(&invalid_live).is_err());
    let invalid_gui = GuiStartRequest {
        request: StartRequest {
            kind: SessionKind::Exec,
            argv: vec!["cmd.exe".to_owned()],
            working_directory: None,
        },
        ..gui.clone()
    };
    assert!(invalid_gui.encode().is_err());
    let reserved_gui = GuiStartRequest {
        environment: ProcessEnvironment::new(vec![(
            "lsw_desktop_token".to_owned(),
            "override".to_owned(),
        )])
        .unwrap(),
        ..gui
    };
    assert!(reserved_gui.encode().is_err());
    assert!(GuiIconRequest {
        user_name: "desktop-user".to_owned(),
        program: "notepad".to_owned(),
    }
    .encode()
    .is_err());
}

#[test]
fn seamless_window_v3_protocol_is_append_only_bounded_and_strict() {
    assert_eq!(CAPABILITY_GUI_WINDOW_V3, "gui-window-v3");
    assert_eq!(FrameKind::GuiWindowOpen as u8, 47);
    assert_eq!(FrameKind::GuiWindowReady as u8, 48);
    assert_eq!(FrameKind::GuiWindowDamage as u8, 49);
    assert_eq!(FrameKind::GuiWindowInput as u8, 50);
    assert_eq!(FrameKind::GuiWindowResize as u8, 51);
    assert_eq!(FrameKind::GuiWindowClose as u8, 52);
    assert_eq!(FrameKind::GuiWindowClosed as u8, 53);
    assert_eq!(FrameKind::GuiWindowAction as u8, 54);
    assert_eq!(FrameKind::GuiWindowDragHint as u8, 55);
    for value in 47..=55 {
        assert!(FrameKind::try_from(value).is_ok());
    }

    let ready = GuiWindowReady {
        process_id: 4242,
        window_id: 0x10203,
        width: 800,
        height: 600,
        title: "Notepad".to_owned(),
    };
    assert_eq!(
        GuiWindowReady::decode(&ready.encode().unwrap()).unwrap(),
        ready
    );
    assert!(GuiWindowReady {
        process_id: 0,
        ..ready.clone()
    }
    .encode()
    .is_err());

    let damage = GuiWindowDamage {
        sequence: 7,
        x: 128,
        y: 64,
        width: 2,
        height: 2,
        bgra: vec![0x10; 16],
    };
    assert_eq!(
        GuiWindowDamage::decode(&damage.encode().unwrap()).unwrap(),
        damage
    );
    let mut invalid_damage = damage.clone();
    invalid_damage.bgra.pop();
    assert!(invalid_damage.encode().is_err());
    assert!(GuiWindowDamage {
        width: MAX_GUI_DAMAGE_DIMENSION + 1,
        ..damage
    }
    .encode()
    .is_err());

    let input = [
        GuiInputEvent::Focus { focused: true },
        GuiInputEvent::Key {
            virtual_key: 0x41,
            scan_code: 0x1e,
            pressed: true,
            extended: false,
        },
        GuiInputEvent::PointerMove { x: 12, y: 34 },
        GuiInputEvent::PointerButton {
            button: GuiPointerButton::Left,
            pressed: true,
            x: 12,
            y: 34,
        },
        GuiInputEvent::PointerWheel {
            delta: 120,
            horizontal: false,
            x: 12,
            y: 34,
        },
    ];
    for event in input {
        assert_eq!(
            GuiInputEvent::decode(&event.encode().unwrap()).unwrap(),
            event
        );
    }
    assert!(GuiInputEvent::decode(&[1, 2]).is_err());
    assert!(GuiInputEvent::Key {
        virtual_key: 0,
        scan_code: 0,
        pressed: true,
        extended: false,
    }
    .encode()
    .is_err());
    assert!(GuiInputEvent::Key {
        virtual_key: 0x100,
        scan_code: 0,
        pressed: true,
        extended: false,
    }
    .encode()
    .is_err());

    for (action, encoded) in [
        (GuiWindowAction::Move, 1),
        (GuiWindowAction::Minimize, 2),
        (GuiWindowAction::Maximize, 3),
        (GuiWindowAction::Close, 4),
        (GuiWindowAction::ResizeTopLeft, 5),
        (GuiWindowAction::ResizeTop, 6),
        (GuiWindowAction::ResizeTopRight, 7),
        (GuiWindowAction::ResizeRight, 8),
        (GuiWindowAction::ResizeBottomRight, 9),
        (GuiWindowAction::ResizeBottom, 10),
        (GuiWindowAction::ResizeBottomLeft, 11),
        (GuiWindowAction::ResizeLeft, 12),
        (GuiWindowAction::Restore, 13),
    ] {
        assert_eq!(action.encode(), vec![encoded]);
        assert_eq!(GuiWindowAction::decode(&[encoded]).unwrap(), action);
    }
    assert!(GuiWindowAction::decode(&[]).is_err());
    assert!(GuiWindowAction::decode(&[14]).is_err());
    assert!(GuiWindowAction::decode(&[5, 0]).is_err());

    for action in [
        None,
        Some(GuiWindowAction::Move),
        Some(GuiWindowAction::ResizeBottomRight),
    ] {
        let hint = GuiWindowDragHint {
            x: 12,
            y: 34,
            action,
        };
        assert_eq!(
            GuiWindowDragHint::decode(&hint.encode().unwrap()).unwrap(),
            hint
        );
    }
    assert!(GuiWindowDragHint {
        x: MAX_GUI_WINDOW_DIMENSION,
        y: 0,
        action: None,
    }
    .encode()
    .is_err());
    assert!(GuiWindowDragHint {
        x: 0,
        y: 0,
        action: Some(GuiWindowAction::Close),
    }
    .encode()
    .is_err());
    assert!(GuiWindowDragHint::decode(&[0; 8]).is_err());
    assert!(GuiWindowDragHint::decode(&[4, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());

    let resize = GuiWindowResize {
        width: 1280,
        height: 720,
    };
    assert_eq!(
        GuiWindowResize::decode(&resize.encode().unwrap()).unwrap(),
        resize
    );
    assert!(GuiWindowResize {
        width: 0,
        height: 720,
    }
    .encode()
    .is_err());

    let closed = GuiWindowClosed { exit_code: 17 };
    assert_eq!(GuiWindowClosed::decode(&closed.encode()).unwrap(), closed);
    assert!(GuiWindowClosed::decode(&[0, 0, 0]).is_err());
}

#[test]
fn windows_sudo_protocol_is_append_only_and_strict() {
    assert_eq!(FrameKind::WindowsSudoQuery as u8, 35);
    assert_eq!(FrameKind::WindowsSudoConfigure as u8, 36);
    assert_eq!(FrameKind::WindowsSudoStatus as u8, 37);
    assert_eq!(
        FrameKind::try_from(37).expect("Windows sudo status kind should decode"),
        FrameKind::WindowsSudoStatus
    );

    for enable in [false, true] {
        let request = WindowsSudoConfigureRequest { enable };
        assert_eq!(
            WindowsSudoConfigureRequest::decode(&request.encode())
                .expect("Windows sudo request should decode"),
            request
        );
    }
    assert!(WindowsSudoConfigureRequest::decode(&[]).is_err());
    assert!(WindowsSudoConfigureRequest::decode(&[2]).is_err());

    let status = WindowsSudoStatus {
        available: true,
        configured_mode: WindowsSudoMode::Normal,
        policy_mode: Some(WindowsSudoMode::ForceNewWindow),
    };
    assert_eq!(
        WindowsSudoStatus::decode(&status.encode()).expect("Windows sudo status should decode"),
        status
    );
    assert_eq!(status.effective_mode(), WindowsSudoMode::ForceNewWindow);
    assert_eq!(
        WindowsSudoStatus {
            available: true,
            configured_mode: WindowsSudoMode::Disabled,
            policy_mode: Some(WindowsSudoMode::ForceNewWindow),
        }
        .effective_mode(),
        WindowsSudoMode::Disabled
    );
    assert!(WindowsSudoStatus::decode(&[1, 3]).is_err());
    assert!(WindowsSudoStatus::decode(&[2, 0, u8::MAX]).is_err());
    assert!(WindowsSudoStatus::decode(&[1, 4, u8::MAX]).is_err());
    assert!(WindowsSudoStatus::decode(&[1, 0, 4]).is_err());
}

#[test]
fn live_share_protocol_is_append_only_and_strict() {
    assert_eq!(FrameKind::LiveShareQuery as u8, 38);
    assert_eq!(FrameKind::LiveShareConfigure as u8, 39);
    assert_eq!(FrameKind::LiveShareStatus as u8, 40);
    assert_eq!(
        FrameKind::try_from(40).expect("live-share status kind should decode"),
        FrameKind::LiveShareStatus
    );
    for enable in [false, true] {
        let request = LiveShareConfigureRequest { enable };
        assert_eq!(
            LiveShareConfigureRequest::decode(&request.encode())
                .expect("live-share request should decode"),
            request
        );
        let status = LiveShareStatus { mapped: enable };
        assert_eq!(
            LiveShareStatus::decode(&status.encode()).expect("live-share status should decode"),
            status
        );
    }
    assert!(LiveShareConfigureRequest::decode(&[]).is_err());
    assert!(LiveShareConfigureRequest::decode(&[2]).is_err());
    assert!(LiveShareStatus::decode(&[]).is_err());
    assert!(LiveShareStatus::decode(&[2]).is_err());
}

#[test]
fn terminal_start_is_capability_gated_without_changing_start() {
    let request = StartRequest {
        kind: SessionKind::Shell,
        argv: vec!["pwsh.exe".to_owned(), "cmd.exe".to_owned()],
        working_directory: Some("C:\\src".to_owned()),
    };
    let terminal = TerminalStartRequest {
        size: TerminalSize::new(42, 132).expect("terminal size should be valid"),
        request,
    };
    assert_eq!(
        TerminalStartRequest::decode(&terminal.encode().expect("request should encode"))
            .expect("request should decode"),
        terminal
    );
    assert_eq!(
        FrameKind::try_from(16).expect("kind should decode"),
        FrameKind::TerminalStart
    );
}

#[test]
fn session_control_is_append_only_and_strictly_encoded() {
    assert_eq!(
        FrameKind::try_from(17).expect("options kind should decode"),
        FrameKind::SessionOptions
    );
    assert_eq!(
        FrameKind::try_from(18).expect("stdin-close kind should decode"),
        FrameKind::StdinClose
    );
    assert_eq!(
        FrameKind::try_from(19).expect("cancel kind should decode"),
        FrameKind::SessionCancel
    );

    for cancel_on_disconnect in [false, true] {
        let options = SessionOptions {
            cancel_on_disconnect,
        };
        assert_eq!(
            SessionOptions::decode(&options.encode()).expect("options should decode"),
            options
        );
    }
    assert!(SessionOptions::decode(&[]).is_err());
    assert!(SessionOptions::decode(&[0, 0]).is_err());
    assert!(SessionOptions::decode(&[2]).is_err());
}

#[test]
fn session_lease_is_append_only_bounded_and_strictly_encoded() {
    assert_eq!(
        FrameKind::try_from(24).expect("lease kind should decode"),
        FrameKind::SessionLease
    );
    assert_eq!(
        FrameKind::try_from(25).expect("heartbeat kind should decode"),
        FrameKind::SessionHeartbeat
    );

    let lease = SessionLease::standard();
    assert_eq!(
        SessionLease::decode(&lease.encode()).expect("lease should decode"),
        lease
    );
    assert_eq!(lease.heartbeat_interval(), Duration::from_secs(30));
    assert!(SessionLease::decode(&[]).is_err());
    assert!(SessionLease::decode(&[0, 0, 0, 0]).is_err());
    assert!(SessionLease::new(MIN_SESSION_LEASE_TIMEOUT_MILLIS - 1).is_err());
    assert!(SessionLease::new(MIN_SESSION_LEASE_TIMEOUT_MILLIS).is_ok());
    assert!(SessionLease::new(MAX_SESSION_LEASE_TIMEOUT_MILLIS).is_ok());
    assert!(SessionLease::new(MAX_SESSION_LEASE_TIMEOUT_MILLIS + 1).is_err());
}

#[test]
fn session_lease_state_uses_monotonic_input_and_never_resurrects() {
    let lease =
        SessionLease::new(MIN_SESSION_LEASE_TIMEOUT_MILLIS).expect("minimum lease should be valid");
    let mut state = SessionLeaseState::new(lease, 1_000);
    assert_eq!(state.deadline_millis(), 2_000);
    assert!(!state.is_expired(1_999));
    assert!(state.observe_heartbeat(1_500));
    assert_eq!(state.deadline_millis(), 2_500);
    assert!(!state.is_expired(2_499));
    assert!(state.is_expired(2_500));
    assert!(!state.observe_heartbeat(2_500));
    assert_eq!(state.deadline_millis(), 2_500);

    let saturated = SessionLeaseState::new(lease, u64::MAX - 10);
    assert_eq!(saturated.deadline_millis(), u64::MAX);
}

#[test]
fn terminal_dimensions_fit_windows_coord() {
    assert!(TerminalSize::new(0, 80).is_err());
    assert!(TerminalSize::new(24, 0).is_err());
    assert!(TerminalSize::new(MAX_TERMINAL_DIMENSION, MAX_TERMINAL_DIMENSION).is_ok());
    assert!(TerminalSize::new(MAX_TERMINAL_DIMENSION + 1, 80).is_err());
    assert!(TerminalSize::decode(&[0, 24, 0, 80, 0]).is_err());
}
