// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::OsString;
use std::io::{self, Write};

use lsw_core::{StateStore, WindowsSudoMode, WindowsSudoStatus};

use super::{connect_agent, resolve_name};

pub(super) fn command(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments
        .first()
        .and_then(|argument| argument.to_str())
        .ok_or("usage: lsw sudo <status|enable|disable> [NAME]")?;
    let requested = match &arguments[1..] {
        [] => None,
        [name] => Some(name.to_str().ok_or("instance name must be valid UTF-8")?),
        _ => return Err("usage: lsw sudo <status|enable|disable> [NAME]".into()),
    };
    let name = resolve_name(store, requested)?;
    match action {
        "status" => print_status(connect_agent(store, &name)?.windows_sudo_status()?),
        "enable" => configure(store, &name, true)?,
        "disable" => configure(store, &name, false)?,
        _ => return Err("usage: lsw sudo <status|enable|disable> [NAME]".into()),
    }
    Ok(())
}

pub(super) fn offer_after_install(
    store: &StateStore,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = match connect_agent(store, name)?.windows_sudo_status() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("lsw: note: could not inspect native Windows sudo: {error}");
            eprintln!("Run `lsw sudo status {name}` after installation to retry.");
            return Ok(());
        }
    };
    if !status.available {
        println!("Native Windows sudo is unavailable in this Windows build.");
        return Ok(());
    }
    if let Some(maximum) = status.policy_mode {
        println!(
            "Windows sudo is managed by system policy (maximum mode: {}).",
            mode_label(maximum)
        );
        println!("LSW left the local setting unchanged.");
        return Ok(());
    }
    if status.configured_mode == WindowsSudoMode::ForceNewWindow {
        println!("Native Windows sudo is already enabled in new-window mode.");
        return Ok(());
    }

    let description = if status.configured_mode == WindowsSudoMode::Disabled {
        "Enable native Windows sudo in its safer new-window mode? UAC consent remains required. [Y/n]: "
    } else {
        "Change native Windows sudo to its safer new-window mode? UAC consent remains required. [Y/n]: "
    };
    if prompt_choice(description)? {
        configure(store, name, true)?;
    } else {
        println!(
            "Windows sudo was left unchanged. Run `lsw sudo enable {name}` later to enable it."
        );
    }
    Ok(())
}

fn configure(
    store: &StateStore,
    name: &str,
    enable: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let before = connect_agent(store, name)?.windows_sudo_status()?;
    if !before.available {
        return Err(
            "native Windows sudo is unavailable; Windows 11 24H2 or later is required".into(),
        );
    }
    if before.policy_mode.is_some() {
        return Err("Windows sudo is managed by system policy; LSW will not override it".into());
    }
    connect_agent(store, name)?.configure_windows_sudo(enable)?;
    let after = connect_agent(store, name)?.windows_sudo_status()?;
    let expected = if enable {
        WindowsSudoMode::ForceNewWindow
    } else {
        WindowsSudoMode::Disabled
    };
    if after.policy_mode.is_some() || after.configured_mode != expected {
        return Err("Windows did not retain the requested sudo configuration".into());
    }
    if enable {
        println!("Enabled native Windows sudo in new-window mode for {name:?}.");
        println!("Elevation still opens a Windows UAC consent prompt.");
    } else {
        println!("Disabled native Windows sudo for {name:?}.");
    }
    Ok(())
}

fn print_status(status: WindowsSudoStatus) {
    if !status.available {
        println!("Windows sudo: unavailable");
        println!("Required Windows version: Windows 11 24H2 or later");
        return;
    }
    println!("Windows sudo: {}", mode_label(status.effective_mode()));
    println!("Configured mode: {}", mode_label(status.configured_mode));
    match status.policy_mode {
        Some(maximum) => println!("System policy maximum: {}", mode_label(maximum)),
        None => println!("System policy: not configured"),
    }
    println!("UAC consent: required for elevation");
}

fn prompt_choice(prompt: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let mut stderr = io::stderr().lock();
    loop {
        write!(stderr, "{prompt}")?;
        stderr.flush()?;
        let mut value = String::new();
        io::stdin().read_line(&mut value)?;
        match parse_choice(&value) {
            Some(choice) => return Ok(choice),
            None => writeln!(stderr, "Enter y or n.")?,
        }
    }
}

fn parse_choice(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

fn mode_label(mode: WindowsSudoMode) -> &'static str {
    match mode {
        WindowsSudoMode::Disabled => "disabled",
        WindowsSudoMode::ForceNewWindow => "new window",
        WindowsSudoMode::DisableInput => "input closed",
        WindowsSudoMode::Normal => "inline",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_defaults_to_the_recommended_safe_mode() {
        assert_eq!(parse_choice(""), Some(true));
        assert_eq!(parse_choice("yes\n"), Some(true));
        assert_eq!(parse_choice("N\r\n"), Some(false));
        assert_eq!(parse_choice("later"), None);
    }

    #[test]
    fn policy_restricts_the_reported_effective_mode() {
        let status = WindowsSudoStatus {
            available: true,
            configured_mode: WindowsSudoMode::Normal,
            policy_mode: Some(WindowsSudoMode::ForceNewWindow),
        };
        assert_eq!(status.effective_mode(), WindowsSudoMode::ForceNewWindow);
    }
}
