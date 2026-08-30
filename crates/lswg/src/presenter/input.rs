// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Instant;

use lsw_core::{GuiInputEvent, GuiPointerButton, GuiWindowAction, GuiWindowDragHint};
use lsw_host::GuiWindowWriter;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{MouseButton, MouseScrollDelta};
use winit::keyboard::KeyCode;

use super::{
    fit_viewport, trace_gui, FrameState, DRAG_GRANT_TIMEOUT, DRAG_HINT_TIMEOUT,
    FORWARDED_INPUT_IDLE_LEASE,
};

pub(super) fn scale_pointer_position(
    position: PhysicalPosition<f64>,
    window: PhysicalSize<u32>,
    state: &FrameState,
) -> Option<(u32, u32)> {
    scale_pointer(
        position.x,
        position.y,
        (window.width, window.height),
        (state.ready.width, state.ready.height),
    )
}

pub(super) fn guest_pointer_button(button: MouseButton) -> Option<(usize, GuiPointerButton)> {
    match button {
        MouseButton::Left => Some((0, GuiPointerButton::Left)),
        MouseButton::Middle => Some((1, GuiPointerButton::Middle)),
        MouseButton::Right => Some((2, GuiPointerButton::Right)),
        _ => None,
    }
}

#[derive(Default)]
pub(super) struct WheelAccumulator {
    pub(super) horizontal: f64,
    pub(super) vertical: f64,
}

impl WheelAccumulator {
    pub(super) fn events(&mut self, delta: MouseScrollDelta, x: u32, y: u32) -> Vec<GuiInputEvent> {
        let (horizontal, vertical) = match delta {
            MouseScrollDelta::LineDelta(horizontal, vertical) => {
                (f64::from(horizontal) * 120.0, f64::from(vertical) * 120.0)
            }
            // Wayland smooth scrolling is reported in physical pixels. Keep
            // sub-pixel residuals instead of rounding every event to zero.
            MouseScrollDelta::PixelDelta(position) => (position.x, position.y),
        };
        let mut events = Vec::with_capacity(2);
        if let Some(delta) = accumulate_wheel(&mut self.vertical, vertical) {
            events.push(GuiInputEvent::PointerWheel {
                delta,
                horizontal: false,
                x,
                y,
            });
        }
        if let Some(delta) = accumulate_wheel(&mut self.horizontal, horizontal) {
            events.push(GuiInputEvent::PointerWheel {
                delta,
                horizontal: true,
                x,
                y,
            });
        }
        events
    }
}

pub(super) fn accumulate_wheel(residual: &mut f64, delta: f64) -> Option<i16> {
    if !delta.is_finite() {
        return None;
    }
    *residual = (*residual + delta).clamp(-32768.999, 32767.999);
    let integral = residual.trunc();
    if integral == 0.0 {
        return None;
    }
    *residual -= integral;
    Some(integral as i16)
}

#[derive(Default)]
pub(super) struct DragGrant {
    pub(super) armed_at: Option<Instant>,
}

#[derive(Default)]
pub(super) struct DragHintState {
    observed: Option<(GuiWindowDragHint, Instant)>,
}

impl DragHintState {
    pub(super) fn observe(&mut self, hint: GuiWindowDragHint, now: Instant) {
        self.observed = Some((hint, now));
    }

    pub(super) fn observation_for(
        &self,
        pointer: Option<(u32, u32)>,
        now: Instant,
    ) -> Option<Option<GuiWindowAction>> {
        let (x, y) = pointer?;
        let (hint, observed_at) = self.observed?;
        if (hint.x, hint.y) != (x, y)
            || now.saturating_duration_since(observed_at) > DRAG_HINT_TIMEOUT
        {
            return None;
        }
        Some(hint.action.filter(|action| action.is_drag()))
    }
}

impl DragGrant {
    pub(super) fn arm(&mut self, now: Instant) {
        self.armed_at = Some(now);
    }

    pub(super) fn take(&mut self, now: Instant) -> bool {
        let Some(armed_at) = self.armed_at.take() else {
            return false;
        };
        now.saturating_duration_since(armed_at) <= DRAG_GRANT_TIMEOUT
    }
}

#[derive(Debug)]
pub(super) struct HostWindowState {
    pub(super) observed_maximized: bool,
    expected_from_guest: Option<bool>,
}

impl HostWindowState {
    pub(super) fn new(observed_maximized: bool) -> Self {
        Self {
            observed_maximized,
            expected_from_guest: None,
        }
    }

    pub(super) fn expect(&mut self, maximized: bool) {
        self.expected_from_guest = Some(maximized);
    }

    pub(super) fn observe(&mut self, maximized: bool) -> Option<GuiWindowAction> {
        if maximized == self.observed_maximized {
            return None;
        }
        self.observed_maximized = maximized;
        if self.expected_from_guest == Some(maximized) {
            self.expected_from_guest = None;
            return None;
        }
        self.expected_from_guest = None;
        Some(if maximized {
            GuiWindowAction::Maximize
        } else {
            GuiWindowAction::Restore
        })
    }
}

pub(super) fn release_forwarded_input(
    writer: &mut GuiWindowWriter,
    pointer: Option<(u32, u32)>,
    input: &mut ForwardedInputState,
) -> Result<(), Box<dyn std::error::Error>> {
    let events = input.release_events(pointer);
    trace_gui(format_args!(
        "releasing {} forwarded input edge(s)",
        events.len()
    ));
    for event in events {
        writer.send_input(event)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ForwardedKey {
    pub(super) virtual_key: u16,
    pub(super) extended: bool,
}

#[derive(Default)]
pub(super) struct ForwardedInputState {
    pub(super) pressed_keys: Vec<ForwardedKey>,
    pub(super) pressed_buttons: [bool; 3],
    lease_deadline: Option<Instant>,
}

impl ForwardedInputState {
    pub(super) fn has_held_input(&self) -> bool {
        !self.pressed_keys.is_empty() || self.pressed_buttons.iter().any(|pressed| *pressed)
    }

    pub(super) fn note_activity(&mut self, now: Instant) {
        self.lease_deadline = self
            .has_held_input()
            .then_some(now + FORWARDED_INPUT_IDLE_LEASE);
    }

    pub(super) fn lease_deadline(&self) -> Option<Instant> {
        self.lease_deadline
    }

    pub(super) fn lease_expired(&self, now: Instant) -> bool {
        self.lease_deadline.is_some_and(|deadline| now >= deadline)
    }

    pub(super) fn key_event(&mut self, key: KeyCode, pressed: bool) -> Option<GuiInputEvent> {
        let (virtual_key, extended) = windows_key(key)?;
        let forwarded = ForwardedKey {
            virtual_key,
            extended,
        };
        if pressed {
            if !self.pressed_keys.contains(&forwarded) {
                self.pressed_keys.push(forwarded);
            }
        } else {
            let position = self
                .pressed_keys
                .iter()
                .position(|candidate| *candidate == forwarded)?;
            self.pressed_keys.remove(position);
        }
        Some(key_input(forwarded, pressed))
    }

    pub(super) fn pointer_button_event(
        &mut self,
        index: usize,
        button: GuiPointerButton,
        pressed: bool,
        x: u32,
        y: u32,
    ) -> Option<GuiInputEvent> {
        let forwarded = self.pressed_buttons.get_mut(index)?;
        if *forwarded == pressed {
            return None;
        }
        *forwarded = pressed;
        Some(GuiInputEvent::PointerButton {
            button,
            pressed,
            x,
            y,
        })
    }

    pub(super) fn release_events(&mut self, pointer: Option<(u32, u32)>) -> Vec<GuiInputEvent> {
        self.lease_deadline = None;
        let mut events = self
            .pressed_keys
            .drain(..)
            .rev()
            .map(|key| key_input(key, false))
            .collect::<Vec<_>>();
        let (x, y) = pointer.unwrap_or((0, 0));
        for (index, button) in [
            GuiPointerButton::Left,
            GuiPointerButton::Middle,
            GuiPointerButton::Right,
        ]
        .into_iter()
        .enumerate()
        {
            if self.pressed_buttons[index] {
                self.pressed_buttons[index] = false;
                events.push(GuiInputEvent::PointerButton {
                    button,
                    pressed: false,
                    x,
                    y,
                });
            }
        }
        events
    }
}

pub(super) fn key_input(key: ForwardedKey, pressed: bool) -> GuiInputEvent {
    GuiInputEvent::Key {
        virtual_key: key.virtual_key,
        scan_code: 0,
        pressed,
        extended: key.extended,
    }
}

pub(super) fn scale_pointer(
    x: f64,
    y: f64,
    window: (u32, u32),
    guest: (u32, u32),
) -> Option<(u32, u32)> {
    let viewport = fit_viewport(window, guest);
    let left = f64::from(viewport.x);
    let top = f64::from(viewport.y);
    let right = left + f64::from(viewport.width);
    let bottom = top + f64::from(viewport.height);
    if !x.is_finite() || !y.is_finite() || x < left || x >= right || y < top || y >= bottom {
        return None;
    }
    let guest_x = ((x - left) * f64::from(guest.0) / f64::from(viewport.width))
        .floor()
        .clamp(0.0, f64::from(guest.0.saturating_sub(1)));
    let guest_y = ((y - top) * f64::from(guest.1) / f64::from(viewport.height))
        .floor()
        .clamp(0.0, f64::from(guest.1.saturating_sub(1)));
    Some((guest_x as u32, guest_y as u32))
}

pub(super) fn windows_key(key: KeyCode) -> Option<(u16, bool)> {
    let simple = match key {
        KeyCode::Digit0 => 0x30,
        KeyCode::Digit1 => 0x31,
        KeyCode::Digit2 => 0x32,
        KeyCode::Digit3 => 0x33,
        KeyCode::Digit4 => 0x34,
        KeyCode::Digit5 => 0x35,
        KeyCode::Digit6 => 0x36,
        KeyCode::Digit7 => 0x37,
        KeyCode::Digit8 => 0x38,
        KeyCode::Digit9 => 0x39,
        KeyCode::KeyA => 0x41,
        KeyCode::KeyB => 0x42,
        KeyCode::KeyC => 0x43,
        KeyCode::KeyD => 0x44,
        KeyCode::KeyE => 0x45,
        KeyCode::KeyF => 0x46,
        KeyCode::KeyG => 0x47,
        KeyCode::KeyH => 0x48,
        KeyCode::KeyI => 0x49,
        KeyCode::KeyJ => 0x4a,
        KeyCode::KeyK => 0x4b,
        KeyCode::KeyL => 0x4c,
        KeyCode::KeyM => 0x4d,
        KeyCode::KeyN => 0x4e,
        KeyCode::KeyO => 0x4f,
        KeyCode::KeyP => 0x50,
        KeyCode::KeyQ => 0x51,
        KeyCode::KeyR => 0x52,
        KeyCode::KeyS => 0x53,
        KeyCode::KeyT => 0x54,
        KeyCode::KeyU => 0x55,
        KeyCode::KeyV => 0x56,
        KeyCode::KeyW => 0x57,
        KeyCode::KeyX => 0x58,
        KeyCode::KeyY => 0x59,
        KeyCode::KeyZ => 0x5a,
        KeyCode::F1 => 0x70,
        KeyCode::F2 => 0x71,
        KeyCode::F3 => 0x72,
        KeyCode::F4 => 0x73,
        KeyCode::F5 => 0x74,
        KeyCode::F6 => 0x75,
        KeyCode::F7 => 0x76,
        KeyCode::F8 => 0x77,
        KeyCode::F9 => 0x78,
        KeyCode::F10 => 0x79,
        KeyCode::F11 => 0x7a,
        KeyCode::F12 => 0x7b,
        KeyCode::F13 => 0x7c,
        KeyCode::F14 => 0x7d,
        KeyCode::F15 => 0x7e,
        KeyCode::F16 => 0x7f,
        KeyCode::F17 => 0x80,
        KeyCode::F18 => 0x81,
        KeyCode::F19 => 0x82,
        KeyCode::F20 => 0x83,
        KeyCode::F21 => 0x84,
        KeyCode::F22 => 0x85,
        KeyCode::F23 => 0x86,
        KeyCode::F24 => 0x87,
        KeyCode::Backspace => 0x08,
        KeyCode::Tab => 0x09,
        KeyCode::Enter | KeyCode::NumpadEnter => 0x0d,
        KeyCode::Pause => 0x13,
        KeyCode::CapsLock => 0x14,
        KeyCode::Escape => 0x1b,
        KeyCode::Space => 0x20,
        KeyCode::PageUp => 0x21,
        KeyCode::PageDown => 0x22,
        KeyCode::End => 0x23,
        KeyCode::Home => 0x24,
        KeyCode::ArrowLeft => 0x25,
        KeyCode::ArrowUp => 0x26,
        KeyCode::ArrowRight => 0x27,
        KeyCode::ArrowDown => 0x28,
        KeyCode::Insert => 0x2d,
        KeyCode::Delete => 0x2e,
        KeyCode::SuperLeft => 0x5b,
        KeyCode::SuperRight => 0x5c,
        KeyCode::ContextMenu => 0x5d,
        KeyCode::Numpad0 => 0x60,
        KeyCode::Numpad1 => 0x61,
        KeyCode::Numpad2 => 0x62,
        KeyCode::Numpad3 => 0x63,
        KeyCode::Numpad4 => 0x64,
        KeyCode::Numpad5 => 0x65,
        KeyCode::Numpad6 => 0x66,
        KeyCode::Numpad7 => 0x67,
        KeyCode::Numpad8 => 0x68,
        KeyCode::Numpad9 => 0x69,
        KeyCode::NumpadMultiply => 0x6a,
        KeyCode::NumpadAdd => 0x6b,
        KeyCode::NumpadSubtract => 0x6d,
        KeyCode::NumpadDecimal => 0x6e,
        KeyCode::NumpadDivide => 0x6f,
        KeyCode::NumLock => 0x90,
        KeyCode::ScrollLock => 0x91,
        KeyCode::ShiftLeft => 0xa0,
        KeyCode::ShiftRight => 0xa1,
        KeyCode::ControlLeft => 0xa2,
        KeyCode::ControlRight => 0xa3,
        KeyCode::AltLeft => 0xa4,
        KeyCode::AltRight => 0xa5,
        KeyCode::Semicolon => 0xba,
        KeyCode::Equal => 0xbb,
        KeyCode::Comma => 0xbc,
        KeyCode::Minus => 0xbd,
        KeyCode::Period => 0xbe,
        KeyCode::Slash => 0xbf,
        KeyCode::Backquote => 0xc0,
        KeyCode::BracketLeft => 0xdb,
        KeyCode::Backslash => 0xdc,
        KeyCode::BracketRight => 0xdd,
        KeyCode::Quote => 0xde,
        _ => return None,
    };
    let extended = matches!(
        key,
        KeyCode::AltRight
            | KeyCode::ControlRight
            | KeyCode::Insert
            | KeyCode::Delete
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::ArrowLeft
            | KeyCode::ArrowRight
            | KeyCode::ArrowUp
            | KeyCode::ArrowDown
            | KeyCode::NumpadDivide
            | KeyCode::NumpadEnter
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
            | KeyCode::ContextMenu
    );
    Some((simple, extended))
}
