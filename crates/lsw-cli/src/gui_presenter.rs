// SPDX-License-Identifier: GPL-3.0-or-later

//! Damage-aware native Wayland presenter for one Windows HWND.

use std::env;
use std::fmt::Display;
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lsw_core::{
    GuiInputEvent, GuiPointerButton, GuiWindowAction, GuiWindowDamage, GuiWindowDragHint,
    GuiWindowReady, MAX_GUI_FRAME_BYTES, MAX_GUI_WINDOW_DIMENSION,
};
use softbuffer::{Context, Surface};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Event, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::platform::wayland::EventLoopBuilderExtWayland;
use winit::window::{ResizeDirection, Window, WindowBuilder};

use crate::agent_client::{GuiWindowEvent, GuiWindowSession, GuiWindowWriter};

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

pub(crate) fn present(session: GuiWindowSession) -> Result<i32, Box<dyn std::error::Error>> {
    let (ready, reader, mut writer) = session.split()?;
    let mut state = FrameState::new(ready)?;
    ensure_wayland_environment()?;
    let mut builder = EventLoopBuilder::<PresenterWake>::with_user_event();
    builder.with_wayland();
    let mut event_loop = builder.build().map_err(|error| {
        format!(
            "could not connect to the WSLg Wayland compositor; ensure WSLg is running and WAYLAND_DISPLAY is valid: {error}"
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
            // the undecorated host proxy non-resizable while idle so WSLg's
            // invisible outer border cannot steal clicks from the guest's
            // real non-client frame.
            .with_resizable(false)
            .build(&event_loop)
            .map_err(|error| {
                format!("could not create the borderless WSLg Wayland window: {error}")
            })?,
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
            "seamless GUI requires WSLg Wayland, but WAYLAND_DISPLAY is not set; start LSW from a WSLg-enabled interactive WSL session"
                .into(),
        );
    }
    if env::var_os("XDG_RUNTIME_DIR").is_none() {
        return Err(
            "seamless GUI requires WSLg Wayland, but XDG_RUNTIME_DIR is not set; start LSW from a WSLg-enabled interactive WSL session"
                .into(),
        );
    }
    Ok(())
}

fn spawn_event_reader(
    mut reader: crate::agent_client::GuiWindowReader,
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

fn resize_surface(
    surface: &mut Surface<Rc<Window>, Rc<Window>>,
    size: PhysicalSize<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_host_size(size)?;
    let width = NonZeroU32::new(size.width).ok_or("Wayland surface width is zero")?;
    let height = NonZeroU32::new(size.height).ok_or("Wayland surface height is zero")?;
    surface.resize(width, height)?;
    Ok(())
}

fn present_frame(
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

fn render_scaled(
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
struct Viewport {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn fit_viewport(window: (u32, u32), guest: (u32, u32)) -> Viewport {
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

fn host_resize_direction(action: GuiWindowAction) -> Option<ResizeDirection> {
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

fn begin_host_drag(
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
        eprintln!("warning: WSLg rejected the guest {action:?} drag: {error}");
    } else {
        trace_gui(format_args!("submitted native host drag {action:?}"));
    }
    true
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

    fn apply(&mut self, damage: GuiWindowDamage) -> Result<(), Box<dyn std::error::Error>> {
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

fn scale_pointer_position(
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

fn guest_pointer_button(button: MouseButton) -> Option<(usize, GuiPointerButton)> {
    match button {
        MouseButton::Left => Some((0, GuiPointerButton::Left)),
        MouseButton::Middle => Some((1, GuiPointerButton::Middle)),
        MouseButton::Right => Some((2, GuiPointerButton::Right)),
        _ => None,
    }
}

#[derive(Default)]
struct WheelAccumulator {
    horizontal: f64,
    vertical: f64,
}

impl WheelAccumulator {
    fn events(&mut self, delta: MouseScrollDelta, x: u32, y: u32) -> Vec<GuiInputEvent> {
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

fn accumulate_wheel(residual: &mut f64, delta: f64) -> Option<i16> {
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
struct DragGrant {
    armed_at: Option<Instant>,
}

#[derive(Default)]
struct DragHintState {
    observed: Option<(GuiWindowDragHint, Instant)>,
}

impl DragHintState {
    fn observe(&mut self, hint: GuiWindowDragHint, now: Instant) {
        self.observed = Some((hint, now));
    }

    fn observation_for(
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
    fn arm(&mut self, now: Instant) {
        self.armed_at = Some(now);
    }

    fn take(&mut self, now: Instant) -> bool {
        let Some(armed_at) = self.armed_at.take() else {
            return false;
        };
        now.saturating_duration_since(armed_at) <= DRAG_GRANT_TIMEOUT
    }
}

#[derive(Debug)]
struct HostWindowState {
    observed_maximized: bool,
    expected_from_guest: Option<bool>,
}

impl HostWindowState {
    fn new(observed_maximized: bool) -> Self {
        Self {
            observed_maximized,
            expected_from_guest: None,
        }
    }

    fn expect(&mut self, maximized: bool) {
        self.expected_from_guest = Some(maximized);
    }

    fn observe(&mut self, maximized: bool) -> Option<GuiWindowAction> {
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

fn release_forwarded_input(
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
struct ForwardedKey {
    virtual_key: u16,
    extended: bool,
}

#[derive(Default)]
struct ForwardedInputState {
    pressed_keys: Vec<ForwardedKey>,
    pressed_buttons: [bool; 3],
    lease_deadline: Option<Instant>,
}

impl ForwardedInputState {
    fn has_held_input(&self) -> bool {
        !self.pressed_keys.is_empty() || self.pressed_buttons.iter().any(|pressed| *pressed)
    }

    fn note_activity(&mut self, now: Instant) {
        self.lease_deadline = self
            .has_held_input()
            .then_some(now + FORWARDED_INPUT_IDLE_LEASE);
    }

    fn lease_deadline(&self) -> Option<Instant> {
        self.lease_deadline
    }

    fn lease_expired(&self, now: Instant) -> bool {
        self.lease_deadline.is_some_and(|deadline| now >= deadline)
    }

    fn key_event(&mut self, key: KeyCode, pressed: bool) -> Option<GuiInputEvent> {
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

    fn pointer_button_event(
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

    fn release_events(&mut self, pointer: Option<(u32, u32)>) -> Vec<GuiInputEvent> {
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

fn key_input(key: ForwardedKey, pressed: bool) -> GuiInputEvent {
    GuiInputEvent::Key {
        virtual_key: key.virtual_key,
        scan_code: 0,
        pressed,
        extended: key.extended,
    }
}

fn scale_pointer(x: f64, y: f64, window: (u32, u32), guest: (u32, u32)) -> Option<(u32, u32)> {
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

fn windows_key(key: KeyCode) -> Option<(u16, bool)> {
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

fn display_title(title: &str) -> String {
    if title.trim().is_empty() {
        "Windows application - LSW".to_owned()
    } else {
        format!("{title} - LSW")
    }
}

fn minimize_host_window(window: &Window, host_window_title: &str) -> Result<(), String> {
    if !running_under_wsl() {
        trace_gui("requesting native Wayland minimization");
        window.set_minimized(true);
        return Ok(());
    }

    // xdg_toplevel.set_minimized is explicitly advisory and current WSLg
    // releases ignore it. Ask Windows to minimize only the unique exact-title
    // RAIL proxy owned by msrdc. Identity values travel as hex over stdin
    // rather than becoming PowerShell source, so a guest-controlled title
    // cannot inject host commands.
    trace_gui("requesting identity-checked WSLg host minimization");
    let result = run_wslg_host_control("minimize", host_window_title);
    trace_gui(if result.is_ok() {
        "WSLg host minimization completed"
    } else {
        "WSLg host minimization failed"
    });
    result
}

fn running_under_wsl() -> bool {
    env::var_os("WSL_INTEROP").is_some() || env::var_os("WSL_DISTRO_NAME").is_some()
}

fn run_wslg_host_control(operation: &str, host_window_title: &str) -> Result<(), String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
function ConvertFrom-LswHex {
    param([Parameter(Mandatory = $true)][string]$Value)
    if (($Value.Length % 2) -ne 0 -or $Value -notmatch '^[0-9a-f]*$') {
        throw 'the WSLg host-window identity was not canonical hex'
    }
    $bytes = [byte[]]::new($Value.Length / 2)
    for ($index = 0; $index -lt $bytes.Length; $index++) {
        $bytes[$index] = [Convert]::ToByte($Value.Substring($index * 2, 2), 16)
    }
    return [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
}
$identityLines = @([Console]::In.ReadToEnd().Split("`n"))
if ($identityLines.Count -lt 3) {
    throw 'the WSLg host-window identity was incomplete'
}
$operation = $identityLines[0].TrimEnd("`r")
$expectedTitle = ConvertFrom-LswHex $identityLines[1].TrimEnd("`r")
$distroName = ConvertFrom-LswHex $identityLines[2].TrimEnd("`r")
if ([string]::IsNullOrEmpty($expectedTitle) -or [string]::IsNullOrEmpty($distroName)) {
    throw 'the expected WSLg title or distro identity was empty'
}
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;

public static class LswWslgHostWindow {
    public const uint GA_ROOT = 2;
    public delegate bool EnumWindowsCallback(IntPtr hwnd, IntPtr parameter);

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool EnumWindows(EnumWindowsCallback callback, IntPtr parameter);
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")]
    public static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);
    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern int GetWindowTextLengthW(IntPtr hwnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowTextW(IntPtr hwnd, StringBuilder text, int maximum);
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll")]
    public static extern bool PostMessageW(IntPtr hwnd, uint message, IntPtr wParam,
                                           IntPtr lParam);
    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hwnd);
    public static string Title(IntPtr hwnd) {
        int length = GetWindowTextLengthW(hwnd);
        StringBuilder text = new StringBuilder(Math.Max(length + 1, 2));
        GetWindowTextW(hwnd, text, text.Capacity);
        return text.ToString();
    }

    private static bool HasExpectedTitle(IntPtr hwnd, string[] expectedTitles) {
        string observed = Title(hwnd);
        foreach (string expected in expectedTitles) {
            if (String.Equals(observed, expected, StringComparison.Ordinal)) {
                return true;
            }
        }
        return false;
    }

    private static bool IsRailOwner(IntPtr hwnd) {
        uint processId;
        GetWindowThreadProcessId(hwnd, out processId);
        try {
            using (Process owner = Process.GetProcessById((int)processId)) {
                return String.Equals(owner.ProcessName, "msrdc",
                                     StringComparison.OrdinalIgnoreCase);
            }
        } catch {
            return false;
        }
    }

    private static bool IsExactRailWindow(IntPtr hwnd, string[] expectedTitles) {
        return hwnd != IntPtr.Zero && IsWindowVisible(hwnd) &&
               GetAncestor(hwnd, GA_ROOT) == hwnd &&
               HasExpectedTitle(hwnd, expectedTitles) && IsRailOwner(hwnd);
    }

    public static IntPtr Resolve(string[] expectedTitles) {
        IntPtr foreground = GetForegroundWindow();
        if (IsExactRailWindow(foreground, expectedTitles)) {
            return foreground;
        }
        List<IntPtr> matches = new List<IntPtr>();
        if (!EnumWindows(delegate(IntPtr hwnd, IntPtr parameter) {
            if (IsExactRailWindow(hwnd, expectedTitles)) {
                matches.Add(hwnd);
            }
            return true;
        }, IntPtr.Zero)) {
            throw new System.ComponentModel.Win32Exception(
                Marshal.GetLastWin32Error(), "EnumWindows failed");
        }
        if (matches.Count != 1) {
            throw new InvalidOperationException(
                "the exact WSLg RAIL proxy identity was absent or ambiguous");
        }
        return matches[0];
    }
}
'@
[string[]]$allowedTitles = @(
    "$expectedTitle ($distroName)",
    "[WARN:COPY MODE] $expectedTitle ($distroName)"
)
$window = [LswWslgHostWindow]::Resolve($allowedTitles)
switch ($operation) {
    'minimize' {
        if (-not [LswWslgHostWindow]::PostMessageW(
            $window, 0x0112, [IntPtr]::new(0xF020), [IntPtr]::Zero
        )) {
            throw 'PostMessage(SC_MINIMIZE) failed for the identity-checked WSLg proxy'
        }
        $deadline = [DateTime]::UtcNow.AddSeconds(2)
        do {
            if ([LswWslgHostWindow]::IsIconic($window)) {
                return
            }
            [Threading.Thread]::Sleep(20)
        } while ([DateTime]::UtcNow -lt $deadline)
        throw 'the identity-checked WSLg host window did not minimize'
    }
    default { throw 'unsupported WSLg host-window control operation' }
}
"#;

    let distro_name = env::var("WSL_DISTRO_NAME")
        .map_err(|_| "WSL_DISTRO_NAME is required for WSLg host-window control".to_owned())?;
    if distro_name.is_empty() {
        return Err("WSL_DISTRO_NAME is empty during WSLg host-window control".to_owned());
    }
    let identity = format!(
        "{operation}\n{}\n{}",
        hex_encode(host_window_title),
        hex_encode(&distro_name)
    );

    let mut child = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start WSLg {operation} control: {error}"))?;

    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| "could not open WSLg host-window control input".to_owned())
        .and_then(|mut stdin| {
            stdin
                .write_all(identity.as_bytes())
                .map_err(|error| format!("could not send the WSLg host-window identity: {error}"))
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let deadline = Instant::now() + WSLG_HOST_CONTROL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("WSLg {operation} control timed out"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "could not wait for WSLg {operation} control: {error}"
                ));
            }
        }
    };
    let mut stderr = Vec::new();
    if let Some(stream) = child.stderr.take() {
        let _ = stream.take(8 * 1024).read_to_end(&mut stderr);
    }
    let detail = String::from_utf8_lossy(&stderr);
    let detail = detail.trim_matches(['\0', '\r', '\n', ' ']);
    if status.success() {
        if !detail.is_empty() {
            trace_gui(format_args!("WSLg {operation} control: {detail}"));
        }
        return Ok(());
    }
    if detail.is_empty() {
        Err(format!("WSLg {operation} control failed with {status}"))
    } else {
        Err(format!(
            "WSLg {operation} control failed with {status}: {detail}"
        ))
    }
}

fn hex_encode(value: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len().saturating_mul(2));
    for byte in value.bytes() {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wslg_identity_hex_is_canonical_and_preserves_utf8() {
        assert_eq!(hex_encode("Notepad - LSW"), "4e6f7465706164202d204c5357");
        assert_eq!(hex_encode("é\n"), "c3a90a");
    }

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
    fn ready_updates_cannot_replace_the_pinned_guest_window() {
        let ready = GuiWindowReady {
            process_id: 7,
            window_id: 11,
            width: 640,
            height: 480,
            title: "first".to_owned(),
        };
        let mut state = FrameState::new(ready.clone()).unwrap();
        state
            .resize(GuiWindowReady {
                width: 800,
                height: 600,
                title: "renamed".to_owned(),
                ..ready.clone()
            })
            .unwrap();
        assert!(state
            .resize(GuiWindowReady {
                window_id: 12,
                ..ready
            })
            .is_err());
    }

    #[test]
    fn key_and_pointer_translation_are_stable() {
        assert_eq!(windows_key(KeyCode::KeyA), Some((0x41, false)));
        assert_eq!(windows_key(KeyCode::ControlRight), Some((0xa3, true)));
        assert_eq!(windows_key(KeyCode::AudioVolumeUp), None);
        assert_eq!(
            scale_pointer(50.0, 25.0, (100, 50), (200, 100)),
            Some((100, 50))
        );
        assert_eq!(
            fit_viewport((2560, 1380), (1280, 752)),
            Viewport {
                x: 106,
                y: 0,
                width: 2348,
                height: 1380,
            }
        );
        assert_eq!(scale_pointer(50.0, 690.0, (2560, 1380), (1280, 752)), None);
        assert_eq!(
            scale_pointer(1280.0, 690.0, (2560, 1380), (1280, 752)),
            Some((640, 376))
        );
    }

    #[test]
    fn ctrl_c_and_ctrl_v_edges_map_to_windows_virtual_keys() {
        let mut input = ForwardedInputState::default();
        assert_eq!(
            input.key_event(KeyCode::ControlLeft, true),
            Some(GuiInputEvent::Key {
                virtual_key: 0xa2,
                scan_code: 0,
                pressed: true,
                extended: false,
            })
        );
        assert_eq!(windows_key(KeyCode::KeyC), Some((0x43, false)));
        assert_eq!(windows_key(KeyCode::KeyV), Some((0x56, false)));
        assert!(input.key_event(KeyCode::KeyC, true).is_some());
        assert!(input.key_event(KeyCode::KeyC, false).is_some());
        assert!(input.key_event(KeyCode::KeyV, true).is_some());
        assert!(input.key_event(KeyCode::KeyV, false).is_some());
        assert!(input.key_event(KeyCode::ControlLeft, false).is_some());
        assert!(input.release_events(None).is_empty());
    }

    #[test]
    fn guest_border_actions_map_to_all_eight_host_resize_directions() {
        for (action, direction) in [
            (GuiWindowAction::ResizeTopLeft, ResizeDirection::NorthWest),
            (GuiWindowAction::ResizeTop, ResizeDirection::North),
            (GuiWindowAction::ResizeTopRight, ResizeDirection::NorthEast),
            (GuiWindowAction::ResizeRight, ResizeDirection::East),
            (
                GuiWindowAction::ResizeBottomRight,
                ResizeDirection::SouthEast,
            ),
            (GuiWindowAction::ResizeBottom, ResizeDirection::South),
            (
                GuiWindowAction::ResizeBottomLeft,
                ResizeDirection::SouthWest,
            ),
            (GuiWindowAction::ResizeLeft, ResizeDirection::West),
        ] {
            assert_eq!(host_resize_direction(action), Some(direction));
        }
        assert_eq!(host_resize_direction(GuiWindowAction::Move), None);
        assert_eq!(host_resize_direction(GuiWindowAction::Close), None);
    }

    #[test]
    fn forwarded_input_releases_chords_in_reverse_order_and_ignores_stray_releases() {
        let mut input = ForwardedInputState::default();
        assert_eq!(
            input.key_event(KeyCode::ControlLeft, true),
            Some(GuiInputEvent::Key {
                virtual_key: 0xa2,
                scan_code: 0,
                pressed: true,
                extended: false,
            })
        );
        assert!(input.key_event(KeyCode::KeyC, true).is_some());
        assert!(input.key_event(KeyCode::KeyC, true).is_some());
        assert_eq!(input.pressed_keys.len(), 2);

        assert_eq!(
            input.release_events(None),
            vec![
                GuiInputEvent::Key {
                    virtual_key: 0x43,
                    scan_code: 0,
                    pressed: false,
                    extended: false,
                },
                GuiInputEvent::Key {
                    virtual_key: 0xa2,
                    scan_code: 0,
                    pressed: false,
                    extended: false,
                },
            ]
        );
        assert_eq!(input.key_event(KeyCode::KeyC, false), None);
        assert!(input.release_events(None).is_empty());
    }

    #[test]
    fn forwarded_input_idle_lease_bounds_a_missing_focus_leave() {
        let mut input = ForwardedInputState::default();
        let started = Instant::now();
        assert!(input.key_event(KeyCode::ControlLeft, true).is_some());
        input.note_activity(started);
        assert_eq!(
            input.lease_deadline(),
            Some(started + FORWARDED_INPUT_IDLE_LEASE)
        );
        assert!(
            !input.lease_expired(started + FORWARDED_INPUT_IDLE_LEASE - Duration::from_millis(1))
        );
        assert!(input.lease_expired(started + FORWARDED_INPUT_IDLE_LEASE));

        let refreshed = started + Duration::from_secs(1);
        input.note_activity(refreshed);
        assert_eq!(
            input.lease_deadline(),
            Some(refreshed + FORWARDED_INPUT_IDLE_LEASE)
        );
        assert_eq!(input.release_events(None).len(), 1);
        assert_eq!(input.lease_deadline(), None);
    }

    #[test]
    fn smooth_wheel_preserves_fractional_residuals() {
        let mut residual = 0.0;
        assert_eq!(accumulate_wheel(&mut residual, 0.4), None);
        assert_eq!(accumulate_wheel(&mut residual, 0.4), None);
        assert_eq!(accumulate_wheel(&mut residual, 0.4), Some(1));
        assert!((residual - 0.2).abs() < f64::EPSILON * 4.0);
        assert_eq!(accumulate_wheel(&mut residual, f64::NAN), None);
    }

    #[test]
    fn drag_grant_is_recent_and_one_shot() {
        let start = Instant::now();
        let mut grant = DragGrant::default();
        assert!(!grant.take(start));
        grant.arm(start);
        assert!(grant.take(start + Duration::from_millis(10)));
        assert!(!grant.take(start + Duration::from_millis(11)));
        grant.arm(start);
        assert!(!grant.take(start + DRAG_GRANT_TIMEOUT + Duration::from_millis(1)));
    }

    #[test]
    fn drag_hint_requires_the_exact_recent_guest_pointer() {
        let start = Instant::now();
        let mut hint = DragHintState::default();
        hint.observe(
            GuiWindowDragHint {
                x: 12,
                y: 34,
                action: Some(GuiWindowAction::Move),
            },
            start,
        );
        assert_eq!(
            hint.observation_for(Some((12, 34)), start + Duration::from_millis(10))
                .flatten(),
            Some(GuiWindowAction::Move)
        );
        assert_eq!(
            hint.observation_for(Some((13, 34)), start + Duration::from_millis(10))
                .flatten(),
            None
        );
        assert_eq!(
            hint.observation_for(
                Some((12, 34)),
                start + DRAG_HINT_TIMEOUT + Duration::from_millis(1),
            )
            .flatten(),
            None
        );
        hint.observe(
            GuiWindowDragHint {
                x: 12,
                y: 34,
                action: None,
            },
            start,
        );
        assert_eq!(hint.observation_for(Some((12, 34)), start).flatten(), None);
        assert_eq!(hint.observation_for(Some((12, 34)), start), Some(None));
    }

    #[test]
    fn host_window_state_suppresses_guest_echo_but_reports_native_changes() {
        let mut state = HostWindowState::new(false);
        state.expect(true);
        assert_eq!(state.observe(true), None);
        assert_eq!(state.observe(false), Some(GuiWindowAction::Restore));
        assert_eq!(state.observe(false), None);
        assert_eq!(state.observe(true), Some(GuiWindowAction::Maximize));
    }

    #[test]
    fn shutdown_unblocks_a_reader_stalled_on_a_full_presenter_queue() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(1_u8).unwrap();
        let (attempted_sender, attempted_receiver) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            attempted_sender.send(()).unwrap();
            assert!(sender.send(2).is_err());
        });
        attempted_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        stop_event_reader(receiver, || {}, reader_thread).unwrap();
    }

    #[test]
    fn rendering_scales_without_indexing_outside_the_guest_frame() {
        let ready = GuiWindowReady {
            process_id: 1,
            window_id: 2,
            width: 2,
            height: 1,
            title: "test".to_owned(),
        };
        let mut state = FrameState::new(ready).unwrap();
        state.pixels = vec![0x11, 0x22];
        let mut destination = vec![0; 8];
        render_scaled(&state, 4, 2, &mut destination).unwrap();
        assert_eq!(
            destination,
            vec![0x11, 0x11, 0x22, 0x22, 0x11, 0x11, 0x22, 0x22]
        );

        let mut letterboxed = vec![0xff; 16];
        render_scaled(&state, 4, 4, &mut letterboxed).unwrap();
        assert_eq!(
            letterboxed,
            vec![0, 0, 0, 0, 0x11, 0x11, 0x22, 0x22, 0x11, 0x11, 0x22, 0x22, 0, 0, 0, 0,]
        );
    }

    #[test]
    fn forwarded_input_releases_only_buttons_sent_to_the_guest() {
        let mut input = ForwardedInputState::default();
        assert!(input
            .pointer_button_event(0, GuiPointerButton::Left, true, 12, 34)
            .is_some());
        assert_eq!(
            input.pointer_button_event(0, GuiPointerButton::Left, true, 12, 34),
            None
        );
        assert_eq!(
            input.release_events(Some((56, 78))),
            vec![GuiInputEvent::PointerButton {
                button: GuiPointerButton::Left,
                pressed: false,
                x: 56,
                y: 78,
            }]
        );
        assert!(input.release_events(Some((56, 78))).is_empty());
    }
}
