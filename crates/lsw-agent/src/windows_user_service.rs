// SPDX-License-Identifier: GPL-3.0-or-later

//! Demand-start SCM adapter for privileged local-account operations.
//!
//! The service accepts no stop control and exits after one authenticated
//! loopback request, keeping LocalSystem code out of the normal agent path.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_void;
use std::io;
use std::ptr;
use std::sync::Mutex;

use super::{run_user_helper, Configuration};

const SERVICE_NAME: [u16; 14] = [
    b'L' as u16,
    b'S' as u16,
    b'W' as u16,
    b'U' as u16,
    b's' as u16,
    b'e' as u16,
    b'r' as u16,
    b'H' as u16,
    b'e' as u16,
    b'l' as u16,
    b'p' as u16,
    b'e' as u16,
    b'r' as u16,
    0,
];
const SERVICE_WIN32_OWN_PROCESS: u32 = 0x0000_0010;
const SERVICE_STOPPED: u32 = 0x0000_0001;
const SERVICE_START_PENDING: u32 = 0x0000_0002;
const SERVICE_RUNNING: u32 = 0x0000_0004;
const SERVICE_CONTROL_INTERROGATE: u32 = 0x0000_0004;
const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;
const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;

type ServiceMainFunction = extern "system" fn(u32, *mut *mut u16);
type HandlerFunction = extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32;
type ServiceStatusHandle = *mut c_void;

#[repr(C)]
struct ServiceTableEntryW {
    service_name: *mut u16,
    service_main: Option<ServiceMainFunction>,
}

#[repr(C)]
struct ServiceStatus {
    service_type: u32,
    current_state: u32,
    controls_accepted: u32,
    win32_exit_code: u32,
    service_specific_exit_code: u32,
    checkpoint: u32,
    wait_hint: u32,
}

#[link(name = "advapi32")]
extern "system" {
    fn StartServiceCtrlDispatcherW(service_table: *const ServiceTableEntryW) -> i32;
    fn RegisterServiceCtrlHandlerExW(
        service_name: *const u16,
        handler: Option<HandlerFunction>,
        context: *mut c_void,
    ) -> ServiceStatusHandle;
    fn SetServiceStatus(
        status_handle: ServiceStatusHandle,
        service_status: *const ServiceStatus,
    ) -> i32;
}

static CONFIGURATION: Mutex<Option<Configuration>> = Mutex::new(None);
static SERVICE_ERROR: Mutex<Option<String>> = Mutex::new(None);

pub(super) fn run(configuration: Configuration) -> Result<(), Box<dyn std::error::Error>> {
    *CONFIGURATION
        .lock()
        .map_err(|_| "user service configuration lock was poisoned")? = Some(configuration);
    *SERVICE_ERROR
        .lock()
        .map_err(|_| "user service error lock was poisoned")? = None;
    let mut service_name = SERVICE_NAME.to_vec();
    let service_table = [
        ServiceTableEntryW {
            service_name: service_name.as_mut_ptr(),
            service_main: Some(service_main),
        },
        ServiceTableEntryW {
            service_name: ptr::null_mut(),
            service_main: None,
        },
    ];
    // SAFETY: the table is null-terminated, its UTF-16 name and callbacks stay
    // alive for the blocking dispatcher call, and SCM owns no Rust allocation.
    if unsafe { StartServiceCtrlDispatcherW(service_table.as_ptr()) } == 0 {
        return Err(format!(
            "could not connect LSWUserHelper to SCM: {}",
            io::Error::last_os_error()
        )
        .into());
    }
    match SERVICE_ERROR
        .lock()
        .map_err(|_| "user service error lock was poisoned")?
        .take()
    {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

extern "system" fn service_main(_argument_count: u32, _arguments: *mut *mut u16) {
    if let Err(error) = service_main_inner() {
        if let Ok(mut recorded) = SERVICE_ERROR.lock() {
            *recorded = Some(error.to_string());
        }
    }
}

fn service_main_inner() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: SERVICE_NAME has static storage and a trailing NUL; the callback
    // and null context remain valid for the registered service lifetime.
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(
            SERVICE_NAME.as_ptr(),
            Some(service_control_handler),
            ptr::null_mut(),
        )
    };
    if handle.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    report_status(handle, SERVICE_START_PENDING, 0, 0, 1, 10_000)?;
    let configuration = CONFIGURATION
        .lock()
        .map_err(|_| "user service configuration lock was poisoned")?
        .take()
        .ok_or("user service configuration was not initialized")?;
    let result = run_user_helper(configuration, || {
        report_status(handle, SERVICE_RUNNING, 0, 0, 0, 0)?;
        Ok(())
    });
    let (win32_exit, service_exit) = if result.is_ok() {
        (0, 0)
    } else {
        (ERROR_SERVICE_SPECIFIC_ERROR, 1)
    };
    report_status(handle, SERVICE_STOPPED, win32_exit, service_exit, 0, 0)?;
    result
}

extern "system" fn service_control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    if control == SERVICE_CONTROL_INTERROGATE {
        0
    } else {
        ERROR_CALL_NOT_IMPLEMENTED
    }
}

fn report_status(
    handle: ServiceStatusHandle,
    state: u32,
    win32_exit_code: u32,
    service_specific_exit_code: u32,
    checkpoint: u32,
    wait_hint: u32,
) -> io::Result<()> {
    let status = ServiceStatus {
        service_type: SERVICE_WIN32_OWN_PROCESS,
        current_state: state,
        controls_accepted: 0,
        win32_exit_code,
        service_specific_exit_code,
        checkpoint,
        wait_hint,
    };
    // SAFETY: SCM returned the live status handle and `status` remains readable
    // for the complete synchronous call.
    if unsafe { SetServiceStatus(handle, &status) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
