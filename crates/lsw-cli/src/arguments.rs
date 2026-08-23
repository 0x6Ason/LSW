// SPDX-License-Identifier: GPL-3.0-or-later

//! Strict command-line parsing for instance creation and installation.
//!
//! Parsing is side-effect free. Filesystem canonicalization is limited to
//! explicit path values, while all state mutation remains in command modules.

use std::ffi::OsString;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use lsw_core::{
    control_port_for_instance, InstallSeedOptions, NetworkMode, PortForward, WindowsProfile,
    AGENT_CONTROL_PORT_END_EXCLUSIVE, AGENT_CONTROL_PORT_START,
};

use super::absolute_path;

#[derive(Debug)]
pub(super) struct InstallArguments {
    pub(super) requested: Option<String>,
    pub(super) iso: Option<PathBuf>,
    pub(super) edition: Option<String>,
    pub(super) profile: WindowsProfile,
    pub(super) language: String,
    pub(super) cpus: u16,
    pub(super) memory_mib: u32,
    pub(super) disk_gib: Option<u32>,
    pub(super) network: NetworkMode,
    pub(super) port_forwards: Vec<PortForwardRequest>,
    pub(super) accept_windows_license: bool,
    pub(super) allow_unsupported_requirements: bool,
    pub(super) seed: InstallSeedOptions,
    pub(super) without_agent: bool,
    pub(super) no_viewer: bool,
    pub(super) defer_user_setup: bool,
    viewer_option_seen: bool,
    pub(super) seed_option_seen: bool,
    pub(super) create_option_seen: bool,
    pub(super) language_option_seen: bool,
}

impl InstallArguments {
    pub(super) fn parse(arguments: &[OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut parsed = Self {
            requested: None,
            iso: None,
            edition: None,
            profile: WindowsProfile::Slim,
            language: "English".to_owned(),
            cpus: 2,
            memory_mib: 4096,
            disk_gib: None,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            accept_windows_license: false,
            allow_unsupported_requirements: false,
            seed: InstallSeedOptions::default(),
            without_agent: false,
            no_viewer: true,
            defer_user_setup: false,
            viewer_option_seen: false,
            seed_option_seen: false,
            create_option_seen: false,
            language_option_seen: false,
        };
        let mut index = 0;
        while index < arguments.len() {
            let argument = arguments[index]
                .to_str()
                .ok_or("install arguments must be valid UTF-8")?;
            match argument {
                "--iso" => {
                    if parsed.iso.is_some() {
                        return Err("--iso was supplied more than once".into());
                    }
                    parsed.iso = Some(PathBuf::from(next_value(arguments, &mut index, argument)?));
                }
                "--edition" => {
                    if parsed.edition.is_some() {
                        return Err("--edition was supplied more than once".into());
                    }
                    parsed.edition = Some(next_value(arguments, &mut index, argument)?.to_owned());
                    parsed.seed_option_seen = true;
                }
                "--profile" => {
                    parsed.profile = next_value(arguments, &mut index, argument)?.parse()?;
                    parsed.create_option_seen = true;
                }
                "--language" => {
                    parsed.language = next_value(arguments, &mut index, argument)?.to_owned();
                    parsed.language_option_seen = true;
                }
                "--cpus" => {
                    parsed.cpus = parse_number(arguments, &mut index, argument)?;
                    parsed.create_option_seen = true;
                }
                "--memory" => {
                    parsed.memory_mib = parse_number(arguments, &mut index, argument)?;
                    parsed.create_option_seen = true;
                }
                "--disk" => {
                    parsed.disk_gib = Some(parse_number(arguments, &mut index, argument)?);
                    parsed.create_option_seen = true;
                }
                "--network" => {
                    parsed.network = next_value(arguments, &mut index, argument)?.parse()?;
                    parsed.create_option_seen = true;
                }
                "--publish" => {
                    let value = next_value(arguments, &mut index, argument)?;
                    parsed.port_forwards.push(parse_publish(value)?);
                    parsed.create_option_seen = true;
                }
                "--locale" => {
                    parsed.seed.locale = next_value(arguments, &mut index, argument)?.to_owned();
                    parsed.seed_option_seen = true;
                }
                "--unattended-index" => {
                    parsed.seed.unattended_image_index =
                        Some(parse_number(arguments, &mut index, argument)?);
                    parsed.seed_option_seen = true;
                }
                "--agent" => {
                    parsed.seed.agent_binary = Some(absolute_path(Path::new(next_value(
                        arguments, &mut index, argument,
                    )?))?);
                    parsed.seed_option_seen = true;
                }
                "--without-agent" => {
                    if parsed.without_agent {
                        return Err("--without-agent was supplied more than once".into());
                    }
                    parsed.without_agent = true;
                    parsed.seed_option_seen = true;
                }
                "--no-viewer" => {
                    if parsed.viewer_option_seen {
                        return Err("--viewer and --no-viewer may be supplied only once".into());
                    }
                    parsed.no_viewer = true;
                    parsed.viewer_option_seen = true;
                }
                "--viewer" => {
                    if parsed.viewer_option_seen {
                        return Err("--viewer and --no-viewer may be supplied only once".into());
                    }
                    parsed.no_viewer = false;
                    parsed.viewer_option_seen = true;
                }
                "--allow-unsupported-requirements" => {
                    parsed.allow_unsupported_requirements = true;
                    parsed.create_option_seen = true;
                }
                "--accept-windows-license" | "--accept-license" => {
                    if parsed.accept_windows_license {
                        return Err("Windows license acceptance was supplied more than once".into());
                    }
                    parsed.accept_windows_license = true;
                }
                "--defer-user-setup" => {
                    if parsed.defer_user_setup {
                        return Err("--defer-user-setup was supplied more than once".into());
                    }
                    parsed.defer_user_setup = true;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown install option {value:?}").into())
                }
                name => {
                    if parsed.requested.replace(name.to_owned()).is_some() {
                        return Err("usage: lsw install [NAME] [OPTIONS]".into());
                    }
                }
            }
            index += 1;
        }
        Ok(parsed)
    }
}

#[derive(Debug)]
pub(super) struct CreateArguments {
    pub(super) name: String,
    pub(super) iso: PathBuf,
    pub(super) profile: WindowsProfile,
    pub(super) cpus: u16,
    pub(super) memory_mib: u32,
    pub(super) disk_gib: Option<u32>,
    pub(super) network: NetworkMode,
    pub(super) port_forwards: Vec<PortForwardRequest>,
    pub(super) accept_windows_license: bool,
    pub(super) allow_unsupported_requirements: bool,
}

impl CreateArguments {
    pub(super) fn parse(arguments: &[OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let name = arguments
            .first()
            .and_then(|value| value.to_str())
            .ok_or("usage: lsw create NAME --iso PATH --accept-windows-license")?
            .to_owned();
        let mut iso = None;
        let mut profile = WindowsProfile::Slim;
        let mut cpus = 2;
        let mut memory_mib = 4096;
        let mut disk_gib = None;
        let mut network = NetworkMode::Nat;
        let mut port_forwards = Vec::new();
        let mut accept_windows_license = false;
        let mut allow_unsupported_requirements = false;
        let mut index = 1;

        while index < arguments.len() {
            let option = arguments[index]
                .to_str()
                .ok_or("command arguments must be valid UTF-8")?;
            match option {
                "--iso" => {
                    iso = Some(PathBuf::from(next_value(arguments, &mut index, option)?));
                }
                "--profile" => {
                    profile = next_value(arguments, &mut index, option)?.parse()?;
                }
                "--cpus" => cpus = parse_number(arguments, &mut index, option)?,
                "--memory" => memory_mib = parse_number(arguments, &mut index, option)?,
                "--disk" => disk_gib = Some(parse_number(arguments, &mut index, option)?),
                "--network" => network = next_value(arguments, &mut index, option)?.parse()?,
                "--publish" => {
                    let value = next_value(arguments, &mut index, option)?;
                    port_forwards.push(parse_publish(value)?);
                }
                "--accept-windows-license" | "--accept-license" => {
                    if accept_windows_license {
                        return Err("Windows license acceptance was supplied more than once".into());
                    }
                    accept_windows_license = true;
                }
                "--allow-unsupported-requirements" => allow_unsupported_requirements = true,
                unknown => return Err(format!("unknown create option {unknown:?}").into()),
            }
            index += 1;
        }

        Ok(Self {
            name,
            iso: iso.ok_or("--iso PATH is required")?,
            profile,
            cpus,
            memory_mib,
            disk_gib,
            network,
            port_forwards,
            accept_windows_license,
            allow_unsupported_requirements,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PortForwardRequest {
    Fixed(PortForward),
    Dynamic(u16),
}

fn parse_publish(value: &str) -> Result<PortForwardRequest, Box<dyn std::error::Error>> {
    let (host, guest) = value
        .split_once(':')
        .ok_or("--publish requires HOST:GUEST or auto:GUEST")?;
    if host != "auto" && host != "0" {
        return Ok(PortForwardRequest::Fixed(value.parse()?));
    }
    let guest_port = guest
        .parse::<u16>()
        .map_err(|error| format!("invalid guest port in {value:?}: {error}"))?;
    if guest_port == 0 {
        return Err("published guest port must be between 1 and 65535".into());
    }
    Ok(PortForwardRequest::Dynamic(guest_port))
}

pub(super) fn resolve_port_forwards(
    requests: &[PortForwardRequest],
    instance_name: &str,
) -> Result<Vec<PortForward>, Box<dyn std::error::Error>> {
    let mut resolved = Vec::with_capacity(requests.len());
    let reserved_control_port = control_port_for_instance(instance_name)?;
    let fixed_host_ports = requests
        .iter()
        .filter_map(|request| match request {
            PortForwardRequest::Fixed(forward) => Some(forward.host_port),
            PortForwardRequest::Dynamic(_) => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    for request in requests {
        let guest_port = match request {
            PortForwardRequest::Fixed(forward) => {
                resolved.push(*forward);
                continue;
            }
            PortForwardRequest::Dynamic(guest_port) => guest_port,
        };
        let mut selected = None;
        for _ in 0..32 {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            let host_port = listener.local_addr()?.port();
            if host_port != reserved_control_port
                && !(AGENT_CONTROL_PORT_START..AGENT_CONTROL_PORT_END_EXCLUSIVE)
                    .contains(&host_port)
                && !fixed_host_ports.contains(&host_port)
                && resolved
                    .iter()
                    .all(|forward| forward.host_port != host_port)
            {
                selected = Some(PortForward::new(host_port, *guest_port)?);
                break;
            }
        }
        resolved.push(selected.ok_or("could not allocate a unique dynamic host port")?);
    }
    Ok(resolved)
}

pub(super) fn next_value<'a>(
    arguments: &'a [OsString],
    index: &mut usize,
    option: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    *index += 1;
    arguments
        .get(*index)
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{option} requires a value").into())
}

pub(super) fn parse_number<T>(
    arguments: &[OsString],
    index: &mut usize,
    option: &str,
) -> Result<T, Box<dyn std::error::Error>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = next_value(arguments, index, option)?;
    value
        .parse::<T>()
        .map_err(|error| format!("invalid value for {option}: {error}").into())
}
