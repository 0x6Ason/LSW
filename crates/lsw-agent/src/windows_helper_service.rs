// SPDX-License-Identifier: GPL-3.0-or-later

//! One demand-start SCM shell for the privileged LSW helper services.
//!
//! The license, local-account, and maintenance helpers all run as LocalSystem,
//! accept no stop control, and exit after one authenticated loopback request.
//! Only the service name and the request handler differ, so they share this
//! adapter instead of three copies of the SCM plumbing.

use std::ffi::c_void;
use std::io;
use std::sync::Mutex;

use super::windows_scm::{
    self, ServiceStatus, ServiceStatusHandle, ERROR_CALL_NOT_IMPLEMENTED,
    ERROR_SERVICE_SPECIFIC_ERROR, SERVICE_CONTROL_INTERROGATE, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
};
use super::{run_license_helper, run_maintenance_helper, run_user_helper, Configuration};

type HelperResult = Result<(), Box<dyn std::error::Error>>;
type ReadyCallback<'a> = &'a mut dyn FnMut() -> HelperResult;
type HelperRunner = fn(Configuration, ReadyCallback<'_>) -> HelperResult;

/// The privileged helper services the agent binary can host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HelperService {
    License,
    User,
    Maintenance,
}

impl HelperService {
    /// The service name registered with SCM by the installation seed.
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::License => "LSWLicenseHelper",
            Self::User => "LSWUserHelper",
            Self::Maintenance => "LSWMaintenanceHelper",
        }
    }

    fn runner(self) -> HelperRunner {
        match self {
            Self::License => |configuration, ready| run_license_helper(configuration, ready),
            Self::User => |configuration, ready| run_user_helper(configuration, ready),
            Self::Maintenance => {
                |configuration, ready| run_maintenance_helper(configuration, ready)
            }
        }
    }
}

struct PendingService {
    service: HelperService,
    configuration: Configuration,
}

// SCM invokes `service_main` on its own thread without a context pointer, so
// the configuration crosses through this one process-wide slot.
static PENDING: Mutex<Option<PendingService>> = Mutex::new(None);
static SERVICE_ERROR: Mutex<Option<String>> = Mutex::new(None);

pub(super) fn run(service: HelperService, configuration: Configuration) -> HelperResult {
    *PENDING
        .lock()
        .map_err(|_| "helper service configuration lock was poisoned")? = Some(PendingService {
        service,
        configuration,
    });
    *SERVICE_ERROR
        .lock()
        .map_err(|_| "helper service error lock was poisoned")? = None;
    if let Err(error) = windows_scm::dispatch(service.name(), service_main) {
        if let Ok(mut pending) = PENDING.lock() {
            pending.take();
        }
        return Err(format!("could not connect {} to SCM: {error}", service.name()).into());
    }
    match SERVICE_ERROR
        .lock()
        .map_err(|_| "helper service error lock was poisoned")?
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

fn service_main_inner() -> HelperResult {
    let PendingService {
        service,
        configuration,
    } = PENDING
        .lock()
        .map_err(|_| "helper service configuration lock was poisoned")?
        .take()
        .ok_or("helper service configuration was not initialized")?;
    let handle = windows_scm::register_control_handler(service.name(), service_control_handler)?;
    report_status(handle, SERVICE_START_PENDING, 0, 0, 1, 10_000)?;
    let mut ready = || -> HelperResult {
        report_status(handle, SERVICE_RUNNING, 0, 0, 0, 0)?;
        Ok(())
    };
    let result = (service.runner())(configuration, &mut ready);
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
    windows_scm::set_status(
        handle,
        &ServiceStatus {
            service_type: SERVICE_WIN32_OWN_PROCESS,
            current_state: state,
            controls_accepted: 0,
            win32_exit_code,
            service_specific_exit_code,
            checkpoint,
            wait_hint,
        },
    )
}
