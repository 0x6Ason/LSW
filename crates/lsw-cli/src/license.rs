// SPDX-License-Identifier: GPL-3.0-or-later

//! Host-side Windows activation commands.
//!
//! Product keys enter through a masked terminal or stdin, cross the existing
//! authenticated agent session once, and are zeroed from mutable host buffers.

use std::ffi::OsString;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::thread;
use std::time::Duration;

use lsw_core::{SessionKind, StartRequest, StateStore};

use super::{connect_agent, resolve_name};

pub(super) fn license(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or("usage: lsw license status|activate|open [NAME] [OPTIONS]")?;
    match action {
        "status" | "open" => {
            if arguments.len() > 2 {
                return Err(format!("usage: lsw license {action} [NAME]").into());
            }
            let requested = arguments
                .get(1)
                .map(|value| value.to_str().ok_or("instance name must be valid UTF-8"))
                .transpose()?;
            let name = resolve_name(store, requested)?;
            run_guest_license_action(store, &name, action, &[])?;
        }
        "activate" => {
            let mut requested = None;
            let mut key_stdin = false;
            let mut online = false;
            for argument in &arguments[1..] {
                let argument = argument
                    .to_str()
                    .ok_or("license arguments must be valid UTF-8")?;
                match argument {
                    "--key-stdin" if !key_stdin => key_stdin = true,
                    "--online" if !online => online = true,
                    value if value.starts_with('-') => {
                        return Err(format!("unknown license option {value:?}").into())
                    }
                    name if requested.replace(name).is_some() => {
                        return Err(
                            "usage: lsw license activate [NAME] [--key-stdin | --online]".into(),
                        )
                    }
                    _ => {}
                }
            }
            if key_stdin && online {
                return Err("--key-stdin and --online cannot be used together".into());
            }
            let name = resolve_name(store, requested)?;
            if online {
                run_guest_license_action(store, &name, "online", &[])?;
            } else {
                let mut key = read_product_key(key_stdin)?;
                let result = run_guest_license_action(store, &name, "activate", &key);
                key.fill(0);
                result?;
            }
        }
        _ => return Err("usage: lsw license status|activate|open [NAME] [OPTIONS]".into()),
    }
    Ok(())
}

fn run_guest_license_action(
    store: &StateStore,
    name: &str,
    action: &str,
    input: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    let output = capture_guest_license_action(store, name, action, input)?;
    if !output.is_empty() {
        print!("{output}");
    }
    Ok(output)
}

fn capture_guest_license_action(
    store: &StateStore,
    name: &str,
    action: &str,
    input: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: vec![
            r"C:\Program Files\LSW\lsw-agent.exe".to_owned(),
            "--license-client".to_owned(),
            action.to_owned(),
            "--token-file".to_owned(),
            r"C:\ProgramData\LSW\agent.token".to_owned(),
        ],
        working_directory: None,
    };
    let mut secret_input = input.to_vec();
    if !secret_input.is_empty() {
        secret_input.push(b'\n');
    }
    let result = connect_agent(store, name)?.run_capture(&request, &secret_input, 64 * 1024);
    secret_input.fill(0);
    let captured = result?;
    if captured.exit_code != 0 {
        let message = String::from_utf8_lossy(&captured.stderr).trim().to_owned();
        return Err(if message.is_empty() {
            format!(
                "Windows license operation failed with exit code {}",
                captured.exit_code
            )
            .into()
        } else {
            format!("Windows license operation failed: {message}").into()
        });
    }
    Ok(String::from_utf8(captured.stdout)?)
}

pub(super) fn show_activation_notice_once(store: &StateStore, name: &str) {
    let mut output = None;
    for attempt in 0..3 {
        match capture_guest_license_action(store, name, "status", &[]) {
            Ok(captured) => {
                output = Some(captured);
                break;
            }
            Err(error) if attempt == 2 => {
                eprintln!("warning: could not query Windows activation status: {error}");
            }
            Err(_) => thread::sleep(Duration::from_secs(1)),
        }
    }
    let Some(output) = output else {
        return;
    };
    if !license_status_is_unlicensed(&output) {
        return;
    }
    let Ok(instance_dir) = store.instance_dir(name) else {
        return;
    };
    let marker = instance_dir.join("activation-notice-shown");
    let created = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker);
    let Ok(file) = created else {
        return;
    };
    let _ = fs::set_permissions(&marker, fs::Permissions::from_mode(0o600));
    drop(file);
    println!("Windows 11 Pro is not activated.");
    println!("  Activate now:          lsw license activate {name}");
    println!("  Open Windows Activation: lsw license open {name}");
    println!("  Later:                 continue with `lsw`");
}

fn license_status_is_unlicensed(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim_end_matches('\r') == "STATUS=unlicensed")
}

fn read_product_key(from_stdin: bool) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let terminal = std::io::stdin().is_terminal();
    if !from_stdin && !terminal {
        return Err("masked product-key input requires a terminal; use --key-stdin".into());
    }
    let _echo = if from_stdin {
        None
    } else {
        eprint!("Windows product key: ");
        std::io::stderr().flush()?;
        Some(TerminalEchoGuard::disable()?)
    };
    let mut key = read_secret_line(128)?;
    if !from_stdin {
        eprintln!();
    }
    while key.last().is_some_and(|byte| matches!(byte, b'\r' | b'\n')) {
        key.pop();
    }
    key.make_ascii_uppercase();
    let valid = key.len() == 29
        && key.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 5 | 11 | 17 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_alphanumeric()
            }
        });
    if !valid {
        key.fill(0);
        return Err("product key must use five groups of five letters or digits".into());
    }
    Ok(key)
}

fn read_secret_line(limit: usize) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut input = std::io::stdin().lock();
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while bytes.len() <= limit {
        match input.read(&mut byte)? {
            0 => break,
            1 => {
                bytes.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            _ => unreachable!("single-byte reads cannot return more than one byte"),
        }
    }
    if bytes.len() > limit {
        bytes.fill(0);
        return Err("product-key input is too long".into());
    }
    Ok(bytes)
}

struct TerminalEchoGuard;

impl TerminalEchoGuard {
    fn disable() -> Result<Self, Box<dyn std::error::Error>> {
        let status = Command::new("stty").arg("-echo").status()?;
        if !status.success() {
            return Err("could not disable terminal echo".into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        let _ = Command::new("stty").arg("echo").status();
    }
}

#[cfg(test)]
mod tests {
    use super::license_status_is_unlicensed;

    #[test]
    fn activation_notice_accepts_windows_line_endings() {
        assert!(license_status_is_unlicensed(
            "STATUS=unlicensed\r\nLICENSE_STATUS=5\r\n"
        ));
        assert!(!license_status_is_unlicensed(
            "STATUS=licensed\r\nLICENSE_STATUS=1\r\n"
        ));
    }
}
