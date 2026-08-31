// SPDX-License-Identifier: GPL-3.0-or-later

mod guest_setup;
mod unattend;

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use self::guest_setup::{install_agent_script, seed_readme};
use self::unattend::{
    autounattend, generate_setup_account_password, unattend_password_value, validate_locale,
};
use crate::profile_application::profile_script;
use crate::{InstanceManifest, LswError, Result, AGENT_GUEST_PORT};

const MAX_AGENT_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const LICENSE_HELPER_SCRIPT: &[u8] = include_bytes!("../assets/license-helper.ps1");
pub(crate) const OFFLINE_PROFILE_MARKER_NAME: &str = "offline-profile-v2-applied.marker";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallSeedOptions {
    pub locale: String,
    pub unattended_image_index: Option<u32>,
    pub unattended_image_name: Option<String>,
    pub agent_binary: Option<PathBuf>,
}

impl Default for InstallSeedOptions {
    fn default() -> Self {
        Self {
            locale: "en-US".to_owned(),
            unattended_image_index: None,
            unattended_image_name: None,
            agent_binary: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallSeedPlan {
    pub destination: PathBuf,
    pub files: Vec<PathBuf>,
    pub wipes_virtual_disk: bool,
    pub includes_agent: bool,
    generated: BTreeMap<PathBuf, Vec<u8>>,
    setup_account_password_value: String,
}

impl InstallSeedPlan {
    pub fn describe(&self) -> Vec<String> {
        let mut lines = self
            .files
            .iter()
            .map(|file| format!("write {}", self.destination.join(file).display()))
            .collect::<Vec<_>>();
        if self.wipes_virtual_disk {
            lines.push(
                "warning: Autounattend.xml will wipe virtual Disk 0 and install the selected Windows edition"
                    .to_owned(),
            );
        }
        if !self.includes_agent {
            lines.push(
                "note: no lsw-agent.exe was supplied; the seed can be completed later".to_owned(),
            );
        }
        lines
    }

    /// Returns the obfuscated unattend value for the one-shot OOBE account.
    ///
    /// The value is not encrypted. It remains in memory only so the WinPE
    /// backend can generate the offline answer file without persisting a
    /// second copy in the installation seed.
    pub fn setup_account_password_value(&self) -> &str {
        &self.setup_account_password_value
    }
}

pub struct InstallSeedBuilder;

impl InstallSeedBuilder {
    pub fn plan(
        manifest: &InstanceManifest,
        instance_dir: &Path,
        token: &str,
        options: &InstallSeedOptions,
    ) -> Result<InstallSeedPlan> {
        manifest.spec.validate()?;
        validate_locale(&options.locale)?;
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
        if matches!(options.unattended_image_index, Some(0)) {
            return Err(LswError::InvalidValue {
                field: "image index",
                reason: "must be at least 1".to_owned(),
            });
        }
        if options.unattended_image_index.is_some() && options.unattended_image_name.is_some() {
            return Err(LswError::InvalidValue {
                field: "Windows edition",
                reason: "image index and edition name cannot both be selected".to_owned(),
            });
        }
        if let Some(name) = &options.unattended_image_name {
            if name.is_empty()
                || name.len() > 256
                || name.contains(['\r', '\n'])
                || name.chars().any(|character| character.is_control())
            {
                return Err(LswError::InvalidValue {
                    field: "Windows edition",
                    reason: "the media-provided edition name is invalid".to_owned(),
                });
            }
        }

        let destination = instance_dir.join("seed");
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(LswError::InvalidValue {
                field: "install seed",
                reason: format!(
                    "{} already exists; LSW will not replace it automatically",
                    destination.display()
                ),
            });
        }

        let setup_account_password = generate_setup_account_password()?;
        let setup_account_password_value = unattend_password_value(&setup_account_password);
        let mut generated = BTreeMap::new();
        generated.insert(
            PathBuf::from("Autounattend.xml"),
            autounattend(manifest, options, &setup_account_password_value).into_bytes(),
        );
        generated.insert(
            PathBuf::from("lsw/agent.token"),
            format!("{token}\r\n").into_bytes(),
        );
        generated.insert(
            PathBuf::from("lsw/instance.txt"),
            format!(
                "name={}\r\nprofile={}\r\nagent_port={}\r\n",
                manifest.spec.name, manifest.spec.profile, AGENT_GUEST_PORT
            )
            .into_bytes(),
        );
        generated.insert(
            PathBuf::from("lsw/install-agent.ps1"),
            install_agent_script().into_bytes(),
        );
        generated.insert(
            PathBuf::from("lsw/license-helper.ps1"),
            LICENSE_HELPER_SCRIPT.to_vec(),
        );
        generated.insert(
            PathBuf::from("lsw/apply-profile.ps1"),
            profile_script(manifest.spec.profile, OFFLINE_PROFILE_MARKER_NAME)?.into_bytes(),
        );
        generated.insert(
            PathBuf::from("README.txt"),
            seed_readme(manifest, options).into_bytes(),
        );

        if let Some(agent) = &options.agent_binary {
            let metadata = fs::symlink_metadata(agent)?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(LswError::InvalidValue {
                    field: "agent binary",
                    reason: format!("{} is not a regular file", agent.display()),
                });
            }
            if metadata.len() < 2 || metadata.len() > MAX_AGENT_BINARY_BYTES {
                return Err(LswError::InvalidValue {
                    field: "agent binary",
                    reason: format!("{} must be between 2 bytes and 64 MiB", agent.display()),
                });
            }
            let binary = fs::read(agent)?;
            if !binary.starts_with(b"MZ") {
                return Err(LswError::InvalidValue {
                    field: "agent binary",
                    reason: format!("{} is not a Windows PE executable", agent.display()),
                });
            }
            generated.insert(PathBuf::from("lsw/lsw-agent.exe"), binary);
        }

        let files = generated.keys().cloned().collect();
        Ok(InstallSeedPlan {
            destination,
            files,
            wipes_virtual_disk: options.unattended_image_index.is_some()
                || options.unattended_image_name.is_some(),
            includes_agent: options.agent_binary.is_some(),
            generated,
            setup_account_password_value,
        })
    }

    pub fn apply(plan: &InstallSeedPlan) -> Result<()> {
        if fs::symlink_metadata(&plan.destination).is_ok() {
            return Err(LswError::InvalidValue {
                field: "install seed",
                reason: format!(
                    "refusing to replace existing {}",
                    plan.destination.display()
                ),
            });
        }
        let parent = plan
            .destination
            .parent()
            .ok_or_else(|| LswError::InvalidValue {
                field: "install seed",
                reason: "destination has no parent directory".to_owned(),
            })?;
        let staging = parent.join(format!("seed.tmp-{}", std::process::id()));
        if fs::symlink_metadata(&staging).is_ok() {
            return Err(LswError::InvalidValue {
                field: "install seed",
                reason: format!("staging path {} already exists", staging.display()),
            });
        }
        fs::create_dir(&staging)?;
        set_private_directory_permissions(&staging)?;

        let result = (|| {
            for (relative, contents) in &plan.generated {
                let destination = staging.join(relative);
                if let Some(directory) = destination.parent() {
                    fs::create_dir_all(directory)?;
                    set_private_directory_permissions(directory)?;
                }
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&destination)?;
                set_private_file_permissions(&destination)?;
                file.write_all(contents)?;
                file.sync_all()?;
            }
            fs::rename(&staging, &plan.destination)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{
        InstanceSpec, NetworkMode, WindowsProfile, LICENSE_HELPER_GUEST_PORT,
        MAINTENANCE_HELPER_GUEST_PORT, USER_HELPER_GUEST_PORT,
    };

    use super::*;

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (PathBuf, InstanceManifest) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        // Emulated guests may expose a coarse wall clock, so time alone cannot
        // distinguish tests that the harness starts concurrently.
        let fixture_id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "lsw-seed-test-{}-{nonce}-{fixture_id}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture should be created");
        let iso = root.join("windows.iso");
        fs::write(&iso, b"media").expect("ISO fixture should be written");
        let manifest = InstanceManifest::new(InstanceSpec {
            name: "win-dev".to_owned(),
            source_iso: iso,
            profile: WindowsProfile::Slim,
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

    #[test]
    fn installation_seed_completes_oobe_without_logon_or_product_keys() {
        let (root, manifest) = fixture();
        let plan = InstallSeedBuilder::plan(
            &manifest,
            &root,
            &"a".repeat(64),
            &InstallSeedOptions::default(),
        )
        .expect("seed plan should be generated");
        let answer = String::from_utf8(
            plan.generated
                .get(Path::new("Autounattend.xml"))
                .expect("answer file should exist")
                .clone(),
        )
        .expect("answer file should be UTF-8");
        assert!(answer.contains("<AcceptEula>true</AcceptEula>"));
        assert!(!answer.contains("ProductKey"));
        assert!(!answer.contains("SkipMachineOOBE"));
        assert!(answer.contains("<HideEULAPage>true</HideEULAPage>"));
        assert!(answer.contains("<HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>"));
        assert!(answer.contains("<HideOnlineAccountScreens>true</HideOnlineAccountScreens>"));
        assert!(answer.contains("<ProtectYourPC>3</ProtectYourPC>"));
        assert!(answer.contains("<Name>LSWSetup</Name>"));
        assert!(answer.contains("<Group>Users</Group>"));
        assert!(answer.contains("<PlainText>false</PlainText>"));
        assert!(answer.contains(plan.setup_account_password_value()));
        assert!(!answer.contains("AutoLogon"));
        assert!(!answer.contains("FirstLogonCommands"));
        let computer_name = answer
            .find("<ComputerName>")
            .expect("computer name should exist");
        let oobe = answer
            .find("<settings pass=\"oobeSystem\">")
            .expect("OOBE pass should exist");
        assert!(computer_name < oobe);
        assert!(!answer.contains("WillWipeDisk"));

        let installer = String::from_utf8(
            plan.generated
                .get(Path::new("lsw/install-agent.ps1"))
                .expect("agent installer should exist")
                .clone(),
        )
        .expect("agent installer should be UTF-8");
        let license_helper = String::from_utf8(
            plan.generated
                .get(Path::new("lsw/license-helper.ps1"))
                .expect("license helper script should exist")
                .clone(),
        )
        .expect("license helper script should be UTF-8");
        assert!(installer.contains("$ServiceName = 'LSWAgent'"));
        assert!(installer.contains("$ServiceDisplayName = 'LSW Guest Agent'"));
        assert!(installer.contains("$ServiceAccount = 'NT SERVICE\\LSWAgent'"));
        assert!(installer.contains("--service --token-file"));
        assert!(installer.contains("'create', $ServiceName"));
        assert!(installer.contains("'config', $ServiceName"));
        assert!(installer.contains("'start=', 'auto'"));
        assert!(installer.contains("'obj=', $ServiceAccount"));
        assert!(installer.contains("function ConvertTo-ScBinaryPathArgument"));
        assert!(installer.contains("$Command.Replace('\"', '\\\"')"));
        assert!(installer.contains("$ScAgentCommand = ConvertTo-ScBinaryPathArgument"));
        assert_eq!(installer.matches("'binPath=', $ScAgentCommand").count(), 2);
        assert!(!installer.contains("'binPath=', $AgentCommand"));
        assert!(installer.contains("$ConfiguredService.StartName"));
        assert!(installer.contains("$ConfiguredService.PathName -cne $AgentCommand"));
        assert!(!installer.contains("password="));
        assert!(installer.contains("'failure', $ServiceName"));
        assert!(installer.contains("$ExitCode = $LASTEXITCODE"));
        assert!(installer.contains("$ServiceIdentity"));
        assert!(installer.contains("$ServiceIdentity, $Modify, $Inherit"));
        assert!(installer.contains("$ServiceIdentity, $Modify, $Allow"));
        assert!(installer.contains("$LicenseServiceName = 'LSWLicenseHelper'"));
        assert!(installer.contains("$UserServiceName = 'LSWUserHelper'"));
        assert!(installer.contains("$MaintenanceServiceName = 'LSWMaintenanceHelper'"));
        assert!(installer.contains("--license-helper --token-file"));
        assert!(installer.contains("--maintenance-helper --token-file"));
        assert!(installer.contains("$LicenseScriptSource"));
        assert!(installer.contains("$LicenseScriptTarget"));
        assert!(installer.contains(
            "Copy-Item -LiteralPath $LicenseScriptSource -Destination $LicenseScriptTarget -Force"
        ));
        assert!(installer.contains(&format!("--listen 127.0.0.1:{LICENSE_HELPER_GUEST_PORT}")));
        assert!(installer.contains(&format!("--listen 127.0.0.1:{USER_HELPER_GUEST_PORT}")));
        assert!(installer.contains(&format!(
            "--listen 127.0.0.1:{MAINTENANCE_HELPER_GUEST_PORT}"
        )));
        assert!(!installer.contains("__LSW_MAINTENANCE_HELPER_PORT__"));
        assert!(installer.contains(&format!("-LocalPort {AGENT_GUEST_PORT}")));
        assert!(installer.contains("$LogTarget = Join-Path $DataRoot 'agent.log'"));
        assert!(installer.contains("Set-Acl -LiteralPath $LogTarget"));
        assert!(installer.contains("'start=', 'demand'"));
        assert!(installer.contains("'obj=', 'LocalSystem'"));
        assert!(installer.contains("$LicenseServiceSddl"));
        assert!(installer.contains("$MaintenanceServiceSddl"));
        assert!(installer.contains("$ServiceIdentity.Value"));
        assert!(!installer.contains("ProductKey"));
        assert!(installer.contains("$StartedService.WaitForStatus"));
        assert!(installer.contains("$StartedService.Status -ne 'Running'"));
        assert!(installer.contains("$AgentTargetFullPath"));
        assert!(
            installer.contains("Invoke-CimMethod -InputObject $StaleAgent -MethodName Terminate")
        );
        assert!(installer.contains("$TerminationDeadline"));
        assert!(installer.contains("Remove-ItemProperty -Path $RunKey -Name 'LSWAgent'"));
        assert!(installer.contains("/v HiberbootEnabled /t REG_DWORD /d 0 /f"));
        assert!(installer.contains("Remove-Item -LiteralPath $TokenSource -Force"));
        assert!(installer.contains("$SetupRootFullPath.StartsWith($DataRootFullPath"));
        assert!(installer.contains("net.exe user \"LSWSetup\" /delete"));
        assert!(installer.contains("net.exe user \"defaultuser0\" /delete"));
        assert!(installer.contains("/v AutoAdminLogon /t REG_SZ /d 0 /f"));
        assert!(installer.contains("DefaultUserName DefaultDomainName DefaultPassword"));
        assert!(installer.contains("/v DisablePrivacyExperience /t REG_DWORD /d 1 /f"));
        assert!(installer.contains("/v LaunchUserOOBE /t REG_DWORD /d 0 /f"));
        assert!(installer.contains("/v DefaultAccountAction /t REG_DWORD /d 0 /f"));
        assert!(installer.contains("DefaultAccountSAMName DefaultAccountSID"));
        assert!(installer.contains("setup-complete.marker"));
        assert!(installer.contains("LSW-SETUP-COMPLETE"));
        for stage in [
            "installing-agent",
            "configuring-services",
            "applying-profile",
            "starting-agent",
            "waiting-for-oobe",
            "cleanup",
            "complete",
        ] {
            assert!(installer.contains(stage));
        }
        assert!(
            installer.find("Set-LswSetupStage 'starting-agent'")
                < installer.find("Set-LswSetupStage 'applying-profile'")
        );
        assert!(installer.contains("setup-progress.marker.tmp"));
        assert!(installer.contains("Move-Item -LiteralPath $SetupProgressTemporary"));
        assert!(installer.contains("%WINDIR%\\Panther\\unattend.xml"));
        assert!(installer.contains("del /f /q \"%~f0\""));
        assert!(!installer.contains("New-ItemProperty"));
        assert!(!installer.contains("Start-Process"));
        assert!(license_helper.contains("[Console]::In.ReadLine()"));
        assert!(license_helper.contains("[Console]::Out.WriteLine($Value)"));
        assert!(license_helper.contains("STATUS=unlicensed"));
        assert!(license_helper.contains("STATUS=activation-requested"));
        assert!(license_helper.contains("exit 1"));

        let wait_for_stop = installer
            .find("$ExistingServiceToStop.WaitForStatus")
            .expect("installer should wait for the old service to stop");
        let terminate_stale_agent = installer
            .find("Invoke-CimMethod -InputObject $StaleAgent -MethodName Terminate")
            .expect("installer should terminate the exact-path legacy agent");
        let remove_autorun = installer
            .find("Remove-ItemProperty -Path $RunKey -Name 'LSWAgent'")
            .expect("installer should remove the legacy autorun value");
        let replace_binary = installer
            .find("Copy-Item -LiteralPath $AgentSource")
            .expect("installer should replace the agent binary");
        assert!(wait_for_stop < replace_binary);
        assert!(terminate_stale_agent < remove_autorun);
        assert!(remove_autorun < replace_binary);
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn unattend_password_encoding_matches_windows_sim() {
        assert_eq!(
            unattend_password_value("pw"),
            "cAB3AFAAYQBzAHMAdwBvAHIAZAA="
        );
    }

    #[test]
    fn unattended_seed_is_explicit_and_atomic() {
        let (root, manifest) = fixture();
        let options = InstallSeedOptions {
            locale: "zh-HK".to_owned(),
            unattended_image_index: Some(3),
            unattended_image_name: None,
            agent_binary: None,
        };
        let plan = InstallSeedBuilder::plan(&manifest, &root, &"b".repeat(64), &options)
            .expect("seed plan should be generated");
        assert!(plan.wipes_virtual_disk);
        InstallSeedBuilder::apply(&plan).expect("seed should be written");
        assert!(root.join("seed/Autounattend.xml").is_file());
        assert!(InstallSeedBuilder::apply(&plan).is_err());
        let profile = fs::read_to_string(root.join("seed/lsw/apply-profile.ps1"))
            .expect("profile should be readable");
        assert!(profile.contains("Remove-AppxProvisionedPackage"));
        assert!(profile.contains(OFFLINE_PROFILE_MARKER_NAME));
        assert!(profile.contains("WinPE servicing marker is present"));
        assert!(profile.contains("report.json"));
        assert!(profile.contains("compact.exe /CompactOS:always"));
        assert!(profile.contains("CompactOS failed with exit code"));
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn unattended_edition_uses_the_media_name_and_escapes_xml() {
        let (root, manifest) = fixture();
        let options = InstallSeedOptions {
            unattended_image_name: Some("Windows 11 Pro & Development".to_owned()),
            ..InstallSeedOptions::default()
        };
        let plan = InstallSeedBuilder::plan(&manifest, &root, &"c".repeat(64), &options)
            .expect("edition-name seed should be generated");
        let answer = String::from_utf8(
            plan.generated
                .get(Path::new("Autounattend.xml"))
                .expect("answer file should exist")
                .clone(),
        )
        .expect("answer file should be UTF-8");
        assert!(answer.contains("<Key>/IMAGE/NAME</Key>"));
        assert!(answer.contains("Windows 11 Pro &amp; Development"));
        assert!(plan.wipes_virtual_disk);
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn seed_rejects_a_non_windows_agent_binary() {
        let (root, manifest) = fixture();
        let agent = root.join("not-an-agent.bin");
        fs::write(&agent, b"ELF").expect("agent fixture should be written");
        let options = InstallSeedOptions {
            agent_binary: Some(agent),
            ..InstallSeedOptions::default()
        };
        assert!(InstallSeedBuilder::plan(&manifest, &root, &"c".repeat(64), &options).is_err());
        fs::remove_dir_all(root).expect("fixture should be removed");
    }
}
