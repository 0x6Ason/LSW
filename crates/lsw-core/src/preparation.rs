// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{HostCapabilities, InstanceManifest, LswError, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparationStep {
    CreateDirectory(PathBuf),
    CopyFirmwareVariables {
        source: PathBuf,
        destination: PathBuf,
    },
    CreateDisk {
        program: PathBuf,
        destination: PathBuf,
        size_gib: u32,
    },
}

impl PreparationStep {
    pub fn describe(&self) -> String {
        match self {
            Self::CreateDirectory(path) => format!("create directory {}", path.display()),
            Self::CopyFirmwareVariables {
                source,
                destination,
            } => format!(
                "copy firmware variables {} -> {}",
                source.display(),
                destination.display()
            ),
            Self::CreateDisk {
                program,
                destination,
                size_gib,
            } => format!(
                "{} create -f qcow2 {} {}G",
                program.display(),
                destination.display(),
                size_gib
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparationPlan {
    pub steps: Vec<PreparationStep>,
    pub missing_capabilities: Vec<&'static str>,
}

impl PreparationPlan {
    pub fn is_ready(&self) -> bool {
        self.steps.is_empty() && self.missing_capabilities.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct Provisioner {
    capabilities: HostCapabilities,
}

impl Provisioner {
    pub fn new(capabilities: HostCapabilities) -> Self {
        Self { capabilities }
    }

    pub fn plan(
        &self,
        manifest: &InstanceManifest,
        instance_dir: &Path,
    ) -> Result<PreparationPlan> {
        manifest.spec.validate()?;
        require_directory(instance_dir, "instance directory")?;

        let mut steps = Vec::new();
        let mut missing_capabilities = Vec::new();

        for directory in [instance_dir.join("run"), instance_dir.join("swtpm-state")] {
            if path_is_missing(&directory, "runtime directory")? {
                steps.push(PreparationStep::CreateDirectory(directory));
            } else {
                require_directory(&directory, "runtime directory")?;
            }
        }

        let firmware_destination = instance_dir.join("OVMF_VARS.fd");
        if path_is_missing(&firmware_destination, "firmware variable store")? {
            let firmware_source = self
                .capabilities
                .firmware_vars(manifest.spec.profile)
                .cloned()
                .unwrap_or_else(|| PathBuf::from("/path/to/OVMF_VARS.fd"));
            steps.push(PreparationStep::CopyFirmwareVariables {
                source: firmware_source,
                destination: firmware_destination,
            });
            if self
                .capabilities
                .firmware_vars(manifest.spec.profile)
                .is_none()
            {
                missing_capabilities.push(if manifest.spec.profile.security().secure_boot {
                    "enrolled Secure Boot OVMF variable template"
                } else {
                    "OVMF variable template"
                });
            }
        } else {
            require_regular_file(&firmware_destination, "firmware variable store")?;
        }

        let disk_destination = instance_dir.join("disk.qcow2");
        if path_is_missing(&disk_destination, "system disk")? {
            let qemu_img = self
                .capabilities
                .qemu_img
                .clone()
                .unwrap_or_else(|| PathBuf::from("qemu-img"));
            steps.push(PreparationStep::CreateDisk {
                program: qemu_img,
                destination: disk_destination,
                size_gib: manifest.spec.disk_gib,
            });
            if self.capabilities.qemu_img.is_none() {
                missing_capabilities.push("qemu-img");
            }
        } else {
            require_regular_file(&disk_destination, "system disk")?;
        }

        Ok(PreparationPlan {
            steps,
            missing_capabilities,
        })
    }

    pub fn apply(&self, plan: &PreparationPlan) -> Result<()> {
        if !plan.missing_capabilities.is_empty() {
            return Err(LswError::MissingCapabilities(
                plan.missing_capabilities.clone(),
            ));
        }

        for step in &plan.steps {
            match step {
                PreparationStep::CreateDirectory(path) => create_directory(path)?,
                PreparationStep::CopyFirmwareVariables {
                    source,
                    destination,
                } => copy_new_file(source, destination)?,
                PreparationStep::CreateDisk {
                    program,
                    destination,
                    size_gib,
                } => create_disk(program, destination, *size_gib)?,
            }
        }
        Ok(())
    }
}

fn path_is_missing(path: &Path, field: &'static str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(LswError::InvalidValue {
            field,
            reason: format!("{} must not be a symbolic link", path.display()),
        }),
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn require_directory(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(LswError::from)?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a real directory", path.display()),
        })
    }
}

fn require_regular_file(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(LswError::from)?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a regular file", path.display()),
        })
    }
}

fn create_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => set_private_directory_permissions(path),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            require_directory(path, "runtime directory")?;
            set_private_directory_permissions(path)
        }
        Err(error) => Err(error.into()),
    }
}

fn copy_new_file(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_file() {
        return Err(LswError::InvalidValue {
            field: "OVMF variable template",
            reason: format!("{} is not a regular file", source.display()),
        });
    }
    let mut source_file = fs::File::open(source)?;
    let mut destination_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    set_private_file_permissions(destination)?;
    if let Err(error) = io::copy(&mut source_file, &mut destination_file) {
        drop(destination_file);
        let _ = fs::remove_file(destination);
        return Err(error.into());
    }
    destination_file.flush()?;
    destination_file.sync_all()?;
    Ok(())
}

fn create_disk(program: &Path, destination: &Path, size_gib: u32) -> Result<()> {
    if !path_is_missing(destination, "system disk")? {
        return Err(LswError::InvalidValue {
            field: "system disk",
            reason: format!("refusing to overwrite {}", destination.display()),
        });
    }
    let arguments: [OsString; 5] = [
        "create".into(),
        "-f".into(),
        "qcow2".into(),
        destination.as_os_str().to_owned(),
        format!("{size_gib}G").into(),
    ];
    let status = Command::new(program).args(&arguments).status()?;
    if !status.success() {
        let _ = fs::remove_file(destination);
        return Err(LswError::ExternalCommandFailed {
            program: program.to_owned(),
            status: status.code(),
        });
    }
    require_regular_file(destination, "system disk")?;
    set_private_file_permissions(destination)
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{InstanceSpec, NetworkMode, WindowsProfile};

    use super::*;

    fn fixture() -> (PathBuf, InstanceManifest) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-prepare-test-{nonce}"));
        fs::create_dir_all(&root).expect("fixture directory should be created");
        let iso = root.join("windows.iso");
        fs::write(&iso, b"media").expect("fixture ISO should be created");
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
        (root, manifest)
    }

    fn headless_capabilities() -> HostCapabilities {
        HostCapabilities::unavailable(crate::HostPlatform::Linux)
    }

    #[test]
    fn headless_plan_is_explicit_about_missing_tools() {
        let (root, manifest) = fixture();
        let plan = Provisioner::new(headless_capabilities())
            .plan(&manifest, &root)
            .expect("plan should be created");
        assert_eq!(plan.steps.len(), 4);
        assert_eq!(
            plan.missing_capabilities,
            vec!["OVMF variable template", "qemu-img"]
        );
        assert!(Provisioner::new(headless_capabilities())
            .apply(&plan)
            .is_err());
        fs::remove_dir_all(root).expect("fixture directory should be removable");
    }

    #[cfg(unix)]
    #[test]
    fn apply_is_idempotent_and_never_overwrites_disk() {
        use std::os::unix::fs::PermissionsExt;

        let (root, manifest) = fixture();
        let firmware = root.join("firmware-template.fd");
        fs::write(&firmware, b"vars").expect("firmware fixture should be written");
        let qemu_img = root.join("fake-qemu-img");
        fs::write(&qemu_img, b"#!/bin/sh\n: > \"$4\"\n").expect("fake qemu-img should be written");
        fs::set_permissions(&qemu_img, fs::Permissions::from_mode(0o700))
            .expect("fake qemu-img should be executable");
        let mut capabilities = headless_capabilities();
        capabilities.qemu_img = Some(qemu_img);
        capabilities.ovmf_vars = Some(firmware);
        let provisioner = Provisioner::new(capabilities);
        let plan = provisioner
            .plan(&manifest, &root)
            .expect("plan should be created");
        provisioner.apply(&plan).expect("plan should apply");
        let second_plan = provisioner
            .plan(&manifest, &root)
            .expect("second plan should be created");
        assert!(second_plan.is_ready());
        fs::write(root.join("disk.qcow2"), b"preserved").expect("disk fixture should be writable");
        assert!(provisioner
            .plan(&manifest, &root)
            .expect("existing files should still be valid")
            .is_ready());
        assert_eq!(
            fs::read(root.join("disk.qcow2")).expect("disk should be readable"),
            b"preserved"
        );
        fs::remove_dir_all(root).expect("fixture directory should be removable");
    }
}
