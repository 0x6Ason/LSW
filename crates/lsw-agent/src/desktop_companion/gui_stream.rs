// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) enum GuiControl {
    Frame(Frame),
    Disconnected(String),
}

pub(super) fn stream_gui_window(
    stream: &mut TcpStream,
    request: &GuiStartRequest,
    claim: GuiSessionClaim<windows_capture::WindowHandle>,
    recovered: Option<windows_capture::WindowHandle>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(mut window) = recovered {
        // An abnormal disconnect normally releases every injected key/button
        // before retention. Retry that fail-safe boundary before exposing the
        // reattached HWND in case the original SendInput release failed.
        if let Err(error) = window.release_injected_input() {
            return Err(retain_recovered_window_after_setup_failure(
                claim,
                window,
                format!(
                    "could not release input before reattaching the recovered GUI window: {error}"
                ),
            ));
        }
        let process_id = window.process_id();
        let _dpi_awareness = match windows_capture::ThreadDpiAwareness::per_monitor_v2() {
            Ok(awareness) => awareness,
            Err(error) => {
                return Err(retain_recovered_window_after_setup_failure(
                    claim,
                    window,
                    format!(
                        "could not enter physical-pixel DPI mode before reattaching the recovered GUI window: {error}"
                    ),
                ));
            }
        };
        return stream_selected_gui_window(stream, window, process_id, None, claim);
    }
    let existing_windows = windows_capture::visible_windows()?;
    if aam_activation_is_eligible(request) {
        if let Some(activation) =
            windows_capture::activate_packaged_alias(&request.request.argv[0])?
        {
            // Enter physical pixels after AAM has created the application so
            // LSW cannot impose a DPI policy on the activated process.
            let _dpi_awareness = windows_capture::ThreadDpiAwareness::per_monitor_v2()?;
            let (window, window_process_id) =
                windows_capture::find_activated_window(activation, &existing_windows)?;
            return stream_selected_gui_window(stream, window, window_process_id, None, claim);
        }
    }

    let mut child = spawn_gui(request)?;
    let process_id = child.id();
    // Enter the physical-pixel coordinate space only after spawning the child
    // so LSW cannot accidentally impose a DPI policy on the application.
    let _dpi_awareness = windows_capture::ThreadDpiAwareness::per_monitor_v2()?;
    let (window, window_process_id) =
        windows_capture::find_process_window(process_id, &existing_windows, &mut child)?;
    let result = stream_selected_gui_window(
        stream,
        window,
        window_process_id,
        Some((&mut child, process_id)),
        claim,
    );
    // Reap a launcher that exited naturally, but never force-kill a GUI
    // process: it may be showing a native save confirmation or own unsaved
    // data that must remain available for explicit recovery.
    let _ = child.try_wait();
    result
}

pub(super) fn aam_activation_is_eligible(request: &GuiStartRequest) -> bool {
    request.request.argv.len() == 1
        && request.request.working_directory.is_none()
        && request.environment.is_empty()
}

pub(super) fn stream_selected_gui_window(
    stream: &mut TcpStream,
    mut window: windows_capture::WindowHandle,
    window_process_id: u32,
    mut launcher: Option<(&mut Child, u32)>,
    claim: GuiSessionClaim<windows_capture::WindowHandle>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut explicit_close_requested = false;
    let mut reader_thread = None;
    let session_result: Result<(), Box<dyn std::error::Error>> = (|| {
        let mut capture = windows_capture::CaptureSession::start(&window)?;
        let first_deadline = Instant::now() + FIRST_CAPTURE_TIMEOUT;
        let first = loop {
            if let Some(frame) = capture.next_frame(&window, Duration::from_millis(250))? {
                break frame;
            }
            if !window.is_open() {
                return Err(
                    "GUI window closed before its first Windows Graphics Capture frame".into(),
                );
            }
            if Instant::now() >= first_deadline {
                return Err(
                    "timed out waiting for the first Windows Graphics Capture frame".into(),
                );
            }
        };
        let mut width = first.width;
        let mut height = first.height;
        window.set_capture_size(width, height);
        write_frame(
            stream,
            &Frame::new(
                FrameKind::GuiWindowReady,
                window.ready(window_process_id, width, height)?.encode()?,
            ),
        )?;
        if let Some(action) = initial_window_state_action(window.is_maximized()?) {
            // A packaged desktop app can remember a maximized state from its last
            // run. Mirror that state immediately after Ready so the new host
            // presenter never starts out divergent from the guest HWND.
            send_gui_action(stream, action)?;
        }
        let mut damage = DamageTracker::default();
        send_damages(stream, damage.update(width, height, &first.bgra)?)?;

        let mut reader = stream.try_clone()?;
        let (control_sender, control_receiver) = mpsc::sync_channel(128);
        reader_thread = Some(thread::spawn(move || loop {
            match read_frame(&mut reader) {
                Ok(frame) => {
                    if control_sender.send(GuiControl::Frame(frame)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = control_sender.send(GuiControl::Disconnected(error.to_string()));
                    return;
                }
            }
        }));

        'session: loop {
            if !window.is_open() {
                break acknowledge_closed_gui_window(
                    stream,
                    &mut window,
                    &mut launcher,
                    window_process_id,
                );
            }
            'control: for _ in 0..MAX_CONTROLS_PER_CAPTURE_POLL {
                match control_receiver.try_recv() {
                    Ok(GuiControl::Frame(frame)) => match frame.kind {
                        FrameKind::GuiWindowInput => {
                            let outcome = GuiInputEvent::decode(&frame.payload)
                                .map_err(|error| error.into())
                                .and_then(|event| {
                                    window.input(event).map_err(|error| error.into())
                                });
                            match outcome {
                                Ok(windows_capture::GuiInputOutcome::Action(action)) => {
                                    if let Err(error) = send_gui_action(stream, action) {
                                        break 'session Err(error);
                                    }
                                }
                                Ok(windows_capture::GuiInputOutcome::DragHint(hint)) => {
                                    if let Err(error) = send_gui_drag_hint(stream, hint) {
                                        break 'session Err(error);
                                    }
                                }
                                Ok(windows_capture::GuiInputOutcome::None) => {}
                                Err(_) if !window.is_open() => {
                                    break 'session acknowledge_closed_gui_window(
                                        stream,
                                        &mut window,
                                        &mut launcher,
                                        window_process_id,
                                    );
                                }
                                Err(error) => break 'session Err(error),
                            }
                        }
                        FrameKind::GuiWindowResize => {
                            if let Err(error) = GuiWindowResize::decode(&frame.payload)
                                .map_err(|error| error.into())
                                .and_then(|resize| {
                                    window.resize(resize).map_err(|error| error.into())
                                })
                            {
                                if !window.is_open() {
                                    break 'session acknowledge_closed_gui_window(
                                        stream,
                                        &mut window,
                                        &mut launcher,
                                        window_process_id,
                                    );
                                }
                                break 'session Err(error);
                            }
                        }
                        FrameKind::GuiWindowAction => {
                            let maximized = match GuiWindowAction::decode(&frame.payload) {
                                Ok(GuiWindowAction::Maximize) => true,
                                Ok(GuiWindowAction::Restore) => false,
                                Ok(_) => {
                                    break 'session Err(
                                        "host may send only explicit maximize or restore state"
                                            .into(),
                                    )
                                }
                                Err(error) => break 'session Err(error.into()),
                            };
                            if let Err(error) = window.set_maximized(maximized) {
                                if !window.is_open() {
                                    break 'session acknowledge_closed_gui_window(
                                        stream,
                                        &mut window,
                                        &mut launcher,
                                        window_process_id,
                                    );
                                }
                                break 'session Err(error.into());
                            }
                        }
                        FrameKind::GuiWindowClose if frame.payload.is_empty() => {
                            explicit_close_requested = true;
                            if let Err(error) = window.release_injected_input() {
                                break 'session Err(error.into());
                            }
                            // SendInput queues release edges asynchronously.
                            // Do not let a subsequently posted WM_CLOSE overtake
                            // the releases and expose a stuck key/button to the
                            // application's FormClosing/save-confirmation path.
                            if let Err(error) = window.settle_released_input() {
                                break 'session Err(error.into());
                            }
                            if let Err(error) = window.close() {
                                if window.is_open() {
                                    break 'session Err(error.into());
                                }
                            }
                        }
                        FrameKind::GuiWindowClose => {
                            break 'session Err("GUI_WINDOW_CLOSE payload must be empty".into());
                        }
                        _ => {
                            break 'session Err("invalid frame in a seamless GUI session".into());
                        }
                    },
                    Ok(GuiControl::Disconnected(error)) => {
                        break 'session Err(
                            format!("seamless GUI client disconnected: {error}").into()
                        );
                    }
                    Err(TryRecvError::Empty) => break 'control,
                    Err(TryRecvError::Disconnected) => {
                        break 'session Err("seamless GUI input channel closed".into());
                    }
                }
            }

            if !window.is_open() {
                break acknowledge_closed_gui_window(
                    stream,
                    &mut window,
                    &mut launcher,
                    window_process_id,
                );
            }
            match capture.next_frame(&window, CAPTURE_POLL_INTERVAL) {
                Ok(Some(frame)) => {
                    if (frame.width, frame.height) != (width, height) {
                        width = frame.width;
                        height = frame.height;
                        window.set_capture_size(width, height);
                        let ready = window
                            .ready(window_process_id, width, height)
                            .and_then(|ready| Ok(ready.encode()?));
                        match ready.and_then(|payload| {
                            write_frame(stream, &Frame::new(FrameKind::GuiWindowReady, payload))
                                .map_err(|error| error.into())
                        }) {
                            Ok(()) => {}
                            Err(error) => break 'session Err(error),
                        }
                    }
                    match damage
                        .update(width, height, &frame.bgra)
                        .map_err(|error| error.into())
                        .and_then(|damages| send_damages(stream, damages))
                    {
                        Ok(()) => {}
                        Err(error) => break 'session Err(error),
                    }
                }
                Ok(None) => {}
                Err(_) if !window.is_open() => {
                    break 'session acknowledge_closed_gui_window(
                        stream,
                        &mut window,
                        &mut launcher,
                        window_process_id,
                    );
                }
                Err(error) => break 'session Err(error),
            }
        }
    })();

    let end_policy = gui_session_end_policy(explicit_close_requested, window.is_open());
    let session_result = if session_result.is_ok() && end_policy == GuiSessionEndPolicy::Detach {
        Err("seamless GUI session ended without closing its live HWND".into())
    } else {
        session_result
    };
    let result = match end_policy {
        GuiSessionEndPolicy::NaturalGuestClose => merge_session_results(
            session_result,
            window
                .release_injected_input()
                .map_err(|error| error.into()),
            "could not release injected input after the guest window closed",
        ),
        GuiSessionEndPolicy::ExplicitHostClose => merge_session_results(
            session_result,
            window
                .release_injected_input()
                .map_err(|error| error.into()),
            "could not release injected input after explicit GUI close",
        ),
        GuiSessionEndPolicy::Detach => {
            let release = window
                .release_injected_input()
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() });
            let detached_result = merge_session_results(
                session_result,
                release,
                "could not release injected input before retaining the GUI window",
            );
            let retain = if window.is_open() {
                claim.retain(window)
            } else {
                Ok(())
            };
            merge_session_results(
                detached_result,
                retain,
                "could not retain the live GUI window for recovery",
            )
        }
    };
    if let Err(error) = &result {
        let _ = send_error(stream, &error.to_string());
    }
    let _ = stream.shutdown(Shutdown::Both);
    let reader_result = reader_thread.map_or(Ok(()), |thread| {
        thread
            .join()
            .map_err(|_| "seamless GUI control reader panicked".into())
    });
    merge_session_results(result, reader_result, "GUI control reader cleanup failed")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GuiSessionEndPolicy {
    NaturalGuestClose,
    ExplicitHostClose,
    Detach,
}

pub(super) fn gui_session_end_policy(
    explicit_close_requested: bool,
    window_is_live: bool,
) -> GuiSessionEndPolicy {
    if window_is_live {
        GuiSessionEndPolicy::Detach
    } else if explicit_close_requested {
        GuiSessionEndPolicy::ExplicitHostClose
    } else {
        GuiSessionEndPolicy::NaturalGuestClose
    }
}

pub(super) fn merge_session_results(
    primary: Result<(), Box<dyn std::error::Error>>,
    secondary: Result<(), Box<dyn std::error::Error>>,
    secondary_context: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match (primary, secondary) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(format!("{secondary_context}: {error}").into()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(secondary)) => {
            Err(format!("{error}; {secondary_context}: {secondary}").into())
        }
    }
}

pub(super) fn send_damages(
    stream: &mut TcpStream,
    damages: Vec<lsw_core::GuiWindowDamage>,
) -> Result<(), Box<dyn std::error::Error>> {
    for damage in damages {
        write_frame(
            stream,
            &Frame::new(FrameKind::GuiWindowDamage, damage.encode()?),
        )?;
    }
    Ok(())
}

pub(super) fn send_gui_closed(
    stream: &mut TcpStream,
    exit_code: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(
        stream,
        &Frame::new(
            FrameKind::GuiWindowClosed,
            GuiWindowClosed { exit_code }.encode().to_vec(),
        ),
    )?;
    Ok(())
}

pub(super) fn send_gui_action(
    stream: &mut TcpStream,
    action: GuiWindowAction,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(
        stream,
        &Frame::new(FrameKind::GuiWindowAction, action.encode()),
    )?;
    Ok(())
}

pub(super) fn send_gui_drag_hint(
    stream: &mut TcpStream,
    hint: GuiWindowDragHint,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(
        stream,
        &Frame::new(FrameKind::GuiWindowDragHint, hint.encode()?),
    )?;
    Ok(())
}

pub(super) fn initial_window_state_action(is_maximized: bool) -> Option<GuiWindowAction> {
    is_maximized.then_some(GuiWindowAction::Maximize)
}

pub(super) fn acknowledge_closed_gui_window(
    stream: &mut TcpStream,
    window: &mut windows_capture::WindowHandle,
    launcher: &mut Option<(&mut Child, u32)>,
    window_process_id: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    window.release_injected_input()?;
    let exit_code = match launcher.as_mut() {
        Some((child, process_id)) => {
            observed_gui_close_exit_code(child, window_process_id == *process_id)?
        }
        None => 0,
    };
    send_gui_closed(stream, exit_code)
}

pub(super) fn observed_gui_close_exit_code(
    child: &mut Child,
    launcher_owns_window: bool,
) -> io::Result<i32> {
    let observed = if launcher_owns_window {
        child.try_wait()?.and_then(|status| status.code())
    } else {
        None
    };
    Ok(close_ack_exit_code(observed))
}

pub(super) fn close_ack_exit_code(observed: Option<i32>) -> i32 {
    // A destroyed requested HWND is the user-visible completion condition. A
    // launcher still draining at that instant is not reported as a failed GUI
    // action; teardown never force-kills a process with unsaved data.
    observed.unwrap_or(0)
}
