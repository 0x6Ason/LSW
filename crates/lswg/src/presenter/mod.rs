// SPDX-License-Identifier: GPL-3.0-or-later

//! Damage-aware native Wayland presenter for one Windows HWND.

mod input;
mod render;
mod wslg;

use std::env;
use std::fmt::Display;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lsw_core::{GuiInputEvent, GuiWindowAction, MAX_GUI_FRAME_BYTES, MAX_GUI_WINDOW_DIMENSION};
use lsw_host::{GuiWindowEvent, GuiWindowReader, GuiWindowSession, GuiWindowWriter};
use softbuffer::{Context, Surface};
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Event, MouseButton, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use winit::keyboard::PhysicalKey;
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::platform::wayland::EventLoopBuilderExtWayland;
use winit::window::{Window, WindowBuilder};

use input::*;
use render::*;
use wslg::*;

const EVENT_QUEUE_DEPTH: usize = 256;
const DRAG_GRANT_TIMEOUT: Duration = Duration::from_millis(750);
const DRAG_HINT_TIMEOUT: Duration = Duration::from_millis(750);
const DRAG_HINT_QUERY_TIMEOUT: Duration = Duration::from_millis(150);
const WSLG_HOST_CONTROL_TIMEOUT: Duration = Duration::from_secs(4);
// WSLg can keep a Wayland keyboard seat logically focused after its outer RAIL
// HWND loses Windows foreground focus. Bound every injected key/button hold so
// a missing leave event cannot strand Ctrl, Alt, or a mouse button in the guest.
// Legitimate repeats and pointer motion refresh this lease.
const FORWARDED_INPUT_IDLE_LEASE: Duration = Duration::from_secs(2);
// A repaint-heavy guest can continuously fill the bounded event channel. Give
// rendering, focus, input, resize, and local-close handling a turn even while
// damage is arriving faster than the host can present it.
const MAX_GUI_EVENTS_PER_WAKE: usize = 64;

type AgentEvent = Result<GuiWindowEvent, String>;

#[derive(Clone, Copy, Debug)]
enum PresenterWake {
    AgentEvent,
}

pub fn present(session: GuiWindowSession) -> Result<i32, Box<dyn std::error::Error>> {
    let (ready, reader, mut writer) = session.split()?;
    let mut state = FrameState::new(ready)?;
    ensure_wayland_environment()?;
    let mut builder = EventLoopBuilder::<PresenterWake>::with_user_event();
    builder.with_wayland();
    let mut event_loop = builder.build().map_err(|error| {
        format!(
            "could not connect to the Wayland compositor; ensure the graphical session is running and WAYLAND_DISPLAY is valid: {error}"
        )
    })?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let initial_size = PhysicalSize::new(u32::try_from(state.width)?, u32::try_from(state.height)?);
    let mut host_window_title = display_title(&state.ready.title);
    let window = Rc::new(
        WindowBuilder::new()
            .with_title(host_window_title.clone())
            .with_inner_size(initial_size)
            .with_decorations(false)
            // Windows owns every visible caption and resize affordance. Keep
            // the undecorated host surface non-resizable while idle so a
            // compositor resize border cannot steal clicks from the guest's
            // real non-client frame.
            .with_resizable(false)
            .build(&event_loop)
            .map_err(|error| format!("could not create the borderless Wayland window: {error}"))?,
    );
    let context = Context::new(window.clone())
        .map_err(|error| format!("could not initialize Wayland software rendering: {error}"))?;
    let mut surface = Surface::new(&context, window.clone())
        .map_err(|error| format!("could not create the Wayland software surface: {error}"))?;
    let mut window_size = window.inner_size();
    resize_surface(&mut surface, window_size)?;

    let mut dirty = true;
    let mut focused = window.has_focus();
    writer.send_input(GuiInputEvent::Focus { focused })?;
    let mut pointer = None;
    let mut pointer_inside = false;
    let mut pointer_over_guest = false;
    let mut input = ForwardedInputState::default();
    let mut wheel = WheelAccumulator::default();
    let mut drag_grant = DragGrant::default();
    let mut drag_hint = DragHintState::default();
    let mut host_window_state = HostWindowState::new(window.is_maximized());
    let mut outcome: Option<Result<i32, String>> = None;
    window.request_redraw();

    let proxy = event_loop.create_proxy();
    let (event_receiver, reader_thread) = spawn_event_reader(reader, proxy.clone());
    let loop_result = event_loop.run_on_demand(|event, event_loop| {
        if outcome.is_some() {
            event_loop.exit();
            return;
        }

        match event {
            Event::UserEvent(PresenterWake::AgentEvent) => {
                let hit_limit = drain_agent_events(
                    &event_receiver,
                    &window,
                    &mut writer,
                    &mut state,
                    &mut dirty,
                    &mut input,
                    pointer,
                    &mut drag_grant,
                    &mut drag_hint,
                    &mut host_window_state,
                    &mut host_window_title,
                    &mut outcome,
                    event_loop,
                );
                if hit_limit && outcome.is_none() {
                    let _ = proxy.send_event(PresenterWake::AgentEvent);
                }
                if dirty {
                    window.request_redraw();
                }
            }
            Event::WindowEvent { window_id, event } if window_id == window.id() => match event {
                WindowEvent::CloseRequested => {
                    trace_gui("Wayland close requested");
                    if let Err(error) = begin_graceful_close(&mut writer, pointer, &mut input) {
                        stop_with_error(event_loop, &mut outcome, error);
                    }
                }
                WindowEvent::Focused(observed_focus) => {
                    trace_gui(format_args!("Wayland focus changed to {observed_focus}"));
                    if observed_focus != focused {
                        if !observed_focus {
                            if let Err(error) =
                                release_forwarded_input(&mut writer, pointer, &mut input)
                            {
                                stop_with_error(event_loop, &mut outcome, error);
                                return;
                            }
                        }
                        focused = observed_focus;
                        if let Err(error) = writer.send_input(GuiInputEvent::Focus { focused }) {
                            stop_with_error(event_loop, &mut outcome, error);
                        }
                    }
                }
                WindowEvent::Resized(observed_size)
                    if observed_size.width > 0 && observed_size.height > 0 =>
                {
                    if let Err(error) = validate_host_size(observed_size)
                        .and_then(|()| resize_surface(&mut surface, observed_size))
                    {
                        stop_with_error(event_loop, &mut outcome, error);
                        return;
                    }
                    if observed_size != window_size {
                        window_size = observed_size;
                        let observed_maximized = window.is_maximized();
                        let state_transitioned =
                            observed_maximized != host_window_state.observed_maximized;
                        if let Some(action) = host_window_state.observe(observed_maximized) {
                            if let Err(error) = writer.window_action(action) {
                                stop_with_error(event_loop, &mut outcome, error);
                                return;
                            }
                        }
                        // Native maximize/restore owns the corresponding guest
                        // transition. Do not race it with SetWindowPos or alter
                        // the guest's restore bounds using a transitional host
                        // configure. Ordinary non-maximized resizes remain
                        // host-authoritative.
                        if !state_transitioned && !observed_maximized {
                            if let Err(error) =
                                writer.resize(observed_size.width, observed_size.height)
                            {
                                stop_with_error(event_loop, &mut outcome, error);
                                return;
                            }
                        }
                    }
                    dirty = true;
                    window.request_redraw();
                }
                WindowEvent::KeyboardInput {
                    event,
                    is_synthetic,
                    ..
                } if focused && !is_synthetic => {
                    if let PhysicalKey::Code(code) = event.physical_key {
                        trace_gui(format_args!(
                            "Wayland key input code={code:?} pressed={}",
                            event.state == ElementState::Pressed
                        ));
                        if let Some(input_event) =
                            input.key_event(code, event.state == ElementState::Pressed)
                        {
                            if let Err(error) = writer.send_input(input_event) {
                                stop_with_error(event_loop, &mut outcome, error);
                            } else {
                                input.note_activity(Instant::now());
                            }
                        }
                    }
                }
                WindowEvent::CursorEntered { .. } => {
                    // Wayland pointer focus is independent from keyboard
                    // focus. The first click is often what grants keyboard
                    // focus, so pointer input must use this surface-enter
                    // state rather than WindowEvent::Focused.
                    trace_gui("Wayland pointer entered");
                    pointer_inside = true;
                }
                WindowEvent::CursorLeft { .. } => {
                    trace_gui("Wayland pointer left");
                    pointer_inside = false;
                    pointer_over_guest = false;
                }
                WindowEvent::CursorMoved { position, .. } => {
                    // WSLg can deliver the first surface-scoped motion before
                    // CursorEntered when a RAIL proxy regains pointer focus.
                    // Treat that motion as the re-entry proof so the first
                    // click after focus recovery uses its current coordinate
                    // instead of being dropped with stale outside state.
                    pointer_inside = true;
                    let scaled = scale_pointer_position(position, window_size, &state);
                    trace_gui(format_args!(
                        "Wayland pointer moved physical=({:.2},{:.2}) window={}x{} guest={}x{} scaled={scaled:?}",
                        position.x,
                        position.y,
                        window_size.width,
                        window_size.height,
                        state.ready.width,
                        state.ready.height,
                    ));
                    if let Some(next) = scaled {
                        pointer_over_guest = true;
                        if Some(next) != pointer {
                            pointer = Some(next);
                            if let Err(error) = writer.send_input(GuiInputEvent::PointerMove {
                                x: next.0,
                                y: next.1,
                            }) {
                                stop_with_error(event_loop, &mut outcome, error);
                            } else {
                                input.note_activity(Instant::now());
                            }
                        }
                    } else {
                        pointer_over_guest = false;
                    }
                }
                WindowEvent::MouseInput {
                    state: button_state,
                    button,
                    ..
                } => {
                    let pressed = button_state == ElementState::Pressed;
                    trace_gui(format_args!(
                        "Wayland mouse input button={button:?} pressed={pressed} pointer_inside={pointer_inside} pointer_over_guest={pointer_over_guest} pointer={pointer:?}"
                    ));
                    // A press must begin while this Wayland surface owns
                    // pointer focus. A release for an already-forwarded press
                    // remains valid after CursorLeft because Wayland implicit
                    // grabs can deliver that edge outside the surface.
                    if pressed && (!pointer_inside || !pointer_over_guest) {
                        return;
                    }
                    let drag_action = if button == MouseButton::Left && pressed {
                        let action = pointer.and_then(|point| {
                            query_drag_hint(
                                &event_receiver,
                                &window,
                                &mut writer,
                                &mut state,
                                &mut dirty,
                                &mut input,
                                pointer,
                                &mut drag_grant,
                                &mut drag_hint,
                                &mut host_window_state,
                                &mut host_window_title,
                                &mut outcome,
                                event_loop,
                                point,
                            )
                        });
                        if outcome.is_some() {
                            return;
                        }
                        drag_grant.arm(Instant::now());
                        trace_gui(format_args!(
                            "left press at {pointer:?} used drag hint {action:?}"
                        ));
                        action
                    } else {
                        None
                    };
                    let mut forwarded = false;
                    if let (Some((index, guest_button)), Some((x, y))) =
                        (guest_pointer_button(button), pointer)
                    {
                        if let Some(input_event) =
                            input.pointer_button_event(index, guest_button, pressed, x, y)
                        {
                            if let Err(error) = writer.send_input(input_event) {
                                stop_with_error(event_loop, &mut outcome, error);
                            } else {
                                input.note_activity(Instant::now());
                                forwarded = true;
                            }
                        }
                    }
                    if forwarded {
                        if let Some(action) = drag_action {
                            begin_host_drag(&window, action, &mut drag_grant, Instant::now());
                        }
                    }
                }
                WindowEvent::MouseWheel { delta, .. } if pointer_inside && pointer_over_guest => {
                    if let Some((x, y)) = pointer {
                        for input_event in wheel.events(delta, x, y) {
                            if let Err(error) = writer.send_input(input_event) {
                                stop_with_error(event_loop, &mut outcome, error);
                                return;
                            } else {
                                input.note_activity(Instant::now());
                            }
                        }
                    }
                }
                WindowEvent::RedrawRequested if dirty => {
                    if let Err(error) = present_frame(&mut surface, &state, window_size) {
                        stop_with_error(event_loop, &mut outcome, error);
                    } else {
                        dirty = false;
                    }
                }
                WindowEvent::Occluded(false) => {
                    dirty = true;
                    window.request_redraw();
                }
                _ => {}
            },
            Event::AboutToWait => {
                if input.lease_expired(Instant::now()) {
                    if let Err(error) = release_forwarded_input(&mut writer, pointer, &mut input) {
                        stop_with_error(event_loop, &mut outcome, error);
                        return;
                    }
                }
                if dirty {
                    window.request_redraw();
                }
                event_loop.set_control_flow(match input.lease_deadline() {
                    Some(deadline) => ControlFlow::WaitUntil(deadline),
                    None => ControlFlow::Wait,
                });
            }
            _ => {}
        }
    });

    // Drop the bounded queue receiver before shutting down the socket. This
    // wakes a reader blocked in `send` on a full queue; dropping the writer
    // then wakes a reader blocked in `read_event`.
    drop(surface);
    drop(context);
    drop(window);
    let reader_result = stop_event_reader(event_receiver, move || drop(writer), reader_thread);
    loop_result.map_err(|error| format!("Wayland event loop failed: {error}"))?;
    reader_result?;
    match outcome {
        Some(Ok(exit_code)) => Ok(exit_code),
        Some(Err(error)) => Err(error.into()),
        None => Err("Wayland event loop exited without a guest result".into()),
    }
}

fn ensure_wayland_environment() -> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(
            "seamless GUI requires a Wayland desktop, but WAYLAND_DISPLAY is not set; start LSW from an interactive Linux graphical session"
                .into(),
        );
    }
    if env::var_os("XDG_RUNTIME_DIR").is_none() {
        return Err(
            "seamless GUI requires a Wayland desktop, but XDG_RUNTIME_DIR is not set; start LSW from an interactive Linux graphical session"
                .into(),
        );
    }
    Ok(())
}

fn spawn_event_reader(
    mut reader: GuiWindowReader,
    proxy: EventLoopProxy<PresenterWake>,
) -> (Receiver<AgentEvent>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE_DEPTH);
    let thread = thread::spawn(move || loop {
        let result = reader.read_event().map_err(|error| error.to_string());
        let terminal = matches!(result, Ok(GuiWindowEvent::Closed(_))) || result.is_err();
        if sender.send(result).is_err() {
            return;
        }
        let _ = proxy.send_event(PresenterWake::AgentEvent);
        if terminal {
            return;
        }
    });
    (receiver, thread)
}

fn join_event_reader(thread: JoinHandle<()>) -> Result<(), Box<dyn std::error::Error>> {
    thread
        .join()
        .map_err(|_| "seamless GUI event reader panicked".into())
}

fn stop_event_reader<T>(
    receiver: Receiver<T>,
    shutdown_socket: impl FnOnce(),
    thread: JoinHandle<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    drop(receiver);
    shutdown_socket();
    join_event_reader(thread)
}

#[allow(clippy::too_many_arguments)]
fn drain_agent_events(
    receiver: &Receiver<AgentEvent>,
    window: &Window,
    writer: &mut GuiWindowWriter,
    state: &mut FrameState,
    dirty: &mut bool,
    input: &mut ForwardedInputState,
    pointer: Option<(u32, u32)>,
    drag_grant: &mut DragGrant,
    drag_hint: &mut DragHintState,
    host_window_state: &mut HostWindowState,
    host_window_title: &mut String,
    outcome: &mut Option<Result<i32, String>>,
    event_loop: &EventLoopWindowTarget<PresenterWake>,
) -> bool {
    let mut processed = 0;
    while processed < MAX_GUI_EVENTS_PER_WAKE {
        let event = match receiver.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                stop_with_error(
                    event_loop,
                    outcome,
                    "seamless GUI event stream closed unexpectedly",
                );
                return false;
            }
        };
        processed += 1;
        if !handle_agent_event(
            event,
            window,
            writer,
            state,
            dirty,
            input,
            pointer,
            drag_grant,
            drag_hint,
            host_window_state,
            host_window_title,
            outcome,
            event_loop,
        ) {
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn handle_agent_event(
    event: AgentEvent,
    window: &Window,
    writer: &mut GuiWindowWriter,
    state: &mut FrameState,
    dirty: &mut bool,
    input: &mut ForwardedInputState,
    pointer: Option<(u32, u32)>,
    drag_grant: &mut DragGrant,
    drag_hint: &mut DragHintState,
    host_window_state: &mut HostWindowState,
    host_window_title: &mut String,
    outcome: &mut Option<Result<i32, String>>,
    event_loop: &EventLoopWindowTarget<PresenterWake>,
) -> bool {
    match event {
        Ok(GuiWindowEvent::Ready(ready)) => {
            let next_title = display_title(&ready.title);
            window.set_title(&next_title);
            *host_window_title = next_title;
            if let Err(error) = state.resize(ready) {
                stop_with_error(event_loop, outcome, error);
                return false;
            }
            // The host compositor owns the Wayland extent after initial
            // creation. A Ready frame reports the guest framebuffer size;
            // forcing that size back into Wayland here creates a resize
            // feedback loop under compositor rounding and maximize.
            *dirty = true;
        }
        Ok(GuiWindowEvent::Damage(damage)) => {
            if let Err(error) = state.apply(damage) {
                stop_with_error(event_loop, outcome, error);
                return false;
            }
            *dirty = true;
        }
        Ok(GuiWindowEvent::DragHint(hint)) => drag_hint.observe(hint, Instant::now()),
        Ok(GuiWindowEvent::Action(action)) => {
            trace_gui(format_args!("guest window action: {action:?}"));
            if action.is_drag() {
                begin_host_drag(window, action, drag_grant, Instant::now());
            } else {
                match action {
                    GuiWindowAction::Minimize => {
                        if let Err(error) = minimize_host_window(window, host_window_title) {
                            stop_with_error(event_loop, outcome, error);
                            return false;
                        }
                    }
                    GuiWindowAction::Maximize => {
                        host_window_state.expect(true);
                        window.set_maximized(true);
                    }
                    GuiWindowAction::Restore => {
                        host_window_state.expect(false);
                        window.set_minimized(false);
                        window.set_maximized(false);
                    }
                    GuiWindowAction::Close => {
                        if let Err(error) = begin_graceful_close(writer, pointer, input) {
                            stop_with_error(event_loop, outcome, error);
                            return false;
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(GuiWindowEvent::Closed(closed)) => {
            *outcome = Some(Ok(closed.exit_code));
            event_loop.exit();
            return false;
        }
        Err(error) => {
            stop_with_error(event_loop, outcome, error);
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn query_drag_hint(
    receiver: &Receiver<AgentEvent>,
    window: &Window,
    writer: &mut GuiWindowWriter,
    state: &mut FrameState,
    dirty: &mut bool,
    input: &mut ForwardedInputState,
    pointer: Option<(u32, u32)>,
    drag_grant: &mut DragGrant,
    drag_hint: &mut DragHintState,
    host_window_state: &mut HostWindowState,
    host_window_title: &mut String,
    outcome: &mut Option<Result<i32, String>>,
    event_loop: &EventLoopWindowTarget<PresenterWake>,
    point: (u32, u32),
) -> Option<GuiWindowAction> {
    if let Some(observation) = drag_hint.observation_for(Some(point), Instant::now()) {
        return observation;
    }

    // A fast move-and-click can reach this callback before the asynchronous
    // guest WM_NCHITTEST reply. Repeat the bounded hover query, then drain
    // agent events for a short period while the original Wayland button serial
    // is still valid. This avoids falling back to an async drag request that a
    // compositor must ignore after the callback returns.
    if let Err(error) = writer.send_input(GuiInputEvent::PointerMove {
        x: point.0,
        y: point.1,
    }) {
        stop_with_error(event_loop, outcome, error);
        return None;
    }
    input.note_activity(Instant::now());
    let deadline = Instant::now() + DRAG_HINT_QUERY_TIMEOUT;
    loop {
        if let Some(observation) = drag_hint.observation_for(Some(point), Instant::now()) {
            return observation;
        }
        let now = Instant::now();
        if now >= deadline {
            trace_gui(format_args!("timed out waiting for drag hint at {point:?}"));
            return None;
        }
        match receiver.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(event) => {
                if !handle_agent_event(
                    event,
                    window,
                    writer,
                    state,
                    dirty,
                    input,
                    pointer,
                    drag_grant,
                    drag_hint,
                    host_window_state,
                    host_window_title,
                    outcome,
                    event_loop,
                ) {
                    return None;
                }
                if *dirty {
                    window.request_redraw();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                trace_gui(format_args!("timed out waiting for drag hint at {point:?}"));
                return None;
            }
            Err(RecvTimeoutError::Disconnected) => {
                stop_with_error(
                    event_loop,
                    outcome,
                    "seamless GUI event stream closed unexpectedly",
                );
                return None;
            }
        }
    }
}

fn begin_graceful_close(
    writer: &mut GuiWindowWriter,
    pointer: Option<(u32, u32)>,
    input: &mut ForwardedInputState,
) -> Result<(), Box<dyn std::error::Error>> {
    // WM_CLOSE can open a save-confirmation HWND. Keep the transport and input
    // path alive for an unbounded human decision, and allow a later host close
    // request after the user chose Cancel. The guest closes this session only
    // after the selected HWND actually disappears.
    release_forwarded_input(writer, pointer, input)?;
    writer.close()?;
    Ok(())
}

fn stop_with_error<T: 'static>(
    event_loop: &EventLoopWindowTarget<T>,
    outcome: &mut Option<Result<i32, String>>,
    error: impl Display,
) {
    if outcome.is_none() {
        *outcome = Some(Err(error.to_string()));
    }
    event_loop.exit();
}

fn trace_gui(message: impl Display) {
    if env::var_os("LSW_GUI_TRACE").is_some() {
        eprintln!("lsw gui trace: {message}");
    }
}

fn validate_host_size(size: PhysicalSize<u32>) -> Result<(), Box<dyn std::error::Error>> {
    if size.width > MAX_GUI_WINDOW_DIMENSION || size.height > MAX_GUI_WINDOW_DIMENSION {
        return Err("Wayland window size exceeds the seamless GUI protocol limit".into());
    }
    let bytes = usize::try_from(size.width)?
        .checked_mul(usize::try_from(size.height)?)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("Wayland surface size overflowed")?;
    if bytes > MAX_GUI_FRAME_BYTES {
        return Err("Wayland surface exceeds the seamless GUI framebuffer limit".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
