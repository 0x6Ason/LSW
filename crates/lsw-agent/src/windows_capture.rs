// SPDX-License-Identifier: GPL-3.0-or-later

//! Documented Windows Graphics Capture and HWND input bridge for Slice 4.

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    GuiInputEvent, GuiPointerButton, GuiWindowAction, GuiWindowDragHint, GuiWindowReady,
    GuiWindowResize, MAX_GUI_FRAME_BYTES, MAX_GUI_WINDOW_DIMENSION,
};
use windows::core::{factory, Error, Interface, HRESULT, HSTRING, PCWSTR, PWSTR};
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Management::Deployment::PackageManager;
use windows::Win32::Foundation::{
    CloseHandle, APPMODEL_ERROR_NO_APPLICATION, APPMODEL_ERROR_NO_PACKAGE, BOOL,
    ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, ERROR_SUCCESS, FILETIME, HANDLE, HWND, LPARAM,
    POINT, RECT, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Storage::Packaging::Appx::{
    GetApplicationUserModelId, GetPackageFamilyName, GetPackageFullName, GetPackagePathByFullName,
    APPLICATION_USER_MODEL_ID_MAX_LENGTH, PACKAGE_FAMILY_NAME_MAX_LENGTH,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::Win32::System::{
    RemoteDesktop::ProcessIdToSessionId,
    Threading::{
        AttachThreadInput, GetCurrentThreadId, GetProcessTimes, OpenProcess,
        QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    },
};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, GetDpiForWindow, GetSystemMetricsForDpi,
    GetThreadDpiAwarenessContext, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, SetActiveWindow, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD,
    INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_MOVE_NOCOALESCE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_ESCAPE,
    VK_LBUTTON, VK_MBUTTON, VK_MENU, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::Shell::{
    ApplicationActivationManager, IApplicationActivationManager, AO_NOERRORUI,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetAncestor, GetForegroundWindow, GetSystemMetrics, GetWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsChild, IsWindow, IsWindowVisible, IsZoomed, PeekMessageW,
    PostMessageW, SendMessageTimeoutW, SetCursorPos, SetForegroundWindow, SetWindowPos,
    WindowFromPoint, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, GW_OWNER, HTBOTTOM, HTBOTTOMLEFT,
    HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTCLOSE, HTLEFT, HTMAXBUTTON, HTMINBUTTON, HTRIGHT, HTTOP,
    HTTOPLEFT, HTTOPRIGHT, MSG, PM_NOREMOVE, SC_MAXIMIZE, SC_RESTORE, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SM_CXVIRTUALSCREEN, SM_CYSIZEFRAME, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, WM_CLOSE, WM_NCHITTEST,
    WM_SYSCOMMAND, WS_EX_TOOLWINDOW, WS_POPUP, WS_THICKFRAME,
};
use xml::reader::{ParserConfig, XmlEvent};

const WINDOW_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const WINDOW_DISCOVERY_INTERVAL: Duration = Duration::from_millis(25);
const CAPTURE_ITEM_BIND_TIMEOUT: Duration = Duration::from_secs(2);
const FRAME_POOL_BUFFERS: i32 = 2;
const MAX_WINDOW_TITLE_UNITS: usize = 4096;
const HIT_TEST_TIMEOUT_MS: u32 = 150;
const INPUT_RELEASE_SETTLE_TIMEOUT: Duration = Duration::from_millis(500);
const INPUT_RELEASE_SETTLE_INTERVAL: Duration = Duration::from_millis(5);
const INPUT_RELEASE_DISPATCH_GRACE: Duration = Duration::from_millis(100);
const INPUT_RELEASE_TRACKING_WINDOW: Duration = Duration::from_secs(2);
const MAX_CAPTURE_BOUNDS_DELTA: i64 = 64;
const MAX_PROCESS_IMAGE_UNITS: usize = 32_768;
const MAX_PACKAGE_NAME_UNITS: usize = 4096;
const MAX_PACKAGE_PATH_UNITS: usize = 32_768;
const MAX_PACKAGE_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PACKAGE_MANIFEST_EVENTS: usize = 100_000;
const MAX_PACKAGE_MANIFEST_DEPTH: usize = 256;
const MAX_CURRENT_USER_PACKAGES: usize = 4096;
const MAX_PROCESS_SNAPSHOT_ENTRIES: usize = 131_072;
const MAX_COMPOSITE_SECONDARY_WINDOWS: usize = 8;
const MAX_WINDOW_OWNER_DEPTH: usize = MAX_COMPOSITE_SECONDARY_WINDOWS + 1;
const MIN_SYNTHETIC_RESIZE_GRAB_PIXELS: i32 = 4;
const MAX_SYNTHETIC_RESIZE_GRAB_PIXELS: i32 = 32;
const FOUNDATION_MANIFEST_NAMESPACE: &str =
    "http://schemas.microsoft.com/appx/manifest/foundation/windows10";
const UAP3_MANIFEST_NAMESPACE: &str = "http://schemas.microsoft.com/appx/manifest/uap/windows10/3";
const UAP5_MANIFEST_NAMESPACE: &str = "http://schemas.microsoft.com/appx/manifest/uap/windows10/5";
const UAP8_MANIFEST_NAMESPACE: &str = "http://schemas.microsoft.com/appx/manifest/uap/windows10/8";
const DESKTOP_MANIFEST_NAMESPACE: &str =
    "http://schemas.microsoft.com/appx/manifest/desktop/windows10";
const INVALID_ARGUMENT: HRESULT = HRESULT(0x8007_0057u32 as i32);

/// Keeps HWND geometry, WGC/DWM physical pixels, and injected screen
/// coordinates in one per-monitor-aware coordinate space on this thread.
/// The GUI child is spawned before this guard is entered so its inherited DPI
/// policy remains entirely application-controlled.
#[must_use]
pub(super) struct ThreadDpiAwareness {
    previous: DPI_AWARENESS_CONTEXT,
    _not_send: std::marker::PhantomData<Rc<()>>,
}

impl ThreadDpiAwareness {
    pub(super) fn per_monitor_v2() -> windows::core::Result<Self> {
        // SAFETY: SetThreadDpiAwarenessContext affects only the calling thread
        // and returns the exact context token needed for restoration.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.0 == 0 {
            return Err(Error::from_win32());
        }
        // SAFETY: both calls inspect only the calling thread and compare opaque
        // DPI context tokens documented for this purpose.
        let active = unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
            .as_bool()
        };
        if !active {
            // SAFETY: previous was returned by the successful set above.
            let _ = unsafe { SetThreadDpiAwarenessContext(previous) };
            return Err(Error::new(
                INVALID_ARGUMENT,
                "could not enter per-monitor-v2 DPI awareness for GUI capture",
            ));
        }
        Ok(Self {
            previous,
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for ThreadDpiAwareness {
    fn drop(&mut self) {
        // SAFETY: previous is an opaque context returned by the successful
        // SetThreadDpiAwarenessContext call on this same thread. The guard is
        // deliberately not Send, because DPI awareness is thread-scoped.
        let _ = unsafe { SetThreadDpiAwarenessContext(self.previous) };
    }
}

// Windows 11 24H2 added this interface after the metadata used by the pinned
// windows-rs release. Keeping the one-method ABI declaration here avoids a
// workspace-wide bindings upgrade while still using the documented WinRT API.
windows::core::imp::com_interface!(
    IGraphicsCaptureSession6,
    IGraphicsCaptureSession6_Vtbl,
    0xd7419236_be20_5e9f_bcd6_c4e98fd6afdc
);

#[repr(C)]
pub struct IGraphicsCaptureSession6_Vtbl {
    base__: windows::core::IInspectable_Vtbl,
    include_secondary_windows:
        unsafe extern "system" fn(*mut std::ffi::c_void, *mut bool) -> HRESULT,
    set_include_secondary_windows:
        unsafe extern "system" fn(*mut std::ffi::c_void, bool) -> HRESULT,
}

pub(super) struct CapturedFrame {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bgra: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InjectedKey {
    virtual_key: u16,
    scan_code: u16,
    extended: bool,
}

#[derive(Clone, Copy, Debug)]
struct ReleasedVirtualKey {
    virtual_key: u16,
    released_at: Instant,
}

#[derive(Default)]
struct InjectedInputState {
    keys: Vec<InjectedKey>,
    buttons: [bool; 5],
    recent_releases: Vec<ReleasedVirtualKey>,
}

impl InjectedInputState {
    fn note_press(&mut self, virtual_key: u16) {
        self.recent_releases
            .retain(|release| release.virtual_key != virtual_key);
    }

    fn note_release(&mut self, virtual_key: u16) {
        if virtual_key == 0 {
            return;
        }
        self.note_press(virtual_key);
        self.recent_releases.push(ReleasedVirtualKey {
            virtual_key,
            released_at: Instant::now(),
        });
    }

    fn recent_release_keys(&mut self) -> Vec<u16> {
        let now = Instant::now();
        self.recent_releases.retain(|release| {
            now.saturating_duration_since(release.released_at) <= INPUT_RELEASE_TRACKING_WINDOW
        });
        self.recent_releases
            .iter()
            .map(|release| release.virtual_key)
            .collect()
    }
}

pub(super) struct WindowHandle {
    hwnd: HWND,
    owner: OwnedProcess,
    capture_size: (u32, u32),
    injected: InjectedInputState,
}

pub(super) enum GuiInputOutcome {
    None,
    Action(GuiWindowAction),
    DragHint(GuiWindowDragHint),
}

struct OwnedProcess {
    handle: HANDLE,
    pid: u32,
    creation_time: u64,
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // SAFETY: this value exclusively owns the process handle returned by
        // OpenProcess, and no operation retains it after this drop.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

impl WindowHandle {
    pub(super) fn process_id(&self) -> u32 {
        self.owner.pid
    }

    pub(super) fn id(&self) -> u64 {
        u64::try_from(self.hwnd.0 as usize).expect("HWND fits u64")
    }

    pub(super) fn ready(
        &self,
        process_id: u32,
        width: u32,
        height: u32,
    ) -> Result<GuiWindowReady, Box<dyn std::error::Error>> {
        self.validate_identity()?;
        Ok(GuiWindowReady {
            process_id,
            window_id: self.id(),
            width,
            height,
            title: window_title(self.hwnd)?,
        })
    }

    pub(super) fn is_open(&self) -> bool {
        self.validate_identity().is_ok()
    }

    pub(super) fn validate_identity(&self) -> windows::core::Result<()> {
        self.validate_owner_identity()?;
        if !window_owner_matches(self.hwnd, self.owner.pid) {
            return Err(Error::new(
                INVALID_ARGUMENT,
                "captured GUI HWND no longer belongs to its selected process",
            ));
        }
        Ok(())
    }

    fn validate_owner_identity(&self) -> windows::core::Result<()> {
        // SAFETY: the owned process handle includes synchronization and limited
        // query rights. A non-timeout result means the pinned process object is
        // no longer a live capture owner.
        if unsafe { WaitForSingleObject(self.owner.handle, 0) } != WAIT_TIMEOUT
            || process_creation_time(self.owner.handle).ok() != Some(self.owner.creation_time)
        {
            return Err(Error::new(
                INVALID_ARGUMENT,
                "captured GUI process is no longer running",
            ));
        }
        Ok(())
    }

    pub(super) fn set_capture_size(&mut self, width: u32, height: u32) {
        self.capture_size = (width, height);
    }

    pub(super) fn input(&mut self, event: GuiInputEvent) -> windows::core::Result<GuiInputOutcome> {
        match event {
            GuiInputEvent::Focus { focused } => {
                if focused {
                    self.validate_identity()?;
                    let _ = activate_and_verify(self.hwnd, self.owner.pid)?;
                    self.validate_identity()?;
                } else {
                    self.release_injected_input()?;
                }
                Ok(GuiInputOutcome::None)
            }
            GuiInputEvent::Key {
                virtual_key,
                scan_code,
                pressed,
                extended,
            } => {
                let key = InjectedKey {
                    virtual_key,
                    scan_code,
                    extended,
                };
                if pressed {
                    self.validate_identity()?;
                    if !activate_and_verify(self.hwnd, self.owner.pid)? {
                        return Ok(GuiInputOutcome::None);
                    }
                    self.validate_identity()?;
                    if !foreground_belongs_to_capture(self.hwnd, self.owner.pid)? {
                        return Ok(GuiInputOutcome::None);
                    }
                    send_keyboard(virtual_key, scan_code, true, extended)?;
                    self.injected.note_press(virtual_key);
                    if !self.injected.keys.contains(&key) {
                        self.injected.keys.push(key);
                    }
                } else if let Some(index) = self
                    .injected
                    .keys
                    .iter()
                    .position(|candidate| *candidate == key)
                {
                    send_keyboard(virtual_key, scan_code, false, extended)?;
                    self.injected.keys.remove(index);
                    self.injected.note_release(virtual_key);
                }
                Ok(GuiInputOutcome::None)
            }
            GuiInputEvent::PointerMove { x, y } => {
                let action = match self.place_pointer_on_target(x, y)? {
                    Some((screen_x, screen_y, target))
                        if is_main_window_target(self.hwnd, target)
                            && foreground_belongs_to_capture(self.hwnd, self.owner.pid)?
                            && pointer_target_is_valid(
                                self.hwnd,
                                self.owner.pid,
                                screen_x,
                                screen_y,
                            )? =>
                    {
                        self.validate_identity()?;
                        match non_client_action(self.hwnd, screen_x, screen_y) {
                            Some(NonClientHitAction::Forward(action)) if action.is_drag() => {
                                Some(action)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                Ok(GuiInputOutcome::DragHint(GuiWindowDragHint {
                    x,
                    y,
                    action,
                }))
            }
            GuiInputEvent::PointerButton {
                button,
                pressed,
                x,
                y,
            } => {
                let button_index = pointer_button_index(button);
                let (down, up, data) = match button {
                    GuiPointerButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
                    GuiPointerButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
                    GuiPointerButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
                    GuiPointerButton::Back => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1),
                    GuiPointerButton::Forward => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2),
                };
                if pressed {
                    let Some((screen_x, screen_y, target)) = self.place_pointer_on_target(x, y)?
                    else {
                        return Ok(GuiInputOutcome::None);
                    };
                    // An inactive-window click is first allowed to activate
                    // only the exact hovered capture-owned HWND. Do not turn
                    // that first click into Close/Maximize/drag until Windows
                    // already considers this capture foreground.
                    let target_was_foreground =
                        foreground_belongs_to_capture(self.hwnd, self.owner.pid)?;
                    if button == GuiPointerButton::Left
                        && target_was_foreground
                        && is_main_window_target(self.hwnd, target)
                    {
                        if let Some(hit_action) = non_client_action(self.hwnd, screen_x, screen_y) {
                            self.validate_identity()?;
                            if !pointer_target_is_valid(
                                self.hwnd,
                                self.owner.pid,
                                screen_x,
                                screen_y,
                            )? {
                                return Ok(GuiInputOutcome::None);
                            }
                            match hit_action {
                                NonClientHitAction::MaximizeOrRestore => {
                                    // Derive one explicit desired state from
                                    // the live guest HWND, post its matching
                                    // native system command, and then tell the
                                    // presenter to apply that same state.
                                    return Ok(GuiInputOutcome::Action(
                                        self.request_maximize_transition()?,
                                    ));
                                }
                                NonClientHitAction::Forward(GuiWindowAction::Close) => {
                                    // Keep the presenter/capture alive while
                                    // the application handles its native close
                                    // flow. Save confirmations are secondary
                                    // HWNDs and remain visible and interactive
                                    // through WGC.
                                    self.close()?;
                                    return Ok(GuiInputOutcome::None);
                                }
                                NonClientHitAction::Forward(action) => {
                                    return Ok(GuiInputOutcome::Action(action));
                                }
                            }
                        }
                    }
                    if !pointer_target_belongs_to_capture(
                        self.hwnd,
                        self.owner.pid,
                        screen_x,
                        screen_y,
                    )? {
                        return Ok(GuiInputOutcome::None);
                    }
                    self.validate_identity()?;
                    if !pointer_target_belongs_to_capture(
                        self.hwnd,
                        self.owner.pid,
                        screen_x,
                        screen_y,
                    )? {
                        return Ok(GuiInputOutcome::None);
                    }
                    send_pointer_down_at(screen_x, screen_y, down, data)?;
                    self.injected.note_press(pointer_button_virtual_key(button));
                    self.injected.buttons[button_index] = true;
                    let accepted = self.validate_identity().and_then(|()| {
                        Ok(foreground_belongs_to_capture(self.hwnd, self.owner.pid)?
                            && pointer_target_belongs_to_capture(
                                self.hwnd,
                                self.owner.pid,
                                screen_x,
                                screen_y,
                            )?)
                    });
                    match accepted {
                        Ok(true) => {}
                        Ok(false) => {
                            send_mouse(up, data)?;
                            self.injected.buttons[button_index] = false;
                            self.injected
                                .note_release(pointer_button_virtual_key(button));
                            return Err(Error::new(
                                INVALID_ARGUMENT,
                                "Windows rejected focus for the exact captured GUI pointer target",
                            ));
                        }
                        Err(error) => {
                            if send_mouse(up, data).is_ok() {
                                self.injected.buttons[button_index] = false;
                                self.injected
                                    .note_release(pointer_button_virtual_key(button));
                            }
                            return Err(error);
                        }
                    }
                } else if self.injected.buttons[button_index] {
                    if let Ok(Some(_)) = self.place_pointer_on_target(x, y) {
                        // The pointer was positioned at the host-reported release
                        // coordinate. If validation fails, releasing without moving
                        // still clears the guest's global injected button state.
                    }
                    send_mouse(up, data)?;
                    self.injected.buttons[button_index] = false;
                    self.injected
                        .note_release(pointer_button_virtual_key(button));
                }
                Ok(GuiInputOutcome::None)
            }
            GuiInputEvent::PointerWheel {
                delta,
                horizontal,
                x,
                y,
            } => {
                let Some((screen_x, screen_y, _)) = self.place_pointer_on_target(x, y)? else {
                    return Ok(GuiInputOutcome::None);
                };
                if !pointer_target_is_valid(self.hwnd, self.owner.pid, screen_x, screen_y)? {
                    return Ok(GuiInputOutcome::None);
                }
                self.validate_identity()?;
                if !pointer_target_is_valid(self.hwnd, self.owner.pid, screen_x, screen_y)? {
                    return Ok(GuiInputOutcome::None);
                }
                send_mouse(
                    if horizontal {
                        MOUSEEVENTF_HWHEEL
                    } else {
                        MOUSEEVENTF_WHEEL
                    },
                    u32::from_ne_bytes(i32::from(delta).to_ne_bytes()),
                )?;
                Ok(GuiInputOutcome::None)
            }
        }
    }

    pub(super) fn resize(&self, resize: GuiWindowResize) -> windows::core::Result<()> {
        self.validate_identity()?;
        let (width, height) = resize_outer_extent(self.hwnd, resize.width, resize.height)?;
        let (x, y) = resize_origin_within_monitor_work_area(self.hwnd, width, height)?;
        // The visible DWM frame and GetWindowRect calls above are separate
        // Win32 observations. Pin the HWND/process identity again immediately
        // before mutating it so a destroyed and reused numeric handle cannot
        // receive the resize.
        self.validate_identity()?;
        // SAFETY: SetWindowPos validates the HWND and receives bounded positive
        // outer dimensions derived from the bounded requested DWM/WGC extent.
        unsafe {
            SetWindowPos(
                self.hwnd,
                HWND(0),
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
    }

    pub(super) fn close(&self) -> windows::core::Result<()> {
        self.validate_identity()?;
        post(self.hwnd, WM_CLOSE, 0, 0)
    }

    fn request_maximize_transition(&self) -> windows::core::Result<GuiWindowAction> {
        self.validate_identity()?;
        // SAFETY: identity validation pins this live HWND to the exact process
        // object. IsZoomed only reads the current native show state.
        let (action, command) = maximize_transition(unsafe { IsZoomed(self.hwnd).as_bool() });
        // Revalidate immediately before posting a process-affecting action so
        // a destroyed/reused numeric HWND cannot receive WM_SYSCOMMAND.
        self.validate_identity()?;
        post(self.hwnd, WM_SYSCOMMAND, command, 0)?;
        Ok(action)
    }

    pub(super) fn is_maximized(&self) -> windows::core::Result<bool> {
        self.validate_identity()?;
        // SAFETY: identity validation pins this live HWND to the exact process
        // object. IsZoomed only reads its current native show state.
        Ok(unsafe { IsZoomed(self.hwnd).as_bool() })
    }

    pub(super) fn set_maximized(&self, maximized: bool) -> windows::core::Result<()> {
        self.validate_identity()?;
        // SAFETY: identity validation pins this HWND to its exact process and
        // IsZoomed only reads the native show state.
        if unsafe { IsZoomed(self.hwnd).as_bool() } == maximized {
            return Ok(());
        }
        self.validate_identity()?;
        post(
            self.hwnd,
            WM_SYSCOMMAND,
            usize::try_from(if maximized { SC_MAXIMIZE } else { SC_RESTORE })
                .expect("system command fits usize"),
            0,
        )
    }

    pub(super) fn release_injected_input(&mut self) -> windows::core::Result<()> {
        let mut first_error = None;
        for key in self.injected.keys.clone().into_iter().rev() {
            match send_keyboard(key.virtual_key, key.scan_code, false, key.extended) {
                Ok(()) => {
                    self.injected.keys.retain(|candidate| *candidate != key);
                    self.injected.note_release(key.virtual_key);
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            };
        }
        for button in [
            GuiPointerButton::Left,
            GuiPointerButton::Middle,
            GuiPointerButton::Right,
            GuiPointerButton::Back,
            GuiPointerButton::Forward,
        ] {
            let index = pointer_button_index(button);
            if !self.injected.buttons[index] {
                continue;
            }
            let (up, data) = match button {
                GuiPointerButton::Left => (MOUSEEVENTF_LEFTUP, 0),
                GuiPointerButton::Right => (MOUSEEVENTF_RIGHTUP, 0),
                GuiPointerButton::Middle => (MOUSEEVENTF_MIDDLEUP, 0),
                GuiPointerButton::Back => (MOUSEEVENTF_XUP, 1),
                GuiPointerButton::Forward => (MOUSEEVENTF_XUP, 2),
            };
            match send_mouse(up, data) {
                Ok(()) => {
                    self.injected.buttons[index] = false;
                    self.injected
                        .note_release(pointer_button_virtual_key(button));
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(super) fn settle_released_input(&mut self) -> windows::core::Result<()> {
        let releases = self.injected.recent_release_keys();
        let result = wait_for_virtual_key_releases(&releases);
        if result.is_ok() {
            self.injected.recent_releases.clear();
        }
        result
    }

    fn place_pointer_on_target(
        &self,
        x: u32,
        y: u32,
    ) -> windows::core::Result<Option<(i32, i32, HWND)>> {
        self.validate_identity()?;
        let Some((screen_x, screen_y)) = capture_to_screen(self.hwnd, self.capture_size, x, y)?
        else {
            // A resize/maximize can update the HWND before WGC publishes its
            // first frame at the new extent (or the reverse). Drop pointer
            // input during that short transition instead of targeting a
            // guessed global screen coordinate or ending the GUI session.
            return Ok(None);
        };
        self.validate_identity()?;
        // SAFETY: capture_to_screen validated a live window rectangle and
        // bounded the coordinate to the current WGC content dimensions.
        unsafe { SetCursorPos(screen_x, screen_y)? };
        let mut target = window_from_point(screen_x, screen_y);
        if window_belongs_to_capture(self.hwnd, self.owner.pid, target)?
            && foreground_belongs_to_capture(self.hwnd, self.owner.pid)?
        {
            return Ok(Some((screen_x, screen_y, target)));
        }
        let _ = activate_and_verify(self.hwnd, self.owner.pid)?;
        self.validate_identity()?;
        // Raising the captured window changes the window under the same screen
        // coordinate; query it again before any process-global SendInput call.
        // SAFETY: capture_to_screen validated this screen coordinate above.
        unsafe { SetCursorPos(screen_x, screen_y)? };
        target = window_from_point(screen_x, screen_y);
        self.validate_identity()?;
        Ok(
            window_belongs_to_capture(self.hwnd, self.owner.pid, target)?
                .then_some((screen_x, screen_y, target)),
        )
    }
}

impl Drop for WindowHandle {
    fn drop(&mut self) {
        // A WindowHandle can leave a live recovery registry because the HWND
        // closed independently, the companion is shutting down, or an
        // invariant rejected reattachment. Dropping an ownership handle must
        // never be interpreted as user intent to close an application with
        // potentially unsaved state. Explicit host close goes through
        // the normal close acknowledgement path; natural guest close needs no message.
        let _ = self.release_injected_input();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NonClientHitAction {
    Forward(GuiWindowAction),
    MaximizeOrRestore,
}

fn non_client_action(hwnd: HWND, x: i32, y: i32) -> Option<NonClientHitAction> {
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

fn software_resize_action(hwnd: HWND, x: i32, y: i32, native_hit: u32) -> Option<GuiWindowAction> {
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

fn resize_action_from_edges(
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

fn hit_test_action(hit: u32) -> Option<NonClientHitAction> {
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

fn maximize_transition(is_zoomed: bool) -> (GuiWindowAction, usize) {
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

fn point_lparam(x: i32, y: i32) -> Option<isize> {
    let x = i16::try_from(x).ok()?;
    let y = i16::try_from(y).ok()?;
    let x = u32::from(u16::from_ne_bytes(x.to_ne_bytes()));
    let y = u32::from(u16::from_ne_bytes(y.to_ne_bytes()));
    Some(isize::try_from(x | (y << 16)).expect("packed screen coordinates fit isize"))
}

fn pointer_button_index(button: GuiPointerButton) -> usize {
    match button {
        GuiPointerButton::Left => 0,
        GuiPointerButton::Middle => 1,
        GuiPointerButton::Right => 2,
        GuiPointerButton::Back => 3,
        GuiPointerButton::Forward => 4,
    }
}

fn pointer_button_virtual_key(button: GuiPointerButton) -> u16 {
    match button {
        GuiPointerButton::Left => VK_LBUTTON.0,
        GuiPointerButton::Right => VK_RBUTTON.0,
        GuiPointerButton::Middle => VK_MBUTTON.0,
        GuiPointerButton::Back => VK_XBUTTON1.0,
        GuiPointerButton::Forward => VK_XBUTTON2.0,
    }
}

fn wait_for_virtual_key_releases(virtual_keys: &[u16]) -> windows::core::Result<()> {
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

fn release_dispatch_grace_complete(
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

fn window_owner_matches(hwnd: HWND, expected_pid: u32) -> bool {
    owner_pid_matches(expected_pid, observed_window_owner_pid(hwnd))
}

fn owner_pid_matches(expected_pid: u32, observed_pid: Option<u32>) -> bool {
    expected_pid != 0 && observed_pid == Some(expected_pid)
}

fn window_is_owned_by(candidate: HWND, expected_owner: HWND, expected_pid: u32) -> bool {
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

fn observed_window_owner_pid(hwnd: HWND) -> Option<u32> {
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

fn window_from_point(x: i32, y: i32) -> HWND {
    // SAFETY: POINT contains ordinary screen coordinates and WindowFromPoint
    // returns either a live HWND or the documented null handle.
    unsafe { WindowFromPoint(POINT { x, y }) }
}

fn window_belongs_to_capture(
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

fn is_main_window_target(hwnd: HWND, candidate: HWND) -> bool {
    // SAFETY: GetAncestor accepts a possibly stale handle and returns null if
    // it cannot resolve the target. Owned popup roots remain distinct here.
    candidate == hwnd || unsafe { GetAncestor(candidate, GA_ROOT) == hwnd }
}

fn foreground_belongs_to_capture(hwnd: HWND, expected_pid: u32) -> windows::core::Result<bool> {
    // SAFETY: GetForegroundWindow returns either a live top-level HWND or null.
    window_belongs_to_capture(hwnd, expected_pid, unsafe { GetForegroundWindow() })
}

fn pointer_target_is_valid(
    hwnd: HWND,
    expected_pid: u32,
    x: i32,
    y: i32,
) -> windows::core::Result<bool> {
    Ok(foreground_belongs_to_capture(hwnd, expected_pid)?
        && pointer_target_belongs_to_capture(hwnd, expected_pid, x, y)?)
}

fn pointer_target_belongs_to_capture(
    hwnd: HWND,
    expected_pid: u32,
    x: i32,
    y: i32,
) -> windows::core::Result<bool> {
    window_belongs_to_capture(hwnd, expected_pid, window_from_point(x, y))
}

fn activate_and_verify(hwnd: HWND, expected_pid: u32) -> windows::core::Result<bool> {
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

fn activate_via_joined_input_queues(hwnd: HWND) {
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

fn dismissible_windows_shell_surface(hwnd: HWND) -> bool {
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

fn dismissible_windows_shell_identity(image_path: &str, package_family: &str) -> bool {
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

fn send_balanced_key_tap(virtual_key: u16) -> windows::core::Result<()> {
    let pressed = send_keyboard(virtual_key, 0, true, false);
    let released = send_keyboard(virtual_key, 0, false, false);
    pressed?;
    released
}

fn secondary_window_input_eligible(
    same_pinned_process: bool,
    owned_by_main: bool,
    main_bounds: RECT,
    candidate_bounds: RECT,
) -> bool {
    same_pinned_process && owned_by_main && rectangles_intersect(main_bounds, candidate_bounds)
}

fn rectangles_intersect(left: RECT, right: RECT) -> bool {
    left.left < left.right
        && left.top < left.bottom
        && right.left < right.right
        && right.top < right.bottom
        && left.left < right.right
        && right.left < left.right
        && left.top < right.bottom
        && right.top < left.bottom
}

fn send_keyboard(
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

fn send_mouse(flags: MOUSE_EVENT_FLAGS, data: u32) -> windows::core::Result<()> {
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

fn send_pointer_down_at(
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

fn absolute_virtual_desktop_point(x: i32, y: i32) -> windows::core::Result<(i32, i32)> {
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

fn normalize_absolute_axis(coordinate: i32, origin: i32, extent: i32) -> Option<i32> {
    if extent <= 1 {
        return None;
    }
    let relative = i64::from(coordinate) - i64::from(origin);
    if relative < 0 || relative >= i64::from(extent) {
        return None;
    }
    i32::try_from((relative * 65_535 + i64::from(extent - 1) / 2) / i64::from(extent - 1)).ok()
}

fn send_native_input(input: INPUT) -> windows::core::Result<()> {
    send_native_inputs(&[input])
}

fn send_native_inputs(inputs: &[INPUT]) -> windows::core::Result<()> {
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

pub(super) fn find_process_window(
    process_id: u32,
    existing_windows: &BTreeSet<isize>,
    child: &mut Child,
) -> Result<(WindowHandle, u32), Box<dyn std::error::Error>> {
    let launcher_handle = HANDLE(child.as_raw_handle() as isize);
    let launcher = ProcessKey {
        pid: process_id,
        creation_time: process_creation_time(launcher_handle)?,
    };
    let session_id = process_session_id(std::process::id())?;
    let deadline = Instant::now() + WINDOW_DISCOVERY_TIMEOUT;
    let mut launcher_status = None;
    let mut stable_exact = CandidateStability::default();
    loop {
        let candidates = enumerate_process_windows(existing_windows)?;
        match select_exact_window_candidate(&candidates, launcher, session_id, false, None) {
            Some(candidate) => {
                if let Some(candidate) = stable_exact.observe(candidate) {
                    return open_selected_window(candidate, launcher);
                }
            }
            None => stable_exact.reset(),
        }
        if launcher_status.is_none() {
            launcher_status = child.try_wait()?;
        }
        if Instant::now() >= deadline {
            return if let Some(status) = launcher_status {
                Err(format!(
                    "GUI launcher exited with {status} without creating a unique visible top-level window"
                )
                .into())
            } else {
                Err("timed out waiting for the GUI process to create a visible window".into())
            };
        }
        thread::sleep(WINDOW_DISCOVERY_INTERVAL);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActivatedPackageIdentity {
    package_full_name: String,
    package_family_name: String,
    aumid: String,
}

pub(super) struct ActivatedApplication {
    owner: OwnedProcess,
    process: ProcessKey,
    session_id: u32,
    package: ActivatedPackageIdentity,
    activation_filetime: u64,
}

impl ActivatedApplication {
    fn validate(&self) -> windows::core::Result<()> {
        // SAFETY: owner.handle pins the exact process object returned by AAM
        // and includes SYNCHRONIZE for the lifetime of this activation.
        if unsafe { WaitForSingleObject(self.owner.handle, 0) } != WAIT_TIMEOUT
            || process_creation_time(self.owner.handle).ok() != Some(self.process.creation_time)
            || self.process.creation_time < self.activation_filetime
        {
            return Err(Error::new(
                INVALID_ARGUMENT,
                "AAM-activated GUI process is no longer the pinned launch result",
            ));
        }
        Ok(())
    }
}

pub(super) fn activate_packaged_alias(
    requested_executable: &str,
) -> Result<Option<ActivatedApplication>, Box<dyn std::error::Error>> {
    let Some(expected_alias) = packaged_activation_alias_name(requested_executable)? else {
        return Ok(None);
    };
    let _apartment = ApartmentGuard::initialize()?;
    let Some(package) = resolve_current_user_packaged_alias(&expected_alias)? else {
        return Ok(None);
    };
    // SAFETY: the calling thread has an active WinRT/COM apartment. The class
    // and interface are the documented ApplicationActivationManager pair.
    let manager: IApplicationActivationManager =
        unsafe { CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_INPROC_SERVER)? };
    let aumid = nul_terminated_utf16(&package.aumid, "application user model ID")?;
    let empty = [0_u16];
    // Snapshot and timestamp only after every fallible activation prerequisite
    // is ready, leaving no package scan, COM creation, or string-allocation gap
    // in which an unrelated singleton could start and look launch-correlated.
    let pre_activation_processes = process_id_snapshot()?;
    // SAFETY: this reads the precise system FILETIME directly before the
    // documented activation call. Process creation times use the same epoch.
    let activation_filetime = filetime_value(unsafe { GetSystemTimePreciseAsFileTime() });
    // SAFETY: both PCWSTR values are NUL-terminated and live for the complete
    // synchronous activation. AO_NOERRORUI suppresses only OS activation error
    // UI. AO_NOSPLASHSCREEN is deliberately not used: Microsoft documents it
    // for debug-enabled packages and PLM may otherwise terminate a retail app.
    let pid = unsafe {
        manager.ActivateApplication(PCWSTR(aumid.as_ptr()), PCWSTR(empty.as_ptr()), AO_NOERRORUI)?
    };
    if pid == 0 {
        return Err("ApplicationActivationManager returned a zero PID".into());
    }
    let owner = OwnedProcess::open(pid)?;
    if !activation_process_is_new(
        pid,
        owner.creation_time,
        activation_filetime,
        &pre_activation_processes,
    ) {
        return Err(
            "packaged GUI activation reused an existing/singleton process; refusing ambiguous HWND correlation"
                .into(),
        );
    }
    let session_id = process_session_id(pid)?;
    if session_id != process_session_id(std::process::id())? {
        return Err("AAM activated the GUI application in a different Windows session".into());
    }
    validate_process_package_identity(owner.handle, &package)?;
    Ok(Some(ActivatedApplication {
        process: ProcessKey {
            pid,
            creation_time: owner.creation_time,
        },
        owner,
        session_id,
        package,
        activation_filetime,
    }))
}

pub(super) fn find_activated_window(
    activation: ActivatedApplication,
    existing_windows: &BTreeSet<isize>,
) -> Result<(WindowHandle, u32), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + WINDOW_DISCOVERY_TIMEOUT;
    let mut stability = CandidateStability::default();
    loop {
        activation.validate()?;
        let candidates = enumerate_process_windows(existing_windows)?;
        match select_exact_window_candidate(
            &candidates,
            activation.process,
            activation.session_id,
            true,
            Some(&activation.package),
        ) {
            Some(candidate) => {
                if let Some(candidate) = stability.observe(candidate) {
                    activation.validate()?;
                    let selected = open_selected_window(candidate, activation.process)?;
                    drop(activation);
                    return Ok(selected);
                }
            }
            None => stability.reset(),
        }
        if Instant::now() >= deadline {
            return Err(
                "AAM activation did not create one unique stable visible HWND owned by its returned PID"
                    .into(),
            );
        }
        thread::sleep(WINDOW_DISCOVERY_INTERVAL);
    }
}

fn resolve_current_user_packaged_alias(
    expected_alias: &str,
) -> Result<Option<ActivatedPackageIdentity>, Box<dyn std::error::Error>> {
    // Empty SID is the documented current-user form. FindPackages() without a
    // SID spans all users and would violate the interactive companion scope.
    let packages = PackageManager::new()?.FindPackagesByUserSecurityId(&HSTRING::new())?;
    let iterator = packages.First()?;
    let mut scanned = 0_usize;
    let mut matches = Vec::new();
    while iterator.HasCurrent()? {
        scanned += 1;
        if scanned > MAX_CURRENT_USER_PACKAGES {
            return Err(format!(
                "current-user package scan exceeds the {MAX_CURRENT_USER_PACKAGES} package safety limit"
            )
            .into());
        }
        let package = iterator.Current()?;
        let id = package.Id()?;
        let package_full_name = strict_hstring(id.FullName()?, "package full name")?;
        let package_family_name = strict_hstring(id.FamilyName()?, "package family name")?;
        if package_full_name.is_empty() || package_family_name.is_empty() {
            return Err("PackageManager returned an empty package identity".into());
        }
        if let Some(application_id) =
            package_manifest_alias_application(&package_full_name, expected_alias)?
        {
            if application_id.contains(['!', '\0']) {
                return Err("package manifest application ID is invalid for an AUMID".into());
            }
            let aumid = format!("{package_family_name}!{application_id}");
            validate_aumid(&aumid)?;
            matches.push(ActivatedPackageIdentity {
                package_full_name,
                package_family_name,
                aumid,
            });
            if matches.len() > 1 {
                return Err(format!(
                    "AppExecutionAlias {expected_alias:?} is declared by multiple current-user packages"
                )
                .into());
            }
        }
        iterator.MoveNext()?;
    }
    Ok(matches.pop())
}

fn validate_process_package_identity(
    handle: HANDLE,
    expected: &ActivatedPackageIdentity,
) -> Result<(), Box<dyn std::error::Error>> {
    let full_name = process_package_full_name(handle)?
        .ok_or("AAM returned an unpackaged process for a packaged alias")?;
    let family_name = process_package_family_name(handle)?
        .ok_or("AAM returned a process without a package family")?;
    let aumid = process_application_user_model_id(handle)?
        .ok_or("AAM returned a process without an application user model ID")?;
    if !windows_ordinal_eq_ignore_case(&full_name, &expected.package_full_name)
        || !windows_ordinal_eq_ignore_case(&family_name, &expected.package_family_name)
        || !windows_ordinal_eq_ignore_case(&aumid, &expected.aumid)
    {
        return Err("AAM returned PID package/AUMID does not match the resolved alias".into());
    }
    Ok(())
}

fn activation_process_is_new(
    pid: u32,
    creation_time: u64,
    activation_filetime: u64,
    pre_activation_processes: &BTreeSet<u32>,
) -> bool {
    pid != 0 && !pre_activation_processes.contains(&pid) && creation_time >= activation_filetime
}

struct ProcessSnapshotHandle(HANDLE);

impl Drop for ProcessSnapshotHandle {
    fn drop(&mut self) {
        // SAFETY: this value exclusively owns the Toolhelp snapshot handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn process_id_snapshot() -> Result<BTreeSet<u32>, Box<dyn std::error::Error>> {
    let snapshot = ProcessSnapshotHandle(
        // SAFETY: TH32CS_SNAPPROCESS with PID zero creates a caller-owned snapshot
        // of system process identifiers and does not inspect process memory.
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? },
    );
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>())?,
        ..Default::default()
    };
    // SAFETY: entry has the required dwSize and remains writable for iteration.
    unsafe { Process32FirstW(snapshot.0, &mut entry)? };
    let mut processes = BTreeSet::new();
    loop {
        if entry.th32ProcessID != 0 {
            processes.insert(entry.th32ProcessID);
            if processes.len() > MAX_PROCESS_SNAPSHOT_ENTRIES {
                return Err("process snapshot exceeds the safety entry limit".into());
            }
        }
        // SAFETY: snapshot and entry remain valid for the next synchronous
        // Toolhelp iteration call.
        match unsafe { Process32NextW(snapshot.0, &mut entry) } {
            Ok(()) => {}
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) => {
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(processes)
}

fn filetime_value(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

fn validate_aumid(value: &str) -> Result<(), Box<dyn std::error::Error>> {
    let units = value.encode_utf16().count().saturating_add(1);
    if aumid_parts(value).is_none()
        || units == 1
        || units > usize::try_from(APPLICATION_USER_MODEL_ID_MAX_LENGTH)?
    {
        return Err("resolved package application user model ID is invalid".into());
    }
    Ok(())
}

fn nul_terminated_utf16(value: &str, field: &str) -> Result<Vec<u16>, Box<dyn std::error::Error>> {
    let mut units = value.encode_utf16().collect::<Vec<_>>();
    if units.is_empty() || units.contains(&0) {
        return Err(format!("{field} is empty or contains NUL").into());
    }
    units.push(0);
    Ok(units)
}

fn strict_hstring(value: HSTRING, field: &str) -> Result<String, Box<dyn std::error::Error>> {
    let units = value.as_wide();
    if units.is_empty() || units.contains(&0) {
        return Err(format!("PackageManager returned an invalid {field}").into());
    }
    Ok(String::from_utf16(units)?)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessKey {
    pid: u32,
    creation_time: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WindowCandidate {
    hwnd: isize,
    pid: u32,
    creation_time: u64,
    session_id: u32,
    image_path: Option<String>,
    package_full_name: Option<String>,
    package_family_name: Option<String>,
    application_user_model_id: Option<String>,
    new_window: bool,
}

#[derive(Default)]
struct CandidateStability {
    previous: Option<WindowCandidate>,
}

impl CandidateStability {
    fn observe(&mut self, candidate: WindowCandidate) -> Option<WindowCandidate> {
        if self.previous.as_ref() == Some(&candidate) {
            self.previous = None;
            Some(candidate)
        } else {
            self.previous = Some(candidate);
            None
        }
    }

    fn reset(&mut self) {
        self.previous = None;
    }
}

struct WindowEnumeration {
    windows: Vec<(isize, u32)>,
}

pub(super) fn visible_windows() -> windows::core::Result<BTreeSet<isize>> {
    let mut windows = BTreeSet::new();
    // SAFETY: LPARAM contains a valid BTreeSet pointer for this synchronous
    // EnumWindows invocation. The callback only inserts HWND integer values.
    unsafe {
        EnumWindows(
            Some(collect_visible_window),
            LPARAM((&mut windows as *mut BTreeSet<isize>) as isize),
        )?;
    }
    Ok(windows)
}

unsafe extern "system" fn collect_visible_window(hwnd: HWND, parameter: LPARAM) -> BOOL {
    // SAFETY: visible_windows passes this exact pointer and EnumWindows invokes
    // callbacks synchronously before the stack allocation is dropped.
    let windows = unsafe { &mut *(parameter.0 as *mut BTreeSet<isize>) };
    // SAFETY: IsWindowVisible accepts the HWND supplied by EnumWindows.
    if unsafe { IsWindowVisible(hwnd).as_bool() } {
        windows.insert(hwnd.0);
    }
    BOOL(1)
}

fn enumerate_process_windows(
    existing_windows: &BTreeSet<isize>,
) -> windows::core::Result<Vec<WindowCandidate>> {
    let mut enumeration = WindowEnumeration {
        windows: Vec::new(),
    };
    // SAFETY: LPARAM contains a valid WindowEnumeration pointer for this
    // synchronous EnumWindows invocation. The callback stores only integer
    // HWND/PID pairs and does not retain the pointer.
    unsafe {
        EnumWindows(
            Some(enumerate_visible_window),
            LPARAM((&mut enumeration as *mut WindowEnumeration) as isize),
        )?;
    }
    let mut candidates = Vec::new();
    for (hwnd, pid) in enumeration.windows {
        let Ok(process) = ProcessSnapshot::open(pid) else {
            continue;
        };
        candidates.push(WindowCandidate {
            hwnd,
            pid,
            creation_time: process.creation_time,
            session_id: process.session_id,
            image_path: process.image_path,
            package_full_name: process.package_full_name,
            package_family_name: process.package_family_name,
            application_user_model_id: process.application_user_model_id,
            new_window: !existing_windows.contains(&hwnd),
        });
    }
    Ok(candidates)
}

unsafe extern "system" fn enumerate_visible_window(hwnd: HWND, parameter: LPARAM) -> BOOL {
    // SAFETY: enumerate_process_windows passes this exact pointer and
    // EnumWindows invokes callbacks synchronously before the stack allocation
    // is dropped.
    let enumeration = unsafe { &mut *(parameter.0 as *mut WindowEnumeration) };
    // SAFETY: All queried functions accept an HWND supplied by EnumWindows and
    // the process-id output points at live stack storage.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut observed = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut observed));
        if observed != 0 {
            enumeration.windows.push((hwnd.0, observed));
        }
    }
    BOOL(1)
}

fn select_exact_window_candidate(
    candidates: &[WindowCandidate],
    expected: ProcessKey,
    session_id: u32,
    require_new_window: bool,
    package_identity: Option<&ActivatedPackageIdentity>,
) -> Option<WindowCandidate> {
    let exact = candidates
        .iter()
        .filter(|candidate| {
            candidate.pid == expected.pid
                && candidate.creation_time == expected.creation_time
                && candidate.session_id == session_id
                && (!require_new_window || candidate.new_window)
                && package_identity.map_or(true, |identity| {
                    candidate.package_full_name.as_deref() == Some(&identity.package_full_name)
                        && candidate.package_family_name.as_deref()
                            == Some(&identity.package_family_name)
                        && candidate.application_user_model_id.as_deref() == Some(&identity.aumid)
                })
        })
        .collect::<Vec<_>>();
    match exact.as_slice() {
        [candidate] => Some((*candidate).clone()),
        // Splash/main coexistence can be transient. Wait for one unique HWND
        // and then require it to remain stable on the next poll. Ambiguity
        // never permits a temporal or other-PID fallback.
        _ => None,
    }
}

fn open_selected_window(
    candidate: WindowCandidate,
    expected: ProcessKey,
) -> Result<(WindowHandle, u32), Box<dyn std::error::Error>> {
    let owner = OwnedProcess::open(candidate.pid)?;
    if owner.pid != expected.pid
        || owner.creation_time != expected.creation_time
        || owner.creation_time != candidate.creation_time
    {
        return Err("GUI window owner changed identity during discovery".into());
    }
    let hwnd = HWND(candidate.hwnd);
    if !window_owner_matches(hwnd, owner.pid) {
        return Err("GUI HWND owner changed during discovery".into());
    }
    let owner_pid = owner.pid;
    Ok((
        WindowHandle {
            hwnd,
            owner,
            capture_size: (0, 0),
            injected: InjectedInputState::default(),
        },
        owner_pid,
    ))
}

struct ProcessSnapshot {
    creation_time: u64,
    session_id: u32,
    image_path: Option<String>,
    package_full_name: Option<String>,
    package_family_name: Option<String>,
    application_user_model_id: Option<String>,
}

impl ProcessSnapshot {
    fn open(pid: u32) -> windows::core::Result<Self> {
        let process = OwnedProcess::open(pid)?;
        let image_path = process_image_path(process.handle).ok();
        let package_full_name = process_package_full_name(process.handle).ok().flatten();
        let package_family_name = process_package_family_name(process.handle).ok().flatten();
        let application_user_model_id = process_application_user_model_id(process.handle)
            .ok()
            .flatten();
        Ok(Self {
            creation_time: process.creation_time,
            session_id: process_session_id(pid)?,
            image_path,
            package_full_name,
            package_family_name,
            application_user_model_id,
        })
    }
}

impl OwnedProcess {
    fn open(pid: u32) -> windows::core::Result<Self> {
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
        // SAFETY: OpenProcess receives a numeric PID and returns a caller-owned
        // query/synchronization handle. GUI teardown never requests terminate
        // rights or force-kills an application with potentially unsaved data.
        let handle = unsafe { OpenProcess(access, false, pid)? };
        let mut process = Self {
            handle,
            pid,
            creation_time: 0,
        };
        process.creation_time = process_creation_time(process.handle)?;
        Ok(process)
    }
}

fn process_creation_time(handle: HANDLE) -> windows::core::Result<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all output pointers refer to initialized writable FILETIME values
    // and handle has PROCESS_QUERY_LIMITED_INFORMATION.
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user)? };
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn process_session_id(pid: u32) -> windows::core::Result<u32> {
    let mut session = 0_u32;
    // SAFETY: session points at initialized writable storage and PID is a
    // numeric process identifier obtained from Windows.
    unsafe { ProcessIdToSessionId(pid, &mut session)? };
    Ok(session)
}

fn process_image_path(handle: HANDLE) -> Result<String, Box<dyn std::error::Error>> {
    let mut buffer = vec![0_u16; MAX_PROCESS_IMAGE_UNITS];
    let mut length = u32::try_from(buffer.len())?;
    // SAFETY: buffer is writable for length UTF-16 units, length remains valid,
    // and the process handle has limited query permission.
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )?
    };
    buffer.truncate(usize::try_from(length)?);
    let image = String::from_utf16(&buffer)?;
    if image.is_empty() {
        return Err("GUI process image path is empty".into());
    }
    Ok(image)
}

fn process_package_full_name(handle: HANDLE) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut length = 0_u32;
    // SAFETY: the first call is the documented size query. The process handle
    // has limited query permission and no output buffer is provided.
    let status = unsafe { GetPackageFullName(handle, &mut length, PWSTR::null()) };
    if status == APPMODEL_ERROR_NO_PACKAGE {
        return Ok(None);
    }
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(win32_status_error(
            "GetPackageFullName size query",
            status.0,
        ));
    }
    let length = usize::try_from(length)?;
    if length == 0 || length > MAX_PACKAGE_NAME_UNITS {
        return Err("Windows returned an invalid package full-name length".into());
    }
    let mut buffer = vec![0_u16; length];
    let mut written = u32::try_from(buffer.len())?;
    // SAFETY: buffer is writable for written UTF-16 units and the process
    // handle remains live for this synchronous query.
    let status = unsafe { GetPackageFullName(handle, &mut written, PWSTR(buffer.as_mut_ptr())) };
    if status != ERROR_SUCCESS {
        return Err(win32_status_error("GetPackageFullName", status.0));
    }
    let package = String::from_utf16(validated_nul_terminated_units(
        &buffer,
        written,
        "package full name",
    )?)?;
    if package.is_empty() {
        return Err("Windows returned an empty package full name".into());
    }
    Ok(Some(package))
}

fn process_package_family_name(
    handle: HANDLE,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut length = 0_u32;
    // SAFETY: this is the documented size query and handle has limited process
    // query permission. No output buffer is supplied on the first call.
    let status = unsafe { GetPackageFamilyName(handle, &mut length, PWSTR::null()) };
    if status == APPMODEL_ERROR_NO_PACKAGE {
        return Ok(None);
    }
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(win32_status_error(
            "GetPackageFamilyName size query",
            status.0,
        ));
    }
    if length == 0 || length > PACKAGE_FAMILY_NAME_MAX_LENGTH + 1 {
        return Err("Windows returned an invalid package family-name length".into());
    }
    let mut buffer = vec![0_u16; usize::try_from(length)?];
    let mut written = u32::try_from(buffer.len())?;
    // SAFETY: buffer is writable for written UTF-16 units and handle remains
    // live with limited query permission for this synchronous call.
    let status = unsafe { GetPackageFamilyName(handle, &mut written, PWSTR(buffer.as_mut_ptr())) };
    if status != ERROR_SUCCESS {
        return Err(win32_status_error("GetPackageFamilyName", status.0));
    }
    let family = String::from_utf16(validated_nul_terminated_units(
        &buffer,
        written,
        "package family name",
    )?)?;
    if family.is_empty() {
        return Err("Windows returned an empty package family name".into());
    }
    Ok(Some(family))
}

fn process_application_user_model_id(
    handle: HANDLE,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut length = 0_u32;
    // SAFETY: this is the documented size query and handle has limited process
    // query permission. No output buffer is supplied on the first call.
    let status = unsafe { GetApplicationUserModelId(handle, &mut length, PWSTR::null()) };
    if status == APPMODEL_ERROR_NO_APPLICATION {
        return Ok(None);
    }
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(win32_status_error(
            "GetApplicationUserModelId size query",
            status.0,
        ));
    }
    if length == 0 || length > APPLICATION_USER_MODEL_ID_MAX_LENGTH {
        return Err("Windows returned an invalid application user model ID length".into());
    }
    let mut buffer = vec![0_u16; usize::try_from(length)?];
    let mut written = u32::try_from(buffer.len())?;
    // SAFETY: buffer is writable for written UTF-16 units and handle remains
    // live with limited query permission for this synchronous call.
    let status =
        unsafe { GetApplicationUserModelId(handle, &mut written, PWSTR(buffer.as_mut_ptr())) };
    if status != ERROR_SUCCESS {
        return Err(win32_status_error("GetApplicationUserModelId", status.0));
    }
    let aumid = String::from_utf16(validated_nul_terminated_units(
        &buffer,
        written,
        "application user model ID",
    )?)?;
    if aumid_parts(&aumid).is_none() {
        return Err("Windows returned an invalid application user model ID".into());
    }
    Ok(Some(aumid))
}

fn package_manifest_alias_application(
    package_full_name: &str,
    expected_alias: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let package_path = package_path_by_full_name(package_full_name)?;
    let manifest_path = package_path.join("AppxManifest.xml");
    let file = File::open(manifest_path)?;
    if file.metadata()?.len() > MAX_PACKAGE_MANIFEST_BYTES {
        return Err(format!(
            "package manifest exceeds the {} byte correlation limit",
            MAX_PACKAGE_MANIFEST_BYTES
        )
        .into());
    }
    let mut bytes = Vec::new();
    BufReader::new(file)
        .take(MAX_PACKAGE_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len())? > MAX_PACKAGE_MANIFEST_BYTES {
        return Err("package manifest grew beyond the correlation limit while reading".into());
    }
    manifest_xml_alias_application(&bytes, expected_alias)
}

fn package_path_by_full_name(
    package_full_name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let package_wide = package_full_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let package = PCWSTR(package_wide.as_ptr());
    let mut length = 0_u32;
    // SAFETY: package points at a NUL-terminated UTF-16 package full name for
    // both synchronous calls. The first call is the documented size query.
    let status = unsafe { GetPackagePathByFullName(package, &mut length, PWSTR::null()) };
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(win32_status_error(
            "GetPackagePathByFullName size query",
            status.0,
        ));
    }
    let length = usize::try_from(length)?;
    if length == 0 || length > MAX_PACKAGE_PATH_UNITS {
        return Err("Windows returned an invalid package path length".into());
    }
    let mut buffer = vec![0_u16; length];
    let mut written = u32::try_from(buffer.len())?;
    // SAFETY: the output buffer is writable for written UTF-16 units and the
    // input package name remains NUL terminated for the synchronous call.
    let status =
        unsafe { GetPackagePathByFullName(package, &mut written, PWSTR(buffer.as_mut_ptr())) };
    if status != ERROR_SUCCESS {
        return Err(win32_status_error("GetPackagePathByFullName", status.0));
    }
    let path_units = validated_nul_terminated_units(&buffer, written, "package path")?;
    if path_units.is_empty() {
        return Err("Windows returned an empty package path".into());
    }
    Ok(PathBuf::from(OsString::from_wide(path_units)))
}

fn validated_nul_terminated_units<'a>(
    buffer: &'a [u16],
    written: u32,
    field: &str,
) -> Result<&'a [u16], Box<dyn std::error::Error>> {
    let written = usize::try_from(written)?;
    if written == 0 || written > buffer.len() || buffer[written - 1] != 0 {
        return Err(format!("Windows returned an invalid {field} buffer").into());
    }
    let value = &buffer[..written - 1];
    if value.contains(&0) {
        return Err(format!("Windows returned an interior NUL in {field}").into());
    }
    Ok(value)
}

fn manifest_xml_alias_application(
    manifest: &[u8],
    expected_alias: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if u64::try_from(manifest.len())? > MAX_PACKAGE_MANIFEST_BYTES {
        return Err("package manifest exceeds the correlation parser limit".into());
    }
    let mut parser = ParserConfig::new()
        .trim_whitespace(true)
        .ignore_comments(true)
        .coalesce_characters(false)
        .allow_multiple_root_elements(false)
        .max_entity_expansion_length(64 * 1024)
        .max_entity_expansion_depth(4)
        .max_name_length(256)
        .max_attributes(64)
        .max_attribute_length(4096)
        .max_data_length(1024 * 1024)
        .create_reader(manifest);
    let mut depth = 0_usize;
    let mut package_root = None::<usize>;
    let mut applications = None::<usize>;
    let mut application = None::<(usize, String)>;
    let mut application_extensions = None::<usize>;
    let mut alias_extension = None::<(usize, String, AliasExtensionSchema)>;
    let mut alias_container = None::<(usize, String, AliasExtensionSchema)>;
    let mut matching_applications = BTreeSet::new();
    let mut event_index = 0_usize;
    loop {
        if event_index >= MAX_PACKAGE_MANIFEST_EVENTS {
            return Err("package manifest exceeds the XML event limit".into());
        }
        event_index += 1;
        let event = parser.next()?;
        let end_document = matches!(event, XmlEvent::EndDocument);
        match event {
            XmlEvent::StartElement {
                name, attributes, ..
            } => {
                depth = depth
                    .checked_add(1)
                    .ok_or("package manifest depth overflowed")?;
                if depth > MAX_PACKAGE_MANIFEST_DEPTH {
                    return Err("package manifest exceeds the XML depth limit".into());
                }
                if depth == 1
                    && name.local_name == "Package"
                    && name.namespace.as_deref() == Some(FOUNDATION_MANIFEST_NAMESPACE)
                {
                    package_root = Some(depth);
                } else if applications.is_none()
                    && package_root.is_some_and(|root_depth| depth == root_depth + 1)
                    && name.local_name == "Applications"
                    && name.namespace.as_deref() == Some(FOUNDATION_MANIFEST_NAMESPACE)
                {
                    applications = Some(depth);
                } else if application.is_none()
                    && applications
                        .is_some_and(|applications_depth| depth == applications_depth + 1)
                    && name.local_name == "Application"
                    && name.namespace.as_deref() == Some(FOUNDATION_MANIFEST_NAMESPACE)
                {
                    if let Some(application_id) = attributes
                        .iter()
                        .find(|attribute| {
                            attribute.name.local_name == "Id" && attribute.name.namespace.is_none()
                        })
                        .map(|attribute| attribute.value.clone())
                        .filter(|application_id| !application_id.is_empty())
                    {
                        application = Some((depth, application_id));
                    }
                } else if application_extensions.is_none()
                    && application
                        .as_ref()
                        .is_some_and(|(application_depth, _)| depth == application_depth + 1)
                    && name.local_name == "Extensions"
                    && name.namespace.as_deref() == Some(FOUNDATION_MANIFEST_NAMESPACE)
                {
                    application_extensions = Some(depth);
                } else if alias_extension.is_none()
                    && application_extensions
                        .is_some_and(|extensions_depth| depth == extensions_depth + 1)
                    && name.local_name == "Extension"
                    && alias_extension_schema(name.namespace.as_deref()).is_some()
                    && attributes.iter().any(|attribute| {
                        attribute.name.local_name == "Category"
                            && attribute.name.namespace.is_none()
                            && attribute
                                .value
                                .eq_ignore_ascii_case("windows.appExecutionAlias")
                    })
                {
                    if let Some((_, application_id)) = application.as_ref() {
                        alias_extension = Some((
                            depth,
                            application_id.clone(),
                            alias_extension_schema(name.namespace.as_deref())
                                .expect("schema checked above"),
                        ));
                    }
                } else if let Some((extension_depth, application_id, schema)) =
                    alias_extension.as_ref()
                {
                    if alias_container.is_none()
                        && depth == extension_depth + 1
                        && name.local_name == "AppExecutionAlias"
                        && name.namespace.as_deref() == Some(schema.extension_namespace())
                    {
                        alias_container = Some((depth, application_id.clone(), *schema));
                    }
                }
                if let Some((container_depth, application_id, schema)) = alias_container.as_ref() {
                    if depth == container_depth + 1
                        && name.local_name == "ExecutionAlias"
                        && schema.execution_namespace_allowed(name.namespace.as_deref())
                        && attributes.iter().any(|attribute| {
                            attribute.name.local_name == "Alias"
                                && attribute.name.namespace.is_none()
                                && windows_ordinal_eq_ignore_case(&attribute.value, expected_alias)
                        })
                    {
                        matching_applications.insert(application_id.clone());
                    }
                }
            }
            XmlEvent::EndElement { .. } => {
                if alias_container
                    .as_ref()
                    .is_some_and(|(container_depth, _, _)| *container_depth == depth)
                {
                    alias_container = None;
                }
                if alias_extension
                    .as_ref()
                    .is_some_and(|(extension_depth, _, _)| *extension_depth == depth)
                {
                    alias_extension = None;
                }
                if application_extensions.is_some_and(|extensions_depth| extensions_depth == depth)
                {
                    application_extensions = None;
                }
                if application
                    .as_ref()
                    .is_some_and(|(application_depth, _)| *application_depth == depth)
                {
                    application = None;
                }
                if applications.is_some_and(|applications_depth| applications_depth == depth) {
                    applications = None;
                }
                if package_root.is_some_and(|root_depth| root_depth == depth) {
                    package_root = None;
                }
                depth = depth
                    .checked_sub(1)
                    .ok_or("package manifest ended outside its root element")?;
            }
            _ => {}
        }
        if end_document {
            break;
        }
    }
    if parser.doctype().is_some() {
        return Err("package manifest DOCTYPE is not accepted for GUI correlation".into());
    }
    match matching_applications.len() {
        0 => Ok(None),
        1 => Ok(matching_applications.into_iter().next()),
        count => Err(format!(
            "package manifest declares {expected_alias} for {count} applications"
        )
        .into()),
    }
}

#[derive(Clone, Copy)]
enum AliasExtensionSchema {
    Uap3,
    Uap5,
}

impl AliasExtensionSchema {
    fn extension_namespace(self) -> &'static str {
        match self {
            Self::Uap3 => UAP3_MANIFEST_NAMESPACE,
            Self::Uap5 => UAP5_MANIFEST_NAMESPACE,
        }
    }

    fn execution_namespace_allowed(self, namespace: Option<&str>) -> bool {
        match self {
            Self::Uap3 => matches!(
                namespace,
                Some(DESKTOP_MANIFEST_NAMESPACE | UAP8_MANIFEST_NAMESPACE)
            ),
            Self::Uap5 => matches!(
                namespace,
                Some(UAP5_MANIFEST_NAMESPACE | UAP8_MANIFEST_NAMESPACE)
            ),
        }
    }
}

fn alias_extension_schema(namespace: Option<&str>) -> Option<AliasExtensionSchema> {
    match namespace {
        Some(UAP3_MANIFEST_NAMESPACE) => Some(AliasExtensionSchema::Uap3),
        Some(UAP5_MANIFEST_NAMESPACE) => Some(AliasExtensionSchema::Uap5),
        _ => None,
    }
}

fn aumid_parts(aumid: &str) -> Option<(&str, &str)> {
    let (family, application_id) = aumid.rsplit_once('!')?;
    (!family.is_empty() && !application_id.is_empty()).then_some((family, application_id))
}

fn windows_ordinal_eq_ignore_case(left: &str, right: &str) -> bool {
    let left = left.encode_utf16().collect::<Vec<_>>();
    let right = right.encode_utf16().collect::<Vec<_>>();
    // SAFETY: both slices are valid UTF-16 buffers for the duration of the
    // synchronous ordinal comparison. No NUL terminator is required.
    (unsafe { CompareStringOrdinal(&left, &right, true) }) == CSTR_EQUAL
}

fn win32_status_error(context: &str, status: u32) -> Box<dyn std::error::Error> {
    format!("{context}: {}", Error::from(HRESULT::from_win32(status))).into()
}

fn normalize_executable_alias(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let leaf = Path::new(value)
        .file_name()
        .and_then(|leaf| leaf.to_str())
        .filter(|leaf| !leaf.is_empty())
        .ok_or("GUI executable path has no file name")?;
    let mut normalized = leaf.to_ascii_lowercase();
    if Path::new(leaf).extension().is_none() {
        normalized.push_str(".exe");
    }
    Ok(normalized)
}

fn packaged_activation_alias_name(
    value: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let path = Path::new(value);
    if path.is_absolute() || path.components().count() != 1 {
        // A path-qualified executable is not proof that Windows resolved the
        // request through an AppExecutionAlias. Exact-PID capture remains
        // available, but documented packaged delegation is disabled.
        return Ok(None);
    }
    normalize_executable_alias(value).map(Some)
}

#[derive(Clone, Copy)]
struct CompositeSecondaryWindow {
    hwnd: HWND,
    bounds: RECT,
}

struct CompositeSecondaryEnumeration {
    main: HWND,
    process_id: u32,
    windows: Vec<isize>,
}

fn composite_secondary_window_eligible(window: &WindowHandle, hwnd: HWND) -> bool {
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

fn composite_secondary_windows(
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

fn composite_secondary_window(
    window: &WindowHandle,
    hwnd: HWND,
) -> Result<Option<CompositeSecondaryWindow>, Box<dyn std::error::Error>> {
    Ok(composite_secondary_windows(window)?
        .into_iter()
        .find(|candidate| candidate.hwnd == hwnd))
}

fn validate_capture_target(
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

fn rectangles_equal(left: RECT, right: RECT) -> bool {
    left.left == right.left
        && left.top == right.top
        && left.right == right.right
        && left.bottom == right.bottom
}

fn window_order_changed(previous: &[isize], current: &[isize]) -> bool {
    previous != current
}

struct CompositeSecondaryCapture {
    window: CompositeSecondaryWindow,
    source: CaptureSource,
    latest: Option<CapturedFrame>,
}

pub(super) struct CaptureSession {
    main: CaptureSource,
    secondary: Vec<CompositeSecondaryCapture>,
    latest_main: Option<CapturedFrame>,
}

impl CaptureSession {
    pub(super) fn start(window: &WindowHandle) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            main: CaptureSource::start(window, window.hwnd)?,
            secondary: Vec::new(),
            latest_main: None,
        })
    }

    pub(super) fn next_frame(
        &mut self,
        window: &WindowHandle,
        timeout: Duration,
    ) -> Result<Option<CapturedFrame>, Box<dyn std::error::Error>> {
        let mut changed = false;
        if let Some(frame) = self.main.next_frame(timeout)? {
            self.latest_main = Some(frame);
            changed = true;
        }
        changed |= self.sync_secondary(window)?;

        let mut index = 0;
        while index < self.secondary.len() {
            match self.secondary[index].source.next_frame(Duration::ZERO) {
                Ok(Some(frame)) => {
                    self.secondary[index].latest = Some(frame);
                    changed = true;
                }
                Ok(None) => {}
                Err(error) => {
                    let hwnd = self.secondary[index].window.hwnd;
                    if composite_secondary_window(window, hwnd)?.is_none() {
                        self.secondary.remove(index);
                        changed = true;
                        continue;
                    }
                    return Err(error);
                }
            }
            index += 1;
        }

        let Some(main) = self.latest_main.as_ref() else {
            return Ok(None);
        };
        if !changed {
            return Ok(None);
        }
        let main_bounds = window_frame_rect(window.hwnd)?;
        let layers = self
            .secondary
            .iter()
            .rev()
            .filter_map(|secondary| {
                secondary
                    .latest
                    .as_ref()
                    .map(|frame| (frame, secondary.window.bounds))
            })
            .collect::<Vec<_>>();
        Ok(Some(composite_captured_frame(main, main_bounds, &layers)?))
    }

    fn sync_secondary(
        &mut self,
        window: &WindowHandle,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let candidates = composite_secondary_windows(window)?;
        let mut previous = std::mem::take(&mut self.secondary);
        let previous_order = previous
            .iter()
            .map(|capture| capture.window.hwnd.0)
            .collect::<Vec<_>>();
        let candidate_order = candidates
            .iter()
            .map(|candidate| candidate.hwnd.0)
            .collect::<Vec<_>>();
        let mut changed = window_order_changed(&previous_order, &candidate_order);
        let mut next = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if let Some(position) = previous
                .iter()
                .position(|capture| capture.window.hwnd == candidate.hwnd)
            {
                let mut capture = previous.remove(position);
                if !rectangles_equal(capture.window.bounds, candidate.bounds) {
                    changed = true;
                    capture.window.bounds = candidate.bounds;
                }
                next.push(capture);
            } else {
                match CaptureSource::start(window, candidate.hwnd) {
                    Ok(source) => {
                        next.push(CompositeSecondaryCapture {
                            window: candidate,
                            source,
                            latest: None,
                        });
                        changed = true;
                    }
                    Err(error) => {
                        // A short-lived dialog may disappear while its WGC
                        // device and item are being created. Skip only targets
                        // that are now ineligible; a live-target setup failure
                        // remains fatal and visible to the presenter.
                        if composite_secondary_window(window, candidate.hwnd)?.is_some() {
                            return Err(error);
                        }
                        changed = true;
                    }
                }
            }
        }
        if !previous.is_empty() {
            changed = true;
        }
        self.secondary = next;
        Ok(changed)
    }
}

fn composite_captured_frame(
    main: &CapturedFrame,
    main_bounds: RECT,
    layers: &[(&CapturedFrame, RECT)],
) -> Result<CapturedFrame, Box<dyn std::error::Error>> {
    let expected_main = usize::try_from(main.width)?
        .checked_mul(usize::try_from(main.height)?)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("main capture frame byte length overflowed")?;
    if main.bgra.len() != expected_main {
        return Err("main capture frame has an invalid byte length".into());
    }
    let mut output = CapturedFrame {
        width: main.width,
        height: main.height,
        bgra: main.bgra.clone(),
    };
    for (layer, bounds) in layers {
        overlay_captured_frame(&mut output, main_bounds, layer, *bounds)?;
    }
    Ok(output)
}

fn overlay_captured_frame(
    destination: &mut CapturedFrame,
    destination_bounds: RECT,
    source: &CapturedFrame,
    source_bounds: RECT,
) -> Result<(), Box<dyn std::error::Error>> {
    let expected_source = usize::try_from(source.width)?
        .checked_mul(usize::try_from(source.height)?)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("secondary capture frame byte length overflowed")?;
    if source.bgra.len() != expected_source {
        return Err("secondary capture frame has an invalid byte length".into());
    }
    let offset_x = i64::from(source_bounds.left) - i64::from(destination_bounds.left);
    let offset_y = i64::from(source_bounds.top) - i64::from(destination_bounds.top);
    for source_y in 0..source.height {
        let destination_y = offset_y + i64::from(source_y);
        if destination_y < 0 || destination_y >= i64::from(destination.height) {
            continue;
        }
        for source_x in 0..source.width {
            let destination_x = offset_x + i64::from(source_x);
            if destination_x < 0 || destination_x >= i64::from(destination.width) {
                continue;
            }
            let source_index = (usize::try_from(source_y)? * usize::try_from(source.width)?
                + usize::try_from(source_x)?)
                * 4;
            let destination_index = (usize::try_from(destination_y)?
                * usize::try_from(destination.width)?
                + usize::try_from(destination_x)?)
                * 4;
            let alpha = u16::from(source.bgra[source_index + 3]);
            if alpha == 0 {
                continue;
            }
            if alpha == 255 {
                destination.bgra[destination_index..destination_index + 4]
                    .copy_from_slice(&source.bgra[source_index..source_index + 4]);
                continue;
            }
            for channel in 0..3 {
                let foreground = u16::from(source.bgra[source_index + channel]);
                let background = u16::from(destination.bgra[destination_index + channel]);
                destination.bgra[destination_index + channel] =
                    u8::try_from((foreground * alpha + background * (255 - alpha) + 127) / 255)?;
            }
            let background_alpha = u16::from(destination.bgra[destination_index + 3]);
            destination.bgra[destination_index + 3] =
                u8::try_from(alpha + (background_alpha * (255 - alpha) + 127) / 255)?;
        }
    }
    Ok(())
}

fn try_get_next_capture_frame(
    pool: &Direct3D11CaptureFramePool,
) -> windows::core::Result<Option<Direct3D11CaptureFrame>> {
    let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: raw is valid output storage for the documented ABI. A successful
    // null result means the frame pool is empty; a non-null result transfers
    // one owned interface reference into the projected WinRT wrapper.
    unsafe {
        (Interface::vtable(pool).TryGetNextFrame)(Interface::as_raw(pool), &mut raw).ok()?;
        Ok((!raw.is_null()).then(|| <Direct3D11CaptureFrame as Interface>::from_raw(raw)))
    }
}

struct CaptureSource {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_handler: TypedEventHandler<Direct3D11CaptureFramePool, windows::core::IInspectable>,
    frame_token: EventRegistrationToken,
    callback_drain: Arc<CaptureCallbackDrain>,
    arrivals: Receiver<()>,
    staging: Option<(u32, u32, ID3D11Texture2D)>,
    pool_size: (u32, u32),
    _apartment: ApartmentGuard,
}

impl CaptureSource {
    fn start(window: &WindowHandle, hwnd: HWND) -> Result<Self, Box<dyn std::error::Error>> {
        let apartment = ApartmentGuard::initialize()?;
        Self::start_initialized(window, hwnd, apartment)
    }

    fn start_initialized(
        window: &WindowHandle,
        hwnd: HWND,
        apartment: ApartmentGuard,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err("Windows Graphics Capture is not supported by this guest".into());
        }
        validate_capture_target(window, hwnd)?;
        let (device, context) = create_d3d_device()?;
        let dxgi: IDXGIDevice = device.cast()?;
        // SAFETY: dxgi is a live D3D11 device interface and the returned
        // inspectable is immediately cast to its documented WinRT interface.
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
        let winrt_device: IDirect3DDevice = inspectable.cast()?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item = create_capture_item_for_window(window, hwnd, &interop)?;
        let size = item.Size()?;
        validate_capture_size(size)?;
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            FRAME_POOL_BUFFERS,
            size,
        )?;
        let (sender, arrivals) = mpsc::sync_channel(1);
        let callback_drain = Arc::new(CaptureCallbackDrain::default());
        let callback_entry = Arc::clone(&callback_drain);
        let handler = TypedEventHandler::new(move |_pool, _args| {
            let Some(_invocation) = callback_entry.enter() else {
                return Ok(());
            };
            let _ = sender.try_send(());
            Ok(())
        });
        let frame_token = frame_pool.FrameArrived(&handler)?;
        let session = frame_pool.CreateCaptureSession(&item)?;
        enable_secondary_windows(&session)?;
        // The presenter keeps the host X11 cursor visible and forwards its
        // coordinates. Capturing the guest cursor as pixels would render a
        // second delayed cursor in the seamless window.
        session.SetIsCursorCaptureEnabled(false)?;
        session.StartCapture()?;
        Ok(Self {
            device,
            context,
            winrt_device,
            frame_pool,
            session,
            frame_handler: handler,
            frame_token,
            callback_drain,
            arrivals,
            staging: None,
            pool_size: (u32::try_from(size.Width)?, u32::try_from(size.Height)?),
            _apartment: apartment,
        })
    }

    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<CapturedFrame>, Box<dyn std::error::Error>> {
        // The bounded notification channel deliberately coalesces wakeups.
        // Poll the pool first on every pass so a newer frame whose callback
        // observed an already-full channel cannot remain stranded after the
        // older wake and frame have been consumed.
        let mut frame = try_get_next_capture_frame(&self.frame_pool)?;
        if frame.is_none() {
            match self.arrivals.recv_timeout(timeout) {
                Ok(()) => frame = try_get_next_capture_frame(&self.frame_pool)?,
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("Windows capture notification channel closed".into())
                }
            }
        }
        let Some(frame) = frame else {
            // A stale coalesced wake can outlive the frames discarded by
            // Recreate. The next real FrameArrived event will wake us again.
            return Ok(None);
        };
        let size = frame.ContentSize()?;
        validate_capture_size(size)?;
        let observed_size = (u32::try_from(size.Width)?, u32::try_from(size.Height)?);
        if self.pool_size != observed_size {
            frame.Close()?;
            while self.arrivals.try_recv().is_ok() {}
            self.frame_pool.Recreate(
                &self.winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                FRAME_POOL_BUFFERS,
                size,
            )?;
            self.pool_size = observed_size;
            self.staging = None;
            return Ok(None);
        }
        let captured = self.copy_surface(&frame.Surface()?, observed_size.0, observed_size.1);
        frame.Close()?;
        captured
    }

    fn copy_surface(
        &mut self,
        surface: &windows::Graphics::DirectX::Direct3D11::IDirect3DSurface,
        width: u32,
        height: u32,
    ) -> Result<Option<CapturedFrame>, Box<dyn std::error::Error>> {
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        // SAFETY: IDirect3DSurface documents IDirect3DDxgiInterfaceAccess as
        // the route to its backing D3D11 texture.
        let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: source_desc is valid writable storage for GetDesc.
        unsafe { source.GetDesc(&mut source_desc) };
        if source_desc.Width < width || source_desc.Height < height {
            // WGC can publish the new ContentSize one notification before the
            // recreated frame pool supplies a matching D3D texture. Dropping
            // that transition frame is required; treating it as fatal tears
            // down the seamless session during maximize or rapid resize.
            return Ok(None);
        }
        let recreate = self
            .staging
            .as_ref()
            .map(|entry| entry.0 != width || entry.1 != height)
            .unwrap_or(true);
        if recreate {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: source_desc.Format,
                SampleDesc: source_desc.SampleDesc,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut texture = None;
            // SAFETY: desc is initialized, initial data is absent, and texture
            // points at valid Option storage populated by D3D11.
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut texture))?;
            }
            self.staging = Some((
                width,
                height,
                texture.ok_or("D3D11 returned no staging texture")?,
            ));
        }
        let staging = &self.staging.as_ref().expect("staging texture exists").2;
        let staging_resource: ID3D11Resource = staging.cast()?;
        let source_resource: ID3D11Resource = source.cast()?;
        let source_box = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        // SAFETY: Both resources use the same format. source_box is bounded by
        // the source descriptor and exactly matches the staging dimensions.
        unsafe {
            self.context.CopySubresourceRegion(
                &staging_resource,
                0,
                0,
                0,
                0,
                &source_resource,
                0,
                Some(&source_box),
            )
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: mapped is valid output storage and the staging resource was
        // created with CPU read access.
        unsafe {
            self.context
                .Map(&staging_resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let copied = copy_mapped_bgra(&mapped, width, height);
        // SAFETY: This balances the successful Map call for subresource zero.
        unsafe { self.context.Unmap(&staging_resource, 0) };
        Ok(Some(CapturedFrame {
            width,
            height,
            bgra: copied?,
        }))
    }
}

fn create_capture_item_for_window(
    window: &WindowHandle,
    hwnd: HWND,
    interop: &IGraphicsCaptureItemInterop,
) -> Result<GraphicsCaptureItem, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + CAPTURE_ITEM_BIND_TIMEOUT;
    loop {
        // Revalidate immediately before every bind so a destroyed or
        // cross-process-reused numeric HWND is never handed to WGC.
        validate_capture_target(window, hwnd)?;
        // SAFETY: validation proves the HWND is live and belongs to the
        // process object pinned by WindowHandle at this binding boundary.
        match unsafe { interop.CreateForWindow(hwnd) } {
            Ok(item) => return Ok(item),
            Err(error) if error.code() == INVALID_ARGUMENT && Instant::now() < deadline => {
                // Windows can retain the previous capture binding briefly
                // after a window closes, especially when the same process
                // creates a replacement HWND immediately. Retry only this
                // transient result while the exact target identity is live.
                validate_capture_target(window, hwnd)?;
                thread::sleep(WINDOW_DISCOVERY_INTERVAL);
            }
            Err(error) => {
                return Err(format!(
                    "could not bind Windows Graphics Capture to the selected HWND: {error}"
                )
                .into())
            }
        }
    }
}

#[derive(Default)]
struct CaptureCallbackDrain {
    closing: AtomicBool,
    active: AtomicUsize,
    wait_lock: Mutex<()>,
    drained: Condvar,
}

impl CaptureCallbackDrain {
    fn enter(self: &Arc<Self>) -> Option<CaptureCallbackInvocation> {
        self.active.fetch_add(1, Ordering::AcqRel);
        if self.closing.load(Ordering::Acquire) {
            self.finish_invocation();
            return None;
        }
        Some(CaptureCallbackInvocation {
            drain: Arc::clone(self),
        })
    }

    fn begin_close(&self) {
        self.closing.store(true, Ordering::Release);
    }

    fn wait_for_callbacks(&self) {
        let mut wait = lock_capture_callback_state(&self.wait_lock);
        while self.active.load(Ordering::Acquire) != 0 {
            wait = self
                .drained
                .wait(wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn finish_invocation(&self) {
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _wait = lock_capture_callback_state(&self.wait_lock);
            self.drained.notify_all();
        }
    }
}

struct CaptureCallbackInvocation {
    drain: Arc<CaptureCallbackDrain>,
}

impl Drop for CaptureCallbackInvocation {
    fn drop(&mut self) {
        self.drain.finish_invocation();
    }
}

fn lock_capture_callback_state<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn enable_secondary_windows(
    session: &GraphicsCaptureSession,
) -> Result<(), Box<dyn std::error::Error>> {
    let secondary: IGraphicsCaptureSession6 = session.cast().map_err(|_| {
        "seamless GUI menus and popup windows require Windows 11 24H2 (build 26100) or newer"
    })?;
    // SAFETY: QueryInterface above proved that this runtime implements the
    // documented IGraphicsCaptureSession6 ABI; the interface remains alive for
    // the synchronous property setter call.
    unsafe {
        (Interface::vtable(&secondary).set_include_secondary_windows)(
            Interface::as_raw(&secondary),
            true,
        )
        .ok()?;
        let mut enabled = false;
        (Interface::vtable(&secondary).include_secondary_windows)(
            Interface::as_raw(&secondary),
            &mut enabled,
        )
        .ok()?;
        if !enabled {
            return Err("Windows did not enable secondary-window capture".into());
        }
    }
    Ok(())
}

impl Drop for CaptureSource {
    fn drop(&mut self) {
        self.callback_drain.begin_close();
        let _ = self.frame_pool.RemoveFrameArrived(self.frame_token);
        // Close the free-threaded frame pool before the session, matching the
        // documented WGC sample teardown. The worker which raises FrameArrived
        // must be stopped before this thread releases the final WinRT apartment
        // reference, otherwise GraphicsCapture.dll can unload beneath it.
        let _ = self.frame_pool.Close();
        let _ = self.session.Close();
        self.callback_drain.wait_for_callbacks();
        // Keep our delegate reference alive through revocation, both Close
        // calls, and the callback drain. Event dispatch owns any reference for
        // an invocation which was already in flight.
        let _ = &self.frame_handler;
    }
}

/// Keeps one thread's WinRT apartment initialized. The desktop companion also
/// retains a guard on its main thread for the process lifetime because
/// windows-rs caches activation factories globally; fully uninitializing WinRT
/// between GUI sessions would leave those generated caches pointing into an
/// unloaded runtime module.
#[must_use]
pub(super) struct ApartmentGuard {
    _not_send: std::marker::PhantomData<Rc<()>>,
}

impl ApartmentGuard {
    pub(super) fn initialize() -> windows::core::Result<Self> {
        // SAFETY: The desktop companion owns the calling thread and the guard
        // balances every successful initialization after COM fields are freed.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        Ok(Self {
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for ApartmentGuard {
    fn drop(&mut self) {
        // SAFETY: This is the same thread that constructed the guard. The guard
        // is the last CaptureSource field, so its WinRT interfaces drop first.
        unsafe { RoUninitialize() };
    }
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), Box<dyn std::error::Error>> {
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let mut context = None;
        // SAFETY: Output pointers refer to live Option storage, no adapter or
        // software module is supplied, and D3D11 owns returned COM interfaces.
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        if result.is_ok() {
            return Ok((
                device.ok_or("D3D11 returned no device")?,
                context.ok_or("D3D11 returned no immediate context")?,
            ));
        }
    }
    Err("could not create a hardware or WARP D3D11 capture device".into())
}

fn validate_capture_size(size: SizeInt32) -> Result<(), Box<dyn std::error::Error>> {
    if size.Width <= 0
        || size.Height <= 0
        || size.Width > i32::try_from(MAX_GUI_WINDOW_DIMENSION).expect("bound fits i32")
        || size.Height > i32::try_from(MAX_GUI_WINDOW_DIMENSION).expect("bound fits i32")
    {
        return Err(format!(
            "captured window dimensions {}x{} are outside the supported range",
            size.Width, size.Height
        )
        .into());
    }
    let bytes = usize::try_from(size.Width)?
        .checked_mul(usize::try_from(size.Height)?)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("captured window byte length overflowed")?;
    if bytes > MAX_GUI_FRAME_BYTES {
        return Err("captured window exceeds the protocol frame limit".into());
    }
    Ok(())
}

fn copy_mapped_bgra(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let row_bytes = usize::try_from(width)?
        .checked_mul(4)
        .ok_or("captured row byte length overflowed")?;
    let row_pitch = usize::try_from(mapped.RowPitch)?;
    if mapped.pData.is_null() || row_pitch < row_bytes {
        return Err("D3D11 returned an invalid mapped capture surface".into());
    }
    let height = usize::try_from(height)?;
    let mut output = Vec::with_capacity(
        row_bytes
            .checked_mul(height)
            .ok_or("captured frame byte length overflowed")?,
    );
    for row in 0..height {
        // SAFETY: D3D11 guarantees RowPitch bytes for each of Height rows while
        // the resource remains mapped; row_bytes is no larger than RowPitch.
        let source = unsafe {
            std::slice::from_raw_parts((mapped.pData as *const u8).add(row * row_pitch), row_bytes)
        };
        output.extend_from_slice(source);
    }
    Ok(output)
}

fn window_title(hwnd: HWND) -> windows::core::Result<String> {
    // SAFETY: The HWND came from EnumWindows and the APIs tolerate a window
    // disappearing between calls.
    let length = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let capacity = length.saturating_add(1).min(MAX_WINDOW_TITLE_UNITS);
    let mut buffer = vec![0u16; capacity.max(1)];
    // SAFETY: buffer is writable for its complete advertised length.
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    buffer.truncate(usize::try_from(copied.max(0)).unwrap_or(0));
    Ok(String::from_utf16_lossy(&buffer))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaptureBounds {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}

fn capture_to_screen(
    hwnd: HWND,
    capture_size: (u32, u32),
    x: u32,
    y: u32,
) -> windows::core::Result<Option<(i32, i32)>> {
    let Some(bounds) = capture_bounds(hwnd, capture_size)? else {
        return Ok(None);
    };
    // Coordinates can also arrive from the host's immediately previous frame
    // while resize notifications cross in flight. Treat them as stale and
    // fail closed without turning a harmless geometry race into a disconnect.
    Ok(map_capture_point(bounds, capture_size, x, y))
}

fn capture_bounds(
    hwnd: HWND,
    capture_size: (u32, u32),
) -> windows::core::Result<Option<CaptureBounds>> {
    Ok(capture_bounds_from_rect(
        window_frame_rect(hwnd)?,
        capture_size,
    ))
}

fn window_frame_rect(hwnd: HWND) -> windows::core::Result<RECT> {
    let mut rectangle = RECT::default();
    // SAFETY: rectangle is valid writable storage and hwnd came from
    // EnumWindows. DWM extended frame bounds are physical visible pixels, which
    // match WGC coordinates and exclude GetWindowRect's invisible resize border.
    let dwm_result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rectangle as *mut RECT).cast(),
            u32::try_from(std::mem::size_of::<RECT>()).expect("RECT size fits u32"),
        )
    };
    if dwm_result.is_err() {
        // GetWindowRect is the documented fallback when DWM extended bounds are
        // unavailable. The caller either validates it against WGC dimensions
        // or uses it only for bounded secondary-window intersection checks.
        // SAFETY: rectangle is writable storage and hwnd came from EnumWindows.
        unsafe { GetWindowRect(hwnd, &mut rectangle)? };
    }
    Ok(rectangle)
}

fn resize_outer_extent(
    hwnd: HWND,
    visible_width: u32,
    visible_height: u32,
) -> windows::core::Result<(i32, i32)> {
    let mut outer = RECT::default();
    // SAFETY: outer is valid writable storage and the caller pins hwnd to the
    // launched process before and after this read-only geometry observation.
    unsafe { GetWindowRect(hwnd, &mut outer)? };
    let visible = window_frame_rect(hwnd)?;
    resize_outer_extent_from_rects(outer, visible, visible_width, visible_height).ok_or_else(|| {
        Error::new(
            INVALID_ARGUMENT,
            "window frame bounds are not usable for a visible-frame resize",
        )
    })
}

fn resize_origin_within_monitor_work_area(
    hwnd: HWND,
    width: i32,
    height: i32,
) -> windows::core::Result<(i32, i32)> {
    let mut outer = RECT::default();
    // SAFETY: outer is writable storage and the caller pins hwnd to the exact
    // launched process before this read-only geometry query.
    unsafe { GetWindowRect(hwnd, &mut outer)? };
    // SAFETY: hwnd is live and MONITOR_DEFAULTTONEAREST always requests a
    // concrete monitor even when the current rectangle is partly off-screen.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.0 == 0 {
        return Err(Error::from_win32());
    }
    let mut monitor_info = MONITORINFO {
        cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>())
            .expect("MONITORINFO size fits u32"),
        ..MONITORINFO::default()
    };
    // SAFETY: monitor is a live handle returned above and monitor_info is
    // correctly sized writable storage for the duration of the call.
    if !unsafe { GetMonitorInfoW(monitor, &mut monitor_info) }.as_bool() {
        return Err(Error::from_win32());
    }
    clamp_window_origin_to_work_area(outer, monitor_info.rcWork, width, height).ok_or_else(|| {
        Error::new(
            INVALID_ARGUMENT,
            "monitor work area is not usable for a GUI window resize",
        )
    })
}

fn clamp_window_origin_to_work_area(
    outer: RECT,
    work: RECT,
    width: i32,
    height: i32,
) -> Option<(i32, i32)> {
    if width <= 0 || height <= 0 || work.right <= work.left || work.bottom <= work.top {
        return None;
    }
    let max_x = work.right.checked_sub(width)?;
    let max_y = work.bottom.checked_sub(height)?;
    let x = if max_x < work.left {
        work.left
    } else {
        outer.left.clamp(work.left, max_x)
    };
    let y = if max_y < work.top {
        work.top
    } else {
        outer.top.clamp(work.top, max_y)
    };
    Some((x, y))
}

fn resize_outer_extent_from_rects(
    outer: RECT,
    visible: RECT,
    visible_width: u32,
    visible_height: u32,
) -> Option<(i32, i32)> {
    let left = i64::from(visible.left).checked_sub(i64::from(outer.left))?;
    let top = i64::from(visible.top).checked_sub(i64::from(outer.top))?;
    let right = i64::from(outer.right).checked_sub(i64::from(visible.right))?;
    let bottom = i64::from(outer.bottom).checked_sub(i64::from(visible.bottom))?;
    for inset in [left, top, right, bottom] {
        if !(0..=MAX_CAPTURE_BOUNDS_DELTA).contains(&inset) {
            return None;
        }
    }
    let width = i64::from(visible_width)
        .checked_add(left)?
        .checked_add(right)?;
    let height = i64::from(visible_height)
        .checked_add(top)?
        .checked_add(bottom)?;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some((i32::try_from(width).ok()?, i32::try_from(height).ok()?))
}

fn capture_bounds_from_rect(rectangle: RECT, capture_size: (u32, u32)) -> Option<CaptureBounds> {
    let width = i64::from(rectangle.right) - i64::from(rectangle.left);
    let height = i64::from(rectangle.bottom) - i64::from(rectangle.top);
    if width <= 0 || height <= 0 || capture_size.0 == 0 || capture_size.1 == 0 {
        return None;
    }
    if (width - i64::from(capture_size.0)).abs() > MAX_CAPTURE_BOUNDS_DELTA
        || (height - i64::from(capture_size.1)).abs() > MAX_CAPTURE_BOUNDS_DELTA
    {
        return None;
    }
    Some(CaptureBounds {
        left: rectangle.left,
        top: rectangle.top,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
    })
}

fn map_capture_point(
    bounds: CaptureBounds,
    capture_size: (u32, u32),
    x: u32,
    y: u32,
) -> Option<(i32, i32)> {
    if capture_size.0 == 0 || capture_size.1 == 0 || x >= capture_size.0 || y >= capture_size.1 {
        return None;
    }
    let scaled_x = u64::from(x).checked_mul(u64::from(bounds.width))? / u64::from(capture_size.0);
    let scaled_y = u64::from(y).checked_mul(u64::from(bounds.height))? / u64::from(capture_size.1);
    let screen_x = i64::from(bounds.left).checked_add(i64::try_from(scaled_x).ok()?)?;
    let screen_y = i64::from(bounds.top).checked_add(i64::try_from(scaled_y).ok()?)?;
    Some((i32::try_from(screen_x).ok()?, i32::try_from(screen_y).ok()?))
}

fn post(hwnd: HWND, message: u32, wparam: usize, lparam: isize) -> windows::core::Result<()> {
    // SAFETY: HWND is owned by the launched child. Messages and packed values
    // use documented Win32 formats and PostMessageW copies them synchronously.
    unsafe { PostMessageW(hwnd, message, WPARAM(wparam), LPARAM(lparam)) }
}

#[cfg(test)]
mod tests {
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

    fn select_exact(
        candidates: &[WindowCandidate],
        expected: ProcessKey,
    ) -> Option<WindowCandidate> {
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
}
