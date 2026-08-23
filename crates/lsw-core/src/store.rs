// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::fmt::Write as FmtWrite;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::manifest::validate_instance_name;
use crate::{InstanceManifest, LswError, Result, AGENT_TOKEN_FILE, MANIFEST_FILE};

const DEFAULT_INSTANCE_FILE: &str = "default-instance";

#[derive(Clone, Debug)]
pub struct StateStore {
    root: PathBuf,
}

impl StateStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn initialize(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        ensure_private_directory(&self.root)?;
        let instances = self.instances_root();
        fs::create_dir_all(&instances)?;
        ensure_private_directory(&instances)?;
        Ok(())
    }

    pub fn create(&self, manifest: &InstanceManifest) -> Result<PathBuf> {
        let encoded = manifest.encode()?;
        let instance_dir = self.instance_dir(&manifest.spec.name)?;
        if instance_dir.exists() {
            return Err(LswError::InstanceAlreadyExists(instance_dir));
        }

        self.initialize()?;
        self.ensure_host_ports_available(manifest, None)?;
        let _port_reservations = reserve_host_ports(manifest)?;
        match fs::create_dir(&instance_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(LswError::InstanceAlreadyExists(instance_dir))
            }
            Err(error) => return Err(error.into()),
        }
        set_private_directory_permissions(&instance_dir)?;
        let manifest_path = instance_dir.join(MANIFEST_FILE);
        if let Err(error) = write_atomic(&manifest_path, encoded.as_bytes()) {
            let _ = fs::remove_dir(&instance_dir);
            return Err(error);
        }
        let token_path = instance_dir.join(AGENT_TOKEN_FILE);
        let token_result = generate_agent_token()
            .and_then(|token| write_new_private(&token_path, token.as_bytes()));
        if let Err(error) = token_result {
            let _ = fs::remove_file(&manifest_path);
            let _ = fs::remove_dir(&instance_dir);
            return Err(error);
        }
        Ok(instance_dir)
    }

    pub fn update(&self, manifest: &InstanceManifest) -> Result<()> {
        let encoded = manifest.encode()?;
        let instance_dir = self.instance_dir(&manifest.spec.name)?;
        if !is_real_directory(&instance_dir)? {
            return Err(LswError::InstanceNotFound(manifest.spec.name.clone()));
        }
        self.ensure_host_ports_available(manifest, Some(&manifest.spec.name))?;
        write_atomic(&instance_dir.join(MANIFEST_FILE), encoded.as_bytes())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let instance_dir = self.instance_dir(name)?;
        if !is_real_directory(&instance_dir)? {
            return Err(LswError::InstanceNotFound(name.to_owned()));
        }
        let manifest = self.load(name)?;
        if matches!(
            manifest.state,
            crate::InstanceState::Installing
                | crate::InstanceState::Running
                | crate::InstanceState::Suspended
        ) {
            return Err(LswError::InvalidValue {
                field: "instance state",
                reason: format!(
                    "instance {name:?} is {}; shut it down before removing it",
                    manifest.state
                ),
            });
        }

        fs::remove_dir_all(&instance_dir)?;
        let default_path = self.root.join(DEFAULT_INSTANCE_FILE);
        if is_regular_file(&default_path).unwrap_or(false)
            && fs::read_to_string(&default_path)
                .map(|value| value.trim() == name)
                .unwrap_or(false)
        {
            fs::remove_file(default_path)?;
        }
        Ok(())
    }

    pub fn load(&self, name: &str) -> Result<InstanceManifest> {
        let instance_dir = self.instance_dir(name)?;
        if !is_real_directory(&instance_dir)? {
            return Err(LswError::InstanceNotFound(name.to_owned()));
        }
        let path = instance_dir.join(MANIFEST_FILE);
        if !is_regular_file(&path)? {
            return Err(LswError::InstanceNotFound(name.to_owned()));
        }
        InstanceManifest::decode(&fs::read_to_string(path)?)
    }

    pub fn list(&self) -> Result<Vec<InstanceManifest>> {
        let root = self.instances_root();
        if !is_real_directory(&root)? {
            return match fs::symlink_metadata(&root) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Ok(_) => Err(LswError::InvalidValue {
                    field: "instances directory",
                    reason: format!("{} must be a real directory", root.display()),
                }),
                Err(error) => Err(error.into()),
            };
        }

        let mut manifests = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() {
                continue;
            }
            let manifest_path = entry.path().join(MANIFEST_FILE);
            if is_regular_file(&manifest_path)? {
                manifests.push(InstanceManifest::decode(&fs::read_to_string(
                    manifest_path,
                )?)?);
            }
        }
        manifests.sort_by(|left, right| left.spec.name.cmp(&right.spec.name));
        Ok(manifests)
    }

    pub fn instance_dir(&self, name: &str) -> Result<PathBuf> {
        validate_instance_name(name)?;
        Ok(self.instances_root().join(name))
    }

    pub fn agent_token_path(&self, name: &str) -> Result<PathBuf> {
        Ok(self.instance_dir(name)?.join(AGENT_TOKEN_FILE))
    }

    pub fn read_agent_token(&self, name: &str) -> Result<String> {
        let instance_dir = self.instance_dir(name)?;
        if !is_real_directory(&instance_dir)? {
            return Err(LswError::InstanceNotFound(name.to_owned()));
        }
        let path = instance_dir.join(AGENT_TOKEN_FILE);
        if !is_regular_file(&path)? {
            return Err(LswError::InvalidValue {
                field: "agent token",
                reason: format!("{} is missing or is not a regular file", path.display()),
            });
        }
        let token = fs::read_to_string(path)?.trim().to_owned();
        if token.len() != 64
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(LswError::InvalidValue {
                field: "agent token",
                reason: "must contain 64 lowercase hexadecimal characters".to_owned(),
            });
        }
        Ok(token)
    }

    pub fn set_default(&self, name: &str) -> Result<()> {
        validate_instance_name(name)?;
        self.load(name)?;
        self.initialize()?;
        write_atomic(
            &self.root.join(DEFAULT_INSTANCE_FILE),
            format!("{name}\n").as_bytes(),
        )
    }

    pub fn default_name(&self) -> Result<Option<String>> {
        let path = self.root.join(DEFAULT_INSTANCE_FILE);
        if !path.exists() {
            return Ok(None);
        }
        if !is_regular_file(&path)? {
            return Err(LswError::InvalidValue {
                field: "default instance",
                reason: format!("{} is not a regular file", path.display()),
            });
        }
        let name = fs::read_to_string(path)?.trim().to_owned();
        validate_instance_name(&name)?;
        self.load(&name)?;
        Ok(Some(name))
    }

    pub fn resolve_name(&self, requested: Option<&str>) -> Result<String> {
        if let Some(name) = requested {
            self.load(name)?;
            return Ok(name.to_owned());
        }
        if let Some(name) = self.default_name()? {
            return Ok(name);
        }
        let instances = self.list()?;
        match instances.as_slice() {
            [only] => Ok(only.spec.name.clone()),
            [] => Err(LswError::InvalidValue {
                field: "default instance",
                reason: "no instances exist; create one first".to_owned(),
            }),
            _ => Err(LswError::InvalidValue {
                field: "default instance",
                reason: "multiple instances exist; run `lsw use NAME`".to_owned(),
            }),
        }
    }

    fn instances_root(&self) -> PathBuf {
        self.root.join("instances")
    }

    fn ensure_host_ports_available(
        &self,
        manifest: &InstanceManifest,
        excluded_instance: Option<&str>,
    ) -> Result<()> {
        let requested_ports = std::iter::once(manifest.control_port)
            .chain(
                manifest
                    .spec
                    .port_forwards
                    .iter()
                    .map(|forward| forward.host_port),
            )
            .collect::<Vec<_>>();
        for existing in self.list()? {
            if excluded_instance == Some(existing.spec.name.as_str()) {
                continue;
            }
            let conflicting_port = requested_ports.iter().copied().find(|requested_port| {
                existing.control_port == *requested_port
                    || existing
                        .spec
                        .port_forwards
                        .iter()
                        .any(|forward| forward.host_port == *requested_port)
            });
            if let Some(port) = conflicting_port {
                return Err(LswError::InvalidValue {
                    field: "host port",
                    reason: format!(
                        "instance {:?} already reserves {port}; choose another instance name or published port",
                        existing.spec.name
                    ),
                });
            }
        }
        Ok(())
    }
}

fn reserve_host_ports(manifest: &InstanceManifest) -> Result<Vec<TcpListener>> {
    std::iter::once(manifest.control_port)
        .chain(
            manifest
                .spec
                .port_forwards
                .iter()
                .map(|forward| forward.host_port),
        )
        .map(|port| {
            TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|error| LswError::InvalidValue {
                field: "host port",
                reason: format!("127.0.0.1:{port} cannot be reserved: {error}"),
            })
        })
        .collect()
}

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    set_private_file_permissions(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn write_new_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    set_private_file_permissions(path)?;
    file.write_all(contents)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn generate_agent_token() -> Result<String> {
    let mut random = fs::File::open("/dev/urandom")?;
    let mut bytes = [0_u8; 32];
    random.read_exact(&mut bytes)?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        write!(token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(token)
}

#[cfg(not(unix))]
pub(crate) fn generate_agent_token() -> Result<String> {
    Err(LswError::InvalidValue {
        field: "agent token",
        reason: "secure token generation is not implemented for this host backend".to_owned(),
    })
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if !is_real_directory(path)? {
        return Err(LswError::InvalidValue {
            field: "state directory",
            reason: format!("{} must be a real directory", path.display()),
        });
    }
    set_private_directory_permissions(path)
}

fn is_real_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn is_regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{InstanceSpec, NetworkMode, WindowsProfile};

    use super::*;

    fn available_published_listener() -> TcpListener {
        loop {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("port should bind");
            let port = listener.local_addr().expect("address should exist").port();
            if !(crate::AGENT_CONTROL_PORT_START..crate::AGENT_CONTROL_PORT_END_EXCLUSIVE)
                .contains(&port)
            {
                return listener;
            }
        }
    }

    fn available_published_port() -> u16 {
        available_published_listener()
            .local_addr()
            .expect("address should exist")
            .port()
    }

    #[test]
    fn store_rejects_traversal_names() {
        let store = StateStore::new(std::env::temp_dir().join("lsw-store-name-test"));
        assert!(store.instance_dir("../outside").is_err());
    }

    #[test]
    fn create_load_and_list_instance() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-store-test-{nonce}"));
        let iso = root.join("windows.iso");
        fs::create_dir_all(&root).expect("test root should be created");
        fs::write(&iso, b"test media").expect("test ISO should be created");

        let manifest = InstanceManifest::new(InstanceSpec {
            name: "win-dev".to_owned(),
            source_iso: iso,
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
        let store = StateStore::new(root.join("state"));
        store.create(&manifest).expect("instance should be stored");
        assert_eq!(
            store.load("win-dev").expect("instance should load"),
            manifest
        );
        let token = store
            .read_agent_token("win-dev")
            .expect("agent token should load");
        assert_eq!(token.len(), 64);
        assert_ne!(token, "0".repeat(64));
        assert_eq!(
            store.resolve_name(None).expect("single instance resolves"),
            "win-dev"
        );
        store
            .set_default("win-dev")
            .expect("default should be stored");
        assert_eq!(
            store.default_name().expect("default should load"),
            Some("win-dev".to_owned())
        );
        assert_eq!(store.list().expect("instances should list"), vec![manifest]);

        fs::remove_dir_all(root).expect("test root should be removable");
    }

    #[test]
    fn remove_deletes_a_stopped_instance_and_its_default_selection() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-store-remove-test-{nonce}"));
        let iso = root.join("windows.iso");
        fs::create_dir_all(&root).expect("test root should be created");
        fs::write(&iso, b"test media").expect("test ISO should be created");
        let mut manifest = InstanceManifest::new(InstanceSpec {
            name: "removable-instance".to_owned(),
            source_iso: iso,
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
        manifest.state = crate::InstanceState::Stopped;
        let store = StateStore::new(root.join("state"));
        let instance_dir = store.create(&manifest).expect("instance should be stored");
        fs::write(instance_dir.join("disk.qcow2"), b"disk").expect("disk should be written");
        store
            .set_default("removable-instance")
            .expect("default should be stored");
        store
            .remove("removable-instance")
            .expect("stopped instance should be removable");
        assert!(!instance_dir.exists());
        assert!(store.default_name().expect("default should load").is_none());
        fs::remove_dir_all(root).expect("test root should be removable");
    }

    #[test]
    fn create_rejects_host_ports_reserved_by_another_instance() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-store-port-test-{nonce}"));
        let iso = root.join("windows.iso");
        fs::create_dir_all(&root).expect("test root should be created");
        fs::write(&iso, b"test media").expect("test ISO should be created");

        let shared_port = available_published_port();
        let second = InstanceManifest::new(InstanceSpec {
            name: "win-second".to_owned(),
            source_iso: iso.clone(),
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: vec![
                crate::PortForward::new(shared_port, 8081).expect("published port should be valid")
            ],
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("second manifest should be valid");
        let first = InstanceManifest::new(InstanceSpec {
            name: "win-first".to_owned(),
            source_iso: iso,
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: vec![
                crate::PortForward::new(shared_port, 8080).expect("published port should be valid")
            ],
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("first manifest should be valid");

        let store = StateStore::new(root.join("state"));
        store
            .create(&first)
            .expect("first instance should be stored");
        assert!(store.create(&second).is_err());
        fs::remove_dir_all(root).expect("test root should be removable");
    }

    #[test]
    fn update_cannot_introduce_a_cross_instance_port_collision() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-store-update-port-test-{nonce}"));
        let iso = root.join("windows.iso");
        fs::create_dir_all(&root).expect("test root should be created");
        fs::write(&iso, b"test media").expect("test ISO should be created");

        let build_manifest = |name: &str| {
            InstanceManifest::new(InstanceSpec {
                name: name.to_owned(),
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
            .expect("manifest should be valid")
        };
        let mut first = build_manifest("port-owner-one");
        let mut second = build_manifest("port-owner-two");
        assert_ne!(first.control_port, second.control_port);
        let shared_port = available_published_port();
        first.spec.port_forwards = vec![
            crate::PortForward::new(shared_port, 8080).expect("published port should be valid")
        ];

        let store = StateStore::new(root.join("state"));
        store
            .create(&first)
            .expect("first instance should be stored");
        store
            .create(&second)
            .expect("second instance should be stored");
        second.spec.port_forwards = vec![
            crate::PortForward::new(shared_port, 8081).expect("published port should be valid")
        ];
        assert!(store.update(&second).is_err());
        assert!(store
            .load("port-owner-two")
            .expect("unchanged manifest should load")
            .spec
            .port_forwards
            .is_empty());

        fs::remove_dir_all(root).expect("test root should be removable");
    }

    #[test]
    fn create_rejects_a_host_port_used_outside_the_state_store() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-store-bound-port-test-{nonce}"));
        let iso = root.join("windows.iso");
        fs::create_dir_all(&root).expect("test root should be created");
        fs::write(&iso, b"test media").expect("test ISO should be created");
        let listener = available_published_listener();
        let port = listener.local_addr().expect("address should exist").port();
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "external-port-owner".to_owned(),
            source_iso: iso,
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: vec![
                crate::PortForward::new(port, 8080).expect("published port should be valid")
            ],
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");

        let store = StateStore::new(root.join("state"));
        assert!(store.create(&manifest).is_err());
        assert!(!store
            .instance_dir("external-port-owner")
            .expect("instance path should resolve")
            .exists());
        drop(listener);
        fs::remove_dir_all(root).expect("test root should be removable");
    }
}
