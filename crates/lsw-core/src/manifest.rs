// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{LswError, Result, WindowsProfile};

const MANIFEST_VERSION: u32 = 7;
pub const DEFAULT_IDLE_TIMEOUT_SECONDS: u64 = 10 * 60;
pub const DEFAULT_HIBERNATE_TIMEOUT_SECONDS: u64 = 5 * 60;
pub const AGENT_CONTROL_PORT_START: u16 = 42_000;
pub const AGENT_CONTROL_PORT_END_EXCLUSIVE: u16 = 44_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkMode {
    Nat,
    Offline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdlePolicy {
    Off,
    PauseHibernate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowsUserRole {
    Standard,
    Administrator,
}

impl fmt::Display for WindowsUserRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Standard => "standard",
            Self::Administrator => "administrator",
        })
    }
}

impl FromStr for WindowsUserRole {
    type Err = LswError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "standard" => Ok(Self::Standard),
            "administrator" => Ok(Self::Administrator),
            _ => Err(LswError::InvalidValue {
                field: "Windows user role",
                reason: format!("unknown role {value:?}; expected standard or administrator"),
            }),
        }
    }
}

impl fmt::Display for IdlePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::PauseHibernate => "pause-hibernate",
        })
    }
}

impl FromStr for IdlePolicy {
    type Err = LswError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(Self::Off),
            "pause-hibernate" => Ok(Self::PauseHibernate),
            _ => Err(LswError::InvalidValue {
                field: "idle policy",
                reason: format!("unknown policy {value:?}; expected off or pause-hibernate"),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolderShareMode {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolderShareTransport {
    Mirror,
    LiveSmb,
}

impl fmt::Display for FolderShareTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Mirror => "mirror",
            Self::LiveSmb => "live-smb",
        })
    }
}

impl FromStr for FolderShareTransport {
    type Err = LswError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "mirror" => Ok(Self::Mirror),
            "live-smb" => Ok(Self::LiveSmb),
            _ => Err(LswError::InvalidValue {
                field: "folder share transport",
                reason: format!("unknown transport {value:?}; expected mirror or live-smb"),
            }),
        }
    }
}

impl fmt::Display for FolderShareMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        })
    }
}

impl FromStr for FolderShareMode {
    type Err = LswError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "ro" => Ok(Self::ReadOnly),
            "rw" => Ok(Self::ReadWrite),
            _ => Err(LswError::InvalidValue {
                field: "folder share mode",
                reason: format!("unknown mode {value:?}; expected ro or rw"),
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderShare {
    pub name: String,
    pub host_path: PathBuf,
    pub guest_path: String,
    pub mode: FolderShareMode,
    pub transport: FolderShareTransport,
}

impl FolderShare {
    pub fn validate(&self) -> Result<()> {
        validate_share_name(&self.name)?;
        validate_serializable_value(&self.host_path, "folder share host path")?;
        if !self.host_path.is_absolute() || self.host_path.parent().is_none() {
            return Err(LswError::InvalidValue {
                field: "folder share host path",
                reason: "must be an absolute directory below the filesystem root".to_owned(),
            });
        }
        if self.transport == FolderShareTransport::LiveSmb {
            if self.mode != FolderShareMode::ReadWrite || self.guest_path != "L:\\" {
                return Err(LswError::InvalidValue {
                    field: "live folder share",
                    reason: "must be read-write and mounted at L:\\".to_owned(),
                });
            }
        } else {
            validate_windows_absolute_path(&self.guest_path)?;
        }
        Ok(())
    }
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Nat => "nat",
            Self::Offline => "offline",
        })
    }
}

impl FromStr for NetworkMode {
    type Err = LswError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "nat" => Ok(Self::Nat),
            "offline" => Ok(Self::Offline),
            _ => Err(LswError::InvalidValue {
                field: "network mode",
                reason: format!("unknown mode {value:?}; expected nat or offline"),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortForward {
    pub host_port: u16,
    pub guest_port: u16,
}

impl PortForward {
    pub fn new(host_port: u16, guest_port: u16) -> Result<Self> {
        if host_port == 0 {
            return Err(LswError::InvalidValue {
                field: "published port",
                reason: "host port must be between 1 and 65535".to_owned(),
            });
        }
        if guest_port == 0 {
            return Err(LswError::InvalidValue {
                field: "published port",
                reason: "guest port must be between 1 and 65535".to_owned(),
            });
        }
        Ok(Self {
            host_port,
            guest_port,
        })
    }
}

impl fmt::Display for PortForward {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.host_port, self.guest_port)
    }
}

impl FromStr for PortForward {
    type Err = LswError;

    fn from_str(value: &str) -> Result<Self> {
        let (host, guest) = value
            .split_once(':')
            .ok_or_else(|| LswError::InvalidValue {
                field: "published port",
                reason: format!("{value:?} must use HOST_PORT:GUEST_PORT syntax"),
            })?;
        if host.is_empty() || guest.is_empty() || guest.contains(':') {
            return Err(LswError::InvalidValue {
                field: "published port",
                reason: format!("{value:?} must use HOST_PORT:GUEST_PORT syntax"),
            });
        }
        let host_port = parse_published_port(host, "host", value)?;
        let guest_port = parse_published_port(guest, "guest", value)?;
        Self::new(host_port, guest_port)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceSpec {
    pub name: String,
    pub source_iso: PathBuf,
    pub profile: WindowsProfile,
    pub cpus: u16,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub network: NetworkMode,
    pub port_forwards: Vec<PortForward>,
    pub license_accepted: bool,
    pub allow_unsupported_requirements: bool,
}

impl InstanceSpec {
    pub fn validate(&self) -> Result<()> {
        validate_instance_name(&self.name)?;
        validate_serializable_path(&self.source_iso)?;

        if self.network == NetworkMode::Offline && !self.port_forwards.is_empty() {
            return Err(LswError::InvalidValue {
                field: "published ports",
                reason: "port publishing requires --network nat".to_owned(),
            });
        }

        let mut host_ports = BTreeSet::new();
        for forward in &self.port_forwards {
            PortForward::new(forward.host_port, forward.guest_port)?;
            if (AGENT_CONTROL_PORT_START..AGENT_CONTROL_PORT_END_EXCLUSIVE)
                .contains(&forward.host_port)
            {
                return Err(LswError::InvalidValue {
                    field: "published ports",
                    reason: format!(
                        "host port {} is reserved for LSW agent control",
                        forward.host_port
                    ),
                });
            }
            if !host_ports.insert(forward.host_port) {
                return Err(LswError::InvalidValue {
                    field: "published ports",
                    reason: format!("host port {} appears more than once", forward.host_port),
                });
            }
        }

        if !(1..=256).contains(&self.cpus) {
            return Err(LswError::InvalidValue {
                field: "CPU count",
                reason: "must be between 1 and 256".to_owned(),
            });
        }

        if !self.allow_unsupported_requirements {
            if self.cpus < 2 {
                return Err(LswError::InvalidValue {
                    field: "CPU count",
                    reason: "Windows 11 profiles require at least 2 virtual CPUs; pass the explicit unsupported flag to override".to_owned(),
                });
            }
            if self.memory_mib < 4096 {
                return Err(LswError::InvalidValue {
                    field: "memory",
                    reason: "Windows 11 profiles require at least 4096 MiB; pass the explicit unsupported flag to override".to_owned(),
                });
            }
            if self.disk_gib < 64 {
                return Err(LswError::InvalidValue {
                    field: "disk size",
                    reason: "Windows 11 profiles require at least 64 GiB; pass the explicit unsupported flag to override".to_owned(),
                });
            }
        } else {
            if self.memory_mib < 512 {
                return Err(LswError::InvalidValue {
                    field: "memory",
                    reason: "must be at least 512 MiB even in unsupported mode".to_owned(),
                });
            }
            if self.disk_gib < 8 {
                return Err(LswError::InvalidValue {
                    field: "disk size",
                    reason: "must be at least 8 GiB even in unsupported mode".to_owned(),
                });
            }
        }

        if !self.license_accepted {
            return Err(LswError::InvalidValue {
                field: "license acceptance",
                reason: "the user must explicitly accept the license for their installation media"
                    .to_owned(),
            });
        }

        Ok(())
    }

    pub fn validate_for_create(&self) -> Result<()> {
        self.validate()?;
        if !self.source_iso.is_file() {
            return Err(LswError::InvalidValue {
                field: "source ISO",
                reason: format!("{} is not a regular file", self.source_iso.display()),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstanceState {
    Configured,
    Installing,
    Stopped,
    Running,
    Suspended,
    Hibernated,
    Failed,
}

impl fmt::Display for InstanceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Configured => "configured",
            Self::Installing => "installing",
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Hibernated => "hibernated",
            Self::Failed => "failed",
        };
        formatter.write_str(value)
    }
}

impl FromStr for InstanceState {
    type Err = LswError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "configured" => Ok(Self::Configured),
            "installing" => Ok(Self::Installing),
            "stopped" => Ok(Self::Stopped),
            "running" => Ok(Self::Running),
            "suspended" => Ok(Self::Suspended),
            "hibernated" => Ok(Self::Hibernated),
            "failed" => Ok(Self::Failed),
            _ => Err(LswError::InvalidManifest(format!(
                "unknown instance state {value:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceManifest {
    pub version: u32,
    pub spec: InstanceSpec,
    pub state: InstanceState,
    pub control_port: u16,
    pub created_unix_seconds: u64,
    pub idle_timeout_seconds: u64,
    pub hibernate_timeout_seconds: u64,
    pub idle_policy: IdlePolicy,
    pub memory_min_mib: u32,
    pub state_changed_unix_seconds: u64,
    pub base_image_key: Option<String>,
    pub default_user: Option<String>,
    pub default_user_role: Option<WindowsUserRole>,
    pub folder_shares: Vec<FolderShare>,
}

impl InstanceManifest {
    pub fn new(spec: InstanceSpec) -> Result<Self> {
        spec.validate_for_create()?;
        let memory_min_mib = spec.memory_mib.min(2048);
        let created_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| LswError::InvalidValue {
                field: "system clock",
                reason: error.to_string(),
            })?
            .as_secs();

        Ok(Self {
            version: MANIFEST_VERSION,
            control_port: control_port_for_instance(&spec.name)?,
            spec,
            state: InstanceState::Configured,
            created_unix_seconds,
            idle_timeout_seconds: DEFAULT_IDLE_TIMEOUT_SECONDS,
            hibernate_timeout_seconds: DEFAULT_HIBERNATE_TIMEOUT_SECONDS,
            idle_policy: IdlePolicy::Off,
            memory_min_mib,
            state_changed_unix_seconds: created_unix_seconds,
            base_image_key: None,
            default_user: None,
            default_user_role: None,
            folder_shares: Vec::new(),
        })
    }

    pub fn encode(&self) -> Result<String> {
        self.spec.validate()?;
        validate_idle_timeout(self.idle_timeout_seconds)?;
        validate_idle_timeout(self.hibernate_timeout_seconds)?;
        validate_runtime_fields(self)?;
        let source_iso = self
            .spec
            .source_iso
            .to_str()
            .ok_or_else(|| LswError::InvalidValue {
                field: "source ISO",
                reason: "path is not valid UTF-8 and cannot be stored portably".to_owned(),
            })?;
        let port_forwards = self
            .spec
            .port_forwards
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let base_image_key = self.base_image_key.as_deref().unwrap_or("");
        let default_user = self.default_user.as_deref().unwrap_or("");
        let default_user_role = self
            .default_user_role
            .map_or(String::new(), |role| role.to_string());
        let mut shares = String::new();
        for (index, share) in self.folder_shares.iter().enumerate() {
            shares.push_str(&format!(
                "share.{index}.name={}\nshare.{index}.host={}\nshare.{index}.guest={}\nshare.{index}.mode={}\nshare.{index}.transport={}\n",
                share.name,
                share.host_path.display(),
                share.guest_path,
                share.mode,
                share.transport
            ));
        }

        Ok(format!(
            concat!(
                "version={}\n",
                "name={}\n",
                "source_iso={}\n",
                "profile={}\n",
                "cpus={}\n",
                "memory_mib={}\n",
                "disk_gib={}\n",
                "network={}\n",
                "port_forwards={}\n",
                "license_accepted={}\n",
                "allow_unsupported_requirements={}\n",
                "state={}\n",
                "control_port={}\n",
                "created_unix_seconds={}\n",
                "idle_timeout_seconds={}\n",
                "hibernate_timeout_seconds={}\n",
                "idle_policy={}\n",
                "memory_min_mib={}\n",
                "state_changed_unix_seconds={}\n",
                "base_image_key={}\n",
                "default_user={}\n",
                "default_user_role={}\n",
                "share_count={}\n",
                "{}"
            ),
            MANIFEST_VERSION,
            self.spec.name,
            source_iso,
            self.spec.profile,
            self.spec.cpus,
            self.spec.memory_mib,
            self.spec.disk_gib,
            self.spec.network,
            port_forwards,
            self.spec.license_accepted,
            self.spec.allow_unsupported_requirements,
            self.state,
            self.control_port,
            self.created_unix_seconds,
            self.idle_timeout_seconds,
            self.hibernate_timeout_seconds,
            self.idle_policy,
            self.memory_min_mib,
            self.state_changed_unix_seconds,
            base_image_key,
            default_user,
            default_user_role,
            self.folder_shares.len(),
            shares,
        ))
    }

    pub fn decode(contents: &str) -> Result<Self> {
        let mut fields = BTreeMap::new();
        for (line_number, line) in contents.lines().enumerate() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| {
                LswError::InvalidManifest(format!("line {} does not contain '='", line_number + 1))
            })?;
            if fields.insert(key, value).is_some() {
                return Err(LswError::InvalidManifest(format!(
                    "field {key:?} appears more than once"
                )));
            }
        }

        let version = parse_field::<u32>(&fields, "version")?;
        if !(1..=MANIFEST_VERSION).contains(&version) {
            return Err(LswError::InvalidManifest(format!(
                "unsupported manifest version {version}"
            )));
        }

        let spec = InstanceSpec {
            name: required_field(&fields, "name")?.to_owned(),
            source_iso: PathBuf::from(required_field(&fields, "source_iso")?),
            profile: WindowsProfile::parse_manifest(required_field(&fields, "profile")?)?,
            cpus: parse_field(&fields, "cpus")?,
            memory_mib: parse_field(&fields, "memory_mib")?,
            disk_gib: parse_field(&fields, "disk_gib")?,
            network: if version >= 2 {
                required_field(&fields, "network")?.parse()?
            } else {
                NetworkMode::Offline
            },
            port_forwards: if version >= 3 {
                parse_port_forwards(required_field(&fields, "port_forwards")?)?
            } else {
                Vec::new()
            },
            license_accepted: parse_field(&fields, "license_accepted")?,
            allow_unsupported_requirements: parse_field(&fields, "allow_unsupported_requirements")?,
        };
        spec.validate()?;

        let control_port = parse_field(&fields, "control_port")?;
        let expected_control_port = control_port_for_instance(&spec.name)?;
        if control_port != expected_control_port {
            return Err(LswError::InvalidManifest(format!(
                "control port {control_port} does not match the deterministic port {expected_control_port} for {:?}",
                spec.name
            )));
        }

        let idle_timeout_seconds = if version >= 4 {
            parse_field(&fields, "idle_timeout_seconds")?
        } else {
            DEFAULT_IDLE_TIMEOUT_SECONDS
        };
        validate_idle_timeout(idle_timeout_seconds)?;

        let hibernate_timeout_seconds = if version >= 5 {
            parse_field(&fields, "hibernate_timeout_seconds")?
        } else {
            0
        };
        validate_idle_timeout(hibernate_timeout_seconds)?;
        let idle_policy = if version >= 5 {
            required_field(&fields, "idle_policy")?.parse()?
        } else {
            IdlePolicy::Off
        };
        let memory_min_mib = if version >= 5 {
            parse_field(&fields, "memory_min_mib")?
        } else {
            spec.memory_mib.min(2048)
        };
        let created_unix_seconds = parse_field(&fields, "created_unix_seconds")?;
        let state_changed_unix_seconds = if version >= 5 {
            parse_field(&fields, "state_changed_unix_seconds")?
        } else {
            created_unix_seconds
        };
        let base_image_key = if version >= 5 {
            optional_nonempty_field(&fields, "base_image_key").map(str::to_owned)
        } else {
            None
        };
        let default_user = if version >= 5 {
            optional_nonempty_field(&fields, "default_user").map(str::to_owned)
        } else {
            None
        };
        let default_user_role = if version >= 6 {
            optional_nonempty_field(&fields, "default_user_role")
                .map(str::parse)
                .transpose()?
        } else {
            default_user.as_ref().map(|_| WindowsUserRole::Standard)
        };
        let folder_shares = if version >= 5 {
            parse_folder_shares(&fields, version)?
        } else {
            Vec::new()
        };

        let manifest = Self {
            version: MANIFEST_VERSION,
            spec,
            state: required_field(&fields, "state")?.parse()?,
            control_port,
            created_unix_seconds,
            idle_timeout_seconds,
            hibernate_timeout_seconds,
            idle_policy,
            memory_min_mib,
            state_changed_unix_seconds,
            base_image_key,
            default_user,
            default_user_role,
            folder_shares,
        };
        validate_runtime_fields(&manifest)?;
        Ok(manifest)
    }
}

fn validate_runtime_fields(manifest: &InstanceManifest) -> Result<()> {
    if !(256..=manifest.spec.memory_mib).contains(&manifest.memory_min_mib) {
        return Err(LswError::InvalidValue {
            field: "minimum memory",
            reason: format!(
                "must be between 256 MiB and memory.max ({} MiB)",
                manifest.spec.memory_mib
            ),
        });
    }
    if let Some(key) = &manifest.base_image_key {
        if key.len() != 64
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(LswError::InvalidValue {
                field: "base image key",
                reason: "must contain 64 lowercase hexadecimal characters".to_owned(),
            });
        }
    }
    if let Some(user) = &manifest.default_user {
        validate_windows_user_name(user)?;
    }
    if manifest.default_user.is_some() != manifest.default_user_role.is_some() {
        return Err(LswError::InvalidValue {
            field: "default Windows user role",
            reason: "must be present exactly when a default Windows user is registered".to_owned(),
        });
    }
    let mut names = BTreeSet::new();
    let mut live_share_seen = false;
    for (index, share) in manifest.folder_shares.iter().enumerate() {
        share.validate()?;
        if share.transport == FolderShareTransport::LiveSmb
            && std::mem::replace(&mut live_share_seen, true)
        {
            return Err(LswError::InvalidValue {
                field: "live folder shares",
                reason: "only one private QEMU SMB root can be mounted per instance".to_owned(),
            });
        }
        if !names.insert(&share.name) {
            return Err(LswError::InvalidValue {
                field: "folder shares",
                reason: format!("share name {:?} appears more than once", share.name),
            });
        }
        for existing in &manifest.folder_shares[..index] {
            if share.host_path.starts_with(&existing.host_path)
                || existing.host_path.starts_with(&share.host_path)
                || windows_roots_overlap(&share.guest_path, &existing.guest_path)
            {
                return Err(LswError::InvalidValue {
                    field: "folder shares",
                    reason: format!(
                        "share roots {:?} and {:?} overlap",
                        existing.name, share.name
                    ),
                });
            }
        }
    }
    Ok(())
}

fn parse_folder_shares(
    fields: &BTreeMap<&str, &str>,
    manifest_version: u32,
) -> Result<Vec<FolderShare>> {
    let count = parse_field::<usize>(fields, "share_count")?;
    if count > 64 {
        return Err(LswError::InvalidManifest(
            "more than 64 folder shares".to_owned(),
        ));
    }
    (0..count)
        .map(|index| {
            let share = FolderShare {
                name: required_field(fields, &format!("share.{index}.name"))?.to_owned(),
                host_path: PathBuf::from(required_field(fields, &format!("share.{index}.host"))?),
                guest_path: required_field(fields, &format!("share.{index}.guest"))?.to_owned(),
                mode: required_field(fields, &format!("share.{index}.mode"))?.parse()?,
                transport: if manifest_version >= 7 {
                    required_field(fields, &format!("share.{index}.transport"))?.parse()?
                } else {
                    FolderShareTransport::Mirror
                },
            };
            share.validate()?;
            Ok(share)
        })
        .collect()
}

fn optional_nonempty_field<'a>(fields: &'a BTreeMap<&str, &str>, key: &str) -> Option<&'a str> {
    fields.get(key).copied().filter(|value| !value.is_empty())
}

fn validate_idle_timeout(seconds: u64) -> Result<()> {
    if seconds <= 31_536_000 {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "idle timeout",
            reason: "must be 0 (disabled) or no more than 365 days".to_owned(),
        })
    }
}

fn required_field<'a>(fields: &'a BTreeMap<&str, &str>, key: &str) -> Result<&'a str> {
    fields
        .get(key)
        .copied()
        .ok_or_else(|| LswError::InvalidManifest(format!("missing field {key:?}")))
}

fn parse_field<T>(fields: &BTreeMap<&str, &str>, key: &str) -> Result<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    let value = required_field(fields, key)?;
    value.parse::<T>().map_err(|error| {
        LswError::InvalidManifest(format!("could not parse {key:?} value {value:?}: {error}"))
    })
}

fn parse_published_port(value: &str, side: &str, pair: &str) -> Result<u16> {
    value.parse().map_err(|error| LswError::InvalidValue {
        field: "published port",
        reason: format!("invalid {side} port in {pair:?}: {error}"),
    })
}

fn parse_port_forwards(value: &str) -> Result<Vec<PortForward>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value.split(',').map(str::parse).collect()
}

pub(crate) fn validate_instance_name(name: &str) -> Result<()> {
    let valid_length = (1..=63).contains(&name.len());
    let valid_characters = name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    let valid_edges = name
        .as_bytes()
        .first()
        .zip(name.as_bytes().last())
        .map(|(first, last)| first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric())
        .unwrap_or(false);

    if valid_length && valid_characters && valid_edges {
        Ok(())
    } else {
        Err(LswError::InvalidInstanceName(name.to_owned()))
    }
}

fn validate_serializable_path(path: &std::path::Path) -> Result<()> {
    validate_serializable_value(path, "source ISO")
}

fn validate_serializable_value(path: &std::path::Path, field: &'static str) -> Result<()> {
    let path = path.to_str().ok_or_else(|| LswError::InvalidValue {
        field,
        reason: "path is not valid UTF-8".to_owned(),
    })?;
    if path.contains('\n') || path.contains('\r') {
        return Err(LswError::InvalidValue {
            field,
            reason: "path cannot contain a newline".to_owned(),
        });
    }
    Ok(())
}

fn validate_share_name(name: &str) -> Result<()> {
    let valid_length = (1..=32).contains(&name.len());
    let valid_characters = name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    let valid_edges = name
        .as_bytes()
        .first()
        .zip(name.as_bytes().last())
        .is_some_and(|(first, last)| first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric());
    if valid_length && valid_characters && valid_edges {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "folder share name",
            reason: "must use 1-32 lowercase letters, digits, or interior hyphens".to_owned(),
        })
    }
}

fn validate_windows_absolute_path(path: &str) -> Result<()> {
    let bytes = path.as_bytes();
    let drive_root = bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let safe_components = drive_root
        && path[3..]
            .split(['\\', '/'])
            .all(windows_share_component_is_safe);
    if drive_root && safe_components {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "folder share guest path",
            reason: "must be an absolute Windows drive path without parent traversal".to_owned(),
        })
    }
}

fn windows_share_component_is_safe(component: &str) -> bool {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with([' ', '.'])
        || component
            .chars()
            .any(|character| character <= '\u{1f}' || "<>:\"/\\|?*".contains(character))
    {
        return false;
    }
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !matches!(
            stem.as_str(),
            "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
}

fn windows_roots_overlap(left: &str, right: &str) -> bool {
    let left = left.replace('/', "\\").to_ascii_lowercase();
    let right = right.replace('/', "\\").to_ascii_lowercase();
    left == right
        || left
            .strip_prefix(&right)
            .is_some_and(|suffix| suffix.starts_with('\\'))
        || right
            .strip_prefix(&left)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

pub fn validate_windows_user_name(name: &str) -> Result<()> {
    const FORBIDDEN: [char; 16] = [
        '"', '/', '\\', '[', ']', ':', ';', '|', '=', ',', '+', '*', '?', '<', '>', '@',
    ];
    let valid = !name.is_empty()
        && name.encode_utf16().count() <= 20
        && name != "."
        && name != ".."
        && !name.ends_with([' ', '.'])
        && !name
            .chars()
            .any(|character| character.is_control() || FORBIDDEN.contains(&character));
    if valid {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "Windows user name",
            reason: "must be 1-20 UTF-16 code units and contain no Windows account-name separators or trailing space/dot".to_owned(),
        })
    }
}

fn stable_control_port(name: &str) -> u16 {
    let hash = name.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    AGENT_CONTROL_PORT_START
        + (hash % u32::from(AGENT_CONTROL_PORT_END_EXCLUSIVE - AGENT_CONTROL_PORT_START)) as u16
}

pub fn control_port_for_instance(name: &str) -> Result<u16> {
    validate_instance_name(name)?;
    Ok(stable_control_port(name))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static ISO_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn temporary_iso() -> PathBuf {
        // Wall-clock resolution can be coarse under emulation, while the test
        // harness may create several ISO fixtures concurrently.
        let fixture_id = ISO_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "lsw-manifest-test-{}-{}-{fixture_id}.iso",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock must be valid")
                .as_nanos()
        ));
        fs::write(&path, b"test media").expect("temporary ISO should be writable");
        path
    }

    fn remove_v5_fields(encoded: String, version: u32) -> String {
        let mut legacy = encoded
            .lines()
            .filter(|line| {
                ![
                    "hibernate_timeout_seconds=",
                    "idle_policy=",
                    "memory_min_mib=",
                    "state_changed_unix_seconds=",
                    "base_image_key=",
                    "default_user=",
                    "default_user_role=",
                    "share_count=",
                    "share.",
                ]
                .iter()
                .any(|prefix| line.starts_with(prefix))
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        legacy[0] = format!("version={version}");
        format!("{}\n", legacy.join("\n"))
    }

    #[test]
    fn manifest_round_trip_is_stable() {
        let iso = temporary_iso();
        let mut manifest = InstanceManifest::new(InstanceSpec {
            name: "win-dev".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Slim,
            cpus: 4,
            memory_mib: 8192,
            disk_gib: 96,
            network: NetworkMode::Nat,
            port_forwards: vec![PortForward::new(8080, 80).expect("ports should be valid")],
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        manifest.hibernate_timeout_seconds = 240;
        manifest.idle_policy = IdlePolicy::PauseHibernate;
        manifest.memory_min_mib = 1024;
        manifest.base_image_key = Some("a".repeat(64));
        manifest.default_user = Some("desktop-user".to_owned());
        manifest.default_user_role = Some(WindowsUserRole::Administrator);
        manifest.folder_shares.push(FolderShare {
            name: "source".to_owned(),
            host_path: PathBuf::from("/srv/source"),
            guest_path: "C:\\Users\\desktop-user\\source".to_owned(),
            mode: FolderShareMode::ReadWrite,
            transport: FolderShareTransport::Mirror,
        });

        let encoded = manifest.encode().expect("manifest should encode");
        let decoded = InstanceManifest::decode(&encoded).expect("manifest should decode");
        assert_eq!(manifest, decoded);

        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn instance_name_cannot_escape_state_directory() {
        assert!(validate_instance_name("../escape").is_err());
        assert!(validate_instance_name("Uppercase").is_err());
        assert!(validate_instance_name("win-dev").is_ok());
    }

    #[test]
    fn folder_share_roots_are_absolute_safe_and_non_overlapping() {
        let (host_source, host_other, host_nested, host_filesystem_root) = if cfg!(windows) {
            (
                "C:\\srv\\source",
                "C:\\srv\\other",
                "C:\\srv\\source\\nested",
                "C:\\",
            )
        } else {
            ("/srv/source", "/srv/other", "/srv/source/nested", "/")
        };
        let valid = FolderShare {
            name: "source".to_owned(),
            host_path: PathBuf::from(host_source),
            guest_path: "C:\\src".to_owned(),
            mode: FolderShareMode::ReadWrite,
            transport: FolderShareTransport::Mirror,
        };
        valid.validate().expect("ordinary roots should be valid");
        for invalid_guest in ["C:\\", "C:\\src\\", "C:\\src:stream", "C:\\CON"] {
            let mut invalid = valid.clone();
            invalid.guest_path = invalid_guest.to_owned();
            assert!(invalid.validate().is_err(), "accepted {invalid_guest:?}");
        }
        let mut host_root = valid.clone();
        host_root.host_path = PathBuf::from(host_filesystem_root);
        assert!(host_root.validate().is_err());

        let iso = temporary_iso();
        let mut manifest = InstanceManifest::new(InstanceSpec {
            name: "share-overlap".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("fixture manifest should be valid");
        manifest.folder_shares.push(valid.clone());
        manifest.folder_shares.push(FolderShare {
            name: "nested".to_owned(),
            host_path: PathBuf::from(host_other),
            guest_path: "c:/SRC/nested".to_owned(),
            mode: FolderShareMode::ReadOnly,
            transport: FolderShareTransport::Mirror,
        });
        assert!(validate_runtime_fields(&manifest).is_err());
        manifest.folder_shares[1].guest_path = "D:\\other".to_owned();
        manifest.folder_shares[1].host_path = PathBuf::from(host_nested);
        assert!(validate_runtime_fields(&manifest).is_err());
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn live_folder_share_has_one_fixed_driverless_mount_boundary() {
        let mut live = FolderShare {
            name: "linux".to_owned(),
            host_path: PathBuf::from(if cfg!(windows) {
                "C:\\Users\\developer\\LSW"
            } else {
                "/home/developer/LSW"
            }),
            guest_path: "L:\\".to_owned(),
            mode: FolderShareMode::ReadWrite,
            transport: FolderShareTransport::LiveSmb,
        };
        live.validate().expect("fixed live mount should validate");
        live.mode = FolderShareMode::ReadOnly;
        assert!(live.validate().is_err());
        live.mode = FolderShareMode::ReadWrite;
        live.guest_path = "M:\\".to_owned();
        assert!(live.validate().is_err());
    }

    #[test]
    fn official_requirements_need_explicit_override() {
        let iso = temporary_iso();
        let spec = InstanceSpec {
            name: "tiny-test".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Ephemeral,
            cpus: 2,
            memory_mib: 2048,
            disk_gib: 32,
            network: NetworkMode::Offline,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        };
        assert!(spec.validate().is_err());

        let unsupported = InstanceSpec {
            allow_unsupported_requirements: true,
            ..spec
        };
        assert!(unsupported.validate().is_ok());
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn version_one_manifests_migrate_to_offline_networking() {
        let iso = temporary_iso();
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "old-instance".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        let legacy = remove_v5_fields(manifest.encode().expect("manifest should encode"), 1)
            .replace("profile=vanilla\n", "profile=standard\n")
            .replace("network=nat\n", "")
            .replace("port_forwards=\n", "")
            .replace("idle_timeout_seconds=600\n", "");
        let migrated = InstanceManifest::decode(&legacy).expect("v1 manifest should migrate");
        assert_eq!(migrated.version, MANIFEST_VERSION);
        assert_eq!(migrated.spec.profile, WindowsProfile::Vanilla);
        assert_eq!(migrated.spec.network, NetworkMode::Offline);
        assert!(migrated.spec.port_forwards.is_empty());
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn version_two_manifests_migrate_without_published_ports() {
        let iso = temporary_iso();
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "version-two".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        let legacy = remove_v5_fields(manifest.encode().expect("manifest should encode"), 2)
            .replace("port_forwards=\n", "")
            .replace("idle_timeout_seconds=600\n", "");
        let migrated = InstanceManifest::decode(&legacy).expect("v2 manifest should migrate");
        assert_eq!(migrated.spec.network, NetworkMode::Nat);
        assert!(migrated.spec.port_forwards.is_empty());
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn version_three_manifests_receive_the_default_idle_timeout() {
        let iso = temporary_iso();
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "version-three".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        let legacy = remove_v5_fields(manifest.encode().expect("manifest should encode"), 3)
            .replace("idle_timeout_seconds=600\n", "");
        let migrated = InstanceManifest::decode(&legacy).expect("v3 manifest should migrate");
        assert_eq!(migrated.idle_timeout_seconds, DEFAULT_IDLE_TIMEOUT_SECONDS);
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn version_four_manifests_keep_beta7_features_opted_out() {
        let iso = temporary_iso();
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "version-four".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        let legacy = remove_v5_fields(manifest.encode().expect("manifest should encode"), 4);
        let migrated = InstanceManifest::decode(&legacy).expect("v4 manifest should migrate");
        assert_eq!(migrated.idle_policy, IdlePolicy::Off);
        assert_eq!(migrated.memory_min_mib, 2048);
        assert!(migrated.base_image_key.is_none());
        assert!(migrated.default_user.is_none());
        assert!(migrated.folder_shares.is_empty());
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn version_five_desktop_users_migrate_without_privilege_escalation() {
        let iso = temporary_iso();
        let mut manifest = InstanceManifest::new(InstanceSpec {
            name: "version-five".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Slim,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("fixture manifest should be valid");
        manifest.default_user = Some("desktop-user".to_owned());
        manifest.default_user_role = Some(WindowsUserRole::Standard);
        let legacy = manifest
            .encode()
            .expect("manifest should encode")
            .lines()
            .filter(|line| !line.starts_with("default_user_role=") && !line.contains(".transport="))
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .join("\n")
            .replacen("version=7", "version=5", 1);

        let migrated = InstanceManifest::decode(&legacy).expect("v5 manifest should migrate");
        assert_eq!(migrated.default_user.as_deref(), Some("desktop-user"));
        assert_eq!(migrated.default_user_role, Some(WindowsUserRole::Standard));
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn version_six_folder_shares_migrate_to_the_mirror_transport() {
        let iso = temporary_iso();
        let mut manifest = InstanceManifest::new(InstanceSpec {
            name: "version-six".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Slim,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("fixture manifest should be valid");
        manifest.folder_shares.push(FolderShare {
            name: "source".to_owned(),
            host_path: PathBuf::from(if cfg!(windows) {
                "C:\\srv\\source"
            } else {
                "/srv/source"
            }),
            guest_path: "C:\\source".to_owned(),
            mode: FolderShareMode::ReadWrite,
            transport: FolderShareTransport::Mirror,
        });
        let legacy = manifest
            .encode()
            .expect("manifest should encode")
            .lines()
            .filter(|line| !line.contains(".transport="))
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .join("\n")
            .replacen("version=7", "version=6", 1);
        let migrated = InstanceManifest::decode(&legacy).expect("v6 manifest should migrate");
        assert_eq!(
            migrated.folder_shares[0].transport,
            FolderShareTransport::Mirror
        );
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn published_ports_are_validated_before_launch() {
        let iso = temporary_iso();
        let base = InstanceSpec {
            name: "port-validation".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        };

        let duplicate = InstanceSpec {
            port_forwards: vec![
                PortForward::new(8080, 80).expect("ports should be valid"),
                PortForward::new(8080, 443).expect("ports should be valid"),
            ],
            ..base.clone()
        };
        assert!(duplicate.validate().is_err());

        let agent_collision = InstanceSpec {
            port_forwards: vec![PortForward::new(stable_control_port(&base.name), 80)
                .expect("ports should be valid")],
            ..base.clone()
        };
        assert!(agent_collision.validate().is_err());

        let offline = InstanceSpec {
            network: NetworkMode::Offline,
            port_forwards: vec![PortForward::new(8080, 80).expect("ports should be valid")],
            ..base
        };
        assert!(offline.validate().is_err());
        assert!("0:80".parse::<PortForward>().is_err());
        assert!("8080:0".parse::<PortForward>().is_err());
        assert!("8080".parse::<PortForward>().is_err());
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn manifest_cannot_redirect_agent_credentials_to_another_port() {
        let iso = temporary_iso();
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "port-guard".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        let encoded = manifest.encode().expect("manifest should encode");
        let tampered = encoded.replace(
            &format!("control_port={}\n", manifest.control_port),
            "control_port=22\n",
        );
        assert!(InstanceManifest::decode(&tampered).is_err());
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }
}
