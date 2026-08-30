// SPDX-License-Identifier: GPL-3.0-or-later

use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Instant;

use lsw_core::{
    GuiWindowAction, GuiWindowDamage, GuiWindowReady, MAX_GUI_FRAME_BYTES, MAX_GUI_WINDOW_DIMENSION,
};
use softbuffer::Surface;
use winit::dpi::PhysicalSize;
use winit::window::{ResizeDirection, Window};

use super::{trace_gui, validate_host_size, DragGrant};

pub(super) fn resize_surface(
    surface: &mut Surface<Rc<Window>, Rc<Window>>,
    size: PhysicalSize<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_host_size(size)?;
    let width = NonZeroU32::new(size.width).ok_or("Wayland surface width is zero")?;
    let height = NonZeroU32::new(size.height).ok_or("Wayland surface height is zero")?;
    surface.resize(width, height)?;
    Ok(())
}

pub(super) fn present_frame(
    surface: &mut Surface<Rc<Window>, Rc<Window>>,
    state: &FrameState,
    size: PhysicalSize<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_host_size(size)?;
    let width = usize::try_from(size.width)?;
    let height = usize::try_from(size.height)?;
    let expected = width
        .checked_mul(height)
        .ok_or("Wayland render buffer size overflowed")?;
    let mut buffer = surface.buffer_mut()?;
    if buffer.len() != expected {
        return Err("Wayland compositor returned an unexpected buffer size".into());
    }
    render_scaled(state, width, height, &mut buffer)?;
    buffer.present()?;
    Ok(())
}

pub(super) fn render_scaled(
    state: &FrameState,
    width: usize,
    height: usize,
    destination: &mut [u32],
) -> Result<(), Box<dyn std::error::Error>> {
    let expected = width
        .checked_mul(height)
        .ok_or("Wayland render buffer size overflowed")?;
    if destination.len() != expected || state.width == 0 || state.height == 0 {
        return Err("Wayland render buffer dimensions are inconsistent".into());
    }
    let viewport = fit_viewport(
        (u32::try_from(width)?, u32::try_from(height)?),
        (state.ready.width, state.ready.height),
    );
    let viewport_x = usize::try_from(viewport.x)?;
    let viewport_y = usize::try_from(viewport.y)?;
    let viewport_width = usize::try_from(viewport.width)?;
    let viewport_height = usize::try_from(viewport.height)?;
    if viewport_x == 0
        && viewport_y == 0
        && viewport_width == state.width
        && viewport_height == state.height
    {
        destination.copy_from_slice(&state.pixels);
        return Ok(());
    }
    destination.fill(0);
    for local_y in 0..viewport_height {
        let source_y = local_y.saturating_mul(state.height) / viewport_height;
        let destination_start = (viewport_y + local_y) * width + viewport_x;
        let row = &mut destination[destination_start..destination_start + viewport_width];
        for (local_x, pixel) in row.iter_mut().enumerate() {
            let source_x = local_x.saturating_mul(state.width) / viewport_width;
            *pixel = state.pixels[source_y * state.width + source_x];
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Viewport {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) fn fit_viewport(window: (u32, u32), guest: (u32, u32)) -> Viewport {
    let window_width = window.0.max(1);
    let window_height = window.1.max(1);
    let guest_width = guest.0.max(1);
    let guest_height = guest.1.max(1);
    let (width, height) = if u64::from(window_width) * u64::from(guest_height)
        <= u64::from(window_height) * u64::from(guest_width)
    {
        let height =
            (u64::from(guest_height) * u64::from(window_width) / u64::from(guest_width)).max(1);
        (window_width, u32::try_from(height).unwrap_or(u32::MAX))
    } else {
        let width =
            (u64::from(guest_width) * u64::from(window_height) / u64::from(guest_height)).max(1);
        (u32::try_from(width).unwrap_or(u32::MAX), window_height)
    };
    Viewport {
        x: window_width.saturating_sub(width) / 2,
        y: window_height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(super) fn host_resize_direction(action: GuiWindowAction) -> Option<ResizeDirection> {
    match action {
        GuiWindowAction::ResizeTopLeft => Some(ResizeDirection::NorthWest),
        GuiWindowAction::ResizeTop => Some(ResizeDirection::North),
        GuiWindowAction::ResizeTopRight => Some(ResizeDirection::NorthEast),
        GuiWindowAction::ResizeRight => Some(ResizeDirection::East),
        GuiWindowAction::ResizeBottomRight => Some(ResizeDirection::SouthEast),
        GuiWindowAction::ResizeBottom => Some(ResizeDirection::South),
        GuiWindowAction::ResizeBottomLeft => Some(ResizeDirection::SouthWest),
        GuiWindowAction::ResizeLeft => Some(ResizeDirection::West),
        GuiWindowAction::Move
        | GuiWindowAction::Minimize
        | GuiWindowAction::Maximize
        | GuiWindowAction::Restore
        | GuiWindowAction::Close => None,
    }
}

pub(super) fn begin_host_drag(
    window: &Window,
    action: GuiWindowAction,
    grant: &mut DragGrant,
    now: Instant,
) -> bool {
    if !action.is_drag() || !grant.take(now) {
        trace_gui(format_args!("ignored ungranted host drag {action:?}"));
        return false;
    }
    let result = if action == GuiWindowAction::Move {
        window.drag_window()
    } else if let Some(direction) = host_resize_direction(action) {
        window.drag_resize_window(direction)
    } else {
        return false;
    };
    if let Err(error) = result {
        eprintln!("warning: the Wayland compositor rejected the guest {action:?} drag: {error}");
    } else {
        trace_gui(format_args!("submitted native host drag {action:?}"));
    }
    true
}

pub(super) struct FrameState {
    pub(super) ready: GuiWindowReady,
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) pixels: Vec<u32>,
    sequence: u64,
}

impl FrameState {
    pub(super) fn new(ready: GuiWindowReady) -> Result<Self, Box<dyn std::error::Error>> {
        let mut state = Self {
            ready: ready.clone(),
            width: 0,
            height: 0,
            pixels: Vec::new(),
            sequence: 0,
        };
        state.resize(ready)?;
        Ok(state)
    }

    pub(super) fn resize(
        &mut self,
        ready: GuiWindowReady,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.width != 0
            && (ready.process_id != self.ready.process_id
                || ready.window_id != self.ready.window_id)
        {
            return Err("seamless GUI Ready changed its pinned process or HWND identity".into());
        }
        if ready.width == 0
            || ready.height == 0
            || ready.width > MAX_GUI_WINDOW_DIMENSION
            || ready.height > MAX_GUI_WINDOW_DIMENSION
        {
            return Err("seamless GUI framebuffer dimensions are invalid".into());
        }
        let width = usize::try_from(ready.width)?;
        let height = usize::try_from(ready.height)?;
        let bytes = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("seamless GUI framebuffer size overflowed")?;
        if bytes > MAX_GUI_FRAME_BYTES {
            return Err("seamless GUI framebuffer exceeds the protocol limit".into());
        }
        self.ready = ready;
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.resize(width * height, 0);
        self.sequence = 0;
        Ok(())
    }

    pub(super) fn apply(
        &mut self,
        damage: GuiWindowDamage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if damage.sequence < self.sequence {
            return Err("seamless GUI damage sequence moved backwards".into());
        }
        let x = usize::try_from(damage.x)?;
        let y = usize::try_from(damage.y)?;
        let width = usize::try_from(damage.width)?;
        let height = usize::try_from(damage.height)?;
        let expected_bytes = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("seamless GUI damage size overflowed")?;
        if damage.bgra.len() != expected_bytes {
            return Err("seamless GUI damage payload length is inconsistent".into());
        }
        if x.checked_add(width)
            .map_or(true, |right| right > self.width)
            || y.checked_add(height)
                .map_or(true, |bottom| bottom > self.height)
        {
            return Err("seamless GUI damage lies outside the current window".into());
        }
        for row in 0..height {
            let source_start = row * width * 4;
            let destination_start = (y + row) * self.width + x;
            for column in 0..width {
                let offset = source_start + column * 4;
                let blue = u32::from(damage.bgra[offset]);
                let green = u32::from(damage.bgra[offset + 1]);
                let red = u32::from(damage.bgra[offset + 2]);
                self.pixels[destination_start + column] = (red << 16) | (green << 8) | blue;
            }
        }
        self.sequence = damage.sequence;
        Ok(())
    }
}
