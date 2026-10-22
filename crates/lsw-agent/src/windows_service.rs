// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows Service Control Manager adapter for the ordinary LSW agent.
//!
//! SCM callbacks cannot carry Rust state directly. The mutex-protected handoff
//! and atomics below keep callback state bounded while preserving stop ordering.

use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Mutex;

use super::windows_scm::{
    self, ServiceStatus, ServiceStatusHandle, ERROR_CALL_NOT_IMPLEMENTED,
    ERROR_SERVICE_SPECIFIC_ERROR, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_WIN32_OWN_PROCESS,
};
use super::{run_agent, Configuration};

const SERVICE_NAME: &str = "LSWAgent";

static CONFIGURATION: Mutex<Option<Configuration>> = Mutex::new(None);
static STOP_SENDER: Mutex<Option<SyncSender<()>>> = Mutex::new(None);
static SERVICE_ERROR: Mutex<Option<String>> = Mutex::new(None);
static STATUS_REPORT_LOCK: Mutex<()> = Mutex::new(());
static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);
static CURRENT_STATE: AtomicUsize = AtomicUsize::new(SERVICE_STOPPED as usize);

pub(super) fn run(configuration: Configuration) -> Result<(), Box<dyn std::error::Error>> {
    *CONFIGURATION
        .lock()
        .map_err(|_| "Windows service configuration lock was poisoned")? = Some(configuration);
    *STOP_SENDER
        .lock()
        .map_err(|_| "Windows service stop lock was poisoned")? = None;
    *SERVICE_ERROR
        .lock()
        .map_err(|_| "Windows service error lock was poisoned")? = None;
    STATUS_HANDLE.store(0, Ordering::Release);
    CURRENT_STATE.store(SERVICE_STOPPED as usize, Ordering::Release);

    if let Err(error) = windows_scm::dispatch(SERVICE_NAME, service_main) {
        if let Ok(mut configuration) = CONFIGURATION.lock() {
            configuration.take();
        }
        return Err(format!(
            "could not connect LSWAgent to the Windows Service Control Manager: {error}; \
             --service must be started by SCM"
        )
        .into());
    }

    STATUS_HANDLE.store(0, Ordering::Release);
    let error = SERVICE_ERROR
        .lock()
        .map_err(|_| "Windows service error lock was poisoned")?
        .take();
    match error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

extern "system" fn service_main(_argument_count: u32, _arguments: *mut *mut u16) {
    let result = service_main_inner();
    let failed = result.is_err();
    let (win32_exit_code, service_specific_exit_code) = if failed {
        (ERROR_SERVICE_SPECIFIC_ERROR, 1)
    } else {
        (0, 0)
    };
    let stopped_result = report_status(
        SERVICE_STOPPED,
        0,
        win32_exit_code,
        service_specific_exit_code,
        0,
        0,
    );

    if let Err(error) = result {
        record_error(error.to_string());
    }
    if let Err(error) = stopped_result {
        record_error(format!("could not report LSWAgent as stopped: {error}"));
    }
}

fn service_main_inner() -> Result<(), Box<dyn std::error::Error>> {
    let status_handle =
        windows_scm::register_control_handler(SERVICE_NAME, service_control_handler).map_err(
            |error| format!("could not register the LSWAgent service control handler: {error}"),
        )?;
    STATUS_HANDLE.store(status_handle as usize, Ordering::Release);
    report_status(SERVICE_START_PENDING, 0, 0, 0, 1, 10_000)?;

    let configuration = CONFIGURATION
        .lock()
        .map_err(|_| "Windows service configuration lock was poisoned")?
        .take()
        .ok_or("Windows service configuration was not initialized")?;
    let (stop_sender, stop_receiver) = mpsc::sync_channel(1);
    *STOP_SENDER
        .lock()
        .map_err(|_| "Windows service stop lock was poisoned")? = Some(stop_sender);

    let result = run_agent(configuration, Some(&stop_receiver), |_address| {
        report_status(
            SERVICE_RUNNING,
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
            0,
            0,
            0,
            0,
        )?;
        Ok(())
    });

    if let Ok(mut stop_sender) = STOP_SENDER.lock() {
        stop_sender.take();
    }
    if CURRENT_STATE.load(Ordering::Acquire) == SERVICE_RUNNING as usize {
        report_status(SERVICE_STOP_PENDING, 0, 0, 0, 1, 10_000)?;
    }
    result
}

extern "system" fn service_control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            let _ = report_status(SERVICE_STOP_PENDING, 0, 0, 0, 1, 10_000);
            if let Ok(stop_sender) = STOP_SENDER.lock() {
                if let Some(stop_sender) = stop_sender.as_ref() {
                    let _ = stop_sender.try_send(());
                }
            }
            0
        }
        // SCM already retains the last status sent with SetServiceStatus;
        // acknowledging INTERROGATE asks it to return that cached state.
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

fn report_status(
    state: u32,
    controls_accepted: u32,
    win32_exit_code: u32,
    service_specific_exit_code: u32,
    checkpoint: u32,
    wait_hint: u32,
) -> io::Result<()> {
    let _status_guard = STATUS_REPORT_LOCK
        .lock()
        .map_err(|_| io::Error::other("service status lock was poisoned"))?;
    let status_handle = STATUS_HANDLE.load(Ordering::Acquire) as ServiceStatusHandle;
    windows_scm::set_status(
        status_handle,
        &ServiceStatus {
            service_type: SERVICE_WIN32_OWN_PROCESS,
            current_state: state,
            controls_accepted,
            win32_exit_code,
            service_specific_exit_code,
            checkpoint,
            wait_hint,
        },
    )?;
    CURRENT_STATE.store(state as usize, Ordering::Release);
    Ok(())
}

fn record_error(message: String) {
    if let Ok(mut service_error) = SERVICE_ERROR.lock() {
        if service_error.is_none() {
            *service_error = Some(message);
        }
    }
}
