// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{LswError, Result, WindowsProfile};

const MANIFEST_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkMode {
    Nat,
    Offline,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceSpec {
    pub name: String,
    pub source_iso: PathBuf,
    pub profile: WindowsProfile,
    pub cpus: u16,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub network: NetworkMode,
    pub license_accepted: bool,
    pub allow_unsupported_requirements: bool,
}

impl InstanceSpec {
    pub fn validate(&self) -> Result<()> {
        validate_instance_name(&self.name)?;
        validate_serializable_path(&self.source_iso)?;

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
}

impl InstanceManifest {
    pub fn new(spec: InstanceSpec) -> Result<Self> {
        spec.validate_for_create()?;
        let created_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| LswError::InvalidValue {
                field: "system clock",
                reason: error.to_string(),
            })?
            .as_secs();

        Ok(Self {
            version: MANIFEST_VERSION,
            control_port: stable_control_port(&spec.name),
            spec,
            state: InstanceState::Configured,
            created_unix_seconds,
        })
    }

    pub fn encode(&self) -> Result<String> {
        self.spec.validate()?;
        let source_iso = self
            .spec
            .source_iso
            .to_str()
            .ok_or_else(|| LswError::InvalidValue {
                field: "source ISO",
                reason: "path is not valid UTF-8 and cannot be stored portably".to_owned(),
            })?;

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
                "license_accepted={}\n",
                "allow_unsupported_requirements={}\n",
                "state={}\n",
                "control_port={}\n",
                "created_unix_seconds={}\n"
            ),
            MANIFEST_VERSION,
            self.spec.name,
            source_iso,
            self.spec.profile,
            self.spec.cpus,
            self.spec.memory_mib,
            self.spec.disk_gib,
            self.spec.network,
            self.spec.license_accepted,
            self.spec.allow_unsupported_requirements,
            self.state,
            self.control_port,
            self.created_unix_seconds,
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
        if !matches!(version, 1 | MANIFEST_VERSION) {
            return Err(LswError::InvalidManifest(format!(
                "unsupported manifest version {version}"
            )));
        }

        let spec = InstanceSpec {
            name: required_field(&fields, "name")?.to_owned(),
            source_iso: PathBuf::from(required_field(&fields, "source_iso")?),
            profile: required_field(&fields, "profile")?.parse()?,
            cpus: parse_field(&fields, "cpus")?,
            memory_mib: parse_field(&fields, "memory_mib")?,
            disk_gib: parse_field(&fields, "disk_gib")?,
            network: if version >= 2 {
                required_field(&fields, "network")?.parse()?
            } else {
                NetworkMode::Offline
            },
            license_accepted: parse_field(&fields, "license_accepted")?,
            allow_unsupported_requirements: parse_field(&fields, "allow_unsupported_requirements")?,
        };
        spec.validate()?;

        let control_port = parse_field(&fields, "control_port")?;
        let expected_control_port = stable_control_port(&spec.name);
        if control_port != expected_control_port {
            return Err(LswError::InvalidManifest(format!(
                "control port {control_port} does not match the deterministic port {expected_control_port} for {:?}",
                spec.name
            )));
        }

        Ok(Self {
            version: MANIFEST_VERSION,
            spec,
            state: required_field(&fields, "state")?.parse()?,
            control_port,
            created_unix_seconds: parse_field(&fields, "created_unix_seconds")?,
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
    let path = path.to_str().ok_or_else(|| LswError::InvalidValue {
        field: "source ISO",
        reason: "path is not valid UTF-8".to_owned(),
    })?;
    if path.contains('\n') || path.contains('\r') {
        return Err(LswError::InvalidValue {
            field: "source ISO",
            reason: "path cannot contain a newline".to_owned(),
        });
    }
    Ok(())
}

fn stable_control_port(name: &str) -> u16 {
    let hash = name.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    42_000 + (hash % 2_000) as u16
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temporary_iso() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lsw-manifest-test-{}-{}.iso",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock must be valid")
                .as_nanos()
        ));
        fs::write(&path, b"test media").expect("temporary ISO should be writable");
        path
    }

    #[test]
    fn manifest_round_trip_is_stable() {
        let iso = temporary_iso();
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "win-dev".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Slim,
            cpus: 4,
            memory_mib: 8192,
            disk_gib: 96,
            network: NetworkMode::Nat,
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");

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
            profile: WindowsProfile::Standard,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        let legacy = manifest
            .encode()
            .expect("manifest should encode")
            .replace("version=2\n", "version=1\n")
            .replace("network=nat\n", "");
        let migrated = InstanceManifest::decode(&legacy).expect("v1 manifest should migrate");
        assert_eq!(migrated.version, MANIFEST_VERSION);
        assert_eq!(migrated.spec.network, NetworkMode::Offline);
        fs::remove_file(iso).expect("temporary ISO should be removable");
    }

    #[test]
    fn manifest_cannot_redirect_agent_credentials_to_another_port() {
        let iso = temporary_iso();
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "port-guard".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Standard,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
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
