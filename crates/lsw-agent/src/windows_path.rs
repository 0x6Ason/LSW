// SPDX-License-Identifier: GPL-3.0-or-later

#![deny(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_void, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
const INVALID_HANDLE_VALUE: isize = -1;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_TRAVERSE: u32 = 0x0020;
const DELETE_ACCESS: u32 = 0x0001_0000;
const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const FILE_SHARE_DELETE: u32 = 0x0000_0004;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_ATTRIBUTE_TAG_INFO_CLASS: u32 = 9;
const FILE_RENAME_INFORMATION_CLASS: u32 = 10;
const FILE_DISPOSITION_INFO_CLASS: u32 = 4;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_NO_MORE_FILES: i32 = 18;
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[link(name = "kernel32")]
extern "system" {
    fn FindFirstVolumeW(volume_name: *mut u16, buffer_length: u32) -> *mut c_void;
    fn FindNextVolumeW(find_volume: *mut c_void, volume_name: *mut u16, buffer_length: u32) -> i32;
    fn FindVolumeClose(find_volume: *mut c_void) -> i32;
    fn GetFileAttributesW(file_name: *const u16) -> u32;
    fn CreateFileW(
        file_name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *const c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: *mut c_void,
    ) -> *mut c_void;
    fn GetFileInformationByHandleEx(
        file: *mut c_void,
        information_class: u32,
        information: *mut c_void,
        buffer_size: u32,
    ) -> i32;
    fn SetFileInformationByHandle(
        file: *mut c_void,
        information_class: u32,
        information: *const c_void,
        buffer_size: u32,
    ) -> i32;
    fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
}

#[link(name = "ntdll")]
extern "system" {
    fn NtSetInformationFile(
        file: *mut c_void,
        io_status: *mut IoStatusBlock,
        information: *const c_void,
        length: u32,
        information_class: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

#[repr(C)]
struct FileAttributeTagInfo {
    attributes: u32,
    reparse_tag: u32,
}

#[repr(C)]
struct FileDispositionInfo {
    delete_file: u8,
}

#[repr(C)]
struct IoStatusBlock {
    status_or_pointer: usize,
    information: usize,
}

#[repr(C)]
struct FileRenameInformationPrefix {
    replace_if_exists_or_flags: u32,
    root_directory: *mut c_void,
    file_name_length: u32,
    file_name: [u16; 1],
}

/// An incomplete upload whose exact file handle carries DELETE access.
///
/// Windows can allow a service to create a file in a directory while denying
/// a later path-based delete or rename. Requesting DELETE when the file is
/// created lets us publish or discard that same object by handle without
/// requiring hard-link rights or reopening it with broader ACL access.
pub(super) struct UploadFile {
    file: fs::File,
    published: bool,
}

impl UploadFile {
    pub(super) fn create(path: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .access_mode(GENERIC_WRITE | DELETE_ACCESS)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)?;
        Ok(Self {
            file,
            published: false,
        })
    }

    pub(super) fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// Atomically gives this upload its final, previously unused name.
    ///
    /// `ReplaceIfExists` is deliberately false, so a destination created
    /// after the protocol's initial existence check wins and is never
    /// overwritten. The parent chain is rejected if it contains a reparse
    /// point. The final directory is opened without following a reparse point,
    /// then the kernel resolves only the final filename relative to that exact
    /// directory handle, so replacing its old pathname cannot redirect publish.
    pub(super) fn publish_new(&mut self, destination: &Path) -> io::Result<()> {
        self.publish_new_with_parent_open_hook(destination, || {})
    }

    #[cfg(test)]
    pub(super) fn publish_new_after_parent_open(
        &mut self,
        destination: &Path,
        after_parent_open: impl FnOnce(),
    ) -> io::Result<()> {
        self.publish_new_with_parent_open_hook(destination, after_parent_open)
    }

    fn publish_new_with_parent_open_hook(
        &mut self,
        destination: &Path,
        after_parent_open: impl FnOnce(),
    ) -> io::Result<()> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        ensure_no_reparse_components(parent)?;
        let parent = open_publish_directory(parent)?;
        after_parent_open();
        let file_name = destination.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "file destination has no file name",
            )
        })?;
        let file_name = file_name.encode_wide().collect::<Vec<_>>();
        if file_name.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file destination has an empty file name",
            ));
        }
        let file_name_bytes = file_name
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file destination name is too long",
                )
            })?;

        let (pointer_offset, length_offset, name_offset) = file_rename_information_layout();
        let buffer_bytes = name_offset
            .checked_add(file_name_bytes as usize)
            .map(|length| length.max(std::mem::size_of::<FileRenameInformationPrefix>()))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file destination name is too long",
                )
            })?;
        let buffer_words = buffer_bytes
            .checked_add(std::mem::size_of::<usize>() - 1)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file destination name is too long",
                )
            })?
            / std::mem::size_of::<usize>();
        let mut information = vec![0_usize; buffer_words];
        let buffer = information.as_mut_ptr().cast::<u8>();
        // SAFETY: information is pointer-aligned and large enough for every
        // write at the ABI-derived offsets. The validated parent handle and
        // filename remain live until NtSetInformationFile returns.
        unsafe {
            ptr::write(buffer.cast::<u32>(), 0); // ReplaceIfExists = FALSE.
            ptr::write(
                buffer.add(pointer_offset).cast::<*mut c_void>(),
                parent.as_raw_handle().cast(),
            );
            ptr::write(buffer.add(length_offset).cast::<u32>(), file_name_bytes);
            ptr::copy_nonoverlapping(
                file_name.as_ptr().cast::<u8>(),
                buffer.add(name_offset),
                file_name_bytes as usize,
            );
        }
        let buffer_size = u32::try_from(buffer_bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "file destination name is too long",
            )
        })?;
        let mut io_status = IoStatusBlock {
            status_or_pointer: 0,
            information: 0,
        };
        // SAFETY: self.file is live and was created with DELETE access. The
        // initialized buffer is a FILE_RENAME_INFORMATION with
        // ReplaceIfExists false and a single-component name relative to the
        // exact, held-open parent. The synchronous source handle keeps the
        // IO_STATUS_BLOCK live until this call completes.
        let status = unsafe {
            NtSetInformationFile(
                self.file.as_raw_handle().cast(),
                &mut io_status,
                buffer.cast(),
                buffer_size,
                FILE_RENAME_INFORMATION_CLASS,
            )
        };
        if status < 0 {
            // SAFETY: status was returned by NtSetInformationFile and the
            // conversion routine accepts any NTSTATUS value.
            let error = unsafe { RtlNtStatusToDosError(status) };
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        self.published = true;
        Ok(())
    }
}

impl Write for UploadFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for UploadFile {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let information = FileDispositionInfo { delete_file: 1 };
        // SAFETY: self.file is still live and was created with DELETE access;
        // information is the complete FILE_DISPOSITION_INFO input buffer.
        let _ = unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle().cast(),
                FILE_DISPOSITION_INFO_CLASS,
                (&information as *const FileDispositionInfo).cast(),
                std::mem::size_of::<FileDispositionInfo>() as u32,
            )
        };
    }
}

fn open_publish_directory(path: &Path) -> io::Result<fs::File> {
    let path = wide_path(path);
    // SAFETY: path is a live NUL-terminated UTF-16 string. The returned handle
    // is immediately transferred to File for exactly-once closure.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle as isize == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a unique owned handle and ownership is
    // transferred to this File exactly once.
    let directory = unsafe { fs::File::from_raw_handle(handle.cast()) };
    let mut information = FileAttributeTagInfo {
        attributes: 0,
        reparse_tag: 0,
    };
    // SAFETY: directory is live and information is a writable buffer of the
    // exact size required for FileAttributeTagInfo.
    if unsafe {
        GetFileInformationByHandleEx(
            directory.as_raw_handle().cast(),
            FILE_ATTRIBUTE_TAG_INFO_CLASS,
            (&mut information as *mut FileAttributeTagInfo).cast(),
            std::mem::size_of::<FileAttributeTagInfo>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if information.attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upload destination parent is not a directory",
        ));
    }
    if information.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upload destination parent is a Windows reparse point",
        ));
    }
    Ok(directory)
}

fn file_rename_information_layout() -> (usize, usize, usize) {
    let uninitialized = std::mem::MaybeUninit::<FileRenameInformationPrefix>::uninit();
    let base = uninitialized.as_ptr();
    // SAFETY: addr_of! forms raw pointers to fields without reading the
    // uninitialized value. Subtracting addresses within the same allocation
    // yields the C ABI offsets used to build the variable-length buffer.
    unsafe {
        (
            ptr::addr_of!((*base).root_directory) as usize - base as usize,
            ptr::addr_of!((*base).file_name_length) as usize - base as usize,
            ptr::addr_of!((*base).file_name) as usize - base as usize,
        )
    }
}

#[cfg(test)]
pub(super) fn file_rename_information_abi() -> (usize, usize, usize, usize, usize) {
    let (root, length, name) = file_rename_information_layout();
    (
        root,
        length,
        name,
        std::mem::size_of::<FileRenameInformationPrefix>(),
        std::mem::size_of::<IoStatusBlock>(),
    )
}

struct VolumeSearch(*mut c_void);

impl Drop for VolumeSearch {
    fn drop(&mut self) {
        // SAFETY: this handle came from a successful FindFirstVolumeW call and
        // this guard owns the only close operation for it.
        let _ = unsafe { FindVolumeClose(self.0) };
    }
}

pub(super) fn volume_roots() -> std::io::Result<Vec<PathBuf>> {
    const VOLUME_NAME_CAPACITY: usize = 1024;

    let mut buffer = [0_u16; VOLUME_NAME_CAPACITY];
    // SAFETY: buffer is writable for the declared number of UTF-16 code units
    // and remains live for the synchronous enumeration call.
    let handle = unsafe { FindFirstVolumeW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if handle as isize == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let search = VolumeSearch(handle);
    let mut roots = Vec::new();
    loop {
        let terminator = buffer.iter().position(|value| *value == 0).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Windows returned an unterminated volume name",
            )
        })?;
        let root = PathBuf::from(OsString::from_wide(&buffer[..terminator]));
        roots.push(root);

        buffer.fill(0);
        // SAFETY: search owns a live volume-enumeration handle and buffer is
        // writable for the declared number of UTF-16 code units.
        if unsafe { FindNextVolumeW(search.0, buffer.as_mut_ptr(), buffer.len() as u32) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                break;
            }
            return Err(error);
        }
    }
    Ok(roots)
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
