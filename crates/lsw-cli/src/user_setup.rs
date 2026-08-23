// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::process::{Command, Stdio};

use lsw_core::{validate_windows_user_name, StateStore, UserCreateRequest};

use super::{connect_agent, resolve_name};

pub(super) fn command(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = UserSetupArguments::parse(arguments)?;
    let name = resolve_name(store, parsed.requested.as_deref())?;
    setup(
        store,
        &name,
        parsed.user_name,
        parsed.password_stdin,
        parsed.administrator,
    )
}

pub(super) fn after_install(
    store: &StateStore,
    name: &str,
    deferred: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = store.load(name)?;
    if let Some(user) = manifest.default_user {
        println!("Windows desktop user {user:?} is already registered.");
        return Ok(());
    }
    if deferred {
        println!("Windows user registration deferred. Run `lsw user setup {name}` later.");
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(
            "interactive Windows user registration requires a terminal; use --defer-user-setup and run `lsw user setup --password-stdin` later"
                .into(),
        );
    }
    println!("Create the permanent Windows desktop user for {name:?}.");
    setup(store, name, None, false, false)
}

fn setup(
    store: &StateStore,
    name: &str,
    supplied_user_name: Option<String>,
    password_stdin: bool,
    administrator: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = store.load(name)?;
    if let Some(existing) = manifest.default_user {
        return Err(
            format!("instance {name:?} already has default Windows user {existing:?}").into(),
        );
    }
    let user_name = match supplied_user_name {
        Some(user_name) => user_name,
        None => prompt_user_name()?,
    };
    validate_windows_user_name(&user_name)?;
    let mut password = if password_stdin {
        read_password_stdin()?
    } else {
        read_confirmed_password()?
    };
    let request = UserCreateRequest {
        user_name: user_name.clone(),
        password: std::mem::take(&mut password),
        administrator,
    };
    let result = connect_agent(store, name)?.create_user(&request);
    password.fill(0);
    result?;

    let mut manifest = store.load(name)?;
    manifest.default_user = Some(user_name.clone());
    store.update(&manifest)?;
    println!("Created or securely verified Windows user {user_name:?} for {name:?}.");
    if administrator {
        println!("The user was explicitly added to the local Administrators group.");
    } else {
        println!("The user is a standard account. Use elevation only when Windows requests it.");
    }
    println!("AutoLogon remains disabled.");
    Ok(())
}

fn prompt_user_name() -> Result<String, Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err("--username USER is required without an interactive terminal".into());
    }
    let mut stderr = io::stderr().lock();
    write!(stderr, "Windows user name: ")?;
    stderr.flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim_end_matches(['\r', '\n']).to_owned())
}

fn read_confirmed_password() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err("--password-stdin is required without an interactive terminal".into());
    }
    let mut first = read_secret_line("Windows password: ")?;
    let mut second = read_secret_line("Confirm password: ")?;
    if first != second {
        first.fill(0);
        second.fill(0);
        return Err("password confirmation did not match".into());
    }
    second.fill(0);
    if let Err(error) = validate_password(&first) {
        first.fill(0);
        return Err(error);
    }
    Ok(first)
}

fn read_secret_line(prompt: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut stderr = io::stderr().lock();
    write!(stderr, "{prompt}")?;
    stderr.flush()?;
    let guard = EchoGuard::disable()?;
    let mut value = String::new();
    let read_result = io::stdin().read_line(&mut value);
    drop(guard);
    writeln!(stderr)?;
    read_result?;
    while value.ends_with(['\r', '\n']) {
        value.pop();
    }
    Ok(value.into_bytes())
}

fn read_password_stdin() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut value = Vec::new();
    io::stdin().take(4097).read_to_end(&mut value)?;
    if value.len() > 4096 {
        value.fill(0);
        return Err("password input is too long".into());
    }
    while value
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        value.pop();
    }
    if value.contains(&b'\r') || value.contains(&b'\n') {
        value.fill(0);
        return Err("--password-stdin accepts exactly one line".into());
    }
    if let Err(error) = validate_password(&value) {
        value.fill(0);
        return Err(error);
    }
    Ok(value)
}

fn validate_password(password: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    if password.is_empty() {
        return Err("password must not be empty".into());
    }
    let password = std::str::from_utf8(password)?;
    if password.contains('\0') || password.encode_utf16().count() > 256 {
        return Err("password must contain at most 256 UTF-16 code units and no NUL".into());
    }
    Ok(())
}

struct EchoGuard;

impl EchoGuard {
    fn disable() -> Result<Self, Box<dyn std::error::Error>> {
        let status = Command::new("stty")
            .arg("-echo")
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err("could not disable terminal echo with stty".into());
        }
        Ok(Self)
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = Command::new("stty")
            .arg("echo")
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[derive(Debug)]
struct UserSetupArguments {
    requested: Option<String>,
    user_name: Option<String>,
    password_stdin: bool,
    administrator: bool,
}

impl UserSetupArguments {
    fn parse(arguments: &[OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut parsed = Self {
            requested: None,
            user_name: None,
            password_stdin: false,
            administrator: false,
        };
        let mut index = 0;
        while index < arguments.len() {
            let argument = arguments[index]
                .to_str()
                .ok_or("user setup arguments must be valid UTF-8")?;
            match argument {
                "--username" => {
                    index += 1;
                    let value = arguments
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or("--username requires a value")?;
                    if parsed.user_name.replace(value.to_owned()).is_some() {
                        return Err("--username was supplied more than once".into());
                    }
                }
                "--password-stdin" => {
                    if parsed.password_stdin {
                        return Err("--password-stdin was supplied more than once".into());
                    }
                    parsed.password_stdin = true;
                }
                "--administrator" => {
                    if parsed.administrator {
                        return Err("--administrator was supplied more than once".into());
                    }
                    parsed.administrator = true;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown user setup option {value:?}").into())
                }
                name => {
                    if parsed.requested.replace(name.to_owned()).is_some() {
                        return Err(Self::usage().into());
                    }
                }
            }
            index += 1;
        }
        if parsed.password_stdin && parsed.user_name.is_none() {
            return Err("--password-stdin requires --username USER".into());
        }
        Ok(parsed)
    }

    fn usage() -> &'static str {
        "usage: lsw user setup [NAME] [--username USER] [--password-stdin] [--administrator]"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_keeps_administrator_explicit() {
        let parsed = UserSetupArguments::parse(&[
            OsString::from("dev"),
            OsString::from("--username"),
            OsString::from("jason"),
            OsString::from("--password-stdin"),
        ])
        .unwrap();
        assert_eq!(parsed.requested.as_deref(), Some("dev"));
        assert_eq!(parsed.user_name.as_deref(), Some("jason"));
        assert!(!parsed.administrator);
    }

    #[test]
    fn password_stdin_requires_a_noninteractive_user_name() {
        assert!(UserSetupArguments::parse(&[OsString::from("--password-stdin")]).is_err());
        assert!(UserSetupArguments::parse(&[OsString::from("--administrator")]).is_ok());
    }
}
