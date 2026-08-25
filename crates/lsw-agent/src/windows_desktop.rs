// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixed LocalSystem-to-user-session launch boundary for the desktop companion.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::env;
use std::ffi::c_void;
use std::io;
use std::mem;
use std::path::Path;
use std::ptr;

use lsw_core::{
    derive_scoped_credential, DESKTOP_COMPANION_CREDENTIAL_SCOPE, DESKTOP_COMPANION_GUEST_PORT,
    LIVE_SHARE_CREDENTIAL_SCOPE,
};

const WTS_ACTIVE: i32 = 0;
const WTS_USER_NAME: u32 = 5;
const TOKEN_USER_CLASS: u32 = 1;
const TOKEN_ELEVATION_TYPE_CLASS: u32 = 18;
const TOKEN_LINKED_TOKEN_CLASS: u32 = 19;
const TOKEN_ELEVATION_CLASS: u32 = 20;
const TOKEN_ELEVATION_TYPE_DEFAULT: u32 = 1;
const TOKEN_ELEVATION_TYPE_FULL: u32 = 2;
const TOKEN_ELEVATION_TYPE_LIMITED: u32 = 3;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_ENVIRONMENT_UNITS: usize = 1024 * 1024;

type Handle = *mut c_void;

#[repr(C)]
struct WtsSessionInfoW {
    session_id: u32,
    window_station_name: *mut u16,
    state: i32,
}

#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

#[repr(C)]
struct TokenLinkedToken {
    linked_token: Handle,
}

#[repr(C)]
struct StartupInfoW {
    size: u32,
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
    reserved2_size: u16,
    reserved2: *mut u8,
    standard_input: Handle,
    standard_output: Handle,
    standard_error: Handle,
}

#[repr(C)]
struct ProcessInformation {
    process: Handle,
    thread: Handle,
    process_id: u32,
    thread_id: u32,
}

#[link(name = "wtsapi32")]
extern "system" {
    fn WTSEnumerateSessionsW(
        server: Handle,
        reserved: u32,
        version: u32,
        sessions: *mut *mut WtsSessionInfoW,
        count: *mut u32,
    ) -> i32;
    fn WTSQuerySessionInformationW(
        server: Handle,
        session_id: u32,
        class: u32,
        buffer: *mut *mut u16,
        bytes: *mut u32,
    ) -> i32;
    fn WTSQueryUserToken(session_id: u32, token: *mut Handle) -> i32;
    fn WTSFreeMemory(memory: *mut c_void);
}

#[link(name = "advapi32")]
extern "system" {
    fn LookupAccountNameW(
        system: *const u16,
        account: *const u16,
        sid: *mut c_void,
        sid_bytes: *mut u32,
        domain: *mut u16,
        domain_units: *mut u32,
        sid_use: *mut u32,
    ) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        information: *mut c_void,
        information_bytes: u32,
        returned_bytes: *mut u32,
    ) -> i32;
    fn EqualSid(left: *const c_void, right: *const c_void) -> i32;
    fn CreateProcessAsUserW(
        token: Handle,
        application_name: *const u16,
        command_line: *mut u16,
        process_attributes: *const c_void,
        thread_attributes: *const c_void,
        inherit_handles: i32,
        creation_flags: u32,
        environment: *mut c_void,
        current_directory: *const u16,
        startup_info: *mut StartupInfoW,
        process_information: *mut ProcessInformation,
    ) -> i32;
}

#[link(name = "userenv")]
extern "system" {
    fn CreateEnvironmentBlock(environment: *mut *mut c_void, token: Handle, inherit: i32) -> i32;
    fn DestroyEnvironmentBlock(environment: *mut c_void) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CloseHandle(handle: Handle) -> i32;
}

pub(super) fn launch_companion(
    user_name: &str,
    agent_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    lsw_core::validate_windows_user_name(user_name)?;
    let expected_sid = local_account_sid(user_name)?;
    let token = active_user_token(user_name, &expected_sid)?;
    let _token = OwnedHandle(token);

    let desktop_token = derive_scoped_credential(agent_token, DESKTOP_COMPANION_CREDENTIAL_SCOPE)?;
    let live_share_token = derive_scoped_credential(agent_token, LIVE_SHARE_CREDENTIAL_SCOPE)?;
    let executable = env::current_exe()?;
    launch_in_session(
        token,
        &executable,
        user_name,
        &desktop_token,
        &live_share_token,
    )
}

fn active_user_token(
    user_name: &str,
    expected_sid: &[u8],
) -> Result<Handle, Box<dyn std::error::Error>> {
    let mut sessions = ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: WTS writes one allocated session array and count; the array is
    // released with WTSFreeMemory after the bounded slice traversal.
    if unsafe { WTSEnumerateSessionsW(ptr::null_mut(), 0, 1, &mut sessions, &mut count) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let sessions_guard = WtsMemory(sessions.cast());
    if count == 0 || sessions.is_null() {
        return Err(format!(
            "Windows desktop user {user_name:?} is not signed in; run `lsw view` and sign in once before launching GUI applications"
        )
        .into());
    }
    if count > 4096 {
        return Err("Windows returned an unreasonable desktop-session count".into());
    }
    // SAFETY: WTSEnumerateSessionsW returned `count` initialized entries and
    // ownership remains with sessions_guard for this entire traversal.
    let entries = unsafe { std::slice::from_raw_parts(sessions, count as usize) };
    for session in entries {
        if session.state != WTS_ACTIVE {
            continue;
        }
        let observed_name = session_user_name(session.session_id)?;
        if !observed_name.eq_ignore_ascii_case(user_name) {
            continue;
        }
        let mut token = ptr::null_mut();
        // SAFETY: LocalSystem calls WTS for a live active session and receives
        // an owned primary-token handle on success.
        if unsafe { WTSQueryUserToken(session.session_id, &mut token) } == 0 {
            continue;
        }
        match token_sid_matches(token, expected_sid) {
            Ok(true) => return select_unelevated_token(token, expected_sid),
            Ok(false) => close_handle(token),
            Err(error) => {
                close_handle(token);
                return Err(error.into());
            }
        }
    }
    drop(sessions_guard);
    Err(format!(
        "Windows desktop user {user_name:?} is not signed in; run `lsw view` and sign in once before launching GUI applications"
    )
    .into())
}

fn session_user_name(session_id: u32) -> Result<String, Box<dyn std::error::Error>> {
    let mut buffer = ptr::null_mut();
    let mut bytes = 0_u32;
    // SAFETY: WTS allocates a UTF-16 result and reports its byte length. The
    // allocation is released by WtsMemory below.
    if unsafe {
        WTSQuerySessionInformationW(
            ptr::null_mut(),
            session_id,
            WTS_USER_NAME,
            &mut buffer,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let _buffer = WtsMemory(buffer.cast());
    let units = usize::try_from(bytes / 2)?;
    if units == 0 || units > 1024 {
        return Ok(String::new());
    }
    // SAFETY: WTS reported `bytes` initialized bytes in the live allocation.
    let value = unsafe { std::slice::from_raw_parts(buffer, units) };
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    Ok(String::from_utf16(&value[..end])?)
}

fn local_account_sid(user_name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let account = wide(&format!(r".\{user_name}"));
    let mut sid_bytes = 0_u32;
    let mut domain_units = 0_u32;
    let mut sid_use = 0_u32;
    // SAFETY: this sizing call intentionally supplies null output buffers and
    // receives the required lengths only.
    unsafe {
        LookupAccountNameW(
            ptr::null(),
            account.as_ptr(),
            ptr::null_mut(),
            &mut sid_bytes,
            ptr::null_mut(),
            &mut domain_units,
            &mut sid_use,
        );
    }
    if sid_bytes == 0 || sid_bytes > 64 * 1024 || domain_units > 32 * 1024 {
        return Err(format!("could not resolve local Windows user {user_name:?}").into());
    }
    let mut sid = vec![0_u8; sid_bytes as usize];
    let mut domain = vec![0_u16; domain_units.max(1) as usize];
    // SAFETY: both output buffers have the exact capacities reported by the
    // sizing call and all length pointers remain live for the synchronous call.
    if unsafe {
        LookupAccountNameW(
            ptr::null(),
            account.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_bytes,
            domain.as_mut_ptr(),
            &mut domain_units,
            &mut sid_use,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    sid.truncate(sid_bytes as usize);
    Ok(sid)
}

fn token_sid_matches(token: Handle, expected_sid: &[u8]) -> io::Result<bool> {
    let mut bytes = 0_u32;
    // SAFETY: this is the documented sizing call for TOKEN_USER.
    unsafe {
        GetTokenInformation(token, TOKEN_USER_CLASS, ptr::null_mut(), 0, &mut bytes);
    }
    if bytes == 0 || bytes > 64 * 1024 {
        return Err(io::Error::last_os_error());
    }
    let mut information = vec![0_u8; bytes as usize];
    // SAFETY: the allocation has the reported size and TOKEN_USER begins with
    // SID_AND_ATTRIBUTES on every supported Windows architecture.
    if unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            information.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TOKEN_USER output begins with SID_AND_ATTRIBUTES. Use
    // an unaligned read because Vec<u8> does not promise pointer alignment.
    let token_user =
        unsafe { ptr::read_unaligned(information.as_ptr().cast::<SidAndAttributes>()) };
    // SAFETY: both pointers reference valid SIDs for the duration of this call.
    Ok(unsafe { EqualSid(token_user.sid, expected_sid.as_ptr().cast()) } != 0)
}

fn select_unelevated_token(
    token: Handle,
    expected_sid: &[u8],
) -> Result<Handle, Box<dyn std::error::Error>> {
    let elevation_type = match token_u32(token, TOKEN_ELEVATION_TYPE_CLASS) {
        Ok(value) => value,
        Err(error) => {
            close_handle(token);
            return Err(error.into());
        }
    };
    match elevation_type {
        TOKEN_ELEVATION_TYPE_FULL => {
            let linked = match linked_token(token) {
                Ok(linked) => linked,
                Err(error) => {
                    close_handle(token);
                    return Err(error.into());
                }
            };
            close_handle(token);
            let valid = token_u32(linked, TOKEN_ELEVATION_TYPE_CLASS)
                .is_ok_and(|value| value == TOKEN_ELEVATION_TYPE_LIMITED)
                && token_u32(linked, TOKEN_ELEVATION_CLASS).is_ok_and(|value| value == 0)
                && token_sid_matches(linked, expected_sid).unwrap_or(false);
            if valid {
                Ok(linked)
            } else {
                close_handle(linked);
                Err("Windows did not provide the registered user's filtered UAC token".into())
            }
        }
        TOKEN_ELEVATION_TYPE_DEFAULT | TOKEN_ELEVATION_TYPE_LIMITED => {
            match token_u32(token, TOKEN_ELEVATION_CLASS) {
                Ok(0) => Ok(token),
                Ok(_) => {
                    close_handle(token);
                    Err("refusing to launch the desktop companion with an elevated token".into())
                }
                Err(error) => {
                    close_handle(token);
                    Err(error.into())
                }
            }
        }
        _ => {
            close_handle(token);
            Err("Windows returned an unknown token elevation type".into())
        }
    }
}

fn token_u32(token: Handle, class: u32) -> io::Result<u32> {
    let mut value = 0_u32;
    let mut bytes = 0_u32;
    // SAFETY: the requested token classes both return one DWORD and the output
    // buffer has exactly that initialized writable size.
    if unsafe {
        GetTokenInformation(
            token,
            class,
            (&mut value as *mut u32).cast(),
            u32::try_from(mem::size_of::<u32>()).expect("DWORD size fits u32"),
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if bytes != u32::try_from(mem::size_of::<u32>()).expect("DWORD size fits u32") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an invalid token information size",
        ));
    }
    Ok(value)
}

fn linked_token(token: Handle) -> io::Result<Handle> {
    let mut linked = TokenLinkedToken {
        linked_token: ptr::null_mut(),
    };
    let mut bytes = 0_u32;
    // SAFETY: TOKEN_LINKED_TOKEN is a one-handle output structure with the
    // documented size. Its returned handle becomes caller-owned on success.
    if unsafe {
        GetTokenInformation(
            token,
            TOKEN_LINKED_TOKEN_CLASS,
            (&mut linked as *mut TokenLinkedToken).cast(),
            u32::try_from(mem::size_of::<TokenLinkedToken>())
                .expect("linked-token structure size fits u32"),
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if linked.linked_token.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a null linked token",
        ));
    }
    Ok(linked.linked_token)
}

fn launch_in_session(
    token: Handle,
    executable: &Path,
    user_name: &str,
    desktop_token: &str,
    live_share_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut source_environment = ptr::null_mut();
    // SAFETY: token is a live primary token and Userenv returns one owned
    // environment allocation on success.
    if unsafe { CreateEnvironmentBlock(&mut source_environment, token, 0) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let _source_environment = EnvironmentBlock(source_environment);
    let mut environment = copy_environment(source_environment.cast())?;
    replace_environment(&mut environment, "LSW_DESKTOP_TOKEN", desktop_token)?;
    replace_environment(&mut environment, "LSW_LIVE_SHARE_TOKEN", live_share_token)?;
    replace_environment(&mut environment, "LSW_DESKTOP_USER", user_name)?;
    environment.sort_by_cached_key(|entry| String::from_utf16_lossy(entry).to_uppercase());
    let mut environment = encode_environment(&environment)?;

    let executable_text = executable
        .to_str()
        .ok_or("desktop companion path is not valid Unicode")?;
    let application = wide(executable_text);
    let mut command_line = wide(&format!(
        "\"{executable_text}\" --desktop-companion --listen 127.0.0.1:{DESKTOP_COMPANION_GUEST_PORT}"
    ));
    let mut desktop = wide(r"winsta0\default");
    let current_directory = executable.parent().and_then(Path::to_str).map(wide);
    // SAFETY: STARTUPINFOW permits all optional fields to be zero initialized;
    // the required size and desktop fields are assigned immediately below.
    let mut startup: StartupInfoW = unsafe { mem::zeroed() };
    startup.size = u32::try_from(mem::size_of::<StartupInfoW>())?;
    startup.desktop = desktop.as_mut_ptr();
    // SAFETY: PROCESS_INFORMATION is a pure output structure initialized by
    // CreateProcessAsUserW before any field is read.
    let mut process: ProcessInformation = unsafe { mem::zeroed() };
    // SAFETY: every pointer references a live NUL-terminated UTF-16 buffer or
    // initialized Windows structure. The environment is double-NUL terminated,
    // handles are not inherited, and Windows copies all inputs synchronously.
    let created = unsafe {
        CreateProcessAsUserW(
            token,
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
            environment.as_mut_ptr().cast(),
            current_directory
                .as_ref()
                .map_or(ptr::null(), |directory| directory.as_ptr()),
            &mut startup,
            &mut process,
        )
    };
    environment.fill(0);
    if created == 0 {
        return Err(format!(
            "could not start the LSW desktop companion: {}",
            io::Error::last_os_error()
        )
        .into());
    }
    close_handle(process.thread);
    close_handle(process.process);
    Ok(())
}

fn copy_environment(source: *const u16) -> Result<Vec<Vec<u16>>, Box<dyn std::error::Error>> {
    if source.is_null() {
        return Err("Windows returned a null user environment".into());
    }
    let mut output = Vec::new();
    let mut entry = Vec::new();
    let mut index = 0_usize;
    loop {
        if index >= MAX_ENVIRONMENT_UNITS {
            return Err("Windows user environment is unreasonably large".into());
        }
        // SAFETY: CreateEnvironmentBlock returns a double-NUL-terminated block;
        // the explicit maximum bounds traversal if Windows violates that contract.
        let unit = unsafe { *source.add(index) };
        if unit == 0 {
            if entry.is_empty() {
                break;
            }
            output.push(mem::take(&mut entry));
        } else {
            entry.push(unit);
        }
        index += 1;
    }
    Ok(output)
}

fn replace_environment(
    environment: &mut Vec<Vec<u16>>,
    name: &str,
    value: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if value.contains('\0') {
        return Err("desktop companion environment value contains NUL".into());
    }
    environment.retain(|entry| !environment_name_matches(entry, name));
    environment.push(format!("{name}={value}").encode_utf16().collect());
    Ok(())
}

fn environment_name_matches(entry: &[u16], expected: &str) -> bool {
    let Some(separator) = entry.iter().position(|unit| *unit == u16::from(b'=')) else {
        return false;
    };
    separator == expected.len()
        && entry[..separator]
            .iter()
            .zip(expected.bytes())
            .all(|(observed, expected)| {
                *observed <= u16::from(u8::MAX) && (*observed as u8).eq_ignore_ascii_case(&expected)
            })
}

fn encode_environment(entries: &[Vec<u16>]) -> Result<Vec<u16>, Box<dyn std::error::Error>> {
    let units = entries.iter().try_fold(1_usize, |total, entry| {
        total
            .checked_add(entry.len() + 1)
            .ok_or("Windows user environment length overflowed")
    })?;
    if units > MAX_ENVIRONMENT_UNITS {
        return Err("Windows user environment is unreasonably large".into());
    }
    let mut output = Vec::with_capacity(units);
    for entry in entries {
        output.extend_from_slice(entry);
        output.push(0);
    }
    output.push(0);
    Ok(output)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn close_handle(handle: Handle) {
    if !handle.is_null() {
        // SAFETY: the caller transfers one live owned handle and this helper
        // closes it exactly once.
        unsafe {
            CloseHandle(handle);
        }
    }
}

struct OwnedHandle(Handle);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        close_handle(self.0);
    }
}

struct WtsMemory(*mut c_void);

impl Drop for WtsMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: WTS allocated this block and WTSFreeMemory is its matching
            // release operation.
            unsafe { WTSFreeMemory(self.0) };
        }
    }
}

struct EnvironmentBlock(*mut c_void);

impl Drop for EnvironmentBlock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Userenv allocated this block and DestroyEnvironmentBlock
            // is its matching release operation.
            unsafe {
                DestroyEnvironmentBlock(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_environment_replaces_user_controlled_duplicates() {
        let mut entries = vec![
            "Path=C:\\Windows".encode_utf16().collect(),
            "lsw_desktop_token=attacker".encode_utf16().collect(),
            "=C:=C:\\Windows".encode_utf16().collect(),
        ];
        replace_environment(&mut entries, "LSW_DESKTOP_TOKEN", "trusted")
            .expect("environment should update");
        assert_eq!(
            entries
                .iter()
                .filter(|entry| environment_name_matches(entry, "LSW_DESKTOP_TOKEN"))
                .count(),
            1
        );
        let encoded = encode_environment(&entries).expect("environment should encode");
        assert_eq!(&encoded[encoded.len() - 2..], &[0, 0]);
    }
}
