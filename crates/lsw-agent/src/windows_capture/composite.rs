// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[derive(Clone, Copy)]
pub(crate) struct CompositeSecondaryWindow {
    pub(crate) hwnd: HWND,
    pub(crate) bounds: RECT,
}

pub(crate) struct CompositeSecondaryEnumeration {
    main: HWND,
    process_id: u32,
    windows: Vec<isize>,
}

pub(crate) fn composite_secondary_window_eligible(window: &WindowHandle, hwnd: HWND) -> bool {
    if hwnd == window.hwnd || !window_owner_matches(hwnd, window.owner.pid) {
        return false;
    }
    // SAFETY: these queries accept a possibly stale numeric HWND and retain no
    // handle. Any failed/stale observation returns zero/false and is rejected.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool()
            || GetAncestor(hwnd, GA_ROOT) != hwnd
            || !window_is_owned_by(hwnd, window.hwnd, window.owner.pid)
        {
            return false;
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let extended_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        // Windows 11 IncludeSecondaryWindows already composes these classes
        // into the main texture. Capture only ordinary owned top-level dialogs
        // here so menus are not duplicated and resource use remains bounded.
        style & WS_POPUP.0 == 0 && extended_style & WS_EX_TOOLWINDOW.0 == 0
    }
}

unsafe extern "system" fn enumerate_composite_secondary_window(
    hwnd: HWND,
    parameter: LPARAM,
) -> BOOL {
    // SAFETY: composite_secondary_windows passes this exact pointer and
    // EnumWindows invokes callbacks synchronously before the stack value drops.
    let enumeration = unsafe { &mut *(parameter.0 as *mut CompositeSecondaryEnumeration) };
    // SAFETY: every API accepts an HWND supplied by EnumWindows and returns
    // only scalar state. A disappearing window simply fails these predicates.
    unsafe {
        if hwnd == enumeration.main || !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut process_id = 0_u32;
        if GetWindowThreadProcessId(hwnd, Some(&mut process_id)) == 0
            || process_id != enumeration.process_id
        {
            return BOOL(1);
        }
        if GetAncestor(hwnd, GA_ROOT) != hwnd
            || !window_is_owned_by(hwnd, enumeration.main, enumeration.process_id)
        {
            return BOOL(1);
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let extended_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        // Windows 11 IncludeSecondaryWindows already composes these classes
        // into the main texture. Capture only ordinary owned top-level dialogs
        // here so menus are not duplicated and resource use remains bounded.
        if style & WS_POPUP.0 != 0 || extended_style & WS_EX_TOOLWINDOW.0 != 0 {
            return BOOL(1);
        }
        enumeration.windows.push(hwnd.0);
    }
    BOOL(1)
}

pub(crate) fn composite_secondary_windows(
    window: &WindowHandle,
) -> Result<Vec<CompositeSecondaryWindow>, Box<dyn std::error::Error>> {
    window.validate_identity()?;
    let main_bounds = window_frame_rect(window.hwnd)?;
    let mut enumeration = CompositeSecondaryEnumeration {
        main: window.hwnd,
        process_id: window.owner.pid,
        windows: Vec::new(),
    };
    // SAFETY: the callback receives a pointer to live stack storage only for
    // this synchronous enumeration and stores numeric HWND values, not refs.
    unsafe {
        EnumWindows(
            Some(enumerate_composite_secondary_window),
            LPARAM((&mut enumeration as *mut CompositeSecondaryEnumeration) as isize),
        )?;
    }
    window.validate_identity()?;
    let mut windows = Vec::new();
    for value in enumeration.windows {
        let hwnd = HWND(value);
        if !composite_secondary_window_eligible(window, hwnd) {
            continue;
        }
        let bounds = match window_frame_rect(hwnd) {
            Ok(bounds) => bounds,
            Err(error) => {
                // An owned dialog can close between EnumWindows and the DWM
                // geometry query. Treat that ordinary race as removal, while
                // preserving a real geometry failure for a still-live target.
                if !composite_secondary_window_eligible(window, hwnd) {
                    continue;
                }
                return Err(error.into());
            }
        };
        if !composite_secondary_window_eligible(window, hwnd) {
            continue;
        }
        if rectangles_intersect(main_bounds, bounds) {
            windows.push(CompositeSecondaryWindow { hwnd, bounds });
        }
    }
    if windows.len() > MAX_COMPOSITE_SECONDARY_WINDOWS {
        return Err(format!(
            "captured GUI owns {} ordinary dialog windows; at most {} are supported",
            windows.len(),
            MAX_COMPOSITE_SECONDARY_WINDOWS
        )
        .into());
    }
    Ok(windows)
}

pub(crate) fn composite_secondary_window(
    window: &WindowHandle,
    hwnd: HWND,
) -> Result<Option<CompositeSecondaryWindow>, Box<dyn std::error::Error>> {
    Ok(composite_secondary_windows(window)?
        .into_iter()
        .find(|candidate| candidate.hwnd == hwnd))
}

pub(crate) fn validate_capture_target(
    window: &WindowHandle,
    hwnd: HWND,
) -> Result<(), Box<dyn std::error::Error>> {
    window.validate_identity()?;
    if hwnd != window.hwnd && composite_secondary_window(window, hwnd)?.is_none() {
        return Err("owned dialog capture target is no longer eligible".into());
    }
    window.validate_identity()?;
    Ok(())
}

pub(crate) fn rectangles_equal(left: RECT, right: RECT) -> bool {
    left.left == right.left
        && left.top == right.top
        && left.right == right.right
        && left.bottom == right.bottom
}

pub(crate) fn window_order_changed(previous: &[isize], current: &[isize]) -> bool {
    previous != current
}
