// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(crate) struct CompositeSecondaryCapture {
    window: CompositeSecondaryWindow,
    source: CaptureSource,
    latest: Option<CapturedFrame>,
}

pub(crate) struct CaptureSession {
    main: CaptureSource,
    secondary: Vec<CompositeSecondaryCapture>,
    latest_main: Option<CapturedFrame>,
}

impl CaptureSession {
    pub(crate) fn start(window: &WindowHandle) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            main: CaptureSource::start(window, window.hwnd)?,
            secondary: Vec::new(),
            latest_main: None,
        })
    }

    pub(crate) fn next_frame(
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

pub(crate) fn composite_captured_frame(
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

pub(crate) fn overlay_captured_frame(
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

pub(crate) fn try_get_next_capture_frame(
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

pub(crate) struct CaptureSource {
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

pub(crate) fn create_capture_item_for_window(
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
pub(crate) struct CaptureCallbackDrain {
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

pub(crate) struct CaptureCallbackInvocation {
    drain: Arc<CaptureCallbackDrain>,
}

impl Drop for CaptureCallbackInvocation {
    fn drop(&mut self) {
        self.drain.finish_invocation();
    }
}

pub(crate) fn lock_capture_callback_state<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn enable_secondary_windows(
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
pub(crate) struct ApartmentGuard {
    _not_send: std::marker::PhantomData<Rc<()>>,
}

impl ApartmentGuard {
    pub(crate) fn initialize() -> windows::core::Result<Self> {
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

pub(crate) fn create_d3d_device(
) -> Result<(ID3D11Device, ID3D11DeviceContext), Box<dyn std::error::Error>> {
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

pub(crate) fn validate_capture_size(size: SizeInt32) -> Result<(), Box<dyn std::error::Error>> {
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

pub(crate) fn copy_mapped_bgra(
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

pub(crate) fn window_title(hwnd: HWND) -> windows::core::Result<String> {
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
pub(crate) struct CaptureBounds {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

pub(crate) fn capture_to_screen(
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

pub(crate) fn capture_bounds(
    hwnd: HWND,
    capture_size: (u32, u32),
) -> windows::core::Result<Option<CaptureBounds>> {
    Ok(capture_bounds_from_rect(
        window_frame_rect(hwnd)?,
        capture_size,
    ))
}

pub(crate) fn window_frame_rect(hwnd: HWND) -> windows::core::Result<RECT> {
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

pub(crate) fn resize_outer_extent(
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

pub(crate) fn resize_origin_within_monitor_work_area(
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

pub(crate) fn clamp_window_origin_to_work_area(
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

pub(crate) fn resize_outer_extent_from_rects(
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

pub(crate) fn capture_bounds_from_rect(
    rectangle: RECT,
    capture_size: (u32, u32),
) -> Option<CaptureBounds> {
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

pub(crate) fn map_capture_point(
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

pub(crate) fn post(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> windows::core::Result<()> {
    // SAFETY: HWND is owned by the launched child. Messages and packed values
    // use documented Win32 formats and PostMessageW copies them synchronously.
    unsafe { PostMessageW(hwnd, message, WPARAM(wparam), LPARAM(lparam)) }
}
