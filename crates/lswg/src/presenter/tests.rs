// SPDX-License-Identifier: GPL-3.0-or-later

use lsw_core::{GuiPointerButton, GuiWindowDamage, GuiWindowDragHint, GuiWindowReady};
use winit::keyboard::KeyCode;
use winit::window::ResizeDirection;

use super::*;

#[test]
fn wslg_identity_hex_is_canonical_and_preserves_utf8() {
    assert_eq!(hex_encode("Notepad - LSW"), "4e6f7465706164202d204c5357");
    assert_eq!(hex_encode("é\n"), "c3a90a");
}

#[test]
fn damage_is_bounded_and_converts_bgra_to_rgb() {
    let ready = GuiWindowReady {
        process_id: 1,
        window_id: 2,
        width: 2,
        height: 1,
        title: "test".to_owned(),
    };
    let mut state = FrameState::new(ready).unwrap();
    state
        .apply(GuiWindowDamage {
            sequence: 1,
            x: 1,
            y: 0,
            width: 1,
            height: 1,
            bgra: vec![0x11, 0x22, 0x33, 0xff],
        })
        .unwrap();
    assert_eq!(state.pixels, vec![0, 0x0033_2211]);
    assert!(state
        .apply(GuiWindowDamage {
            sequence: 2,
            x: 2,
            y: 0,
            width: 1,
            height: 1,
            bgra: vec![0; 4],
        })
        .is_err());
}

#[test]
fn ready_updates_cannot_replace_the_pinned_guest_window() {
    let ready = GuiWindowReady {
        process_id: 7,
        window_id: 11,
        width: 640,
        height: 480,
        title: "first".to_owned(),
    };
    let mut state = FrameState::new(ready.clone()).unwrap();
    state
        .resize(GuiWindowReady {
            width: 800,
            height: 600,
            title: "renamed".to_owned(),
            ..ready.clone()
        })
        .unwrap();
    assert!(state
        .resize(GuiWindowReady {
            window_id: 12,
            ..ready
        })
        .is_err());
}

#[test]
fn key_and_pointer_translation_are_stable() {
    assert_eq!(windows_key(KeyCode::KeyA), Some((0x41, false)));
    assert_eq!(windows_key(KeyCode::ControlRight), Some((0xa3, true)));
    assert_eq!(windows_key(KeyCode::AudioVolumeUp), None);
    assert_eq!(
        scale_pointer(50.0, 25.0, (100, 50), (200, 100)),
        Some((100, 50))
    );
    assert_eq!(
        fit_viewport((2560, 1380), (1280, 752)),
        Viewport {
            x: 106,
            y: 0,
            width: 2348,
            height: 1380,
        }
    );
    assert_eq!(scale_pointer(50.0, 690.0, (2560, 1380), (1280, 752)), None);
    assert_eq!(
        scale_pointer(1280.0, 690.0, (2560, 1380), (1280, 752)),
        Some((640, 376))
    );
}

#[test]
fn ctrl_c_and_ctrl_v_edges_map_to_windows_virtual_keys() {
    let mut input = ForwardedInputState::default();
    assert_eq!(
        input.key_event(KeyCode::ControlLeft, true),
        Some(GuiInputEvent::Key {
            virtual_key: 0xa2,
            scan_code: 0,
            pressed: true,
            extended: false,
        })
    );
    assert_eq!(windows_key(KeyCode::KeyC), Some((0x43, false)));
    assert_eq!(windows_key(KeyCode::KeyV), Some((0x56, false)));
    assert!(input.key_event(KeyCode::KeyC, true).is_some());
    assert!(input.key_event(KeyCode::KeyC, false).is_some());
    assert!(input.key_event(KeyCode::KeyV, true).is_some());
    assert!(input.key_event(KeyCode::KeyV, false).is_some());
    assert!(input.key_event(KeyCode::ControlLeft, false).is_some());
    assert!(input.release_events(None).is_empty());
}

#[test]
fn guest_border_actions_map_to_all_eight_host_resize_directions() {
    for (action, direction) in [
        (GuiWindowAction::ResizeTopLeft, ResizeDirection::NorthWest),
        (GuiWindowAction::ResizeTop, ResizeDirection::North),
        (GuiWindowAction::ResizeTopRight, ResizeDirection::NorthEast),
        (GuiWindowAction::ResizeRight, ResizeDirection::East),
        (
            GuiWindowAction::ResizeBottomRight,
            ResizeDirection::SouthEast,
        ),
        (GuiWindowAction::ResizeBottom, ResizeDirection::South),
        (
            GuiWindowAction::ResizeBottomLeft,
            ResizeDirection::SouthWest,
        ),
        (GuiWindowAction::ResizeLeft, ResizeDirection::West),
    ] {
        assert_eq!(host_resize_direction(action), Some(direction));
    }
    assert_eq!(host_resize_direction(GuiWindowAction::Move), None);
    assert_eq!(host_resize_direction(GuiWindowAction::Close), None);
}

#[test]
fn forwarded_input_releases_chords_in_reverse_order_and_ignores_stray_releases() {
    let mut input = ForwardedInputState::default();
    assert_eq!(
        input.key_event(KeyCode::ControlLeft, true),
        Some(GuiInputEvent::Key {
            virtual_key: 0xa2,
            scan_code: 0,
            pressed: true,
            extended: false,
        })
    );
    assert!(input.key_event(KeyCode::KeyC, true).is_some());
    assert!(input.key_event(KeyCode::KeyC, true).is_some());
    assert_eq!(input.pressed_keys.len(), 2);

    assert_eq!(
        input.release_events(None),
        vec![
            GuiInputEvent::Key {
                virtual_key: 0x43,
                scan_code: 0,
                pressed: false,
                extended: false,
            },
            GuiInputEvent::Key {
                virtual_key: 0xa2,
                scan_code: 0,
                pressed: false,
                extended: false,
            },
        ]
    );
    assert_eq!(input.key_event(KeyCode::KeyC, false), None);
    assert!(input.release_events(None).is_empty());
}

#[test]
fn forwarded_input_idle_lease_bounds_a_missing_focus_leave() {
    let mut input = ForwardedInputState::default();
    let started = Instant::now();
    assert!(input.key_event(KeyCode::ControlLeft, true).is_some());
    input.note_activity(started);
    assert_eq!(
        input.lease_deadline(),
        Some(started + FORWARDED_INPUT_IDLE_LEASE)
    );
    assert!(!input.lease_expired(started + FORWARDED_INPUT_IDLE_LEASE - Duration::from_millis(1)));
    assert!(input.lease_expired(started + FORWARDED_INPUT_IDLE_LEASE));

    let refreshed = started + Duration::from_secs(1);
    input.note_activity(refreshed);
    assert_eq!(
        input.lease_deadline(),
        Some(refreshed + FORWARDED_INPUT_IDLE_LEASE)
    );
    assert_eq!(input.release_events(None).len(), 1);
    assert_eq!(input.lease_deadline(), None);
}

#[test]
fn smooth_wheel_preserves_fractional_residuals() {
    let mut residual = 0.0;
    assert_eq!(accumulate_wheel(&mut residual, 0.4), None);
    assert_eq!(accumulate_wheel(&mut residual, 0.4), None);
    assert_eq!(accumulate_wheel(&mut residual, 0.4), Some(1));
    assert!((residual - 0.2).abs() < f64::EPSILON * 4.0);
    assert_eq!(accumulate_wheel(&mut residual, f64::NAN), None);
}

#[test]
fn drag_grant_is_recent_and_one_shot() {
    let start = Instant::now();
    let mut grant = DragGrant::default();
    assert!(!grant.take(start));
    grant.arm(start);
    assert!(grant.take(start + Duration::from_millis(10)));
    assert!(!grant.take(start + Duration::from_millis(11)));
    grant.arm(start);
    assert!(!grant.take(start + DRAG_GRANT_TIMEOUT + Duration::from_millis(1)));
}

#[test]
fn drag_hint_requires_the_exact_recent_guest_pointer() {
    let start = Instant::now();
    let mut hint = DragHintState::default();
    hint.observe(
        GuiWindowDragHint {
            x: 12,
            y: 34,
            action: Some(GuiWindowAction::Move),
        },
        start,
    );
    assert_eq!(
        hint.observation_for(Some((12, 34)), start + Duration::from_millis(10))
            .flatten(),
        Some(GuiWindowAction::Move)
    );
    assert_eq!(
        hint.observation_for(Some((13, 34)), start + Duration::from_millis(10))
            .flatten(),
        None
    );
    assert_eq!(
        hint.observation_for(
            Some((12, 34)),
            start + DRAG_HINT_TIMEOUT + Duration::from_millis(1),
        )
        .flatten(),
        None
    );
    hint.observe(
        GuiWindowDragHint {
            x: 12,
            y: 34,
            action: None,
        },
        start,
    );
    assert_eq!(hint.observation_for(Some((12, 34)), start).flatten(), None);
    assert_eq!(hint.observation_for(Some((12, 34)), start), Some(None));
}

#[test]
fn host_window_state_suppresses_guest_echo_but_reports_native_changes() {
    let mut state = HostWindowState::new(false);
    state.expect(true);
    assert_eq!(state.observe(true), None);
    assert_eq!(state.observe(false), Some(GuiWindowAction::Restore));
    assert_eq!(state.observe(false), None);
    assert_eq!(state.observe(true), Some(GuiWindowAction::Maximize));
}

#[test]
fn shutdown_unblocks_a_reader_stalled_on_a_full_presenter_queue() {
    let (sender, receiver) = mpsc::sync_channel(1);
    sender.send(1_u8).unwrap();
    let (attempted_sender, attempted_receiver) = mpsc::channel();
    let reader_thread = thread::spawn(move || {
        attempted_sender.send(()).unwrap();
        assert!(sender.send(2).is_err());
    });
    attempted_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    stop_event_reader(receiver, || {}, reader_thread).unwrap();
}

#[test]
fn rendering_scales_without_indexing_outside_the_guest_frame() {
    let ready = GuiWindowReady {
        process_id: 1,
        window_id: 2,
        width: 2,
        height: 1,
        title: "test".to_owned(),
    };
    let mut state = FrameState::new(ready).unwrap();
    state.pixels = vec![0x11, 0x22];
    let mut destination = vec![0; 8];
    render_scaled(&state, 4, 2, &mut destination).unwrap();
    assert_eq!(
        destination,
        vec![0x11, 0x11, 0x22, 0x22, 0x11, 0x11, 0x22, 0x22]
    );

    let mut letterboxed = vec![0xff; 16];
    render_scaled(&state, 4, 4, &mut letterboxed).unwrap();
    assert_eq!(
        letterboxed,
        vec![0, 0, 0, 0, 0x11, 0x11, 0x22, 0x22, 0x11, 0x11, 0x22, 0x22, 0, 0, 0, 0,]
    );
}

#[test]
fn forwarded_input_releases_only_buttons_sent_to_the_guest() {
    let mut input = ForwardedInputState::default();
    assert!(input
        .pointer_button_event(0, GuiPointerButton::Left, true, 12, 34)
        .is_some());
    assert_eq!(
        input.pointer_button_event(0, GuiPointerButton::Left, true, 12, 34),
        None
    );
    assert_eq!(
        input.release_events(Some((56, 78))),
        vec![GuiInputEvent::PointerButton {
            button: GuiPointerButton::Left,
            pressed: false,
            x: 56,
            y: 78,
        }]
    );
    assert!(input.release_events(Some((56, 78))).is_empty());
}
