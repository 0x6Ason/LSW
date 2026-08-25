// SPDX-License-Identifier: GPL-3.0-or-later

//! Damage-aware native Wayland/X11 presenter for one Windows HWND.

use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    GuiInputEvent, GuiPointerButton, GuiWindowDamage, GuiWindowReady, MAX_GUI_FRAME_BYTES,
    MAX_GUI_WINDOW_DIMENSION,
};
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, ScaleMode, Window, WindowOptions};

use crate::agent_client::{GuiWindowEvent, GuiWindowSession, GuiWindowWriter};

const EVENT_QUEUE_DEPTH: usize = 256;
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
const GRACEFUL_CLOSE_TIMEOUT: Duration = Duration::from_secs(6);

pub(crate) fn present(session: GuiWindowSession) -> Result<i32, Box<dyn std::error::Error>> {
    let (ready, mut reader, mut writer) = session.split()?;
    let (event_sender, event_receiver) = mpsc::sync_channel(EVENT_QUEUE_DEPTH);
    thread::spawn(move || loop {
        let result = reader.read_event().map_err(|error| error.to_string());
        let terminal = matches!(result, Ok(GuiWindowEvent::Closed(_))) || result.is_err();
        if event_sender.send(result).is_err() || terminal {
            return;
        }
    });

    let mut state = FrameState::new(ready)?;
    let title = display_title(&state.ready.title);
    let mut window = Window::new(
        &title,
        state.width,
        state.height,
        WindowOptions {
            resize: true,
            scale_mode: ScaleMode::Stretch,
            ..WindowOptions::default()
        },
    )
    .map_err(|error| {
        format!("could not open a Wayland or X11 window for the Windows application: {error}")
    })?;
    window.limit_update_rate(Some(FRAME_INTERVAL));
    let mut dirty = true;
    let mut focused = window.is_active();
    writer.send_input(GuiInputEvent::Focus { focused })?;
    let mut window_size = window.get_size();
    let mut pointer = None;
    let mut buttons = [false; 3];

    while window.is_open() {
        'events: loop {
            match event_receiver.try_recv() {
                Ok(Ok(GuiWindowEvent::Ready(ready))) => {
                    let title = display_title(&ready.title);
                    window.set_title(&title);
                    state.resize(ready)?;
                    dirty = true;
                }
                Ok(Ok(GuiWindowEvent::Damage(damage))) => {
                    state.apply(damage)?;
                    dirty = true;
                }
                Ok(Ok(GuiWindowEvent::Closed(closed))) => return Ok(closed.exit_code),
                Ok(Err(error)) => return Err(error.into()),
                Err(TryRecvError::Empty) => break 'events,
                Err(TryRecvError::Disconnected) => {
                    return Err("seamless GUI event stream closed unexpectedly".into())
                }
            }
        }

        let observed_focus = window.is_active();
        if observed_focus != focused {
            focused = observed_focus;
            writer.send_input(GuiInputEvent::Focus { focused })?;
            if !focused {
                release_pointer_buttons(&mut writer, pointer, &mut buttons)?;
            }
        }
        let observed_size = window.get_size();
        if observed_size != window_size
            && observed_size.0 > 0
            && observed_size.1 > 0
            && observed_size.0 <= MAX_GUI_WINDOW_DIMENSION as usize
            && observed_size.1 <= MAX_GUI_WINDOW_DIMENSION as usize
        {
            window_size = observed_size;
            writer.resize(
                u32::try_from(observed_size.0)?,
                u32::try_from(observed_size.1)?,
            )?;
            dirty = true;
        }

        forward_keyboard(&window, &mut writer)?;
        if focused {
            pointer = forward_pointer(&window, &mut writer, &state, pointer, &mut buttons)?;
        }
        if dirty {
            window.update_with_buffer(&state.pixels, state.width, state.height)?;
            dirty = false;
        } else {
            window.update();
        }
    }

    writer.close()?;
    let deadline = Instant::now() + GRACEFUL_CLOSE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(0);
        }
        match event_receiver.recv_timeout(remaining) {
            Ok(Ok(GuiWindowEvent::Closed(closed))) => return Ok(closed.exit_code),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => return Ok(0),
        }
    }
}

struct FrameState {
    ready: GuiWindowReady,
    width: usize,
    height: usize,
    pixels: Vec<u32>,
    sequence: u64,
}

impl FrameState {
    fn new(ready: GuiWindowReady) -> Result<Self, Box<dyn std::error::Error>> {
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

    fn resize(&mut self, ready: GuiWindowReady) -> Result<(), Box<dyn std::error::Error>> {
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

    fn apply(&mut self, damage: GuiWindowDamage) -> Result<(), Box<dyn std::error::Error>> {
        if damage.sequence < self.sequence {
            return Err("seamless GUI damage sequence moved backwards".into());
        }
        let x = usize::try_from(damage.x)?;
        let y = usize::try_from(damage.y)?;
        let width = usize::try_from(damage.width)?;
        let height = usize::try_from(damage.height)?;
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

fn forward_keyboard(
    window: &Window,
    writer: &mut GuiWindowWriter,
) -> Result<(), Box<dyn std::error::Error>> {
    for key in window.get_keys_pressed(KeyRepeat::Yes) {
        if let Some((virtual_key, extended)) = windows_key(key) {
            writer.send_input(GuiInputEvent::Key {
                virtual_key,
                scan_code: 0,
                pressed: true,
                extended,
            })?;
        }
    }
    for key in window.get_keys_released() {
        if let Some((virtual_key, extended)) = windows_key(key) {
            writer.send_input(GuiInputEvent::Key {
                virtual_key,
                scan_code: 0,
                pressed: false,
                extended,
            })?;
        }
    }
    Ok(())
}

fn forward_pointer(
    window: &Window,
    writer: &mut GuiWindowWriter,
    state: &FrameState,
    previous: Option<(u32, u32)>,
    buttons: &mut [bool; 3],
) -> Result<Option<(u32, u32)>, Box<dyn std::error::Error>> {
    let window_size = window.get_size();
    let pointer = window
        .get_mouse_pos(MouseMode::Clamp)
        .map(|(x, y)| scale_pointer(x, y, window_size, (state.width, state.height)));
    if let Some((x, y)) = pointer {
        if pointer != previous {
            writer.send_input(GuiInputEvent::PointerMove { x, y })?;
        }
        for (index, (native, guest)) in [
            (MouseButton::Left, GuiPointerButton::Left),
            (MouseButton::Middle, GuiPointerButton::Middle),
            (MouseButton::Right, GuiPointerButton::Right),
        ]
        .into_iter()
        .enumerate()
        {
            let down = window.get_mouse_down(native);
            if down != buttons[index] {
                buttons[index] = down;
                writer.send_input(GuiInputEvent::PointerButton {
                    button: guest,
                    pressed: down,
                    x,
                    y,
                })?;
            }
        }
        if let Some((horizontal, vertical)) = window.get_scroll_wheel() {
            send_wheel(writer, vertical, false, x, y)?;
            send_wheel(writer, horizontal, true, x, y)?;
        }
    }
    Ok(pointer)
}

fn send_wheel(
    writer: &mut GuiWindowWriter,
    turns: f32,
    horizontal: bool,
    x: u32,
    y: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let delta = (turns * 120.0).round().clamp(-32768.0, 32767.0) as i16;
    if delta != 0 {
        writer.send_input(GuiInputEvent::PointerWheel {
            delta,
            horizontal,
            x,
            y,
        })?;
    }
    Ok(())
}

fn release_pointer_buttons(
    writer: &mut GuiWindowWriter,
    pointer: Option<(u32, u32)>,
    buttons: &mut [bool; 3],
) -> Result<(), Box<dyn std::error::Error>> {
    let (x, y) = pointer.unwrap_or((0, 0));
    for (index, button) in [
        GuiPointerButton::Left,
        GuiPointerButton::Middle,
        GuiPointerButton::Right,
    ]
    .into_iter()
    .enumerate()
    {
        if buttons[index] {
            buttons[index] = false;
            writer.send_input(GuiInputEvent::PointerButton {
                button,
                pressed: false,
                x,
                y,
            })?;
        }
    }
    Ok(())
}

fn scale_pointer(x: f32, y: f32, window: (usize, usize), guest: (usize, usize)) -> (u32, u32) {
    let width = window.0.max(1) as f32;
    let height = window.1.max(1) as f32;
    let guest_x = (x * guest.0 as f32 / width)
        .floor()
        .clamp(0.0, guest.0.saturating_sub(1) as f32);
    let guest_y = (y * guest.1 as f32 / height)
        .floor()
        .clamp(0.0, guest.1.saturating_sub(1) as f32);
    (guest_x as u32, guest_y as u32)
}

fn windows_key(key: Key) -> Option<(u16, bool)> {
    let simple = match key {
        Key::Key0
        | Key::Key1
        | Key::Key2
        | Key::Key3
        | Key::Key4
        | Key::Key5
        | Key::Key6
        | Key::Key7
        | Key::Key8
        | Key::Key9 => return Some((0x30 + key as u16, false)),
        Key::A
        | Key::B
        | Key::C
        | Key::D
        | Key::E
        | Key::F
        | Key::G
        | Key::H
        | Key::I
        | Key::J
        | Key::K
        | Key::L
        | Key::M
        | Key::N
        | Key::O
        | Key::P
        | Key::Q
        | Key::R
        | Key::S
        | Key::T
        | Key::U
        | Key::V
        | Key::W
        | Key::X
        | Key::Y
        | Key::Z => return Some((0x41 + (key as u16 - Key::A as u16), false)),
        Key::F1
        | Key::F2
        | Key::F3
        | Key::F4
        | Key::F5
        | Key::F6
        | Key::F7
        | Key::F8
        | Key::F9
        | Key::F10
        | Key::F11
        | Key::F12
        | Key::F13
        | Key::F14
        | Key::F15 => return Some((0x70 + (key as u16 - Key::F1 as u16), false)),
        Key::Backspace => 0x08,
        Key::Tab => 0x09,
        Key::Enter | Key::NumPadEnter => 0x0d,
        Key::Pause => 0x13,
        Key::CapsLock => 0x14,
        Key::Escape => 0x1b,
        Key::Space => 0x20,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::End => 0x23,
        Key::Home => 0x24,
        Key::Left => 0x25,
        Key::Up => 0x26,
        Key::Right => 0x27,
        Key::Down => 0x28,
        Key::Insert => 0x2d,
        Key::Delete => 0x2e,
        Key::LeftSuper => 0x5b,
        Key::RightSuper => 0x5c,
        Key::Menu => 0x5d,
        Key::NumPad0
        | Key::NumPad1
        | Key::NumPad2
        | Key::NumPad3
        | Key::NumPad4
        | Key::NumPad5
        | Key::NumPad6
        | Key::NumPad7
        | Key::NumPad8
        | Key::NumPad9 => 0x60 + (key as u16 - Key::NumPad0 as u16),
        Key::NumPadAsterisk => 0x6a,
        Key::NumPadPlus => 0x6b,
        Key::NumPadMinus => 0x6d,
        Key::NumPadDot => 0x6e,
        Key::NumPadSlash => 0x6f,
        Key::NumLock => 0x90,
        Key::ScrollLock => 0x91,
        Key::LeftShift => 0xa0,
        Key::RightShift => 0xa1,
        Key::LeftCtrl => 0xa2,
        Key::RightCtrl => 0xa3,
        Key::LeftAlt => 0xa4,
        Key::RightAlt => 0xa5,
        Key::Semicolon => 0xba,
        Key::Equal => 0xbb,
        Key::Comma => 0xbc,
        Key::Minus => 0xbd,
        Key::Period => 0xbe,
        Key::Slash => 0xbf,
        Key::Backquote => 0xc0,
        Key::LeftBracket => 0xdb,
        Key::Backslash => 0xdc,
        Key::RightBracket => 0xdd,
        Key::Apostrophe => 0xde,
        Key::Unknown | Key::Count => return None,
    };
    let extended = matches!(
        key,
        Key::RightAlt
            | Key::RightCtrl
            | Key::Insert
            | Key::Delete
            | Key::Home
            | Key::End
            | Key::PageUp
            | Key::PageDown
            | Key::Left
            | Key::Right
            | Key::Up
            | Key::Down
            | Key::NumPadSlash
            | Key::NumPadEnter
            | Key::LeftSuper
            | Key::RightSuper
            | Key::Menu
    );
    Some((simple, extended))
}

fn display_title(title: &str) -> String {
    if title.trim().is_empty() {
        "Windows application - LSW".to_owned()
    } else {
        format!("{title} - LSW")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn key_and_pointer_translation_are_stable() {
        assert_eq!(windows_key(Key::A), Some((0x41, false)));
        assert_eq!(windows_key(Key::RightCtrl), Some((0xa3, true)));
        assert_eq!(windows_key(Key::Unknown), None);
        assert_eq!(scale_pointer(50.0, 25.0, (100, 50), (200, 100)), (100, 50));
    }
}
