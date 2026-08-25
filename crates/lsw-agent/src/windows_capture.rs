// SPDX-License-Identifier: GPL-3.0-or-later

//! Documented Windows Graphics Capture and HWND input bridge for Slice 4.

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::process::Child;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    GuiInputEvent, GuiPointerButton, GuiWindowReady, GuiWindowResize, MAX_GUI_FRAME_BYTES,
    MAX_GUI_WINDOW_DIMENSION,
};
use windows::core::{factory, Interface};
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::SystemServices::{
    MK_LBUTTON, MK_MBUTTON, MK_RBUTTON, MK_XBUTTON1, MK_XBUTTON2,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW, SetForegroundWindow,
    SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, WM_CLOSE, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

const WINDOW_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const WINDOW_DISCOVERY_INTERVAL: Duration = Duration::from_millis(25);
const FRAME_POOL_BUFFERS: i32 = 2;
const MAX_WINDOW_TITLE_UNITS: usize = 4096;

pub(super) struct CapturedFrame {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bgra: Vec<u8>,
}

pub(super) struct WindowHandle(HWND);

impl WindowHandle {
    pub(super) fn id(&self) -> u64 {
        u64::try_from(self.0 .0 as usize).expect("HWND fits u64")
    }

    pub(super) fn ready(
        &self,
        process_id: u32,
        width: u32,
        height: u32,
    ) -> Result<GuiWindowReady, Box<dyn std::error::Error>> {
        Ok(GuiWindowReady {
            process_id,
            window_id: self.id(),
            width,
            height,
            title: window_title(self.0)?,
        })
    }

    pub(super) fn is_open(&self) -> bool {
        // SAFETY: The HWND value came from EnumWindows. IsWindow accepts stale
        // handles and returns false, so no pointed-to memory is dereferenced.
        unsafe { IsWindow(self.0).as_bool() }
    }

    pub(super) fn input(&self, event: GuiInputEvent) -> windows::core::Result<()> {
        match event {
            GuiInputEvent::Focus { focused } => {
                if focused {
                    // SAFETY: SetForegroundWindow validates the HWND. Failure to
                    // win foreground activation is intentionally non-fatal.
                    let _ = unsafe { SetForegroundWindow(self.0) };
                }
                Ok(())
            }
            GuiInputEvent::Key {
                virtual_key,
                mut scan_code,
                pressed,
                extended,
            } => {
                if scan_code == 0 {
                    // SAFETY: MapVirtualKeyW is a pure table lookup for the
                    // active desktop keyboard layout.
                    scan_code = unsafe { MapVirtualKeyW(u32::from(virtual_key), MAPVK_VK_TO_VSC) }
                        .try_into()
                        .unwrap_or(0);
                }
                let mut bits = isize::try_from(scan_code).expect("scan code fits isize") << 16;
                if extended {
                    bits |= 1 << 24;
                }
                if !pressed {
                    bits |= (1 << 30) | (1 << 31);
                }
                post(
                    self.0,
                    if pressed { WM_KEYDOWN } else { WM_KEYUP },
                    usize::from(virtual_key),
                    bits,
                )
            }
            GuiInputEvent::PointerMove { x, y } => {
                let (x, y) = capture_to_client(self.0, x, y);
                post(self.0, WM_MOUSEMOVE, 0, point_lparam(x, y))
            }
            GuiInputEvent::PointerButton {
                button,
                pressed,
                x,
                y,
            } => {
                let (x, y) = capture_to_client(self.0, x, y);
                let (down, up, state, xbutton) = match button {
                    GuiPointerButton::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON.0, 0),
                    GuiPointerButton::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON.0, 0),
                    GuiPointerButton::Middle => (WM_MBUTTONDOWN, WM_MBUTTONUP, MK_MBUTTON.0, 0),
                    GuiPointerButton::Back => (WM_XBUTTONDOWN, WM_XBUTTONUP, MK_XBUTTON1.0, 1),
                    GuiPointerButton::Forward => (WM_XBUTTONDOWN, WM_XBUTTONUP, MK_XBUTTON2.0, 2),
                };
                let low = if pressed { state } else { 0 };
                let wparam = low | (xbutton << 16);
                post(
                    self.0,
                    if pressed { down } else { up },
                    usize::try_from(wparam).expect("mouse button state fits usize"),
                    point_lparam(x, y),
                )
            }
            GuiInputEvent::PointerWheel {
                delta,
                horizontal,
                x,
                y,
            } => {
                let (screen_x, screen_y) = capture_to_screen(self.0, x, y);
                let wparam = usize::from(u16::from_ne_bytes(delta.to_ne_bytes())) << 16;
                post(
                    self.0,
                    if horizontal {
                        WM_MOUSEHWHEEL
                    } else {
                        WM_MOUSEWHEEL
                    },
                    wparam,
                    point_lparam(screen_x, screen_y),
                )
            }
        }
    }

    pub(super) fn resize(&self, resize: GuiWindowResize) -> windows::core::Result<()> {
        let width = i32::try_from(resize.width).expect("validated GUI width fits i32");
        let height = i32::try_from(resize.height).expect("validated GUI height fits i32");
        // SAFETY: SetWindowPos validates the HWND and receives bounded positive
        // dimensions decoded by GuiWindowResize.
        unsafe {
            SetWindowPos(
                self.0,
                HWND(0),
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
    }

    pub(super) fn close(&self) -> windows::core::Result<()> {
        post(self.0, WM_CLOSE, 0, 0)
    }
}

pub(super) fn find_process_window(
    process_id: u32,
    child: &mut Child,
) -> Result<WindowHandle, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + WINDOW_DISCOVERY_TIMEOUT;
    loop {
        if let Some(hwnd) = enumerate_process_window(process_id)? {
            return Ok(WindowHandle(hwnd));
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!(
                "GUI process exited with {status} before creating a visible top-level window"
            )
            .into());
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the GUI process to create a visible window".into());
        }
        thread::sleep(WINDOW_DISCOVERY_INTERVAL);
    }
}

struct WindowSearch {
    process_id: u32,
    result: HWND,
}

fn enumerate_process_window(process_id: u32) -> windows::core::Result<Option<HWND>> {
    let mut search = WindowSearch {
        process_id,
        result: HWND(0),
    };
    // SAFETY: LPARAM contains a valid WindowSearch pointer for this synchronous
    // EnumWindows invocation. The callback neither stores nor frees it.
    unsafe {
        EnumWindows(
            Some(enum_window),
            LPARAM((&mut search as *mut WindowSearch) as isize),
        )?;
    }
    Ok((search.result.0 != 0).then_some(search.result))
}

unsafe extern "system" fn enum_window(hwnd: HWND, parameter: LPARAM) -> BOOL {
    // SAFETY: enumerate_process_window passes this exact pointer and EnumWindows
    // invokes callbacks synchronously before the stack allocation is dropped.
    let search = unsafe { &mut *(parameter.0 as *mut WindowSearch) };
    // SAFETY: All queried functions accept an HWND supplied by EnumWindows and
    // the process-id output points at live stack storage.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut observed = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut observed));
        if observed == search.process_id {
            search.result = hwnd;
            return BOOL(0);
        }
    }
    BOOL(1)
}

pub(super) struct CaptureSession {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_token: EventRegistrationToken,
    arrivals: Receiver<()>,
    staging: Option<(u32, u32, ID3D11Texture2D)>,
    pool_size: (u32, u32),
    _apartment: ApartmentGuard,
}

impl CaptureSession {
    pub(super) fn start(window: &WindowHandle) -> Result<Self, Box<dyn std::error::Error>> {
        let apartment = ApartmentGuard::initialize()?;
        Self::start_initialized(window, apartment)
    }

    fn start_initialized(
        window: &WindowHandle,
        apartment: ApartmentGuard,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err("Windows Graphics Capture is not supported by this guest".into());
        }
        let (device, context) = create_d3d_device()?;
        let dxgi: IDXGIDevice = device.cast()?;
        // SAFETY: dxgi is a live D3D11 device interface and the returned
        // inspectable is immediately cast to its documented WinRT interface.
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
        let winrt_device: IDirect3DDevice = inspectable.cast()?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: The HWND came from EnumWindows and remains owned by the child.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(window.0)? };
        let size = item.Size()?;
        validate_capture_size(size)?;
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            FRAME_POOL_BUFFERS,
            size,
        )?;
        let (sender, arrivals) = mpsc::sync_channel(1);
        let handler = TypedEventHandler::new(move |_pool, _args| {
            let _ = sender.try_send(());
            Ok(())
        });
        let frame_token = frame_pool.FrameArrived(&handler)?;
        let session = frame_pool.CreateCaptureSession(&item)?;
        session.SetIsCursorCaptureEnabled(true)?;
        session.StartCapture()?;
        Ok(Self {
            device,
            context,
            winrt_device,
            frame_pool,
            session,
            frame_token,
            arrivals,
            staging: None,
            pool_size: (u32::try_from(size.Width)?, u32::try_from(size.Height)?),
            _apartment: apartment,
        })
    }

    pub(super) fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<CapturedFrame>, Box<dyn std::error::Error>> {
        match self.arrivals.recv_timeout(timeout) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Windows capture notification channel closed".into())
            }
        }
        let frame = self.frame_pool.TryGetNextFrame()?;
        let size = frame.ContentSize()?;
        validate_capture_size(size)?;
        let observed_size = (u32::try_from(size.Width)?, u32::try_from(size.Height)?);
        if self.pool_size != observed_size {
            frame.Close()?;
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
        Ok(Some(captured?))
    }

    fn copy_surface(
        &mut self,
        surface: &windows::Graphics::DirectX::Direct3D11::IDirect3DSurface,
        width: u32,
        height: u32,
    ) -> Result<CapturedFrame, Box<dyn std::error::Error>> {
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        // SAFETY: IDirect3DSurface documents IDirect3DDxgiInterfaceAccess as
        // the route to its backing D3D11 texture.
        let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: source_desc is valid writable storage for GetDesc.
        unsafe { source.GetDesc(&mut source_desc) };
        if source_desc.Width < width || source_desc.Height < height {
            return Err("Windows capture surface is smaller than its content size".into());
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
        Ok(CapturedFrame {
            width,
            height,
            bgra: copied?,
        })
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        let _ = self.frame_pool.RemoveFrameArrived(self.frame_token);
        let _ = self.session.Close();
        let _ = self.frame_pool.Close();
    }
}

struct ApartmentGuard;

impl ApartmentGuard {
    fn initialize() -> windows::core::Result<Self> {
        // SAFETY: The desktop companion owns the calling thread and the guard
        // balances every successful initialization after COM fields are freed.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        Ok(Self)
    }
}

impl Drop for ApartmentGuard {
    fn drop(&mut self) {
        // SAFETY: This is the same thread that constructed the guard. The guard
        // is the last CaptureSession field, so its WinRT interfaces drop first.
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

fn capture_to_client(hwnd: HWND, x: u32, y: u32) -> (i32, i32) {
    let mut window = RECT::default();
    let mut client = RECT::default();
    let mut client_origin = POINT::default();
    // SAFETY: All pointers refer to initialized stack storage and HWND may be
    // stale, in which case the zero defaults provide bounded fallback values.
    unsafe {
        let _ = GetWindowRect(hwnd, &mut window);
        let _ = GetClientRect(hwnd, &mut client);
        let _ = ClientToScreen(hwnd, &mut client_origin);
    }
    let client_x = i64::from(x) - i64::from(client_origin.x - window.left);
    let client_y = i64::from(y) - i64::from(client_origin.y - window.top);
    (
        clamp_coordinate(client_x, client.right - client.left),
        clamp_coordinate(client_y, client.bottom - client.top),
    )
}

fn capture_to_screen(hwnd: HWND, x: u32, y: u32) -> (i32, i32) {
    let mut window = RECT::default();
    // SAFETY: The rectangle points at initialized stack storage and a stale
    // HWND simply leaves the zero fallback in place.
    unsafe {
        let _ = GetWindowRect(hwnd, &mut window);
    }
    (
        i64::from(window.left)
            .saturating_add(i64::from(x))
            .clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i32,
        i64::from(window.top)
            .saturating_add(i64::from(y))
            .clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i32,
    )
}

fn clamp_coordinate(value: i64, extent: i32) -> i32 {
    value.clamp(0, i64::from(extent.saturating_sub(1).max(0))) as i32
}

fn point_lparam(x: i32, y: i32) -> isize {
    let x = u32::from(u16::from_ne_bytes((x as i16).to_ne_bytes()));
    let y = u32::from(u16::from_ne_bytes((y as i16).to_ne_bytes()));
    isize::try_from(x | (y << 16)).expect("packed pointer coordinates fit isize")
}

fn post(hwnd: HWND, message: u32, wparam: usize, lparam: isize) -> windows::core::Result<()> {
    // SAFETY: HWND is owned by the launched child. Messages and packed values
    // use documented Win32 formats and PostMessageW copies them synchronously.
    unsafe { PostMessageW(hwnd, message, WPARAM(wparam), LPARAM(lparam)) }
}
