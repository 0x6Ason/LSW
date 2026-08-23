// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal systemd-compatible socket activation.
//!
//! This module is the only unsafe boundary in the daemon. systemd guarantees
//! that descriptors start at 3 and transfers ownership to the activated
//! process. All environment and socket-path checks happen before that ownership
//! is accepted.

use std::env;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::Path;

const SYSTEMD_LISTEN_FD: RawFd = 3;

pub(super) fn inherited_listener(
    expected_path: &Path,
) -> Result<Option<UnixListener>, Box<dyn std::error::Error>> {
    let listen_pid = env::var("LISTEN_PID").ok();
    let listen_fds = env::var("LISTEN_FDS").ok();
    let listen_names = env::var("LISTEN_FDNAMES").ok();
    let activated = activation_requested(
        std::process::id(),
        listen_pid.as_deref(),
        listen_fds.as_deref(),
        listen_names.as_deref(),
    )?;
    if !activated {
        return Ok(None);
    }

    env::remove_var("LISTEN_PID");
    env::remove_var("LISTEN_FDS");
    env::remove_var("LISTEN_FDNAMES");

    // SAFETY: activation_requested established that systemd assigned exactly
    // one descriptor to this PID. The socket address check below establishes
    // that fd 3 is the private listener LSW expects before it is used.
    let listener = unsafe { UnixListener::from_raw_fd(SYSTEMD_LISTEN_FD) };
    let actual_path = listener
        .local_addr()?
        .as_pathname()
        .ok_or("activated descriptor is not a pathname Unix socket")?
        .to_path_buf();
    if actual_path != expected_path {
        return Err(format!(
            "activated socket {} does not match expected path {}",
            actual_path.display(),
            expected_path.display()
        )
        .into());
    }
    Ok(Some(listener))
}

fn activation_requested(
    current_pid: u32,
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
    listen_names: Option<&str>,
) -> Result<bool, Box<dyn std::error::Error>> {
    match (listen_pid, listen_fds) {
        (None, None) => return Ok(false),
        (Some(pid), Some(fds)) => {
            let pid = pid
                .parse::<u32>()
                .map_err(|_| "LISTEN_PID must be a decimal process ID")?;
            if pid != current_pid {
                return Ok(false);
            }
            let fds = fds
                .parse::<u32>()
                .map_err(|_| "LISTEN_FDS must be a decimal descriptor count")?;
            if fds != 1 {
                return Err("lswd requires exactly one activated descriptor".into());
            }
        }
        _ => return Err("LISTEN_PID and LISTEN_FDS must be supplied together".into()),
    }
    if let Some(names) = listen_names {
        if names != "lswd" {
            return Err("activated descriptor must be named lswd".into());
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_environment_is_strict_and_pid_scoped() {
        assert!(!activation_requested(42, None, None, None).unwrap());
        assert!(!activation_requested(42, Some("41"), Some("1"), Some("lswd")).unwrap());
        assert!(activation_requested(42, Some("42"), Some("1"), Some("lswd")).unwrap());
        assert!(activation_requested(42, Some("42"), Some("2"), Some("lswd")).is_err());
        assert!(activation_requested(42, Some("42"), Some("1"), Some("other")).is_err());
        assert!(activation_requested(42, Some("42"), None, None).is_err());
    }
}
