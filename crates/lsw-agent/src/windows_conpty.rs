// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal Windows ConPTY and process-creation bindings.
//!
//! Handles are wrapped immediately after successful creation. A child starts
//! suspended, enters the kill-on-close Job Object, and only then resumes.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io;
use std::mem::{size_of, size_of_val};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lsw_core::{SessionKind, StartRequest, TerminalSize};

type Handle = RawHandle;
type PseudoConsoleHandle = RawHandle;

const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
const CREATE_SUSPENDED: u32 = 0x0000_0004;
const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const INFINITE: u32 = u32::MAX;
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const HRESULT_PATH_NOT_FOUND: i32 = 0x8007_0003_u32 as i32;
const PSEUDO_CONSOLE_CREATE_ATTEMPTS: usize = 20;
const PSEUDO_CONSOLE_CREATE_RETRY_DELAY: Duration = Duration::from_millis(100);

#[repr(C)]
#[derive(Clone, Copy)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_count_chars: u32,
    y_count_chars: u32,
    fill_attribute: u32,
    flags: u32,
    show_window: u16,
    reserved2_length: u16,
    reserved2: *mut u8,
    standard_input: Handle,
    standard_output: Handle,
    standard_error: Handle,
}

impl StartupInfoW {
    fn empty() -> Self {
        Self {
            cb: 0,
            reserved: ptr::null_mut(),
            desktop: ptr::null_mut(),
            title: ptr::null_mut(),
            x: 0,
            y: 0,
            x_size: 0,
            y_size: 0,
            x_count_chars: 0,
            y_count_chars: 0,
            fill_attribute: 0,
            flags: 0,
            show_window: 0,
            reserved2_length: 0,
            reserved2: ptr::null_mut(),
            standard_input: ptr::null_mut(),
            standard_output: ptr::null_mut(),
            standard_error: ptr::null_mut(),
        }
    }
}

#[repr(C)]
struct StartupInfoExW {
    startup_info: StartupInfoW,
    attribute_list: *mut c_void,
}

#[repr(C)]
struct ProcessInformation {
    process: Handle,
    thread: Handle,
    process_id: u32,
    thread_id: u32,
}

#[link(name = "kernel32")]
extern "system" {
    #[link_name = "CloseHandle"]
    fn close_handle(handle: Handle) -> i32;
    #[link_name = "ClosePseudoConsole"]
    fn close_pseudo_console(console: PseudoConsoleHandle);
    #[link_name = "CreatePipe"]
    fn create_pipe(
        read_pipe: *mut Handle,
        write_pipe: *mut Handle,
        attributes: *mut c_void,
        size: u32,
    ) -> i32;
    #[link_name = "CreateProcessW"]
    fn create_process_w(
        application_name: *const u16,
        command_line: *mut u16,
        process_attributes: *mut c_void,
        thread_attributes: *mut c_void,
        inherit_handles: i32,
        creation_flags: u32,
        environment: *mut c_void,
        current_directory: *const u16,
        startup_info: *mut StartupInfoW,
        process_information: *mut ProcessInformation,
    ) -> i32;
    #[link_name = "CreatePseudoConsole"]
    fn create_pseudo_console(
        size: Coord,
        input: Handle,
        output: Handle,
        flags: u32,
        console: *mut PseudoConsoleHandle,
    ) -> i32;
    #[link_name = "DeleteProcThreadAttributeList"]
    fn delete_proc_thread_attribute_list(list: *mut c_void);
    #[link_name = "GetExitCodeProcess"]
    fn get_exit_code_process(process: Handle, exit_code: *mut u32) -> i32;
    #[link_name = "InitializeProcThreadAttributeList"]
    fn initialize_proc_thread_attribute_list(
        list: *mut c_void,
        attribute_count: u32,
        flags: u32,
        size: *mut usize,
    ) -> i32;
    #[link_name = "ResizePseudoConsole"]
    fn resize_pseudo_console(console: PseudoConsoleHandle, size: Coord) -> i32;
    #[link_name = "TerminateProcess"]
    fn terminate_process_ffi(process: Handle, exit_code: u32) -> i32;
    #[link_name = "UpdateProcThreadAttribute"]
    fn update_proc_thread_attribute(
        list: *mut c_void,
        flags: u32,
        attribute: usize,
        value: *mut c_void,
        size: usize,
        previous_value: *mut c_void,
        return_size: *mut usize,
    ) -> i32;
    #[link_name = "WaitForSingleObject"]
    fn wait_for_single_object(handle: Handle, milliseconds: u32) -> u32;
}

pub(super) struct PseudoConsole {
    handle: PseudoConsoleHandle,
}

// An HPCON is an opaque kernel handle. The bridge serializes resize calls
// on one input thread, and Arc keeps the handle alive until that thread exits.
// SAFETY: all access uses thread-safe ConPTY APIs; the handle remains alive
// through the shared `Arc<PseudoConsole>` owner.
unsafe impl Send for PseudoConsole {}
// SAFETY: resize calls are serialized by the single input bridge, and Drop
// cannot run until the final Arc reference is released.
unsafe impl Sync for PseudoConsole {}

impl PseudoConsole {
    fn create(size: TerminalSize, input: Handle, output: Handle) -> io::Result<Self> {
        for attempt in 1..=PSEUDO_CONSOLE_CREATE_ATTEMPTS {
            let mut handle = ptr::null_mut();
            // SAFETY: both pipe handles are live, `handle` is a valid writable
            // out-parameter, and the numeric terminal size was range-validated.
            let result =
                unsafe { create_pseudo_console(coord(size), input, output, 0, &mut handle) };
            if result >= 0 {
                if handle.is_null() {
                    return Err(io::Error::other(
                        "CreatePseudoConsole returned a null handle",
                    ));
                }
                return Ok(Self { handle });
            }
            if !should_retry_pseudo_console_create(result, attempt) {
                return Err(hresult_error("CreatePseudoConsole", result));
            }
            thread::sleep(PSEUDO_CONSOLE_CREATE_RETRY_DELAY);
        }
        unreachable!("the bounded CreatePseudoConsole loop always returns")
    }

    pub(super) fn resize(&self, size: TerminalSize) -> io::Result<()> {
        // SAFETY: `self.handle` remains live for this call and Coord contains
        // only validated integer dimensions.
        let result = unsafe { resize_pseudo_console(self.handle, coord(size)) };
        if result < 0 {
            Err(hresult_error("ResizePseudoConsole", result))
        } else {
            Ok(())
        }
    }
}

pub(super) fn should_retry_pseudo_console_create(result: i32, attempt: usize) -> bool {
    result == HRESULT_PATH_NOT_FOUND && attempt < PSEUDO_CONSOLE_CREATE_ATTEMPTS
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // SAFETY: this type uniquely owns the non-null HPCON and closes it once.
        unsafe { close_pseudo_console(self.handle) };
    }
}

struct AttributeList {
    list: *mut c_void,
    _storage: Vec<usize>,
}

impl AttributeList {
    fn for_pseudo_console(console: &PseudoConsole) -> io::Result<Self> {
        let mut byte_count = 0_usize;
        // SAFETY: the documented sizing call accepts a null list and writes
        // only the required byte count to the valid out-parameter.
        unsafe { initialize_proc_thread_attribute_list(ptr::null_mut(), 1, 0, &mut byte_count) };
        if byte_count == 0 {
            return Err(io::Error::last_os_error());
        }
        let word_size = size_of::<usize>();
        let word_count = byte_count
            .checked_add(word_size - 1)
            .ok_or_else(|| io::Error::other("attribute list is too large"))?
            / word_size;
        let mut storage = vec![0_usize; word_count];
        let list = storage.as_mut_ptr().cast::<c_void>();
        // SAFETY: the word-aligned storage is at least `byte_count` bytes and
        // remains allocated for the complete attribute-list lifetime.
        if unsafe { initialize_proc_thread_attribute_list(list, 1, 0, &mut byte_count) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let attributes = Self {
            list,
            _storage: storage,
        };
        // SAFETY: the initialized list and live HPCON are valid for the exact
        // pointer-sized attribute value passed synchronously by the API.
        if unsafe {
            update_proc_thread_attribute(
                attributes.list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                console.handle,
                size_of::<PseudoConsoleHandle>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(attributes)
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: `list` was initialized successfully and is deleted exactly
        // once before its backing storage is dropped.
        unsafe { delete_proc_thread_attribute_list(self.list) };
    }
}

pub(super) struct ConPtyProcess {
    pub(super) process: OwnedHandle,
    pub(super) input: File,
    pub(super) output: File,
    pub(super) console: Arc<PseudoConsole>,
    pub(super) job: super::process_tree::Job,
}

pub(super) fn spawn_shell(request: &StartRequest, size: TerminalSize) -> io::Result<ConPtyProcess> {
    if request.kind != SessionKind::Shell {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ConPTY requires a shell request",
        ));
    }
    let mut last_not_found = None;
    for candidate in &request.argv {
        let arguments = match candidate.to_ascii_lowercase().as_str() {
            "pwsh" | "pwsh.exe" => &["-NoLogo"][..],
            "powershell" | "powershell.exe" => &["-NoLogo"][..],
            "cmd" | "cmd.exe" => &["/Q"][..],
            _ => &[][..],
        };
        match spawn_program(
            candidate,
            arguments,
            request.working_directory.as_deref(),
            size,
        ) {
            Ok(process) => return Ok(process),
            Err(error) if error.kind() == io::ErrorKind::NotFound => last_not_found = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_not_found.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no shell candidate was supplied")
    }))
}

fn spawn_program(
    program: &str,
    arguments: &[&str],
    working_directory: Option<&str>,
    size: TerminalSize,
) -> io::Result<ConPtyProcess> {
    let (pseudo_input, input) = new_pipe()?;
    let (output, pseudo_output) = new_pipe()?;
    let job = super::process_tree::Job::new()?;
    let console = Arc::new(PseudoConsole::create(
        size,
        pseudo_input.as_raw_handle(),
        pseudo_output.as_raw_handle(),
    )?);
    drop(pseudo_input);
    drop(pseudo_output);

    let attributes = AttributeList::for_pseudo_console(&console)?;
    let mut startup = StartupInfoExW {
        startup_info: StartupInfoW::empty(),
        attribute_list: attributes.list,
    };
    startup.startup_info.cb = u32::try_from(size_of_val(&startup))
        .map_err(|_| io::Error::other("STARTUPINFOEXW is too large"))?;
    startup.startup_info.flags = STARTF_USESTDHANDLES;

    let command_line = super::windows_command_line(program, arguments);
    let mut command_line = OsStr::new(&command_line)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let current_directory = working_directory.map(|directory| {
        OsStr::new(directory)
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    });
    let current_directory_pointer = current_directory
        .as_ref()
        .map_or(ptr::null(), |directory| directory.as_ptr());
    let mut information = ProcessInformation {
        process: ptr::null_mut(),
        thread: ptr::null_mut(),
        process_id: 0,
        thread_id: 0,
    };
    // SAFETY: all optional pointers are either null or point to live,
    // null-terminated buffers; startup and output structures are writable for
    // the synchronous call and handle inheritance is disabled.
    let created = unsafe {
        create_process_w(
            ptr::null(),
            command_line.as_mut_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
            ptr::null_mut(),
            current_directory_pointer,
            &mut startup.startup_info,
            &mut information,
        )
    };
    if created == 0 {
        close_if_present(information.process);
        close_if_present(information.thread);
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateProcessW returns unique non-null process and
    // thread handles, which are immediately assigned one Rust owner each.
    let process = unsafe { OwnedHandle::from_raw_handle(information.process) };
    // SAFETY: the primary-thread handle is distinct and uniquely owned here.
    let thread = unsafe { OwnedHandle::from_raw_handle(information.thread) };
    if let Err(error) = job.assign(process.as_raw_handle()) {
        let cleanup = terminate_process(&process, super::SESSION_CANCEL_EXIT_CODE as u32)
            .and_then(|()| wait_for_process(&process).map(|_| ()));
        return Err(process_setup_error("Job assignment", error, cleanup));
    }
    if let Err(error) = super::process_tree::resume_thread_handle(thread.as_raw_handle()) {
        let cleanup = job
            .terminate(super::SESSION_CANCEL_EXIT_CODE)
            .and_then(|()| wait_for_process(&process).map(|_| ()));
        return Err(process_setup_error("primary-thread resume", error, cleanup));
    }
    drop(thread);
    Ok(ConPtyProcess {
        process,
        input,
        output,
        console,
        job,
    })
}

pub(super) fn wait_for_process(process: &OwnedHandle) -> io::Result<i32> {
    wait_for_process_timeout(process, INFINITE)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "an infinite process wait unexpectedly timed out",
        )
    })
}

pub(super) fn wait_for_process_timeout(
    process: &OwnedHandle,
    milliseconds: u32,
) -> io::Result<Option<i32>> {
    // SAFETY: `process` owns a live process handle and the wait does not
    // transfer or mutate that ownership.
    let wait_result = unsafe { wait_for_single_object(process.as_raw_handle(), milliseconds) };
    if wait_result == WAIT_TIMEOUT {
        return Ok(None);
    }
    if wait_result != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error());
    }
    let mut exit_code = 0_u32;
    // SAFETY: the handle is signaled and live, and `exit_code` is a valid
    // writable out-parameter for the duration of the call.
    if unsafe { get_exit_code_process(process.as_raw_handle(), &mut exit_code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(exit_code as i32))
}

pub(super) fn terminate_process(process: &OwnedHandle, exit_code: u32) -> io::Result<()> {
    // SAFETY: `process` owns a live handle; TerminateProcess consumes neither
    // the handle nor any caller-owned pointed-to memory.
    if unsafe { terminate_process_ffi(process.as_raw_handle(), exit_code) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn process_setup_error(operation: &str, error: io::Error, cleanup: io::Result<()>) -> io::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => io::Error::other(format!(
            "{operation} failed: {error}; suspended process cleanup also failed: {cleanup_error}"
        )),
    }
}

fn new_pipe() -> io::Result<(File, File)> {
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    // SAFETY: both handle out-parameters are valid and writable; null security
    // attributes request the default non-inheritable pipe configuration.
    if unsafe { create_pipe(&mut read, &mut write, ptr::null_mut(), 0) } == 0 {
        close_if_present(read);
        close_if_present(write);
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreatePipe returns distinct, non-null owned handles.
    let read = unsafe { File::from_raw_handle(read) };
    // SAFETY: the write handle is distinct and receives exactly one owner.
    let write = unsafe { File::from_raw_handle(write) };
    Ok((read, write))
}

fn coord(size: TerminalSize) -> Coord {
    Coord {
        x: size.columns as i16,
        y: size.rows as i16,
    }
}

fn hresult_error(operation: &str, result: i32) -> io::Error {
    io::Error::other(format!(
        "{operation} failed with HRESULT 0x{:08x}",
        result as u32
    ))
}

fn close_if_present(handle: Handle) {
    if !handle.is_null() {
        // SAFETY: this helper receives only unowned handles from a failed setup
        // path and closes each non-null handle at most once.
        unsafe {
            close_handle(handle);
        }
    }
}
