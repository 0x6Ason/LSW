// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use lsw_core::{
    sha256_file, write_frame, Frame, FrameKind, GuiStartRequest, StateStore,
    CAPABILITY_GUI_WINDOW_V3,
};

const MAX_LSWG_OUTPUT_BYTES: usize = 1024;

pub(super) fn present(
    store: &StateStore,
    instance: &str,
    request: &GuiStartRequest,
) -> Result<i32, Box<dyn std::error::Error>> {
    let program = lswg_program()?;
    verify_hash(&program)?;
    verify_version(&program)?;

    let mut child = Command::new(&program)
        .args(["--instance", instance])
        .env("LSW_STATE_DIR", store.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let frame = Frame::new(FrameKind::GuiWindowOpen, request.encode()?);
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| "could not open lswg request input".into())
        .and_then(|mut stdin: std::process::ChildStdin| {
            write_frame(&mut stdin, &frame).map_err(Into::into)
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!("lswg exited with {}", output.status).into());
    }
    parse_exit_output(&output.stdout)
}

fn lswg_program() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let candidate = if let Some(configured) = env::var_os("LSWG") {
        PathBuf::from(configured)
    } else {
        env::current_exe()?
            .parent()
            .ok_or("lsw executable has no parent directory")?
            .join("lswg")
    };
    let metadata = fs::symlink_metadata(&candidate)
        .map_err(|error| format!("lswg was not found at {}: {error}", candidate.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "lswg is not a regular non-symlink file: {}",
            candidate.display()
        )
        .into());
    }
    Ok(fs::canonicalize(candidate)?)
}

fn verify_hash(program: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let expected = if let Some(configured) = env::var_os("LSWG_SHA256") {
        Some(parse_sha256(&configured.to_string_lossy())?)
    } else {
        expected_hash_sidecar(program)?
    };
    let Some(expected) = expected else {
        if cfg!(debug_assertions) {
            return Ok(());
        }
        return Err("release lsw requires an exact lswg SHA-256 sidecar or LSWG_SHA256".into());
    };
    let actual = sha256_file(program)?;
    if actual != expected {
        return Err(
            format!("lswg SHA-256 mismatch: expected {expected}, received {actual}").into(),
        );
    }
    Ok(())
}

fn expected_hash_sidecar(program: &Path) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut candidates = vec![program.with_file_name("lswg.sha256")];
    if let Some(prefix) = env::current_exe()?.parent().and_then(Path::parent) {
        candidates.push(prefix.join("libexec/lsw/lswg.sha256"));
    }
    for candidate in candidates {
        let metadata = match fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(format!(
                "lswg SHA-256 sidecar is not a regular non-symlink file: {}",
                candidate.display()
            )
            .into());
        }
        if metadata.len() > 65 {
            return Err("lswg SHA-256 sidecar exceeds 65 bytes".into());
        }
        return Ok(Some(parse_sha256(&fs::read_to_string(candidate)?)?));
    }
    Ok(None)
}

fn parse_sha256(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value = value.strip_suffix('\n').unwrap_or(value);
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("lswg SHA-256 must be exactly 64 lowercase hexadecimal digits".into());
    }
    Ok(value.to_owned())
}

fn verify_version(program: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if output.stdout.len() > MAX_LSWG_OUTPUT_BYTES || output.stderr.len() > MAX_LSWG_OUTPUT_BYTES {
        return Err("lswg version output exceeded the size limit".into());
    }
    if !output.status.success() {
        return Err(format!("lswg --version exited with {}", output.status).into());
    }
    let expected = format!(
        "lswg {} {CAPABILITY_GUI_WINDOW_V3}\n",
        env!("CARGO_PKG_VERSION")
    );
    if output.stdout != expected.as_bytes() || !output.stderr.is_empty() {
        return Err("lswg version or GUI protocol identity does not match lsw".into());
    }
    Ok(())
}

fn parse_exit_output(output: &[u8]) -> Result<i32, Box<dyn std::error::Error>> {
    if output.len() > MAX_LSWG_OUTPUT_BYTES {
        return Err("lswg result exceeded the size limit".into());
    }
    let output = std::str::from_utf8(output)?;
    let value = output
        .strip_prefix("LSWG_EXIT=")
        .and_then(|value| value.strip_suffix('\n'))
        .ok_or("lswg returned an invalid result")?;
    if value.is_empty() || value.contains(['\r', '\n']) {
        return Err("lswg returned an invalid exit code".into());
    }
    Ok(value.parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_parser_is_exact_and_lowercase() {
        let digest = "a".repeat(64);
        assert_eq!(parse_sha256(&digest).unwrap(), digest);
        assert_eq!(parse_sha256(&format!("{digest}\n")).unwrap(), digest);
        assert!(parse_sha256(&"A".repeat(64)).is_err());
        assert!(parse_sha256(&format!("{digest}\n\n")).is_err());
    }

    #[test]
    fn presenter_exit_result_preserves_full_windows_codes() {
        assert_eq!(
            parse_exit_output(b"LSWG_EXIT=-1073741510\n").unwrap(),
            -1073741510
        );
        assert!(parse_exit_output(b"LSWG_EXIT=0\ntrailing").is_err());
        assert!(parse_exit_output(b"0\n").is_err());
    }
}
