// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixed native Windows sudo configuration for the one-shot maintenance helper.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::env;
use std::ffi::{c_void, OsStr};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

use lsw_core::{WindowsSudoMode, WindowsSudoStatus};

const SUDO_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Sudo";
const SUDO_VALUE: &str = "Enabled";
const SUDO_POLICY_KEY: &str = r"SOFTWARE\Policies\Microsoft\Windows\Sudo";
const SUDO_POLICY_VALUE: &str = "Enabled";

const HKEY_LOCAL_MACHINE: HKey = 0x8000_0002usize as HKey;
const KEY_QUERY_VALUE: u32 = 0x0001;
const KEY_SET_VALUE: u32 = 0x0002;
const KEY_WOW64_64KEY: u32 = 0x0100;
const REG_OPTION_NON_VOLATILE: u32 = 0;
const REG_DWORD: u32 = 4;
const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;

type HKey = *mut c_void;

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(
        key: HKey,
        sub_key: *const u16,
        options: u32,
        desired_access: u32,
        result: *mut HKey,
    ) -> i32;
    fn RegCreateKeyExW(
        key: HKey,
        sub_key: *const u16,
        reserved: u32,
        class: *mut u16,
        options: u32,
        desired_access: u32,
        security_attributes: *const c_void,
        result: *mut HKey,
        disposition: *mut u32,
    ) -> i32;
    fn RegQueryValueExW(
        key: HKey,
        value_name: *const u16,
        reserved: *mut u32,
        value_type: *mut u32,
        data: *mut u8,
        data_size: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: HKey,
        value_name: *const u16,
        reserved: u32,
        value_type: u32,
        data: *const u8,
        data_size: u32,
    ) -> i32;
    fn RegCloseKey(key: HKey) -> i32;
}

struct OwnedKey(HKey);

impl Drop for OwnedKey {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live handle returned by RegOpenKeyExW or
        // RegCreateKeyExW and this owner closes it exactly once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

pub(super) fn status() -> Result<WindowsSudoStatus, Box<dyn std::error::Error>> {
    let available = sudo_binary().is_file();
    let configured_mode = read_mode(SUDO_KEY, SUDO_VALUE)?.unwrap_or(WindowsSudoMode::Disabled);
    let policy_mode = read_mode(SUDO_POLICY_KEY, SUDO_POLICY_VALUE)?;
    Ok(WindowsSudoStatus {
        available,
        configured_mode,
        policy_mode,
    })
}

pub(super) fn configure(enable: bool) -> Result<(), Box<dyn std::error::Error>> {
    let before = status()?;
    if !before.available {
        return Err(
            "native Windows sudo is unavailable; Windows 11 24H2 or later is required".into(),
        );
    }
    if before.policy_mode.is_some() {
        return Err("Windows sudo is managed by system policy; LSW will not override it".into());
    }

    let requested = if enable {
        WindowsSudoMode::ForceNewWindow
    } else {
        WindowsSudoMode::Disabled
    };
    write_dword(SUDO_KEY, SUDO_VALUE, requested as u32)?;

    let after = status()?;
    if after.policy_mode.is_some() {
        return Err("Windows sudo became policy-managed while it was being configured".into());
    }
    if after.configured_mode != requested {
        return Err("Windows did not retain the requested sudo configuration".into());
    }
    Ok(())
}

fn sudo_binary() -> PathBuf {
    env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("sudo.exe")
}

fn read_mode(
    key: &str,
    value: &str,
) -> Result<Option<WindowsSudoMode>, Box<dyn std::error::Error>> {
    read_dword(key, value)?.map(mode_from_dword).transpose()
}

fn mode_from_dword(value: u32) -> Result<WindowsSudoMode, Box<dyn std::error::Error>> {
    match value {
        0 => Ok(WindowsSudoMode::Disabled),
        1 => Ok(WindowsSudoMode::ForceNewWindow),
        2 => Ok(WindowsSudoMode::DisableInput),
        3 => Ok(WindowsSudoMode::Normal),
        _ => Err(format!("Windows sudo registry mode {value} is invalid").into()),
    }
}

fn read_dword(key: &str, value: &str) -> Result<Option<u32>, Box<dyn std::error::Error>> {
    let key = match open_key(key, KEY_QUERY_VALUE | KEY_WOW64_64KEY)? {
        Some(key) => key,
        None => return Ok(None),
    };
    let value = wide(value);
    let mut value_type = 0;
    let mut data = 0u32;
    let mut data_size = std::mem::size_of::<u32>() as u32;
    // SAFETY: all output pointers refer to initialized, writable stack values;
    // the data buffer is exactly `data_size` bytes and the key is live.
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            value.as_ptr(),
            ptr::null_mut(),
            &mut value_type,
            (&mut data as *mut u32).cast(),
            &mut data_size,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(result)?;
    if value_type != REG_DWORD || data_size != std::mem::size_of::<u32>() as u32 {
        return Err("Windows sudo registry value is not a DWORD".into());
    }
    Ok(Some(data))
}

fn write_dword(key: &str, value: &str, data: u32) -> Result<(), Box<dyn std::error::Error>> {
    let key = create_key(key, KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY)?;
    let value = wide(value);
    // SAFETY: the key is live, `value` is NUL-terminated, and the input buffer
    // points to exactly one readable DWORD for the complete synchronous call.
    let result = unsafe {
        RegSetValueExW(
            key.0,
            value.as_ptr(),
            0,
            REG_DWORD,
            (&data as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    check(result)?;
    Ok(())
}

fn open_key(path: &str, access: u32) -> io::Result<Option<OwnedKey>> {
    let path = wide(path);
    let mut key = ptr::null_mut();
    // SAFETY: `path` is NUL-terminated, `key` is writable, and the predefined
    // HKLM handle remains valid for the synchronous call.
    let result = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, access, &mut key) };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(result)?;
    Ok(Some(OwnedKey(key)))
}

fn create_key(path: &str, access: u32) -> io::Result<OwnedKey> {
    let path = wide(path);
    let mut key = ptr::null_mut();
    let mut disposition = 0;
    // SAFETY: `path` is NUL-terminated, both output pointers are writable, and
    // the predefined HKLM handle remains valid for the synchronous call.
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            path.as_ptr(),
            0,
            ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            access,
            ptr::null(),
            &mut key,
            &mut disposition,
        )
    };
    check(result)?;
    Ok(OwnedKey(key))
}

fn check(result: i32) -> io::Result<()> {
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_capability_probe_and_registry_reads_are_non_mutating() {
        let observed = status().expect("Windows sudo status should be readable");
        assert_eq!(observed.available, sudo_binary().is_file());
    }

    #[test]
    fn registry_modes_are_bounded_to_windows_values() {
        assert_eq!(
            mode_from_dword(1).expect("new-window mode should decode"),
            WindowsSudoMode::ForceNewWindow
        );
        assert!(mode_from_dword(4).is_err());
    }
}
