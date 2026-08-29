// SPDX-License-Identifier: GPL-3.0-or-later

//! Narrow bindings for creating a usable local Windows account and changing
//! its local Administrators membership without putting a password in a child
//! process, command line, environment block, or file.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;

const USER_PRIV_USER: u32 = 1;
const UF_SCRIPT: u32 = 0x0001;
const UF_NORMAL_ACCOUNT: u32 = 0x0200;
const NERR_SUCCESS: u32 = 0;
const NERR_USER_EXISTS: u32 = 2224;
const ERROR_MEMBER_NOT_IN_ALIAS: u32 = 1377;
const ERROR_MEMBER_IN_ALIAS: u32 = 1378;
const WIN_BUILTIN_ADMINISTRATORS_SID: i32 = 26;
const WIN_BUILTIN_USERS_SID: i32 = 27;
const LOGON32_LOGON_NETWORK: u32 = 3;
const LOGON32_PROVIDER_DEFAULT: u32 = 0;

#[repr(C)]
struct UserInfo1 {
    name: *mut u16,
    password: *mut u16,
    password_age: u32,
    privilege: u32,
    home_directory: *mut u16,
    comment: *mut u16,
    flags: u32,
    script_path: *mut u16,
}

#[repr(C)]
struct LocalGroupMembersInfo3 {
    domain_and_name: *mut u16,
}

#[link(name = "netapi32")]
extern "system" {
    fn NetUserAdd(
        server_name: *const u16,
        level: u32,
        buffer: *mut u8,
        parameter_error: *mut u32,
    ) -> u32;
    fn NetUserDel(server_name: *const u16, user_name: *const u16) -> u32;
    fn NetLocalGroupAddMembers(
        server_name: *const u16,
        group_name: *const u16,
        level: u32,
        buffer: *mut u8,
        entries: u32,
    ) -> u32;
    fn NetLocalGroupDelMembers(
        server_name: *const u16,
        group_name: *const u16,
        level: u32,
        buffer: *mut u8,
        entries: u32,
    ) -> u32;
}

#[link(name = "advapi32")]
extern "system" {
    fn CreateWellKnownSid(
        sid_type: i32,
        domain_sid: *const c_void,
        sid: *mut c_void,
        sid_bytes: *mut u32,
    ) -> i32;
    fn LookupAccountSidW(
        system_name: *const u16,
        sid: *const c_void,
        name: *mut u16,
        name_characters: *mut u32,
        domain_name: *mut u16,
        domain_characters: *mut u32,
        sid_name_use: *mut i32,
    ) -> i32;
    fn LogonUserW(
        user_name: *const u16,
        domain: *const u16,
        password: *const u16,
        logon_type: u32,
        logon_provider: u32,
        token: *mut *mut c_void,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CloseHandle(handle: *mut c_void) -> i32;
}

struct SecretWide(Vec<u16>);

impl Drop for SecretWide {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub(super) fn create_local_user(
    user_name: &str,
    password: &[u8],
    administrator: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    lsw_core::validate_windows_user_name(user_name)?;
    let password = std::str::from_utf8(password).map_err(|_| "password must be valid UTF-8")?;
    if password.is_empty() || password.contains('\0') || password.encode_utf16().count() > 256 {
        return Err("password must contain 1-256 UTF-16 code units and no NUL".into());
    }

    let mut user_name = wide(user_name);
    let mut password = SecretWide(wide(password));
    let mut info = UserInfo1 {
        name: user_name.as_mut_ptr(),
        password: password.0.as_mut_ptr(),
        password_age: 0,
        privilege: USER_PRIV_USER,
        home_directory: std::ptr::null_mut(),
        comment: std::ptr::null_mut(),
        flags: UF_SCRIPT | UF_NORMAL_ACCOUNT,
        script_path: std::ptr::null_mut(),
    };
    let mut parameter_error = 0_u32;
    // SAFETY: all pointers refer to live, NUL-terminated UTF-16 buffers for the
    // duration of this synchronous NetAPI call; optional pointers are null.
    let status = unsafe {
        NetUserAdd(
            std::ptr::null(),
            1,
            (&mut info as *mut UserInfo1).cast(),
            &mut parameter_error,
        )
    };
    let created = status == NERR_SUCCESS;
    if status == NERR_USER_EXISTS {
        verify_existing_password(user_name.as_ptr(), password.0.as_ptr())?;
    } else if status != NERR_SUCCESS {
        return Err(format!(
            "NetUserAdd failed with status {status} (parameter {parameter_error})"
        )
        .into());
    }

    let role_result = add_to_users(&mut user_name).and_then(|()| {
        if administrator {
            add_to_administrators(&mut user_name)
        } else {
            remove_from_administrators(&mut user_name)
        }
    });
    if let Err(error) = role_result {
        if created {
            // SAFETY: user_name remains a live, NUL-terminated UTF-16 string.
            let _ = unsafe { NetUserDel(std::ptr::null(), user_name.as_ptr()) };
            return Err(format!(
                "could not assign the requested account role; the new account was removed: {error}"
            )
            .into());
        }
        return Err(format!("could not reconcile the existing account role: {error}").into());
    }
    Ok(())
}

pub(super) fn set_local_user_role(
    user_name: &str,
    role: lsw_core::WindowsUserRole,
) -> Result<(), Box<dyn std::error::Error>> {
    lsw_core::validate_windows_user_name(user_name)?;
    let mut user_name = wide(user_name);
    add_to_users(&mut user_name)?;
    match role {
        lsw_core::WindowsUserRole::Standard => remove_from_administrators(&mut user_name),
        lsw_core::WindowsUserRole::Administrator => add_to_administrators(&mut user_name),
    }
}

fn verify_existing_password(
    user_name: *const u16,
    password: *const u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut token = std::ptr::null_mut();
    let local_domain = ['.' as u16, 0];
    // SAFETY: both credential pointers are live NUL-terminated UTF-16 strings;
    // LogonUserW initializes token only on success.
    if unsafe {
        LogonUserW(
            user_name,
            local_domain.as_ptr(),
            password,
            LOGON32_LOGON_NETWORK,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    } == 0
    {
        return Err("the Windows account already exists and its password did not validate".into());
    }
    // SAFETY: a successful LogonUserW returned one owned token handle.
    let close_status = unsafe { CloseHandle(token) };
    if close_status == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn add_to_administrators(user_name: &mut [u16]) -> Result<(), Box<dyn std::error::Error>> {
    let group_name = builtin_group_name(WIN_BUILTIN_ADMINISTRATORS_SID, "Administrators")?;
    add_to_local_group(user_name, &group_name)
}

fn add_to_users(user_name: &mut [u16]) -> Result<(), Box<dyn std::error::Error>> {
    let group_name = builtin_group_name(WIN_BUILTIN_USERS_SID, "Users")?;
    add_to_local_group(user_name, &group_name)
}

fn add_to_local_group(
    user_name: &mut [u16],
    group_name: &[u16],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut member = LocalGroupMembersInfo3 {
        domain_and_name: user_name.as_mut_ptr(),
    };
    // SAFETY: group_name and the member account are live, NUL-terminated
    // UTF-16 strings, and the one-entry structure matches information level 3.
    let status = unsafe {
        NetLocalGroupAddMembers(
            std::ptr::null(),
            group_name.as_ptr(),
            3,
            (&mut member as *mut LocalGroupMembersInfo3).cast(),
            1,
        )
    };
    if status != NERR_SUCCESS && status != ERROR_MEMBER_IN_ALIAS {
        return Err(format!("NetLocalGroupAddMembers failed with status {status}").into());
    }
    Ok(())
}

fn remove_from_administrators(user_name: &mut [u16]) -> Result<(), Box<dyn std::error::Error>> {
    let group_name = builtin_group_name(WIN_BUILTIN_ADMINISTRATORS_SID, "Administrators")?;
    let mut member = LocalGroupMembersInfo3 {
        domain_and_name: user_name.as_mut_ptr(),
    };
    // SAFETY: group_name and the member account are live, NUL-terminated
    // UTF-16 strings, and the one-entry structure matches information level 3.
    let status = unsafe {
        NetLocalGroupDelMembers(
            std::ptr::null(),
            group_name.as_ptr(),
            3,
            (&mut member as *mut LocalGroupMembersInfo3).cast(),
            1,
        )
    };
    if status != NERR_SUCCESS && status != ERROR_MEMBER_NOT_IN_ALIAS {
        return Err(format!("NetLocalGroupDelMembers failed with status {status}").into());
    }
    Ok(())
}

fn builtin_group_name(
    sid_type: i32,
    display_name: &str,
) -> Result<[u16; 256], Box<dyn std::error::Error>> {
    let mut sid = [0_u8; 68];
    let mut sid_bytes = u32::try_from(sid.len()).expect("SID buffer length fits in u32");
    // SAFETY: sid is a writable SECURITY_MAX_SID_SIZE buffer and sid_bytes
    // describes its exact length.
    if unsafe {
        CreateWellKnownSid(
            sid_type,
            std::ptr::null(),
            sid.as_mut_ptr().cast(),
            &mut sid_bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }

    let mut group_name = [0_u16; 256];
    let mut group_characters = u32::try_from(group_name.len()).expect("group buffer fits in u32");
    let mut domain_name = [0_u16; 256];
    let mut domain_characters =
        u32::try_from(domain_name.len()).expect("domain buffer fits in u32");
    let mut sid_name_use = 0_i32;
    // SAFETY: the SID was initialized above and all output buffers and their
    // corresponding lengths remain live for this synchronous call.
    if unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid.as_ptr().cast(),
            group_name.as_mut_ptr(),
            &mut group_characters,
            domain_name.as_mut_ptr(),
            &mut domain_characters,
            &mut sid_name_use,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let terminator = usize::try_from(group_characters).map_err(|_| "invalid group-name length")?;
    if terminator >= group_name.len() {
        return Err(format!("localized {display_name} group name is too long").into());
    }
    group_name[terminator] = 0;
    Ok(group_name)
}

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
