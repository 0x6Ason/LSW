// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt::Write as FmtWrite;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::store::{generate_agent_token, write_atomic};
use crate::{
    sha256_file, HostCapabilities, InstanceManifest, InstanceSpec, InstanceState, LswError, Result,
    StateStore, CLONE_IDENTITY_MARKER_FILE, CLONE_IDENTITY_NAME_FILE, CLONE_IDENTITY_TOKEN_FILE,
};

const IMAGE_FORMAT_VERSION: u32 = 1;
const PREPARATION_IDENTITY: &str = "winpe-dism-v1/offline-setup-v1";
const IMAGE_METADATA_FILE: &str = "image.lsw";
const BASE_DISK_FILE: &str = "base.qcow2";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedImage {
    pub key: String,
    pub directory: PathBuf,
    pub disk: PathBuf,
    pub source_iso_sha256: String,
    pub profile: String,
    pub agent_sha256: String,
    pub firmware_sha256: String,
    pub source_disk_sha256: String,
    pub base_disk_sha256: String,
}

pub struct ImageManager<'a> {
    store: &'a StateStore,
    capabilities: &'a HostCapabilities,
}

impl<'a> ImageManager<'a> {
    pub fn new(store: &'a StateStore, capabilities: &'a HostCapabilities) -> Self {
        Self {
            store,
            capabilities,
        }
    }

    pub fn seal(&self, manifest: &InstanceManifest, agent_binary: &Path) -> Result<SealedImage> {
        if !matches!(
            manifest.state,
            InstanceState::Stopped | InstanceState::Hibernated
        ) {
            return Err(LswError::InvalidValue {
                field: "instance state",
                reason: format!(
                    "instance {:?} must be stopped or hibernated before sealing",
                    manifest.spec.name
                ),
            });
        }
        if manifest.default_user.is_some() {
            return Err(LswError::InvalidValue {
                field: "sealed image",
                reason: "remove the permanent desktop user or seal a pristine instance before user registration"
                    .to_owned(),
            });
        }
        let qemu_img = self
            .capabilities
            .qemu_img
            .as_ref()
            .ok_or_else(|| LswError::MissingCapabilities(vec!["qemu-img"]))?;
        let instance_dir = self.store.instance_dir(&manifest.spec.name)?;
        let source_disk = require_regular_file(&instance_dir.join("disk.qcow2"), "system disk")?;
        let firmware = self
            .capabilities
            .firmware_code(manifest.spec.profile)
            .ok_or_else(|| LswError::MissingCapabilities(vec!["OVMF code firmware"]))?;

        let source_iso_sha256 = sha256_file(&manifest.spec.source_iso)?;
        let agent_sha256 = sha256_file(agent_binary)?;
        let firmware_sha256 = sha256_file(firmware)?;
        let profile = manifest.spec.profile.to_string();
        let source_disk_sha256 = sha256_file(&source_disk)?;
        let key = image_key(
            &source_iso_sha256,
            &profile,
            &agent_sha256,
            &firmware_sha256,
            &source_disk_sha256,
        );

        let images_root = self.store.root().join("images");
        create_private_directory_all(&images_root)?;
        let directory = images_root.join(&key);
        let disk = directory.join(BASE_DISK_FILE);
        if directory.exists() {
            let existing = load_image(&directory, true)?;
            if existing.key == key {
                return Ok(existing);
            }
            return Err(LswError::InvalidValue {
                field: "sealed image",
                reason: format!("{} does not match its content key", directory.display()),
            });
        }

        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let temporary_disk = directory.join(format!(".{BASE_DISK_FILE}.{}", std::process::id()));
        let result = (|| -> Result<()> {
            let status = Command::new(qemu_img)
                .args(["convert", "-p", "-O", "qcow2"])
                .arg(&source_disk)
                .arg(&temporary_disk)
                .status()?;
            if !status.success() {
                return Err(LswError::ExternalCommandFailed {
                    program: qemu_img.clone(),
                    status: status.code(),
                });
            }
            let converted_sha256 = sha256_file(&temporary_disk)?;
            fs::set_permissions(&temporary_disk, fs::Permissions::from_mode(0o400))?;
            fs::rename(&temporary_disk, &disk)?;
            let image = SealedImage {
                key: key.clone(),
                directory: directory.clone(),
                disk: disk.clone(),
                source_iso_sha256,
                profile,
                agent_sha256,
                firmware_sha256,
                source_disk_sha256,
                base_disk_sha256: converted_sha256,
            };
            write_new_private(&directory.join(IMAGE_METADATA_FILE), image.encode())?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary_disk);
            let _ = fs::remove_file(&disk);
            let _ = fs::remove_file(directory.join(IMAGE_METADATA_FILE));
            let _ = fs::remove_dir(&directory);
            return Err(error);
        }
        load_image(&directory, false)
    }

    pub fn clone_instance(&self, source_name: &str, target_name: &str) -> Result<PathBuf> {
        let source = self.store.load(source_name)?;
        let key = source
            .base_image_key
            .as_deref()
            .ok_or_else(|| LswError::InvalidValue {
                field: "sealed image",
                reason: format!(
                    "instance {source_name:?} is not sealed; run `lsw image seal {source_name}`"
                ),
            })?;
        let image = load_image(&self.store.root().join("images").join(key), false)?;
        if image.profile != source.spec.profile.to_string()
            || sha256_file(&source.spec.source_iso)? != image.source_iso_sha256
        {
            return Err(LswError::InvalidValue {
                field: "sealed image",
                reason: "source manifest no longer matches the sealed image contract".to_owned(),
            });
        }
        let firmware = self
            .capabilities
            .firmware_code(source.spec.profile)
            .ok_or_else(|| LswError::MissingCapabilities(vec!["OVMF code firmware"]))?;
        if sha256_file(firmware)? != image.firmware_sha256 {
            return Err(LswError::InvalidValue {
                field: "sealed image",
                reason: "current OVMF firmware does not match the sealed image contract".to_owned(),
            });
        }
        let mut manifest = InstanceManifest::new(InstanceSpec {
            name: target_name.to_owned(),
            source_iso: source.spec.source_iso.clone(),
            profile: source.spec.profile,
            cpus: source.spec.cpus,
            memory_mib: source.spec.memory_mib,
            disk_gib: source.spec.disk_gib,
            network: source.spec.network,
            port_forwards: Vec::new(),
            license_accepted: source.spec.license_accepted,
            allow_unsupported_requirements: source.spec.allow_unsupported_requirements,
        })?;
        manifest.state = InstanceState::Stopped;
        manifest.base_image_key = Some(image.key.clone());
        manifest.idle_timeout_seconds = source.idle_timeout_seconds;
        manifest.hibernate_timeout_seconds = source.hibernate_timeout_seconds;
        manifest.idle_policy = source.idle_policy;
        manifest.memory_min_mib = source.memory_min_mib.min(manifest.spec.memory_mib);

        let instance_dir = self.store.create(&manifest)?;
        let result = (|| -> Result<()> {
            create_private_directory_all(&instance_dir.join("run"))?;
            create_private_directory_all(&instance_dir.join("swtpm-state"))?;
            copy_new_private(
                &self.store.instance_dir(source_name)?.join("OVMF_VARS.fd"),
                &instance_dir.join("OVMF_VARS.fd"),
            )?;
            let qemu_img = self
                .capabilities
                .qemu_img
                .as_ref()
                .ok_or_else(|| LswError::MissingCapabilities(vec!["qemu-img"]))?;
            let base = fs::canonicalize(&image.disk)?;
            let overlay = instance_dir.join("disk.qcow2");
            let status = Command::new(qemu_img)
                .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
                .arg(&base)
                .arg(&overlay)
                .status()?;
            if !status.success() {
                return Err(LswError::ExternalCommandFailed {
                    program: qemu_img.clone(),
                    status: status.code(),
                });
            }
            fs::set_permissions(&overlay, fs::Permissions::from_mode(0o600))?;
            write_clone_identity(self.store, target_name, &instance_dir)?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = self.store.remove(target_name);
            return Err(error);
        }
        Ok(instance_dir)
    }

    /// Stages a fresh boot identity and makes its token the host-side source of
    /// truth. The SCM agent applies it before opening the guest listener.
    pub fn rotate_instance_identity(&self, name: &str) -> Result<()> {
        let manifest = self.store.load(name)?;
        if !matches!(
            manifest.state,
            InstanceState::Stopped | InstanceState::Hibernated
        ) {
            return Err(LswError::InvalidValue {
                field: "instance state",
                reason: "identity rotation requires a stopped or hibernated instance".to_owned(),
            });
        }
        let previous = self.store.read_agent_token(name)?;
        let token = generate_agent_token()?;
        let instance_dir = self.store.instance_dir(name)?;
        stage_identity(name, &instance_dir, &token)?;
        if let Err(error) = write_atomic(
            &self.store.agent_token_path(name)?,
            format!("{token}\n").as_bytes(),
        ) {
            let _ = write_atomic(
                &instance_dir
                    .join("identity-seed")
                    .join("lsw")
                    .join(CLONE_IDENTITY_TOKEN_FILE),
                format!("{previous}\n").as_bytes(),
            );
            return Err(error);
        }
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<SealedImage>> {
        let root = self.store.root().join("images");
        let mut images = Vec::new();
        match fs::read_dir(root) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() && !entry.file_type()?.is_symlink() {
                        images.push(load_image(&entry.path(), false)?);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(images),
            Err(error) => return Err(error.into()),
        }
        images.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(images)
    }

    pub fn verify(&self, key: &str) -> Result<SealedImage> {
        if key.len() != 64
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(LswError::InvalidValue {
                field: "sealed image key",
                reason: "expected one lowercase 64-character SHA-256".to_owned(),
            });
        }
        load_image(&self.store.root().join("images").join(key), true)
    }
}

impl SealedImage {
    fn encode(&self) -> String {
        format!(
            "version={IMAGE_FORMAT_VERSION}\nkey={}\nsource_iso_sha256={}\nprofile={}\npreparation_identity={PREPARATION_IDENTITY}\nagent_sha256={}\nfirmware_sha256={}\nsource_disk_sha256={}\nbase_disk_sha256={}\n",
            self.key,
            self.source_iso_sha256,
            self.profile,
            self.agent_sha256,
            self.firmware_sha256,
            self.source_disk_sha256,
            self.base_disk_sha256
        )
    }
}

fn load_image(directory: &Path, verify_disk: bool) -> Result<SealedImage> {
    let metadata = require_regular_file(&directory.join(IMAGE_METADATA_FILE), "image metadata")?;
    let contents = fs::read_to_string(metadata)?;
    let field = |name: &str| -> Result<&str> {
        contents
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .ok_or_else(|| LswError::InvalidManifest(format!("sealed image is missing {name}")))
    };
    if field("version")? != IMAGE_FORMAT_VERSION.to_string() {
        return Err(LswError::InvalidManifest(
            "unsupported sealed-image version".to_owned(),
        ));
    }
    if field("preparation_identity")? != PREPARATION_IDENTITY {
        return Err(LswError::InvalidManifest(
            "sealed-image preparation identity does not match this build".to_owned(),
        ));
    }
    let image = SealedImage {
        key: field("key")?.to_owned(),
        directory: directory.to_path_buf(),
        disk: require_regular_file(&directory.join(BASE_DISK_FILE), "sealed base disk")?,
        source_iso_sha256: field("source_iso_sha256")?.to_owned(),
        profile: field("profile")?.to_owned(),
        agent_sha256: field("agent_sha256")?.to_owned(),
        firmware_sha256: field("firmware_sha256")?.to_owned(),
        source_disk_sha256: field("source_disk_sha256")?.to_owned(),
        base_disk_sha256: field("base_disk_sha256")?.to_owned(),
    };
    for digest in [
        &image.key,
        &image.source_iso_sha256,
        &image.agent_sha256,
        &image.firmware_sha256,
        &image.source_disk_sha256,
        &image.base_disk_sha256,
    ] {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(LswError::InvalidManifest(
                "sealed image contains an invalid SHA-256".to_owned(),
            ));
        }
    }
    let directory_key = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| LswError::InvalidManifest("sealed image directory has no key".to_owned()))?;
    let expected_key = image_key(
        &image.source_iso_sha256,
        &image.profile,
        &image.agent_sha256,
        &image.firmware_sha256,
        &image.source_disk_sha256,
    );
    if image.key != expected_key || directory_key != expected_key {
        return Err(LswError::InvalidManifest(
            "sealed image metadata does not match its content key".to_owned(),
        ));
    }
    if fs::metadata(&image.disk)?.permissions().mode() & 0o222 != 0 {
        return Err(LswError::InvalidManifest(
            "sealed base disk is writable".to_owned(),
        ));
    }
    if verify_disk && sha256_file(&image.disk)? != image.base_disk_sha256 {
        return Err(LswError::InvalidManifest(
            "sealed base disk does not match its recorded SHA-256".to_owned(),
        ));
    }
    Ok(image)
}

fn image_key(iso: &str, profile: &str, agent: &str, firmware: &str, disk: &str) -> String {
    let mut digest = Sha256::new();
    for value in [
        IMAGE_FORMAT_VERSION.to_string(),
        PREPARATION_IDENTITY.to_owned(),
        iso.to_owned(),
        profile.to_owned(),
        agent.to_owned(),
        firmware.to_owned(),
        disk.to_owned(),
    ] {
        digest.update(value.as_bytes());
        digest.update(b"\n");
    }
    let mut output = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn write_clone_identity(store: &StateStore, name: &str, instance_dir: &Path) -> Result<()> {
    let root = instance_dir.join("identity-seed").join("lsw");
    create_private_directory_all(&root)?;
    write_new_private(
        &root.join(CLONE_IDENTITY_MARKER_FILE),
        b"LSW-CLONE-IDENTITY\n",
    )?;
    write_new_private(
        &root.join(CLONE_IDENTITY_NAME_FILE),
        format!("{name}\n").as_bytes(),
    )?;
    write_new_private(
        &root.join(CLONE_IDENTITY_TOKEN_FILE),
        format!("{}\n", store.read_agent_token(name)?).as_bytes(),
    )
}

fn stage_identity(name: &str, instance_dir: &Path, token: &str) -> Result<()> {
    let root = instance_dir.join("identity-seed").join("lsw");
    create_private_directory_all(&root)?;
    write_atomic(
        &root.join(CLONE_IDENTITY_MARKER_FILE),
        b"LSW-CLONE-IDENTITY\n",
    )?;
    write_atomic(
        &root.join(CLONE_IDENTITY_NAME_FILE),
        format!("{name}\n").as_bytes(),
    )?;
    write_atomic(
        &root.join(CLONE_IDENTITY_TOKEN_FILE),
        format!("{token}\n").as_bytes(),
    )
}

fn require_regular_file(path: &Path, field: &'static str) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(path.to_path_buf())
        }
        _ => Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a regular non-symlink file", path.display()),
        }),
    }
}

fn create_private_directory_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(LswError::InvalidValue {
            field: "private directory",
            reason: format!("{} must be a real directory", path.display()),
        });
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn copy_new_private(source: &Path, destination: &Path) -> Result<()> {
    require_regular_file(source, "clone source")?;
    let mut input = fs::File::open(source)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o600))?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn write_new_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    let mut output = OpenOptions::new().create_new(true).write(true).open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    output.write_all(contents.as_ref())?;
    output.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{HostPlatform, NetworkMode, WindowsProfile};

    use super::*;

    #[test]
    fn image_identity_changes_for_every_contract_component() {
        let base = image_key("iso", "slim", "agent", "firmware", "disk");
        for changed in [
            image_key("iso2", "slim", "agent", "firmware", "disk"),
            image_key("iso", "vanilla", "agent", "firmware", "disk"),
            image_key("iso", "slim", "agent2", "firmware", "disk"),
            image_key("iso", "slim", "agent", "firmware2", "disk"),
            image_key("iso", "slim", "agent", "firmware", "disk2"),
        ] {
            assert_ne!(base, changed);
        }
        assert_eq!(base.len(), 64);
    }

    #[test]
    fn sealed_clone_has_a_tiny_overlay_and_private_identity() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-image-test-{nonce}"));
        fs::create_dir_all(&root).expect("fixture root should be created");
        let iso = root.join("windows.iso");
        let agent = root.join("lsw-agent.exe");
        let firmware = root.join("OVMF_CODE.fd");
        let vars = root.join("OVMF_VARS.fd");
        fs::write(&iso, b"iso").expect("ISO fixture should be written");
        fs::write(&agent, b"agent").expect("agent fixture should be written");
        fs::write(&firmware, b"firmware").expect("firmware fixture should be written");
        fs::write(&vars, b"vars").expect("vars fixture should be written");
        let qemu_img = root.join("qemu-img");
        fs::write(
            &qemu_img,
            b"#!/bin/sh\nset -eu\ncase $1 in\n convert) cp -- \"$5\" \"$6\" ;;\n create) printf overlay > \"$8\" ;;\n *) exit 2 ;;\nesac\n",
        )
        .expect("qemu-img fixture should be written");
        fs::set_permissions(&qemu_img, fs::Permissions::from_mode(0o700))
            .expect("qemu-img fixture should be executable");

        let store = StateStore::new(root.join("state"));
        let mut source = InstanceManifest::new(InstanceSpec {
            name: "source".to_owned(),
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
        .expect("source manifest should be valid");
        source.state = InstanceState::Stopped;
        let source_dir = store.create(&source).expect("source should be created");
        fs::write(source_dir.join("disk.qcow2"), b"installed windows")
            .expect("source disk should be written");
        fs::write(source_dir.join("OVMF_VARS.fd"), b"private vars")
            .expect("source vars should be written");
        let source_token = store
            .read_agent_token("source")
            .expect("source token should exist");
        let mut capabilities = HostCapabilities::unavailable(HostPlatform::Linux);
        capabilities.qemu_img = Some(qemu_img);
        capabilities.ovmf_code = Some(firmware);
        capabilities.ovmf_vars = Some(vars);
        let manager = ImageManager::new(&store, &capabilities);
        let image = manager.seal(&source, &agent).expect("source should seal");
        source.base_image_key = Some(image.key.clone());
        store.update(&source).expect("source should record image");
        let clone_dir = manager
            .clone_instance("source", "clone")
            .expect("clone should be created");
        let clone = store.load("clone").expect("clone should load");
        let clone_token = store
            .read_agent_token("clone")
            .expect("clone token should exist");

        assert_eq!(clone.state, InstanceState::Stopped);
        assert_eq!(clone.base_image_key.as_deref(), Some(image.key.as_str()));
        assert_ne!(source.control_port, clone.control_port);
        assert_ne!(source_token, clone_token);
        assert_eq!(
            fs::read_to_string(
                clone_dir
                    .join("identity-seed/lsw")
                    .join(CLONE_IDENTITY_TOKEN_FILE)
            )
            .expect("identity token should be readable")
            .trim(),
            clone_token
        );
        assert!(
            fs::metadata(clone_dir.join("disk.qcow2"))
                .expect("overlay should exist")
                .len()
                < 256 * 1024 * 1024
        );
        assert_eq!(
            fs::metadata(&image.disk)
                .expect("base should exist")
                .permissions()
                .mode()
                & 0o222,
            0
        );
        manager
            .rotate_instance_identity("clone")
            .expect("clone identity should rotate");
        let rotated = store
            .read_agent_token("clone")
            .expect("rotated token should exist");
        assert_ne!(rotated, clone_token);
        assert_eq!(
            fs::read_to_string(
                clone_dir
                    .join("identity-seed/lsw")
                    .join(CLONE_IDENTITY_TOKEN_FILE)
            )
            .expect("rotated identity token should be readable")
            .trim(),
            rotated
        );
        fs::set_permissions(&image.disk, fs::Permissions::from_mode(0o600))
            .expect("test should make the sealed base writable");
        fs::write(&image.disk, b"corrupt base").expect("test should corrupt the sealed base");
        fs::set_permissions(&image.disk, fs::Permissions::from_mode(0o400))
            .expect("test should restore sealed permissions");
        assert!(manager.verify(&image.key).is_err());
        fs::remove_dir_all(root).expect("fixture should be removable");
    }
}
