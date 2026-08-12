// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{CustomizationPlan, InstanceManifest, LswError, Result, AGENT_GUEST_PORT};

const MAX_AGENT_BINARY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallSeedOptions {
    pub locale: String,
    pub unattended_image_index: Option<u32>,
    pub agent_binary: Option<PathBuf>,
}

impl Default for InstallSeedOptions {
    fn default() -> Self {
        Self {
            locale: "en-US".to_owned(),
            unattended_image_index: None,
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
                "warning: Autounattend.xml will wipe virtual Disk 0 and install the selected image index"
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

        let mut generated = BTreeMap::new();
        generated.insert(
            PathBuf::from("Autounattend.xml"),
            autounattend(manifest, options).into_bytes(),
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
            install_agent_script().as_bytes().to_vec(),
        );
        generated.insert(
            PathBuf::from("lsw/apply-profile.ps1"),
            profile_script(manifest).into_bytes(),
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
            wipes_virtual_disk: options.unattended_image_index.is_some(),
            includes_agent: options.agent_binary.is_some(),
            generated,
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

fn autounattend(manifest: &InstanceManifest, options: &InstallSeedOptions) -> String {
    let disk = options
        .unattended_image_index
        .map(|index| {
            format!(
                r#"
      <DiskConfiguration>
        <Disk wcm:action="add">
          <DiskID>0</DiskID>
          <WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add"><Order>1</Order><Type>EFI</Type><Size>260</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>2</Order><Type>MSR</Type><Size>16</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>3</Order><Type>Primary</Type><Extend>true</Extend></CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add"><Order>1</Order><PartitionID>1</PartitionID><Label>System</Label><Format>FAT32</Format></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>2</Order><PartitionID>3</PartitionID><Label>Windows</Label><Letter>C</Letter><Format>NTFS</Format></ModifyPartition>
          </ModifyPartitions>
        </Disk>
        <WillShowUI>OnError</WillShowUI>
      </DiskConfiguration>
      <ImageInstall>
        <OSImage>
          <InstallFrom><MetaData wcm:action="add"><Key>/IMAGE/INDEX</Key><Value>{index}</Value></MetaData></InstallFrom>
          <InstallTo><DiskID>0</DiskID><PartitionID>3</PartitionID></InstallTo>
          <WillShowUI>OnError</WillShowUI>
        </OSImage>
      </ImageInstall>"#
            )
        })
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <SetupUILanguage><UILanguage>{locale}</UILanguage></SetupUILanguage>
      <InputLocale>{locale}</InputLocale><SystemLocale>{locale}</SystemLocale><UILanguage>{locale}</UILanguage><UserLocale>{locale}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">{disk}
      <UserData><AcceptEula>true</AcceptEula><FullName>LSW User</FullName><Organization>LSW</Organization></UserData>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <InputLocale>{locale}</InputLocale><SystemLocale>{locale}</SystemLocale><UILanguage>{locale}</UILanguage><UserLocale>{locale}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <ComputerName>{computer_name}</ComputerName>
      <FirstLogonCommands>
        <SynchronousCommand wcm:action="add">
          <Order>1</Order><Description>Install LSW guest agent</Description><RequiresUserInput>false</RequiresUserInput>
          <CommandLine>cmd.exe /d /c for %D in (D E F G H I J K L M N O P Q R S T U V W X Y Z) do @if exist "%D:\lsw\install-agent.ps1" powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%D:\lsw\install-agent.ps1"</CommandLine>
        </SynchronousCommand>
      </FirstLogonCommands>
    </component>
  </settings>
</unattend>
"#,
        locale = options.locale,
        computer_name = windows_computer_name(&manifest.spec.name),
    )
}

fn install_agent_script() -> &'static str {
    r#"$ErrorActionPreference = 'Stop'
$AgentSource = Join-Path $PSScriptRoot 'lsw-agent.exe'
if (-not (Test-Path -LiteralPath $AgentSource -PathType Leaf)) {
    Write-Warning 'lsw-agent.exe is not present on the LSW seed. Install it manually and rerun this script.'
    exit 0
}

$InstallRoot = Join-Path $env:ProgramFiles 'LSW'
$DataRoot = Join-Path $env:ProgramData 'LSW'
New-Item -ItemType Directory -Force -Path $InstallRoot, $DataRoot | Out-Null
$AgentTarget = Join-Path $InstallRoot 'lsw-agent.exe'
$TokenTarget = Join-Path $DataRoot 'agent.token'
Copy-Item -LiteralPath $AgentSource -Destination $AgentTarget -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'agent.token') -Destination $TokenTarget -Force

$Acl = New-Object System.Security.AccessControl.FileSecurity
$Acl.SetAccessRuleProtection($true, $false)
$Rights = [System.Security.AccessControl.FileSystemRights]::FullControl
$Allow = [System.Security.AccessControl.AccessControlType]::Allow
$Current = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
$System = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-18')
$Administrators = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-32-544')
foreach ($Identity in @($Current, $System, $Administrators)) {
    $Acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($Identity, $Rights, $Allow)))
}
Set-Acl -LiteralPath $TokenTarget -AclObject $Acl

$AgentCommand = ('"{0}" --token-file "{1}"' -f $AgentTarget, $TokenTarget)
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
New-ItemProperty -Path $RunKey -Name 'LSWAgent' -Value $AgentCommand -PropertyType String -Force | Out-Null
if (-not (Get-NetFirewallRule -DisplayName 'LSW Guest Agent' -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -DisplayName 'LSW Guest Agent' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 5040 -RemoteAddress 10.0.2.2 | Out-Null
}

& (Join-Path $PSScriptRoot 'apply-profile.ps1')
Start-Process -FilePath $AgentTarget -ArgumentList @('--token-file', $TokenTarget) -WindowStyle Hidden
"#
}

fn profile_script(manifest: &InstanceManifest) -> String {
    let plan = CustomizationPlan::for_profile(manifest.spec.profile);
    let patterns = plan
        .remove_provisioned_appx_patterns
        .iter()
        .map(|pattern| format!("    '{pattern}'"))
        .collect::<Vec<_>>()
        .join(",\r\n");
    let removal = if patterns.is_empty() {
        "Write-Host 'LSW profile does not remove provisioned applications.'".to_owned()
    } else {
        format!(
            r#"$RemoveNames = @(
{patterns}
)
Get-AppxProvisionedPackage -Online | Where-Object {{ $RemoveNames -contains $_.DisplayName }} | ForEach-Object {{
    Write-Host ('Removing optional provisioned package: ' + $_.DisplayName)
    Remove-AppxProvisionedPackage -Online -PackageName $_.PackageName -AllUsers | Out-Null
}}"#
        )
    };
    let compact = if plan.compact_os {
        "compact.exe /CompactOS:always"
    } else {
        "Write-Host 'CompactOS not requested for this profile.'"
    };
    format!(
        "$ErrorActionPreference = 'Stop'\r\nWrite-Host 'Applying LSW {} profile.'\r\n{}\r\n{}\r\n",
        manifest.spec.profile, removal, compact
    )
}

fn seed_readme(manifest: &InstanceManifest, options: &InstallSeedOptions) -> String {
    format!(
        "LSW installation seed\r\n\r\nInstance: {}\r\nProfile: {}\r\nLocale: {}\r\n\r\nThis seed contains no Windows image, product key, or activation data.\r\nThe answer file records the user's prior license acceptance but does not hide OOBE, create an account, or bypass activation.\r\n{}\r\n",
        manifest.spec.name,
        manifest.spec.profile,
        options.locale,
        if options.agent_binary.is_some() {
            "lsw-agent.exe is included and will be installed at first administrative logon."
        } else {
            "lsw-agent.exe is not included. Copy a Windows x64 agent build to lsw\\lsw-agent.exe before installation."
        }
    )
}

fn validate_locale(locale: &str) -> Result<()> {
    if (2..=20).contains(&locale.len())
        && locale
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && locale.bytes().any(|byte| byte == b'-')
    {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "locale",
            reason: "must look like en-US or zh-HK".to_owned(),
        })
    }
}

fn windows_computer_name(instance: &str) -> String {
    let mut name = format!("LSW-{}", instance.to_ascii_uppercase())
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(15)
        .collect::<String>();
    while name.ends_with('-') {
        name.pop();
    }
    name
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
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-seed-test-{nonce}"));
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
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        (root, manifest)
    }

    #[test]
    fn guided_seed_keeps_oobe_and_omits_product_keys() {
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
        assert!(!answer.contains("HideEULAPage"));
        assert!(!answer.contains("WillWipeDisk"));
        fs::remove_dir_all(root).expect("fixture should be removed");
    }

    #[test]
    fn unattended_seed_is_explicit_and_atomic() {
        let (root, manifest) = fixture();
        let options = InstallSeedOptions {
            locale: "zh-HK".to_owned(),
            unattended_image_index: Some(3),
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
