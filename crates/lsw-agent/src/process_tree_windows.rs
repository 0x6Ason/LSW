// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows process-tree ownership implemented with a Job Object.
//!
//! The process is created suspended and cannot execute guest code until Job
//! assignment succeeds. Every raw handle is converted to an owned handle.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::ptr;

const CREATE_SUSPENDED: u32 = 0x0000_0004;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
#[cfg(test)]
const JOB_OBJECT_BASIC_PROCESS_ID_LIST_CLASS: u32 = 3;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
const THREAD_SUSPEND_RESUME: u32 = 0x0000_0002;
const RESUME_THREAD_FAILED: u32 = u32::MAX;
#[cfg(test)]
const SYNCHRONIZE: u32 = 0x0010_0000;
#[cfg(test)]
const WAIT_OBJECT_0: u32 = 0;
#[cfg(test)]
const WAIT_TIMEOUT: u32 = 258;
#[cfg(test)]
const ERROR_INVALID_PARAMETER: i32 = 87;

type Handle = RawHandle;

#[repr(C)]
#[derive(Default)]
struct JobObjectBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
#[derive(Default)]
struct JobObjectExtendedLimitInformation {
    basic_limit_information: JobObjectBasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[cfg(test)]
#[repr(C)]
#[derive(Default)]
struct JobObjectBasicProcessIdList {
    number_of_assigned_processes: u32,
    number_of_process_ids_in_list: u32,
    process_ids: [usize; 8],
}

#[repr(C)]
struct ThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

impl ThreadEntry32 {
    fn empty() -> io::Result<Self> {
        Ok(Self {
            size: u32::try_from(size_of::<Self>())
                .map_err(|_| io::Error::other("THREADENTRY32 is too large"))?,
            usage: 0,
            thread_id: 0,
            owner_process_id: 0,
            base_priority: 0,
            delta_priority: 0,
            flags: 0,
        })
    }
}

#[link(name = "kernel32")]
extern "system" {
    #[link_name = "AssignProcessToJobObject"]
    fn assign_process_to_job_object(job: Handle, process: Handle) -> i32;
    #[link_name = "CreateJobObjectW"]
    fn create_job_object_w(attributes: *mut c_void, name: *const u16) -> Handle;
    #[link_name = "CreateToolhelp32Snapshot"]
    fn create_toolhelp32_snapshot(flags: u32, process_id: u32) -> Handle;
    #[link_name = "OpenThread"]
    fn open_thread(access: u32, inherit_handle: i32, thread_id: u32) -> Handle;
    #[cfg(test)]
    #[link_name = "OpenProcess"]
    fn open_process(access: u32, inherit_handle: i32, process_id: u32) -> Handle;
    #[cfg(test)]
    #[link_name = "QueryInformationJobObject"]
    fn query_information_job_object(
        job: Handle,
        information_class: u32,
        information: *mut c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
    #[link_name = "ResumeThread"]
    fn resume_thread(thread: Handle) -> u32;
    #[link_name = "SetInformationJobObject"]
    fn set_information_job_object(
        job: Handle,
        information_class: u32,
        information: *const c_void,
        information_length: u32,
    ) -> i32;
    #[link_name = "TerminateJobObject"]
    fn terminate_job_object(job: Handle, exit_code: u32) -> i32;
    #[link_name = "Thread32First"]
    fn thread32_first(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
    #[link_name = "Thread32Next"]
    fn thread32_next(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
    #[cfg(test)]
    #[link_name = "WaitForSingleObject"]
    fn wait_for_single_object(handle: Handle, milliseconds: u32) -> u32;
}

pub(super) struct Prepared {
    job: Job,
}

impl Prepared {
    pub(super) fn new(command: &mut Command) -> io::Result<Self> {
        let job = Job::new()?;
        command.creation_flags(CREATE_SUSPENDED);
        Ok(Self { job })
    }

    pub(super) fn attach_and_start(self, child: &Child) -> io::Result<Owner> {
        self.job.assign(child.as_raw_handle())?;
        resume_only_suspended_thread(child.id())?;
        Ok(Owner { job: self.job })
    }
}

pub(super) struct Owner {
    job: Job,
}

impl Owner {
    pub(super) fn terminate(&self, exit_code: i32) -> io::Result<()> {
        self.job.terminate(exit_code)
    }

    #[cfg(test)]
    pub(super) fn process_ids(&self) -> io::Result<Vec<u32>> {
        self.job.process_ids()
    }
}

pub(super) struct Job {
    handle: OwnedHandle,
}

impl Job {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: null security attributes and name request an unnamed Job;
        // the returned handle is checked before ownership is assumed.
        let handle = unsafe { create_job_object_w(ptr::null_mut(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a unique, non-null owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut information = JobObjectExtendedLimitInformation::default();
        information.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let information_length = u32::try_from(size_of::<JobObjectExtendedLimitInformation>())
            .map_err(|_| io::Error::other("Job information is too large"))?;
        // SAFETY: both handles are live, and `information` remains valid for
        // the exact byte length passed for the duration of the call.
        if unsafe {
            set_information_job_object(
                handle.as_raw_handle(),
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                (&information as *const JobObjectExtendedLimitInformation).cast::<c_void>(),
                information_length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    pub(super) fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: the caller supplies a live child-process handle and the Job
        // handle remains owned by `self` for the complete call.
        if unsafe { assign_process_to_job_object(self.handle.as_raw_handle(), process) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn terminate(&self, exit_code: i32) -> io::Result<()> {
        let exit_code = u32::try_from(exit_code).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Job exit code must be non-negative",
            )
        })?;
        // SAFETY: the Job handle is live and the converted exit code is a plain
        // value consumed synchronously by TerminateJobObject.
        if unsafe { terminate_job_object(self.handle.as_raw_handle(), exit_code) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    fn process_ids(&self) -> io::Result<Vec<u32>> {
        let mut information = JobObjectBasicProcessIdList::default();
        let information_length = u32::try_from(size_of::<JobObjectBasicProcessIdList>())
            .map_err(|_| io::Error::other("Job process list is too large"))?;
        // SAFETY: the fixed test buffer and its exact length are valid for the
        // synchronous query, and the return-length pointer may be null.
        if unsafe {
            query_information_job_object(
                self.handle.as_raw_handle(),
                JOB_OBJECT_BASIC_PROCESS_ID_LIST_CLASS,
                (&mut information as *mut JobObjectBasicProcessIdList).cast::<c_void>(),
                information_length,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let listed = usize::try_from(information.number_of_process_ids_in_list).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "Job process count is invalid")
        })?;
        if listed > information.process_ids.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Job process list exceeded its test buffer",
            ));
        }
        information.process_ids[..listed]
            .iter()
            .map(|process_id| {
                u32::try_from(*process_id).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Job process id exceeds u32")
                })
            })
            .collect()
    }
}

/// `std::process::Child` does not expose the primary thread handle. A
/// CREATE_SUSPENDED process has exactly one thread before any guest code
/// runs, so identify it by owner PID and fail closed if the snapshot shows
/// zero or multiple candidates.
fn resume_only_suspended_thread(process_id: u32) -> io::Result<()> {
    // SAFETY: the flags and process-id arguments contain no pointers; the
    // returned snapshot handle is validated before it is wrapped.
    let snapshot = unsafe { create_toolhelp32_snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == (-1_isize as RawHandle) {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the validated snapshot is uniquely owned by this function.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = ThreadEntry32::empty()?;
    // SAFETY: `entry` has the required size field and is writable for the
    // duration of the call; the snapshot handle remains live.
    if unsafe { thread32_first(snapshot.as_raw_handle(), &mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut suspended_thread_id = None;
    loop {
        if entry.owner_process_id == process_id
            && suspended_thread_id.replace(entry.thread_id).is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "suspended process unexpectedly has more than one thread",
            ));
        }
        // SAFETY: the same live snapshot and initialized writable entry are
        // reused according to the Toolhelp iteration contract.
        if unsafe { thread32_next(snapshot.as_raw_handle(), &mut entry) } == 0 {
            break;
        }
    }
    let thread_id = suspended_thread_id.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not find the suspended process primary thread",
        )
    })?;
    // SAFETY: `thread_id` came from the snapshot and no pointer arguments are
    // involved; a null result is handled before ownership is assumed.
    let thread = unsafe { open_thread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if thread.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenThread returned a unique, non-null owned handle.
    let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
    resume_thread_handle(thread.as_raw_handle())
}

pub(super) fn resume_thread_handle(thread: RawHandle) -> io::Result<()> {
    // SAFETY: callers pass a live thread handle with THREAD_SUSPEND_RESUME
    // access; ResumeThread consumes neither the handle nor pointed-to memory.
    if unsafe { resume_thread(thread) } == RESUME_THREAD_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn wait_for_process_id_exit(process_id: u32, milliseconds: u32) -> io::Result<bool> {
    // SAFETY: OpenProcess receives a numeric PID and no pointer arguments; a
    // null result is handled before converting the handle to an owner.
    let process = unsafe { open_process(SYNCHRONIZE, 0, process_id) };
    if process.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER) {
            Ok(true)
        } else {
            Err(error)
        };
    }
    // SAFETY: OpenProcess returned a unique, non-null owned handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    // SAFETY: the process handle remains live and waiting does not transfer it.
    match unsafe { wait_for_single_object(process.as_raw_handle(), milliseconds) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(test)]
pub(super) fn ffi_layout_sizes() -> (usize, usize, usize) {
    (
        size_of::<JobObjectBasicLimitInformation>(),
        size_of::<JobObjectExtendedLimitInformation>(),
        size_of::<ThreadEntry32>(),
    )
}
