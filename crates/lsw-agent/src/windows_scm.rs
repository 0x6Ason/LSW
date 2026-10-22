// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal Service Control Manager bindings shared by every LSW service shell.
//!
//! Only the three advapi32 entry points a service needs are declared here so
//! the ordinary agent and the demand-start helpers share one reviewed FFI
//! surface instead of four copies.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_void;
use std::io;

pub(super) const SERVICE_WIN32_OWN_PROCESS: u32 = 0x0000_0010;
pub(super) const SERVICE_STOPPED: u32 = 0x0000_0001;
pub(super) const SERVICE_START_PENDING: u32 = 0x0000_0002;
pub(super) const SERVICE_STOP_PENDING: u32 = 0x0000_0003;
pub(super) const SERVICE_RUNNING: u32 = 0x0000_0004;
pub(super) const SERVICE_ACCEPT_STOP: u32 = 0x0000_0001;
pub(super) const SERVICE_ACCEPT_SHUTDOWN: u32 = 0x0000_0004;
pub(super) const SERVICE_CONTROL_STOP: u32 = 0x0000_0001;
pub(super) const SERVICE_CONTROL_INTERROGATE: u32 = 0x0000_0004;
pub(super) const SERVICE_CONTROL_SHUTDOWN: u32 = 0x0000_0005;
pub(super) const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;
pub(super) const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;

pub(super) type ServiceMainFunction = extern "system" fn(u32, *mut *mut u16);
pub(super) type HandlerFunction = extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32;
pub(super) type ServiceStatusHandle = *mut c_void;

#[repr(C)]
pub(super) struct ServiceTableEntryW {
    pub(super) service_name: *mut u16,
    pub(super) service_main: Option<ServiceMainFunction>,
}

#[repr(C)]
pub(super) struct ServiceStatus {
    pub(super) service_type: u32,
    pub(super) current_state: u32,
    pub(super) controls_accepted: u32,
    pub(super) win32_exit_code: u32,
    pub(super) service_specific_exit_code: u32,
    pub(super) checkpoint: u32,
    pub(super) wait_hint: u32,
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

/// A NUL-terminated UTF-16 service name for the SCM entry points.
pub(super) fn wide_service_name(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Hands the calling thread to SCM until the named service's main returns.
pub(super) fn dispatch(name: &str, service_main: ServiceMainFunction) -> io::Result<()> {
    let mut service_name = wide_service_name(name);
    let service_table = [
        ServiceTableEntryW {
            service_name: service_name.as_mut_ptr(),
            service_main: Some(service_main),
        },
        ServiceTableEntryW {
            service_name: std::ptr::null_mut(),
            service_main: None,
        },
    ];
    // SAFETY: the table is null-terminated, its UTF-16 name and callback stay
    // alive for the blocking dispatcher call, and SCM owns no Rust allocation.
    if unsafe { StartServiceCtrlDispatcherW(service_table.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Registers the control handler for the service whose main is running on
/// this thread and returns its status handle.
pub(super) fn register_control_handler(
    name: &str,
    handler: HandlerFunction,
) -> io::Result<ServiceStatusHandle> {
    let service_name = wide_service_name(name);
    // SAFETY: the UTF-16 name outlives this synchronous call, SCM copies what
    // it needs, and the handler is a plain `extern "system"` function that
    // takes no Rust context.
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(service_name.as_ptr(), Some(handler), std::ptr::null_mut())
    };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

/// Reports one status transition for an own-process service.
pub(super) fn set_status(handle: ServiceStatusHandle, status: &ServiceStatus) -> io::Result<()> {
    if handle.is_null() {
        return Err(io::Error::other("service status handle is not initialized"));
    }
    // SAFETY: SCM returned the live status handle and `status` remains readable
    // for the complete synchronous call.
    if unsafe { SetServiceStatus(handle, status) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
