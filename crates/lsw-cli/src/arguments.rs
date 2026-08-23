// SPDX-License-Identifier: GPL-3.0-or-later

//! Strict command-line parsing for instance creation and installation.
//!
//! Parsing is side-effect free. Filesystem canonicalization is limited to
//! explicit path values, while all state mutation remains in command modules.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use lsw_core::{InstallSeedOptions, NetworkMode, PortForward, WindowsProfile};

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
    pub(super) port_forwards: Vec<PortForward>,
    pub(super) allow_unsupported_requirements: bool,
    pub(super) seed: InstallSeedOptions,
    pub(super) without_agent: bool,
    pub(super) no_viewer: bool,
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
            allow_unsupported_requirements: false,
            seed: InstallSeedOptions::default(),
            without_agent: false,
            no_viewer: true,
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
                    parsed
                        .port_forwards
                        .push(next_value(arguments, &mut index, argument)?.parse()?);
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
                "--accept-license" => {}
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
    pub(super) port_forwards: Vec<PortForward>,
    pub(super) accept_license: bool,
    pub(super) allow_unsupported_requirements: bool,
}

impl CreateArguments {
    pub(super) fn parse(arguments: &[OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let name = arguments
            .first()
            .and_then(|value| value.to_str())
            .ok_or("usage: lsw create NAME --iso PATH --accept-license")?
            .to_owned();
        let mut iso = None;
        let mut profile = WindowsProfile::Slim;
        let mut cpus = 2;
        let mut memory_mib = 4096;
        let mut disk_gib = None;
        let mut network = NetworkMode::Nat;
        let mut port_forwards = Vec::new();
        let mut accept_license = false;
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
                    port_forwards.push(next_value(arguments, &mut index, option)?.parse()?);
                }
                "--accept-license" => accept_license = true,
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
            accept_license,
            allow_unsupported_requirements,
        })
    }
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
