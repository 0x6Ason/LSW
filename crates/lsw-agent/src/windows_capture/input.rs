// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NonClientHitAction {
    Forward(GuiWindowAction),
    MaximizeOrRestore,
}

pub(crate) fn non_client_action(hwnd: HWND, x: i32, y: i32) -> Option<NonClientHitAction> {
    let packed = point_lparam(x, y)?;
    let mut hit = 0usize;
    // SAFETY: WM_NCHITTEST is a bounded synchronous query. The HWND is live,
    // LPARAM contains the documented signed screen-coordinate pair, and the
    // output pointer remains valid for the duration of SendMessageTimeoutW.
    let delivered = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_NCHITTEST,
            WPARAM(0),
            LPARAM(packed),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            HIT_TEST_TIMEOUT_MS,
            Some(&mut hit),
        )
    };
    if delivered.0 == 0 {
        return None;
    }
    let native_hit = hit as u32;
    software_resize_action(hwnd, x, y, native_hit)
        .map(NonClientHitAction::Forward)
        .or_else(|| hit_test_action(native_hit))
}

pub(crate) fn software_resize_action(
    hwnd: HWND,
    x: i32,
    y: i32,
    native_hit: u32,
) -> Option<GuiWindowAction> {
    let (native_left, native_top, native_right, native_bottom) = match native_hit {
        HTTOPLEFT => (true, true, false, false),
        HTTOP => (false, true, false, false),
        HTTOPRIGHT => (false, true, true, false),
        HTRIGHT => (false, false, true, false),
        HTBOTTOMRIGHT => (false, false, true, true),
        HTBOTTOM => (false, false, false, true),
        HTBOTTOMLEFT => (true, false, false, true),
        HTLEFT => (true, false, false, false),
        HTCLIENT => (false, false, false, false),
        // Native caption and control results take precedence over the inner
        // resize affordance so close/maximize/minimize remain clickable.
        _ => return None,
    };
    // SAFETY: all queries accept the identity-validated main HWND and retain
    // no handles. A zero DPI or changed style fails closed without mutation.
    let (resizable, maximized, dpi) = unsafe {
        (
            (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32) & WS_THICKFRAME.0 != 0,
            IsZoomed(hwnd).as_bool(),
            GetDpiForWindow(hwnd),
        )
    };
    if !resizable || maximized || dpi == 0 {
        return None;
    }
    let bounds = window_frame_rect(hwnd).ok()?;
    if x < bounds.left || x >= bounds.right || y < bounds.top || y >= bounds.bottom {
        return None;
    }
    // SAFETY: the indices and nonzero DPI come from documented window-metric
    // APIs. Negative metric failures are clamped out below.
    let (frame_x, frame_y, padding) = unsafe {
        (
            GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi),
            GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi),
            GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi),
        )
    };
    let grab_x = frame_x.max(0).saturating_add(padding.max(0)).clamp(
        MIN_SYNTHETIC_RESIZE_GRAB_PIXELS,
        MAX_SYNTHETIC_RESIZE_GRAB_PIXELS,
    );
    let grab_y = frame_y.max(0).saturating_add(padding.max(0)).clamp(
        MIN_SYNTHETIC_RESIZE_GRAB_PIXELS,
        MAX_SYNTHETIC_RESIZE_GRAB_PIXELS,
    );
    let left = native_left || x < bounds.left.saturating_add(grab_x);
    let top = native_top || y < bounds.top.saturating_add(grab_y);
    let right = native_right || x >= bounds.right.saturating_sub(grab_x);
    let bottom = native_bottom || y >= bounds.bottom.saturating_sub(grab_y);
    resize_action_from_edges(left, top, right, bottom)
}

pub(crate) fn resize_action_from_edges(
    left: bool,
    top: bool,
    right: bool,
    bottom: bool,
) -> Option<GuiWindowAction> {
    match (left, top, right, bottom) {
        (true, true, false, false) => Some(GuiWindowAction::ResizeTopLeft),
        (false, true, true, false) => Some(GuiWindowAction::ResizeTopRight),
        (false, false, true, true) => Some(GuiWindowAction::ResizeBottomRight),
        (true, false, false, true) => Some(GuiWindowAction::ResizeBottomLeft),
        (false, true, false, false) => Some(GuiWindowAction::ResizeTop),
        (false, false, true, false) => Some(GuiWindowAction::ResizeRight),
        (false, false, false, true) => Some(GuiWindowAction::ResizeBottom),
        (true, false, false, false) => Some(GuiWindowAction::ResizeLeft),
        _ => None,
    }
}

pub(crate) fn hit_test_action(hit: u32) -> Option<NonClientHitAction> {
    let forward = |action| Some(NonClientHitAction::Forward(action));
    match hit {
        HTCAPTION => forward(GuiWindowAction::Move),
        HTMINBUTTON => forward(GuiWindowAction::Minimize),
        HTMAXBUTTON => Some(NonClientHitAction::MaximizeOrRestore),
        HTCLOSE => forward(GuiWindowAction::Close),
        HTTOPLEFT => forward(GuiWindowAction::ResizeTopLeft),
        HTTOP => forward(GuiWindowAction::ResizeTop),
        HTTOPRIGHT => forward(GuiWindowAction::ResizeTopRight),
        HTRIGHT => forward(GuiWindowAction::ResizeRight),
        HTBOTTOMRIGHT => forward(GuiWindowAction::ResizeBottomRight),
        HTBOTTOM => forward(GuiWindowAction::ResizeBottom),
        HTBOTTOMLEFT => forward(GuiWindowAction::ResizeBottomLeft),
        HTLEFT => forward(GuiWindowAction::ResizeLeft),
        _ => None,
    }
}

pub(crate) fn maximize_transition(is_zoomed: bool) -> (GuiWindowAction, usize) {
    let (action, command) = if is_zoomed {
        (GuiWindowAction::Restore, SC_RESTORE)
    } else {
        (GuiWindowAction::Maximize, SC_MAXIMIZE)
    };
    (
        action,
        usize::try_from(command).expect("system command fits usize"),
    )
}

pub(crate) fn point_lparam(x: i32, y: i32) -> Option<isize> {
    let x = i16::try_from(x).ok()?;
    let y = i16::try_from(y).ok()?;
    let x = u32::from(u16::from_ne_bytes(x.to_ne_bytes()));
    let y = u32::from(u16::from_ne_bytes(y.to_ne_bytes()));
    Some(isize::try_from(x | (y << 16)).expect("packed screen coordinates fit isize"))
}

pub(crate) fn pointer_button_index(button: GuiPointerButton) -> usize {
    match button {
        GuiPointerButton::Left => 0,
        GuiPointerButton::Middle => 1,
        GuiPointerButton::Right => 2,
        GuiPointerButton::Back => 3,
        GuiPointerButton::Forward => 4,
    }
}

pub(crate) fn pointer_button_virtual_key(button: GuiPointerButton) -> u16 {
    match button {
        GuiPointerButton::Left => VK_LBUTTON.0,
        GuiPointerButton::Right => VK_RBUTTON.0,
        GuiPointerButton::Middle => VK_MBUTTON.0,
        GuiPointerButton::Back => VK_XBUTTON1.0,
        GuiPointerButton::Forward => VK_XBUTTON2.0,
    }
}

pub(crate) fn wait_for_virtual_key_releases(virtual_keys: &[u16]) -> windows::core::Result<()> {
    if virtual_keys.is_empty() {
        return Ok(());
    }
    let deadline = Instant::now() + INPUT_RELEASE_SETTLE_TIMEOUT;
    let mut released_since = None;
    loop {
        let now = Instant::now();
        let all_released = virtual_keys.iter().all(|virtual_key| {
            // SAFETY: GetAsyncKeyState accepts any virtual-key value. Only the
            // documented high bit is observed, and no borrowed state is retained.
            let state = unsafe { GetAsyncKeyState(i32::from(*virtual_key)) } as u16;
            state & 0x8000 == 0
        });
        if release_dispatch_grace_complete(&mut released_since, now, all_released) {
            return Ok(());
        }
        if now >= deadline {
            return Err(Error::new(
                HRESULT(0x8007_05B4_u32 as i32),
                "Windows did not settle an injected GUI input release",
            ));
        }
        thread::sleep(INPUT_RELEASE_SETTLE_INTERVAL);
    }
}

pub(crate) fn release_dispatch_grace_complete(
    released_since: &mut Option<Instant>,
    now: Instant,
    all_released: bool,
) -> bool {
    if !all_released {
        *released_since = None;
        return false;
    }
    let since = *released_since.get_or_insert(now);
    // GetAsyncKeyState can clear before the target GUI thread dequeues the
    // corresponding WM_KEYUP/WM_*BUTTONUP. Require a short stable released
    // interval so a directly posted WM_CLOSE cannot overtake those messages.
    now.saturating_duration_since(since) >= INPUT_RELEASE_DISPATCH_GRACE
}

pub(crate) fn window_owner_matches(hwnd: HWND, expected_pid: u32) -> bool {
    owner_pid_matches(expected_pid, observed_window_owner_pid(hwnd))
}

pub(crate) fn owner_pid_matches(expected_pid: u32, observed_pid: Option<u32>) -> bool {
    expected_pid != 0 && observed_pid == Some(expected_pid)
}

pub(crate) fn window_is_owned_by(candidate: HWND, expected_owner: HWND, expected_pid: u32) -> bool {
    let mut current = candidate;
    for _ in 0..MAX_WINDOW_OWNER_DEPTH {
        // SAFETY: GetWindow accepts a live or stale numeric HWND and returns
        // null when no owner exists. The result is revalidated by exact PID
        // before another link in the bounded chain is followed.
        let owner = unsafe { GetWindow(current, GW_OWNER) };
        if owner.0 == 0 || owner == current {
            return false;
        }
        if owner == expected_owner {
            return window_owner_matches(owner, expected_pid);
        }
        if !window_owner_matches(owner, expected_pid) {
            return false;
        }
        current = owner;
    }
    false
}

pub(crate) fn observed_window_owner_pid(hwnd: HWND) -> Option<u32> {
    // SAFETY: both functions accept a possibly stale numeric HWND. A zero
    // thread or process identifier is treated as an invalid/reused target.
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return None;
        }
        let mut process_id = 0_u32;
        let thread_id = GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        (thread_id != 0 && process_id != 0).then_some(process_id)
    }
}

pub(crate) fn window_from_point(x: i32, y: i32) -> HWND {
    // SAFETY: POINT contains ordinary screen coordinates and WindowFromPoint
    // returns either a live HWND or the documented null handle.
    unsafe { WindowFromPoint(POINT { x, y }) }
}

pub(crate) fn window_belongs_to_capture(
    hwnd: HWND,
    expected_pid: u32,
    candidate: HWND,
) -> windows::core::Result<bool> {
    if candidate.0 == 0 {
        return Ok(false);
    }
    let same_pinned_process = owner_pid_matches(expected_pid, observed_window_owner_pid(candidate));
    if !same_pinned_process {
        return Ok(false);
    }
    if candidate == hwnd {
        return Ok(true);
    }
    // SAFETY: IsChild accepts stale handles and returns false. Child controls
    // remain eligible after the same-pinned-PID check above.
    if unsafe { IsChild(hwnd, candidate).as_bool() } {
        return Ok(true);
    }

    // WindowFromPoint returns the deepest child control, so resolve that child
    // to its top-level root before checking an owned modal. Querying GW_OWNER
    // directly on a Button/Edit child returns no owner and used to discard
    // clicks even though the visible dialog itself was identity-pinned.
    // Cross-process secure/elevated surfaces and unrelated same-process
    // document windows remain excluded by the exact PID, owner chain, and
    // visible-frame intersection checks.
    // SAFETY: GetAncestor accepts a live child or top-level HWND and retains no
    // handle. A null or PID-mismatched root fails closed below.
    let candidate_root = unsafe { GetAncestor(candidate, GA_ROOT) };
    if candidate_root.0 == 0 || !window_owner_matches(candidate_root, expected_pid) {
        return Ok(false);
    }
    if candidate_root == hwnd {
        return Ok(true);
    }
    let owned_by_main = window_is_owned_by(candidate_root, hwnd, expected_pid);
    let main_bounds = window_frame_rect(hwnd)?;
    let candidate_bounds = window_frame_rect(candidate_root)?;
    Ok(secondary_window_input_eligible(
        same_pinned_process,
        owned_by_main,
        main_bounds,
        candidate_bounds,
    ))
}

pub(crate) fn is_main_window_target(hwnd: HWND, candidate: HWND) -> bool {
    // SAFETY: GetAncestor accepts a possibly stale handle and returns null if
    // it cannot resolve the target. Owned popup roots remain distinct here.
    candidate == hwnd || unsafe { GetAncestor(candidate, GA_ROOT) == hwnd }
}

pub(crate) fn foreground_belongs_to_capture(
    hwnd: HWND,
    expected_pid: u32,
) -> windows::core::Result<bool> {
    // SAFETY: GetForegroundWindow returns either a live top-level HWND or null.
    window_belongs_to_capture(hwnd, expected_pid, unsafe { GetForegroundWindow() })
}

pub(crate) fn pointer_target_is_valid(
    hwnd: HWND,
    expected_pid: u32,
    x: i32,
    y: i32,
) -> windows::core::Result<bool> {
    Ok(foreground_belongs_to_capture(hwnd, expected_pid)?
        && pointer_target_belongs_to_capture(hwnd, expected_pid, x, y)?)
}

pub(crate) fn pointer_target_belongs_to_capture(
    hwnd: HWND,
    expected_pid: u32,
    x: i32,
    y: i32,
) -> windows::core::Result<bool> {
    window_belongs_to_capture(hwnd, expected_pid, window_from_point(x, y))
}

pub(crate) fn activate_and_verify(hwnd: HWND, expected_pid: u32) -> windows::core::Result<bool> {
    // SAFETY: the HWND came from EnumWindows. These APIs validate stale HWNDs;
    // foreground restrictions are represented by false return values.
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return Err(Error::new(
                INVALID_ARGUMENT,
                "captured GUI window is no longer available",
            ));
        }
        if foreground_belongs_to_capture(hwnd, expected_pid)? {
            return Ok(true);
        }
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if foreground_belongs_to_capture(hwnd, expected_pid)? {
            return Ok(true);
        }

        // A host-side focus transition arrives through the authenticated GUI
        // channel, not through Windows' local input queue, so foreground-lock
        // policy can reject the ordinary SetForegroundWindow call. Temporarily
        // join the companion, exact target, and exact foreground input queues,
        // establish active/focus state for that one identity-pinned HWND, then
        // detach before returning. Joining only the foreground queue is not
        // sufficient when Start/Search owns foreground and the target belongs
        // to a third UI thread; that leaves seamless input permanently blocked.
        activate_via_joined_input_queues(hwnd);
        if foreground_belongs_to_capture(hwnd, expected_pid)? {
            return Ok(true);
        }

        // Foreground-lock can still reject an attached-queue transition when
        // a shell surface such as Windows Search owns the latest input. A
        // balanced synthetic Alt pulse is the documented user-input signal
        // used by desktop applications to make the immediately following
        // SetForegroundWindow eligible. Always submit the release even if the
        // press reports an error so this recovery path cannot strand Alt down.
        send_balanced_key_tap(VK_MENU.0)?;
        let _ = BringWindowToTop(hwnd);
        let _ = SetActiveWindow(hwnd);
        let _ = SetFocus(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if foreground_belongs_to_capture(hwnd, expected_pid)? {
            return Ok(true);
        }

        let foreground = GetForegroundWindow();
        if !dismissible_windows_shell_surface(foreground) {
            return Ok(false);
        }
        // Start and Search are transient shell-owned surfaces that Windows
        // normally dismisses when the user selects another application. The
        // remote host click cannot reach the covered guest window, so send one
        // balanced Escape only for these exact Microsoft package identities,
        // then retry the identity-pinned activation for a bounded interval.
        send_balanced_key_tap(VK_ESCAPE.0)?;
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(10));
            activate_via_joined_input_queues(hwnd);
            if foreground_belongs_to_capture(hwnd, expected_pid)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub(crate) fn activate_via_joined_input_queues(hwnd: HWND) {
    // SAFETY: hwnd is identity-pinned by the caller. All numeric thread IDs
    // come from live HWNDs. Attachments are recorded independently and always
    // detached in reverse order before returning.
    unsafe {
        let foreground = GetForegroundWindow();
        let current_thread = GetCurrentThreadId();
        // AttachThreadInput fails when either thread has no message queue. GUI
        // input is handled by a bounded network worker rather than the capture
        // process's main loop, so create that worker's queue before attaching.
        let mut ignored_message = MSG::default();
        let _ = PeekMessageW(&mut ignored_message, HWND(0), 0, 0, PM_NOREMOVE);
        let mut ignored_foreground_pid = 0_u32;
        let foreground_thread = if foreground.0 == 0 {
            0
        } else {
            GetWindowThreadProcessId(foreground, Some(&mut ignored_foreground_pid))
        };
        let mut ignored_target_pid = 0_u32;
        let target_thread = GetWindowThreadProcessId(hwnd, Some(&mut ignored_target_pid));
        let attached_foreground = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(current_thread, foreground_thread, true).as_bool();
        let attached_target = target_thread != 0
            && target_thread != current_thread
            && target_thread != foreground_thread
            && AttachThreadInput(current_thread, target_thread, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetActiveWindow(hwnd);
        let _ = SetFocus(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached_target {
            let _ = AttachThreadInput(current_thread, target_thread, false);
        }
        if attached_foreground {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }
    }
}

pub(crate) fn dismissible_windows_shell_surface(hwnd: HWND) -> bool {
    let Some(pid) = observed_window_owner_pid(hwnd) else {
        return false;
    };
    let Ok(process) = OwnedProcess::open(pid) else {
        return false;
    };
    let Ok(image_path) = process_image_path(process.handle) else {
        return false;
    };
    let Ok(Some(package_family)) = process_package_family_name(process.handle) else {
        return false;
    };
    dismissible_windows_shell_identity(&image_path, &package_family)
}

pub(crate) fn dismissible_windows_shell_identity(image_path: &str, package_family: &str) -> bool {
    let Some(image_name) = Path::new(image_path)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    (image_name.eq_ignore_ascii_case("SearchHost.exe")
        && package_family.eq_ignore_ascii_case("MicrosoftWindows.Client.CBS_cw5n1h2txyewy"))
        || (image_name.eq_ignore_ascii_case("StartMenuExperienceHost.exe")
            && package_family
                .eq_ignore_ascii_case("Microsoft.Windows.StartMenuExperienceHost_cw5n1h2txyewy"))
}

pub(crate) fn send_balanced_key_tap(virtual_key: u16) -> windows::core::Result<()> {
    let pressed = send_keyboard(virtual_key, 0, true, false);
    let released = send_keyboard(virtual_key, 0, false, false);
    pressed?;
    released
}

pub(crate) fn secondary_window_input_eligible(
    same_pinned_process: bool,
    owned_by_main: bool,
    main_bounds: RECT,
    candidate_bounds: RECT,
) -> bool {
    same_pinned_process && owned_by_main && rectangles_intersect(main_bounds, candidate_bounds)
}

pub(crate) fn rectangles_intersect(left: RECT, right: RECT) -> bool {
    left.left < left.right
        && left.top < left.bottom
        && right.left < right.right
        && right.top < right.bottom
        && left.left < right.right
        && right.left < left.right
        && left.top < right.bottom
        && right.top < left.bottom
}

pub(crate) fn send_keyboard(
    virtual_key: u16,
    scan_code: u16,
    pressed: bool,
    extended: bool,
) -> windows::core::Result<()> {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    send_native_input(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(virtual_key),
                wScan: scan_code,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    })
}

pub(crate) fn send_mouse(flags: MOUSE_EVENT_FLAGS, data: u32) -> windows::core::Result<()> {
    send_native_input(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    })
}

pub(crate) fn send_pointer_down_at(
    screen_x: i32,
    screen_y: i32,
    button: MOUSE_EVENT_FLAGS,
    data: u32,
) -> windows::core::Result<()> {
    let (absolute_x, absolute_y) = absolute_virtual_desktop_point(screen_x, screen_y)?;
    // Submit the exact absolute pointer move and its button-down edge in one
    // ordered SendInput batch. This minimizes the global-cursor race between
    // the final capture-owned WindowFromPoint check and Windows dispatch.
    send_native_inputs(&[
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: absolute_x,
                    dy: absolute_y,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE
                        | MOUSEEVENTF_MOVE_NOCOALESCE
                        | MOUSEEVENTF_ABSOLUTE
                        | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data,
                    dwFlags: button,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ])
}

pub(crate) fn absolute_virtual_desktop_point(x: i32, y: i32) -> windows::core::Result<(i32, i32)> {
    // SAFETY: GetSystemMetrics returns scalar bounds for the caller's current
    // interactive desktop and retains no pointers.
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 1 || height <= 1 {
        return Err(Error::new(
            INVALID_ARGUMENT,
            "Windows virtual desktop has invalid dimensions",
        ));
    }
    let absolute_x = normalize_absolute_axis(x, left, width);
    let absolute_y = normalize_absolute_axis(y, top, height);
    match (absolute_x, absolute_y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(Error::new(
            INVALID_ARGUMENT,
            "captured GUI pointer coordinate is outside the Windows virtual desktop",
        )),
    }
}

pub(crate) fn normalize_absolute_axis(coordinate: i32, origin: i32, extent: i32) -> Option<i32> {
    if extent <= 1 {
        return None;
    }
    let relative = i64::from(coordinate) - i64::from(origin);
    if relative < 0 || relative >= i64::from(extent) {
        return None;
    }
    i32::try_from((relative * 65_535 + i64::from(extent - 1) / 2) / i64::from(extent - 1)).ok()
}

pub(crate) fn send_native_input(input: INPUT) -> windows::core::Result<()> {
    send_native_inputs(&[input])
}

pub(crate) fn send_native_inputs(inputs: &[INPUT]) -> windows::core::Result<()> {
    // SAFETY: every INPUT is fully initialized and the slice remains valid for the
    // synchronous SendInput call.
    let sent = unsafe {
        SendInput(
            inputs,
            i32::try_from(std::mem::size_of::<INPUT>()).expect("INPUT size fits i32"),
        )
    };
    if usize::try_from(sent).ok() == Some(inputs.len()) {
        Ok(())
    } else {
        Err(Error::from_win32())
    }
}
