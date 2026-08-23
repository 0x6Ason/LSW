// SPDX-License-Identifier: GPL-3.0-or-later

#![deny(clippy::undocumented_unsafe_blocks)]

use std::fs;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[link(name = "kernel32")]
extern "system" {
    fn GetFileAttributesW(file_name: *const u16) -> u32;
    fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
}

pub(super) fn ensure_no_reparse_components(path: &Path) -> std::io::Result<()> {
    for component_path in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        if component_path.as_os_str().is_empty() {
            continue;
        }
        let wide = component_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        // SAFETY: wide is a live NUL-terminated UTF-16 path for this
        // synchronous read-only Win32 call.
        let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
        if attributes == INVALID_FILE_ATTRIBUTES {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND)
            ) {
                continue;
            }
            return Err(error);
        }
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "transfer path crosses a Windows reparse point: {}",
                    component_path.display()
                ),
            ));
        }
    }
    Ok(())
}

pub(super) fn replace_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement path has no parent directory",
        )
    })?;
    let mut temporary_name = path
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "replacement path has no file name",
            )
        })?
        .to_os_string();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    temporary_name.push(format!(".lsw-{}-{nonce}.tmp", std::process::id()));
    let temporary = parent.join(temporary_name);
    let result = (|| {
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        output.write_all(contents)?;
        output.sync_all()?;
        drop(output);
        let existing = wide_path(&temporary);
        let replacement = wide_path(path);
        // SAFETY: both buffers are live NUL-terminated UTF-16 paths. The
        // replacement is on the same volume because it has the same parent.
        if unsafe {
            MoveFileExW(
                existing.as_ptr(),
                replacement.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
