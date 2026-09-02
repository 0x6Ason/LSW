// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[test]
fn only_exact_windows_shell_surfaces_are_dismissible_for_focus_recovery() {
    assert!(dismissible_windows_shell_identity(
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\SearchHost.exe",
        "MicrosoftWindows.Client.CBS_cw5n1h2txyewy",
    ));
    assert!(dismissible_windows_shell_identity(
        r"C:\Windows\SystemApps\Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy\StartMenuExperienceHost.exe",
        "Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy",
    ));
    assert!(!dismissible_windows_shell_identity(
        r"C:\Users\Public\SearchHost.exe",
        "Untrusted.Search_cw5n1h2txyewy",
    ));
    assert!(!dismissible_windows_shell_identity(
        r"C:\Windows\System32\notepad.exe",
        "MicrosoftWindows.Client.CBS_cw5n1h2txyewy",
    ));
}

#[test]
fn per_monitor_v2_dpi_guard_is_thread_scoped_and_restores_context() {
    // SAFETY: these APIs only query opaque context tokens for the current
    // test thread.
    let before = unsafe { GetThreadDpiAwarenessContext() };
    {
        let _guard = ThreadDpiAwareness::per_monitor_v2()
            .expect("test thread should accept a per-monitor-v2 override");
        // SAFETY: context comparison does not dereference either token.
        assert!(unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
            .as_bool()
        });
    }
    // SAFETY: context comparison does not dereference either token.
    assert!(unsafe {
        AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), before).as_bool()
    });
}

fn candidate(
    hwnd: isize,
    pid: u32,
    creation_time: u64,
    session_id: u32,
    image_path: Option<&str>,
    package_full_name: Option<&str>,
    new_window: bool,
) -> WindowCandidate {
    WindowCandidate {
        hwnd,
        pid,
        creation_time,
        session_id,
        image_path: image_path.map(str::to_owned),
        package_full_name: package_full_name.map(str::to_owned),
        package_family_name: package_full_name.map(|_| "family".to_owned()),
        application_user_model_id: package_full_name.map(|_| "family!Notepad".to_owned()),
        new_window,
    }
}

fn activated_package(package_full_name: &str) -> ActivatedPackageIdentity {
    ActivatedPackageIdentity {
        package_full_name: package_full_name.to_owned(),
        package_family_name: "family".to_owned(),
        aumid: "family!Notepad".to_owned(),
    }
}

fn select_exact(candidates: &[WindowCandidate], expected: ProcessKey) -> Option<WindowCandidate> {
    select_exact_window_candidate(candidates, expected, 3, false, None)
}

fn select_aam(
    candidates: &[WindowCandidate],
    expected: ProcessKey,
    package: &ActivatedPackageIdentity,
) -> Option<WindowCandidate> {
    select_exact_window_candidate(candidates, expected, 3, true, Some(package))
}

#[test]
fn exact_launcher_requires_one_candidate_stable_across_polls() {
    let launcher = ProcessKey {
        pid: 42,
        creation_time: 900,
    };
    let splash = candidate(1, 42, 900, 3, Some(r"c:\launcher.exe"), None, true);
    let main = candidate(2, 42, 900, 3, Some(r"c:\launcher.exe"), None, true);
    let delegate = candidate(
        3,
        80,
        901,
        3,
        Some(r"c:\package.exe"),
        Some("Package_1.0_x64__publisher"),
        true,
    );
    assert_eq!(
        select_exact(&[splash.clone(), main.clone(), delegate], launcher),
        None,
        "an ambiguous exact launcher must never fall through to a delegate"
    );

    let selected = select_exact(std::slice::from_ref(&main), launcher).unwrap();
    let mut stability = CandidateStability::default();
    assert_eq!(stability.observe(selected.clone()), None);
    assert_eq!(stability.observe(selected.clone()), Some(selected));
}

#[test]
fn exact_launcher_identity_wins_without_filename_or_ambiguity_fallback() {
    let launcher = ProcessKey {
        pid: 42,
        creation_time: 900,
    };
    let windows = vec![
        candidate(1, 80, 901, 3, Some(r"c:\notepad.exe"), None, true),
        candidate(2, 81, 902, 3, Some(r"c:\notepad.exe"), None, true),
        candidate(3, 42, 900, 3, Some(r"c:\launcher.exe"), None, true),
    ];
    assert_eq!(select_exact(&windows, launcher), Some(windows[2].clone()));
}

#[test]
fn aam_selection_accepts_only_the_exact_returned_pid_and_creation_identity() {
    let activated = ProcessKey {
        pid: 80,
        creation_time: 901,
    };
    let package = activated_package("Package_1.0_x64__publisher");
    let matching = candidate(
        1,
        80,
        901,
        3,
        Some(r"c:\package\notepad.exe"),
        Some("Package_1.0_x64__publisher"),
        true,
    );
    let temporal_counterexample = candidate(
        2,
        81,
        902,
        3,
        Some(r"c:\package\notepad.exe"),
        Some("Package_1.0_x64__publisher"),
        true,
    );
    assert_eq!(
        select_aam(
            &[temporal_counterexample, matching.clone()],
            activated,
            &package,
        ),
        Some(matching)
    );
}

#[test]
fn aam_multiple_hwnds_for_the_returned_pid_fail_closed() {
    let activated = ProcessKey {
        pid: 80,
        creation_time: 901,
    };
    let package = activated_package("Package_1.0_x64__publisher");
    let windows = vec![
        candidate(
            1,
            80,
            901,
            3,
            Some(r"c:\package\notepad.exe"),
            Some("Package_1.0_x64__publisher"),
            true,
        ),
        candidate(
            2,
            80,
            901,
            3,
            Some(r"c:\package\notepad.exe"),
            Some("Package_1.0_x64__publisher"),
            true,
        ),
    ];
    assert_eq!(select_aam(&windows, activated, &package), None);
}

#[test]
fn aam_selection_requires_a_new_window_in_the_activated_session() {
    let activated = ProcessKey {
        pid: 80,
        creation_time: 901,
    };
    let package = activated_package("Package_1.0_x64__publisher");
    let old_window = candidate(
        1,
        80,
        901,
        3,
        Some(r"c:\package\notepad.exe"),
        Some("Package_1.0_x64__publisher"),
        false,
    );
    let other_session = candidate(
        2,
        80,
        901,
        4,
        Some(r"c:\package\notepad.exe"),
        Some("Package_1.0_x64__publisher"),
        true,
    );
    assert_eq!(select_aam(&[old_window], activated, &package), None);
    assert_eq!(select_aam(&[other_session], activated, &package), None);
}

#[test]
fn selected_candidate_must_be_stable_for_two_consecutive_polls() {
    let first = candidate(1, 80, 901, 3, Some(r"c:\notepad.exe"), None, true);
    let second = candidate(2, 81, 902, 3, Some(r"c:\notepad.exe"), None, true);
    let mut stability = CandidateStability::default();
    assert_eq!(stability.observe(first.clone()), None);
    assert_eq!(stability.observe(second), None);
    assert_eq!(stability.observe(first.clone()), None);
    assert_eq!(stability.observe(first.clone()), Some(first));
    stability.reset();
    assert!(stability.previous.is_none());
}

#[test]
fn pid_reuse_does_not_match_the_launcher_creation_identity() {
    let launcher = ProcessKey {
        pid: 42,
        creation_time: 900,
    };
    let reused = candidate(1, 42, 901, 3, Some(r"c:\notepad.exe"), None, true);
    assert_eq!(select_exact(&[reused], launcher), None);
}

#[test]
fn same_full_image_without_documented_causal_identity_is_rejected() {
    let activated = ProcessKey {
        pid: 80,
        creation_time: 901,
    };
    let package = activated_package("Package_1.0_x64__publisher");
    let unrelated = candidate(
        1,
        81,
        902,
        3,
        Some(r"c:\windows\notepad.exe"),
        Some("Package_1.0_x64__publisher"),
        true,
    );
    assert_eq!(select_aam(&[unrelated], activated, &package), None);
}

#[test]
fn aam_returned_pid_must_be_newer_than_activation_and_absent_from_snapshot() {
    let mut before = BTreeSet::from([80]);
    assert!(!activation_process_is_new(80, 1_001, 1_000, &before));
    before.clear();
    assert!(!activation_process_is_new(80, 999, 1_000, &before));
    assert!(!activation_process_is_new(0, 1_001, 1_000, &before));
    assert!(activation_process_is_new(80, 1_000, 1_000, &before));
}

#[test]
fn manifest_execution_alias_correlates_a_packaged_process() {
    let manifest = br#"
      <Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
               xmlns:uap3="http://schemas.microsoft.com/appx/manifest/uap/windows10/3"
               xmlns:desktop="http://schemas.microsoft.com/appx/manifest/desktop/windows10">
        <Applications><Application Id="Notepad">
          <Extensions>
            <uap3:Extension Category="windows.appExecutionAlias">
              <uap3:AppExecutionAlias>
                <desktop:ExecutionAlias Alias="NOTEPAD.EXE" />
              </uap3:AppExecutionAlias>
            </uap3:Extension>
          </Extensions>
        </Application></Applications>
      </Package>
    "#;
    assert_eq!(
        manifest_xml_alias_application(manifest, "notepad.exe").unwrap(),
        Some("Notepad".to_owned())
    );

    let activated = ProcessKey {
        pid: 80,
        creation_time: 901,
    };
    let package = activated_package("Microsoft.Notepad_1.0_x64__publisher");
    let packaged = candidate(
        1,
        80,
        901,
        3,
        Some(r"c:\program files\windowsapps\notepad.exe"),
        Some("Microsoft.Notepad_1.0_x64__publisher"),
        true,
    );
    assert_eq!(
        select_aam(std::slice::from_ref(&packaged), activated, &package),
        Some(packaged)
    );

    let mut wrong_application = candidate(
        2,
        80,
        901,
        3,
        Some(r"c:\program files\windowsapps\notepad.exe"),
        Some("Microsoft.Notepad_1.0_x64__publisher"),
        true,
    );
    wrong_application.application_user_model_id = Some("family!Settings".to_owned());
    assert_eq!(select_aam(&[wrong_application], activated, &package), None);

    let mut wrong_family = candidate(
        3,
        80,
        901,
        3,
        Some(r"c:\program files\windowsapps\notepad.exe"),
        Some("Microsoft.Notepad_1.0_x64__publisher"),
        true,
    );
    wrong_family.package_family_name = Some("unrelated_family".to_owned());
    assert_eq!(select_aam(&[wrong_family], activated, &package), None);
}

#[test]
fn execution_alias_must_be_inside_the_correct_extension_category() {
    let wrong_category = br#"
      <Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10">
      <Applications><Application Id="Notepad"><Extensions>
        <Extension Category="windows.fileTypeAssociation">
          <ExecutionAlias Alias="notepad.exe" />
        </Extension>
      </Extensions></Application></Applications></Package>
    "#;
    assert_eq!(
        manifest_xml_alias_application(wrong_category, "notepad.exe").unwrap(),
        None
    );
}

#[test]
fn execution_alias_rejects_spoof_namespaces_and_ambiguous_applications() {
    let spoofed = br#"
      <Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
               xmlns:spoof="urn:not-a-windows-manifest-schema">
        <Applications><Application Id="Notepad"><Extensions>
          <spoof:Extension Category="windows.appExecutionAlias">
            <spoof:AppExecutionAlias>
              <spoof:ExecutionAlias Alias="notepad.exe" />
            </spoof:AppExecutionAlias>
          </spoof:Extension>
        </Extensions></Application></Applications>
      </Package>
    "#;
    assert_eq!(
        manifest_xml_alias_application(spoofed, "notepad.exe").unwrap(),
        None
    );

    let structurally_spoofed = br#"
      <Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
               xmlns:uap3="http://schemas.microsoft.com/appx/manifest/uap/windows10/3"
               xmlns:desktop="http://schemas.microsoft.com/appx/manifest/desktop/windows10">
        <Properties><Applications><Application Id="Notepad"><Extensions>
          <uap3:Extension Category="windows.appExecutionAlias">
            <uap3:AppExecutionAlias>
              <desktop:ExecutionAlias Alias="notepad.exe" />
            </uap3:AppExecutionAlias>
          </uap3:Extension>
        </Extensions></Application></Applications></Properties>
      </Package>
    "#;
    assert_eq!(
        manifest_xml_alias_application(structurally_spoofed, "notepad.exe").unwrap(),
        None
    );

    let ambiguous = br#"
      <Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
               xmlns:uap5="http://schemas.microsoft.com/appx/manifest/uap/windows10/5">
        <Applications>
          <Application Id="One"><Extensions>
            <uap5:Extension Category="windows.appExecutionAlias">
              <uap5:AppExecutionAlias><uap5:ExecutionAlias Alias="x.exe" /></uap5:AppExecutionAlias>
            </uap5:Extension>
          </Extensions></Application>
          <Application Id="Two"><Extensions>
            <uap5:Extension Category="windows.appExecutionAlias">
              <uap5:AppExecutionAlias><uap5:ExecutionAlias Alias="x.exe" /></uap5:AppExecutionAlias>
            </uap5:Extension>
          </Extensions></Application>
        </Applications>
      </Package>
    "#;
    assert!(manifest_xml_alias_application(ambiguous, "x.exe").is_err());
}

#[test]
fn executable_alias_normalization_is_case_insensitive_and_path_scoped() {
    assert_eq!(
        normalize_executable_alias(r"C:\Windows\System32\NOTEPAD.EXE").unwrap(),
        "notepad.exe"
    );
    assert_eq!(
        normalize_executable_alias("notepad.exe").unwrap(),
        "notepad.exe"
    );
    assert_eq!(
        normalize_executable_alias("NOTEPAD").unwrap(),
        "notepad.exe"
    );
    assert_eq!(
        packaged_activation_alias_name("NOTEPAD").unwrap(),
        Some("notepad.exe".to_owned())
    );
    assert_eq!(
        packaged_activation_alias_name(r"C:\Windows\System32\notepad.exe").unwrap(),
        None
    );
    assert_eq!(
        packaged_activation_alias_name(r".\notepad.exe").unwrap(),
        None
    );
    assert!(normalize_executable_alias("").is_err());
}

#[test]
fn cleanup_identity_never_accepts_a_reused_process_id() {
    assert!(owner_pid_matches(42, Some(42)));
    assert!(!owner_pid_matches(42, Some(43)));
    assert!(!owner_pid_matches(42, None));
}

#[test]
fn absolute_pointer_axes_cover_the_exact_virtual_desktop_endpoints() {
    assert_eq!(normalize_absolute_axis(-1920, -1920, 3840), Some(0));
    assert_eq!(normalize_absolute_axis(1919, -1920, 3840), Some(65_535));
    assert!(normalize_absolute_axis(-1921, -1920, 3840).is_none());
    assert!(normalize_absolute_axis(1920, -1920, 3840).is_none());
    assert!(normalize_absolute_axis(0, 0, 1).is_none());
}

#[test]
fn native_maximize_transition_is_explicit_and_tracks_guest_chrome_state() {
    assert_eq!(
        maximize_transition(false),
        (GuiWindowAction::Maximize, SC_MAXIMIZE as usize)
    );
    assert_eq!(
        maximize_transition(true),
        (GuiWindowAction::Restore, SC_RESTORE as usize)
    );
}

#[test]
fn secondary_input_requires_a_pinned_owned_intersecting_window() {
    let main = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 600,
    };
    let overlapping = RECT {
        left: 100,
        top: 100,
        right: 500,
        bottom: 400,
    };
    assert!(secondary_window_input_eligible(
        true,
        true,
        main,
        overlapping,
    ));
    assert!(!secondary_window_input_eligible(
        false,
        true,
        main,
        overlapping,
    ));
    assert!(!secondary_window_input_eligible(
        true,
        false,
        main,
        overlapping,
    ));
    let disjoint = RECT {
        left: 800,
        top: 100,
        right: 900,
        bottom: 200,
    };
    assert!(!secondary_window_input_eligible(true, true, main, disjoint,));
    assert!(!rectangles_intersect(main, RECT::default()));
}

#[test]
fn owned_dialog_frames_are_clipped_and_composited_in_z_order() {
    let main = CapturedFrame {
        width: 3,
        height: 2,
        bgra: [10_u8, 20, 30, 255].repeat(6),
    };
    let lower = CapturedFrame {
        width: 3,
        height: 1,
        bgra: vec![40, 50, 60, 255, 70, 80, 90, 255, 130, 140, 150, 255],
    };
    let upper = CapturedFrame {
        width: 1,
        height: 1,
        bgra: vec![100, 110, 120, 255],
    };
    let main_bounds = RECT {
        left: 100,
        top: 200,
        right: 103,
        bottom: 202,
    };
    let lower_bounds = RECT {
        left: 99,
        top: 201,
        right: 102,
        bottom: 202,
    };
    let upper_bounds = RECT {
        left: 100,
        top: 201,
        right: 101,
        bottom: 202,
    };
    let output = composite_captured_frame(
        &main,
        main_bounds,
        &[(&lower, lower_bounds), (&upper, upper_bounds)],
    )
    .unwrap();
    assert_eq!(&output.bgra[12..16], &[100, 110, 120, 255]);
    assert_eq!(&output.bgra[16..20], &[130, 140, 150, 255]);
    assert_eq!(&output.bgra[20..24], &[10, 20, 30, 255]);
}

#[test]
fn owned_dialog_z_order_changes_invalidate_the_composite() {
    assert!(!window_order_changed(&[10, 20], &[10, 20]));
    assert!(window_order_changed(&[10, 20], &[20, 10]));
    assert!(window_order_changed(&[10], &[10, 20]));
    assert!(window_order_changed(&[10, 20], &[10]));
}

#[test]
fn hit_test_actions_only_claim_host_window_controls() {
    let forward = |action| Some(NonClientHitAction::Forward(action));
    assert_eq!(hit_test_action(HTCAPTION), forward(GuiWindowAction::Move));
    assert_eq!(
        hit_test_action(HTMINBUTTON),
        forward(GuiWindowAction::Minimize)
    );
    assert_eq!(
        hit_test_action(HTMAXBUTTON),
        Some(NonClientHitAction::MaximizeOrRestore)
    );
    assert_eq!(hit_test_action(HTCLOSE), forward(GuiWindowAction::Close));
    assert_eq!(
        hit_test_action(HTTOPLEFT),
        forward(GuiWindowAction::ResizeTopLeft)
    );
    assert_eq!(hit_test_action(HTTOP), forward(GuiWindowAction::ResizeTop));
    assert_eq!(
        hit_test_action(HTTOPRIGHT),
        forward(GuiWindowAction::ResizeTopRight)
    );
    assert_eq!(
        hit_test_action(HTRIGHT),
        forward(GuiWindowAction::ResizeRight)
    );
    assert_eq!(
        hit_test_action(HTBOTTOMRIGHT),
        forward(GuiWindowAction::ResizeBottomRight)
    );
    assert_eq!(
        hit_test_action(HTBOTTOM),
        forward(GuiWindowAction::ResizeBottom)
    );
    assert_eq!(
        hit_test_action(HTBOTTOMLEFT),
        forward(GuiWindowAction::ResizeBottomLeft)
    );
    assert_eq!(
        hit_test_action(HTLEFT),
        forward(GuiWindowAction::ResizeLeft)
    );
    assert_eq!(hit_test_action(1), None);
}

#[test]
fn synthetic_resize_edges_cover_every_direction_without_ambiguity() {
    assert_eq!(
        resize_action_from_edges(true, true, false, false),
        Some(GuiWindowAction::ResizeTopLeft)
    );
    assert_eq!(
        resize_action_from_edges(false, true, false, false),
        Some(GuiWindowAction::ResizeTop)
    );
    assert_eq!(
        resize_action_from_edges(false, true, true, false),
        Some(GuiWindowAction::ResizeTopRight)
    );
    assert_eq!(
        resize_action_from_edges(false, false, true, false),
        Some(GuiWindowAction::ResizeRight)
    );
    assert_eq!(
        resize_action_from_edges(false, false, true, true),
        Some(GuiWindowAction::ResizeBottomRight)
    );
    assert_eq!(
        resize_action_from_edges(false, false, false, true),
        Some(GuiWindowAction::ResizeBottom)
    );
    assert_eq!(
        resize_action_from_edges(true, false, false, true),
        Some(GuiWindowAction::ResizeBottomLeft)
    );
    assert_eq!(
        resize_action_from_edges(true, false, false, false),
        Some(GuiWindowAction::ResizeLeft)
    );
    assert_eq!(resize_action_from_edges(false, false, false, false), None);
    assert_eq!(resize_action_from_edges(true, false, true, false), None);
}

#[test]
fn signed_hit_test_coordinates_are_bounded_without_clamping() {
    assert!(point_lparam(-12, -34).is_some());
    assert!(point_lparam(i32::from(i16::MAX), i32::from(i16::MIN)).is_some());
    assert!(point_lparam(i32::from(i16::MAX) + 1, 0).is_none());
    assert!(point_lparam(0, i32::from(i16::MIN) - 1).is_none());
}

#[test]
fn capture_geometry_accepts_small_frame_deltas_and_scales_points() {
    let bounds = capture_bounds_from_rect(
        RECT {
            left: -100,
            top: 50,
            right: 924,
            bottom: 818,
        },
        (1020, 764),
    )
    .unwrap();
    assert_eq!((bounds.width, bounds.height), (1024, 768));
    assert_eq!(
        map_capture_point(bounds, (1020, 764), 0, 0),
        Some((-100, 50))
    );
    let last = map_capture_point(bounds, (1020, 764), 1019, 763).unwrap();
    assert!(last.0 < 924 && last.1 < 818);
}

#[test]
fn capture_geometry_rejects_stale_or_unrelated_bounds() {
    let rectangle = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };
    assert!(capture_bounds_from_rect(rectangle, (800, 600)).is_none());
    assert!(capture_bounds_from_rect(RECT::default(), (800, 600)).is_none());
    let bounds = CaptureBounds {
        left: 0,
        top: 0,
        width: 800,
        height: 600,
    };
    assert!(map_capture_point(bounds, (800, 600), 800, 0).is_none());
    assert!(map_capture_point(bounds, (800, 600), 0, 600).is_none());
}

#[test]
fn resize_geometry_transition_drops_pointer_until_capture_extent_catches_up() {
    let maximized = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    // SetWindowPos/ShowWindow can expose the new HWND extent while the
    // most recent WGC frame and host pointer coordinates still use the old
    // extent. That event must be dropped rather than guessed or fatal.
    assert!(capture_bounds_from_rect(maximized, (800, 600)).is_none());

    let current = capture_bounds_from_rect(maximized, (1920, 1080)).unwrap();
    assert_eq!(
        map_capture_point(current, (1920, 1080), 960, 540),
        Some((960, 540))
    );
    assert_eq!(
        map_capture_point(current, (800, 600), 400, 300),
        Some((960, 540))
    );
}

#[test]
fn resize_preserves_the_requested_visible_dwm_extent() {
    let outer = RECT {
        left: 92,
        top: 92,
        right: 1008,
        bottom: 758,
    };
    let visible = RECT {
        left: 100,
        top: 100,
        right: 1000,
        bottom: 750,
    };
    assert_eq!(
        resize_outer_extent_from_rects(outer, visible, 900, 650),
        Some((916, 666))
    );
}

#[test]
fn resize_keeps_the_outer_window_inside_the_monitor_work_area() {
    let work = RECT {
        left: 0,
        top: 0,
        right: 1280,
        bottom: 752,
    };
    let current = RECT {
        left: 112,
        top: 92,
        right: 1028,
        bottom: 758,
    };
    assert_eq!(
        clamp_window_origin_to_work_area(current, work, 916, 701),
        Some((112, 51))
    );

    let already_visible = RECT {
        left: 120,
        top: 40,
        right: 1020,
        bottom: 690,
    };
    assert_eq!(
        clamp_window_origin_to_work_area(already_visible, work, 900, 650),
        Some((120, 40))
    );
}

#[test]
fn oversized_resize_anchors_at_the_work_area_origin() {
    let work = RECT {
        left: -1280,
        top: 24,
        right: 0,
        bottom: 800,
    };
    let current = RECT {
        left: -900,
        top: 100,
        right: -100,
        bottom: 700,
    };
    assert_eq!(
        clamp_window_origin_to_work_area(current, work, 1400, 900),
        Some((-1280, 24))
    );
    assert!(clamp_window_origin_to_work_area(current, RECT::default(), 800, 600).is_none());
    assert!(clamp_window_origin_to_work_area(current, work, 0, 600).is_none());
}

#[test]
fn resize_rejects_unrelated_or_inverted_frame_bounds() {
    let outer = RECT {
        left: 100,
        top: 100,
        right: 900,
        bottom: 700,
    };
    let outside = RECT {
        left: 90,
        top: 100,
        right: 900,
        bottom: 700,
    };
    assert!(resize_outer_extent_from_rects(outer, outside, 800, 600).is_none());

    let excessive = RECT {
        left: 165,
        top: 100,
        right: 900,
        bottom: 700,
    };
    assert!(resize_outer_extent_from_rects(outer, excessive, 800, 600).is_none());
}

#[test]
fn injected_release_tracking_deduplicates_and_forgets_repressed_keys() {
    let mut input = InjectedInputState::default();
    input.note_release(0);
    assert!(input.recent_release_keys().is_empty());

    input.note_release(0xa2);
    input.note_release(0xa2);
    input.note_release(VK_LBUTTON.0);
    assert_eq!(input.recent_release_keys(), vec![0xa2, VK_LBUTTON.0]);

    input.note_press(0xa2);
    assert_eq!(input.recent_release_keys(), vec![VK_LBUTTON.0]);
}

#[test]
fn release_dispatch_grace_requires_one_stable_released_interval() {
    let started = Instant::now();
    let mut released_since = None;
    assert!(!release_dispatch_grace_complete(
        &mut released_since,
        started,
        true
    ));
    assert!(!release_dispatch_grace_complete(
        &mut released_since,
        started + INPUT_RELEASE_DISPATCH_GRACE - Duration::from_millis(1),
        true
    ));
    assert!(!release_dispatch_grace_complete(
        &mut released_since,
        started + INPUT_RELEASE_DISPATCH_GRACE,
        false
    ));
    assert_eq!(released_since, None);
    assert!(!release_dispatch_grace_complete(
        &mut released_since,
        started + INPUT_RELEASE_DISPATCH_GRACE,
        true
    ));
    assert!(release_dispatch_grace_complete(
        &mut released_since,
        started + INPUT_RELEASE_DISPATCH_GRACE * 2,
        true
    ));
}
