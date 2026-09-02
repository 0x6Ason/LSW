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

mod activation;
mod capture;
mod composite;
mod discovery;
mod input;

pub(super) use activation::*;
pub(super) use capture::*;
pub(super) use composite::*;
pub(super) use discovery::*;
pub(super) use input::*;

#[cfg(test)]
mod tests;
