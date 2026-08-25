// SPDX-License-Identifier: GPL-3.0-or-later

//! Authenticated live-folder connection owned by the restricted agent session.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::ptr;
use std::sync::Mutex;

const LOCAL_PATH: &str = "L:";
const REMOTE_PATH: &str = r"\\10.0.2.4\qemu";
const USER_NAME: &str = "lsw";

const RESOURCE_TYPE_DISK: u32 = 0x0000_0001;
const NO_ERROR: u32 = 0;
const ERROR_MORE_DATA: u32 = 234;
const ERROR_NOT_CONNECTED: u32 = 2250;

#[repr(C)]
struct NetResourceW {
    scope: u32,
    resource_type: u32,
    display_type: u32,
    usage: u32,
    local_name: *mut u16,
    remote_name: *mut u16,
    comment: *mut u16,
    provider: *mut u16,
}

#[link(name = "mpr")]
extern "system" {
    fn WNetAddConnection2W(
        resource: *const NetResourceW,
        password: *const u16,
        user_name: *const u16,
        flags: u32,
    ) -> u32;
    fn WNetCancelConnection2W(name: *const u16, flags: u32, force: i32) -> u32;
    fn WNetGetConnectionW(local_name: *const u16, remote_name: *mut u16, length: *mut u32) -> u32;
}

static MAPPING_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn query() -> Result<bool, Box<dyn std::error::Error>> {
    let _guard = MAPPING_LOCK
        .lock()
        .map_err(|_| "Windows live-share mapping lock was poisoned")?;
    match current_remote()? {
        Some(remote) if is_owned_remote(&remote) => Ok(true),
        Some(_) => Err("Windows drive L: is already mapped to a different location".into()),
        None => Ok(false),
    }
}

pub(super) fn configure(enable: bool, credential: &str) -> Result<(), Box<dyn std::error::Error>> {
    if credential.len() != 64
        || !credential
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("live-share credential must be 64 lowercase hexadecimal characters".into());
    }

    let _guard = MAPPING_LOCK
        .lock()
        .map_err(|_| "Windows live-share mapping lock was poisoned")?;
    match current_remote()? {
        Some(remote) if !is_owned_remote(&remote) => {
            return Err("Windows drive L: is already mapped to a different location".into())
        }
        Some(_) if enable => return Ok(()),
        Some(_) => return disconnect(),
        None if !enable => return Ok(()),
        None => {}
    }

    connect(credential)?;
    match current_remote()? {
        Some(remote) if is_owned_remote(&remote) => Ok(()),
        Some(_) => Err("Windows registered L: to an unexpected location".into()),
        None => Err("Windows did not retain the Linux (L:) mapping".into()),
    }
}

fn connect(credential: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut local = wide(LOCAL_PATH);
    let mut remote = wide(REMOTE_PATH);
    let mut user = wide(USER_NAME);
    let mut password = wide(credential);
    let resource = NetResourceW {
        scope: 0,
        resource_type: RESOURCE_TYPE_DISK,
        display_type: 0,
        usage: 0,
        local_name: local.as_mut_ptr(),
        remote_name: remote.as_mut_ptr(),
        comment: ptr::null_mut(),
        provider: ptr::null_mut(),
    };
    // SAFETY: all pointers address NUL-terminated UTF-16 buffers that remain
    // alive and stable for the complete synchronous call. `resource` has the
    // documented NETRESOURCEW layout, and MPR does not retain these pointers.
    let result = unsafe { WNetAddConnection2W(&resource, password.as_ptr(), user.as_ptr(), 0) };
    password.fill(0);
    user.fill(0);
    if result != NO_ERROR {
        return Err(format!(
            "Windows could not connect Linux (L:) to the private SMB share: {}",
            win32_error(result)
        )
        .into());
    }
    Ok(())
}

fn disconnect() -> Result<(), Box<dyn std::error::Error>> {
    let local = wide(LOCAL_PATH);
    // SAFETY: `local` is a live NUL-terminated UTF-16 buffer. The connection
    // was ownership-checked immediately before this call, and no Rust memory is
    // retained by MPR.
    let result = unsafe { WNetCancelConnection2W(local.as_ptr(), 0, 0) };
    if result != NO_ERROR && result != ERROR_NOT_CONNECTED {
        return Err(format!(
            "Windows could not disconnect Linux (L:): {}",
            win32_error(result)
        )
        .into());
    }
    match current_remote()? {
        None => Ok(()),
        Some(remote) if is_owned_remote(&remote) => {
            Err("Windows retained Linux (L:) after disconnect".into())
        }
        Some(_) => Err("Windows drive L: changed ownership during disconnect".into()),
    }
}

fn current_remote() -> Result<Option<String>, Box<dyn std::error::Error>> {
    let local = wide(LOCAL_PATH);
    let mut capacity = 256_u32;
    loop {
        let mut remote = vec![0_u16; capacity as usize];
        let mut length = capacity;
        // SAFETY: both UTF-16 buffers are live for the synchronous call;
        // `length` accurately describes the writable output allocation.
        let result =
            unsafe { WNetGetConnectionW(local.as_ptr(), remote.as_mut_ptr(), &mut length) };
        match result {
            NO_ERROR => {
                let end = remote
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(remote.len());
                return Ok(Some(String::from_utf16(&remote[..end])?));
            }
            ERROR_NOT_CONNECTED => return Ok(None),
            ERROR_MORE_DATA if length > capacity && length <= 32 * 1024 => {
                capacity = length;
            }
            ERROR_MORE_DATA => {
                return Err("Windows returned an invalid live-share path length".into())
            }
            code => {
                return Err(format!(
                    "Windows could not query the Linux (L:) mapping: {}",
                    win32_error(code)
                )
                .into())
            }
        }
    }
}

fn is_owned_remote(remote: &str) -> bool {
    remote
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(REMOTE_PATH)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn win32_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_remote_is_case_insensitive_and_accepts_a_trailing_separator() {
        assert!(is_owned_remote(r"\\10.0.2.4\QEMU"));
        assert!(is_owned_remote(r"\\10.0.2.4\qemu\"));
        assert!(!is_owned_remote(r"\\10.0.2.4\other"));
    }

    #[test]
    fn wide_strings_are_terminated() {
        assert_eq!(wide("L:"), vec![b'L' as u16, b':' as u16, 0]);
    }
}
