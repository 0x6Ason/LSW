// SPDX-License-Identifier: GPL-3.0-or-later

//! Filesystem preconditions shared by the installer, image, and download paths.

use std::fs;
use std::path::Path;

use crate::{LswError, Result};

/// Requires `path` to be an existing regular file that is not a symlink.
pub(crate) fn require_regular_file(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a regular file", path.display()),
        })
    }
}

/// Requires `path` to be an existing directory that is not a symlink.
pub(crate) fn require_real_directory(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a real directory", path.display()),
        })
    }
}

/// Lowercase hexadecimal encoding of `bytes`.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encode_is_lowercase_and_zero_padded() {
        assert_eq!(hex_encode(&[0x00, 0x0a, 0xff]), "000aff");
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn regular_file_and_directory_checks_reject_the_other_kind() {
        let directory = std::env::temp_dir().join(format!("lsw-fs-checks-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("temporary directory");
        let file = directory.join("file");
        fs::write(&file, b"x").expect("temporary file");

        require_regular_file(&file, "file").expect("file is regular");
        require_real_directory(&directory, "directory").expect("directory is real");
        assert!(matches!(
            require_regular_file(&directory, "file"),
            Err(LswError::InvalidValue { field: "file", .. })
        ));
        assert!(matches!(
            require_real_directory(&file, "directory"),
            Err(LswError::InvalidValue {
                field: "directory",
                ..
            })
        ));
        assert!(matches!(
            require_regular_file(&directory.join("missing"), "file"),
            Err(LswError::Io(_))
        ));

        fs::remove_dir_all(&directory).expect("temporary directory removal");
    }
}
