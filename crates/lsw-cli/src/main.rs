// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("the LSW 1.0 beta CLI currently requires a Unix host");

mod arguments;
mod completion;
mod desktop_launcher;
mod file_bench;
mod guest;
mod installation;
mod license;
mod lifecycle;
mod lswg_launcher;
mod management;
mod path_translation;
mod profile;
mod progress;
mod reporting;
mod shares;
mod transfer;
mod user_setup;
mod viewer;
mod windows_sudo;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write as IoWrite};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arguments::{
    next_value, parse_number, resolve_port_forwards, CreateArguments, InstallArguments,
};
use installation::{find_windows_agent, install_instance};
use license::{license, show_activation_notice_once};
use lsw_core::{
    FolderShareTransport, GuiStartRequest, HostCapabilities, IdlePolicy, ImageManager,
    InstallSeedBuilder, InstallSeedOptions, InstanceManifest, InstanceSpec, InstanceState,
    LaunchPhase, LswError, MicrosoftIsoRequest, MicrosoftIsoResolver, PeImage, PeImportSymbol,
    ProcessEnvironment, Provisioner, QemuBackend, QemuPlanner, SessionKind, StartRequest,
    StateStore, VmAccelerator,
};
use lsw_host::{agent_address, AgentClient, DaemonClient};
use progress::{ProgressEvent, ProgressRenderer};
// Windows can spend several minutes servicing a cold boot, and removable
// identity media can arrive after the automatic SCM agent starts.
const AGENT_START_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PIPE_SHELL_NOTICE: &str =
    "lsw: note: this agent provides a pipe shell; ConPTY is not available in this beta build";
fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("lsw: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<OsString>) -> Result<u8, Box<dyn std::error::Error>> {
    if arguments.is_empty() {
        let store = StateStore::new(state_root()?);
        return shell(&store, &[]);
    }
    let command = arguments[0].to_str().ok_or("command must be valid UTF-8")?;
    let remaining = &arguments[1..];

    match command {
        "help" | "--help" | "-h" => {
            print_help();
            return Ok(0);
        }
        "version" | "--version" | "-V" => {
            println!("lsw {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        "inspect" => {
            inspect_pe(remaining)?;
            return Ok(0);
        }
        "profile" => {
            profile::command(remaining)?;
            return Ok(0);
        }
        "media" => {
            media(remaining)?;
            return Ok(0);
        }
        "path" => {
            path_translation::command(remaining)?;
            return Ok(0);
        }
        "completion" => {
            completion::command(remaining)?;
            return Ok(0);
        }
        "__viewer-bridge" => return viewer::bridge_command(remaining),
        _ => {}
    }

    let store = StateStore::new(state_root()?);
    match command {
        "doctor" => doctor(&store, remaining)?,
        "image" => image_command(&store, remaining)?,
        "clone" => clone_instance(&store, remaining)?,
        "bench" => bench(&store, remaining)?,
        "create" => create(&store, remaining)?,
        "prepare" => prepare(&store, remaining)?,
        "seed" => seed(&store, remaining)?,
        "list" => list(&store)?,
        "show" => show(&store, remaining)?,
        "config" => config(&store, remaining)?,
        "logs" => logs(&store, remaining)?,
        "diagnose" => diagnose(&store, remaining)?,
        "remove" => remove_instance(&store, remaining)?,
        "shutdown" => shutdown(&store, remaining)?,
        "view" => view(&store, remaining)?,
        "plan" => plan(&store, remaining)?,
        "use" => select_default(&store, remaining)?,
        "daemon" => daemon(&store, remaining)?,
        "app" => desktop_launcher::command(&store, remaining)?,
        "install" => install_instance(&store, remaining)?,
        "license" => license(&store, remaining)?,
        "user" => user_command(&store, remaining)?,
        "sudo" => windows_sudo::command(&store, remaining)?,
        "share" => shares::command(&store, remaining)?,
        "unshare" => shares::unshare(&store, remaining)?,
        "start" => start_instance(&store, remaining, LaunchPhase::Run)?,
        "status" => status(&store, remaining)?,
        "suspend" => suspend(&store, remaining)?,
        "resume" => resume(&store, remaining)?,
        "hibernate" => hibernate(&store, remaining)?,
        "memory" => memory_command(&store, remaining)?,
        "trim" => trim_instance(&store, remaining)?,
        "compact" => compact_instance(&store, remaining)?,
        "stop" => stop(&store, remaining)?,
        "shell" => return shell(&store, remaining),
        "exec" => return guest_command(&store, remaining, SessionKind::Exec),
        "run" => return guest_command(&store, remaining, SessionKind::Run),
        "push" => transfer::push(&store, remaining)?,
        "pull" => transfer::pull(&store, remaining)?,
        "cp" => transfer::copy(&store, remaining)?,
        "sync" => transfer::sync(&store, remaining)?,
        unknown => {
            return Err(format!("unknown command {unknown:?}; run `lsw help`").into());
        }
    }
    Ok(0)
}

use guest::*;
use lifecycle::*;
use management::*;
use reporting::*;

#[cfg(test)]
mod tests;
