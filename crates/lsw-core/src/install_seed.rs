// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{
    CustomizationPlan, InstanceManifest, LswError, Result, AGENT_GUEST_PORT,
    LICENSE_HELPER_GUEST_PORT, USER_HELPER_GUEST_PORT,
};

const MAX_AGENT_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const LICENSE_HELPER_SCRIPT: &[u8] = include_bytes!("../assets/license-helper.ps1");
const SETUP_ACCOUNT_NAME: &str = "LSWSetup";
pub(crate) const OFFLINE_PROFILE_MARKER_NAME: &str = "offline-profile-applied.marker";

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
            profile_script(manifest)?.into_bytes(),
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

fn autounattend(
    manifest: &InstanceManifest,
    options: &InstallSeedOptions,
    setup_account_password_value: &str,
) -> String {
    let selection = options
        .unattended_image_name
        .as_ref()
        .map(|name| ("/IMAGE/NAME", xml_escape(name)))
        .or_else(|| {
            options
                .unattended_image_index
                .map(|index| ("/IMAGE/INDEX", index.to_string()))
        });
    let disk = selection
        .map(|(key, value)| {
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
          <InstallFrom><MetaData wcm:action="add"><Key>{key}</Key><Value>{value}</Value></MetaData></InstallFrom>
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
  <settings pass="specialize">
    <component name="Microsoft-Windows-Deployment" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <RunSynchronous>
        <RunSynchronousCommand wcm:action="add">
          <Order>1</Order><Description>Install LSW guest services</Description>
          <Path>cmd.exe /d /c for %D in (D E F G H I J K L M N O P Q R S T U V W X Y Z) do @if exist "%D:\lsw\install-agent.ps1" powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%D:\lsw\install-agent.ps1"</Path>
        </RunSynchronousCommand>
      </RunSynchronous>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <ComputerName>{computer_name}</ComputerName>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <InputLocale>{locale}</InputLocale><SystemLocale>{locale}</SystemLocale><UILanguage>{locale}</UILanguage><UserLocale>{locale}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <RegisteredOrganization>LSW</RegisteredOrganization>
      <RegisteredOwner>LSW User</RegisteredOwner>
      <TimeZone>UTC</TimeZone>
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Password><Value>{setup_account_password_value}</Value><PlainText>false</PlainText></Password>
            <Description>Temporary account removed when unattended setup completes</Description>
            <DisplayName>LSW Setup</DisplayName><Group>Users</Group><Name>{setup_account_name}</Name>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
    </component>
  </settings>
</unattend>
"#,
        locale = options.locale,
        computer_name = windows_computer_name(&manifest.spec.name),
        setup_account_name = SETUP_ACCOUNT_NAME,
        setup_account_password_value = setup_account_password_value,
    )
}

fn install_agent_script() -> String {
    r#"$ErrorActionPreference = 'Stop'
$AgentSource = Join-Path $PSScriptRoot 'lsw-agent.exe'
$TokenSource = Join-Path $PSScriptRoot 'agent.token'
$LicenseScriptSource = Join-Path $PSScriptRoot 'license-helper.ps1'
$InstallRoot = Join-Path $env:ProgramFiles 'LSW'
$DataRoot = Join-Path $env:ProgramData 'LSW'
$SetupScriptsRoot = Join-Path $env:SystemRoot 'Setup\Scripts'
$SetupCompletePath = Join-Path $SetupScriptsRoot 'SetupComplete.cmd'
$SetupCompleteMarker = Join-Path $DataRoot 'setup-complete.marker'
$SetupCompleteMarkerTemporary = Join-Path $DataRoot 'setup-complete.marker.tmp'
$SetupProgressPath = Join-Path $DataRoot 'setup-progress.marker'
$SetupProgressTemporary = Join-Path $DataRoot 'setup-progress.marker.tmp'

function Set-LswSetupStage {
    param([Parameter(Mandatory = $true)][string] $Stage)

    [System.IO.File]::WriteAllText($SetupProgressTemporary, $Stage, [System.Text.Encoding]::ASCII)
    Move-Item -LiteralPath $SetupProgressTemporary -Destination $SetupProgressPath -Force
}

New-Item -ItemType Directory -Force -Path $DataRoot, $SetupScriptsRoot | Out-Null
Remove-Item -LiteralPath $SetupCompleteMarker, $SetupCompleteMarkerTemporary, $SetupProgressTemporary -Force -ErrorAction SilentlyContinue
Set-LswSetupStage 'installing-agent'
$SetupCompleteContents = @'
@echo off
setlocal EnableExtensions
>"%ProgramData%\LSW\setup-progress.marker.tmp" echo cleanup
move /y "%ProgramData%\LSW\setup-progress.marker.tmp" "%ProgramData%\LSW\setup-progress.marker" >nul
reg.exe add "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon" /v AutoAdminLogon /t REG_SZ /d 0 /f >nul
if errorlevel 1 exit /b 70
for %%V in (DefaultUserName DefaultDomainName DefaultPassword) do reg.exe delete "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon" /v %%V /f >nul 2>&1
net.exe user "__LSW_SETUP_ACCOUNT__" /delete >nul 2>&1
if errorlevel 1 exit /b 71
del /f /q "%WINDIR%\Panther\unattend.xml" >nul 2>&1
del /f /q "%WINDIR%\Panther\Unattend\unattend.xml" >nul 2>&1
rd /s /q "%ProgramData%\LSW\setup" >nul 2>&1
>"%ProgramData%\LSW\setup-complete.marker.tmp" echo LSW-SETUP-COMPLETE
move /y "%ProgramData%\LSW\setup-complete.marker.tmp" "%ProgramData%\LSW\setup-complete.marker" >nul
if errorlevel 1 exit /b 72
>"%ProgramData%\LSW\setup-progress.marker.tmp" echo complete
move /y "%ProgramData%\LSW\setup-progress.marker.tmp" "%ProgramData%\LSW\setup-progress.marker" >nul
if errorlevel 1 exit /b 73
del /f /q "%~f0" >nul 2>&1
exit /b 0
'@
[System.IO.File]::WriteAllText(
    $SetupCompletePath,
    $SetupCompleteContents + "`r`n",
    [System.Text.Encoding]::ASCII
)

if (-not (Test-Path -LiteralPath $AgentSource -PathType Leaf)) {
    Write-Warning 'lsw-agent.exe is not present on the LSW seed. Unattended setup will finish without terminal access.'
    exit 0
}
if (-not (Test-Path -LiteralPath $LicenseScriptSource -PathType Leaf)) {
    throw 'license-helper.ps1 is not present on the LSW seed.'
}

$ServiceName = 'LSWAgent'
$ServiceDisplayName = 'LSW Guest Agent'
$ServiceAccount = 'NT SERVICE\LSWAgent'
$LicenseServiceName = 'LSWLicenseHelper'
$LicenseServiceDisplayName = 'LSW Windows Activation Helper'
$UserServiceName = 'LSWUserHelper'
$UserServiceDisplayName = 'LSW Windows Account Helper'
$ScExe = Join-Path $env:SystemRoot 'System32\sc.exe'

function ConvertTo-ScBinaryPathArgument {
    param([Parameter(Mandatory = $true)][string] $Command)

    # Windows PowerShell 5.1 does not escape embedded quotes when serializing a
    # native argument with spaces. sc.exe needs them in the binPath value.
    if ($PSVersionTable.PSVersion.Major -le 5) {
        return $Command.Replace('"', '\"')
    }
    return $Command
}

function Invoke-Sc {
    param(
        [Parameter(Mandatory = $true)]
        [string[]] $ArgumentList
    )

    & $ScExe @ArgumentList
    $ExitCode = $LASTEXITCODE
    if ($ExitCode -ne 0) {
        throw ('sc.exe {0} failed with exit code {1}.' -f $ArgumentList[0], $ExitCode)
    }
}

Set-LswSetupStage 'configuring-services'
New-Item -ItemType Directory -Force -Path $InstallRoot, $DataRoot | Out-Null
$AgentTarget = Join-Path $InstallRoot 'lsw-agent.exe'
$LicenseScriptTarget = Join-Path $InstallRoot 'license-helper.ps1'
$TokenTarget = Join-Path $DataRoot 'agent.token'
$LogTarget = Join-Path $DataRoot 'agent.log'
$ExistingService = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
foreach ($ServiceToStop in @($ServiceName, $LicenseServiceName, $UserServiceName)) {
    $ExistingServiceToStop = Get-Service -Name $ServiceToStop -ErrorAction SilentlyContinue
    if ($null -ne $ExistingServiceToStop -and $ExistingServiceToStop.Status -ne 'Stopped') {
        if ($ExistingServiceToStop.Status -ne 'StopPending') {
            Invoke-Sc @('stop', $ServiceToStop)
        }
        $ExistingServiceToStop.WaitForStatus(
            [System.ServiceProcess.ServiceControllerStatus]::Stopped,
            [TimeSpan]::FromSeconds(30)
        )
        $ExistingServiceToStop.Refresh()
        if ($ExistingServiceToStop.Status -ne 'Stopped') {
            throw ('Service {0} did not stop before the agent was replaced.' -f $ServiceToStop)
        }
    }
}

$AgentTargetFullPath = [System.IO.Path]::GetFullPath($AgentTarget)
$StaleAgents = Get-CimInstance -ClassName Win32_Process -Filter "Name = 'lsw-agent.exe'"
foreach ($StaleAgent in $StaleAgents) {
    if ($null -eq $StaleAgent.ExecutablePath) {
        continue
    }
    $StalePath = [System.IO.Path]::GetFullPath($StaleAgent.ExecutablePath)
    if ([string]::Equals($StalePath, $AgentTargetFullPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        $Termination = Invoke-CimMethod -InputObject $StaleAgent -MethodName Terminate
        if ($Termination.ReturnValue -ne 0) {
            throw ('Failed to stop stale LSW agent process {0} (return code {1}).' -f $StaleAgent.ProcessId, $Termination.ReturnValue)
        }
        $TerminationDeadline = [DateTime]::UtcNow.AddSeconds(15)
        do {
            $RemainingAgent = Get-CimInstance -ClassName Win32_Process -Filter (
                'ProcessId = {0}' -f $StaleAgent.ProcessId
            )
            if ($null -eq $RemainingAgent) {
                break
            }
            if ($null -ne $RemainingAgent.ExecutablePath) {
                $RemainingPath = [System.IO.Path]::GetFullPath($RemainingAgent.ExecutablePath)
                if (-not [string]::Equals(
                    $RemainingPath,
                    $AgentTargetFullPath,
                    [System.StringComparison]::OrdinalIgnoreCase
                )) {
                    break
                }
            }
            if ([DateTime]::UtcNow -ge $TerminationDeadline) {
                throw ('Timed out waiting for stale LSW agent process {0} to exit.' -f $StaleAgent.ProcessId)
            }
            Start-Sleep -Milliseconds 100
        } while ($true)
    }
}

$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
Remove-ItemProperty -Path $RunKey -Name 'LSWAgent' -ErrorAction SilentlyContinue
Copy-Item -LiteralPath $AgentSource -Destination $AgentTarget -Force
Copy-Item -LiteralPath $LicenseScriptSource -Destination $LicenseScriptTarget -Force
$AgentCommand = ('"{0}" --service --token-file "{1}"' -f $AgentTarget, $TokenTarget)
$ScAgentCommand = ConvertTo-ScBinaryPathArgument -Command $AgentCommand
if ($null -eq $ExistingService) {
    Invoke-Sc @(
        'create', $ServiceName,
        'binPath=', $ScAgentCommand,
        'DisplayName=', $ServiceDisplayName,
        'start=', 'auto'
    )
}
Invoke-Sc @(
    'config', $ServiceName,
    'binPath=', $ScAgentCommand,
    'DisplayName=', $ServiceDisplayName,
    'start=', 'auto'
)
Invoke-Sc @('config', $ServiceName, 'obj=', $ServiceAccount)
$ConfiguredService = Get-CimInstance -ClassName Win32_Service -Filter "Name = 'LSWAgent'"
if ($null -eq $ConfiguredService -or -not [string]::Equals(
    $ConfiguredService.StartName,
    $ServiceAccount,
    [System.StringComparison]::OrdinalIgnoreCase
)) {
    throw 'SCM did not assign the LSWAgent virtual service account.'
}
if ($ConfiguredService.PathName -cne $AgentCommand) {
    throw 'SCM did not preserve the quoted LSWAgent binary command.'
}
Invoke-Sc @('sidtype', $ServiceName, 'unrestricted')
Invoke-Sc @('description', $ServiceName, 'Provides authenticated command execution for an LSW Windows guest.')
Invoke-Sc @(
    'failure', $ServiceName,
    'reset=', '86400',
    'actions=', 'restart/5000/restart/15000/restart/60000'
)
Invoke-Sc @('failureflag', $ServiceName, '1')

$System = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-18')
$Administrators = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-32-544')
$ServiceIdentity = (New-Object System.Security.Principal.NTAccount($ServiceAccount)).Translate(
    [System.Security.Principal.SecurityIdentifier]
)
$Allow = [System.Security.AccessControl.AccessControlType]::Allow

$LicenseCommand = ('"{0}" --license-helper --token-file "{1}" --listen 127.0.0.1:__LSW_LICENSE_HELPER_PORT__' -f $AgentTarget, $TokenTarget)
$ScLicenseCommand = ConvertTo-ScBinaryPathArgument -Command $LicenseCommand
$ExistingLicenseService = Get-Service -Name $LicenseServiceName -ErrorAction SilentlyContinue
if ($null -eq $ExistingLicenseService) {
    Invoke-Sc @(
        'create', $LicenseServiceName,
        'binPath=', $ScLicenseCommand,
        'DisplayName=', $LicenseServiceDisplayName,
        'start=', 'demand',
        'obj=', 'LocalSystem'
    )
}
Invoke-Sc @(
    'config', $LicenseServiceName,
    'binPath=', $ScLicenseCommand,
    'DisplayName=', $LicenseServiceDisplayName,
    'start=', 'demand',
    'obj=', 'LocalSystem'
)
Invoke-Sc @('sidtype', $LicenseServiceName, 'unrestricted')
Invoke-Sc @('description', $LicenseServiceName, 'Performs authenticated, on-demand Windows WMI activation operations for LSW.')
$LicenseServiceSddl = 'D:(A;;CCLCSWRPWPDTLOCRSDRCWDWO;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPLOCRRC;;;{0})' -f $ServiceIdentity.Value
Invoke-Sc @('sdset', $LicenseServiceName, $LicenseServiceSddl)

$UserCommand = ('"{0}" --user-helper --token-file "{1}" --listen 127.0.0.1:__LSW_USER_HELPER_PORT__' -f $AgentTarget, $TokenTarget)
$ScUserCommand = ConvertTo-ScBinaryPathArgument -Command $UserCommand
$ExistingUserService = Get-Service -Name $UserServiceName -ErrorAction SilentlyContinue
if ($null -eq $ExistingUserService) {
    Invoke-Sc @(
        'create', $UserServiceName,
        'binPath=', $ScUserCommand,
        'DisplayName=', $UserServiceDisplayName,
        'start=', 'demand',
        'obj=', 'LocalSystem'
    )
}
Invoke-Sc @(
    'config', $UserServiceName,
    'binPath=', $ScUserCommand,
    'DisplayName=', $UserServiceDisplayName,
    'start=', 'demand',
    'obj=', 'LocalSystem'
)
Invoke-Sc @('sidtype', $UserServiceName, 'unrestricted')
Invoke-Sc @('description', $UserServiceName, 'Performs one authenticated local-account operation for LSW and exits.')
$UserServiceSddl = 'D:(A;;CCLCSWRPWPDTLOCRSDRCWDWO;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPLOCRRC;;;{0})' -f $ServiceIdentity.Value
Invoke-Sc @('sdset', $UserServiceName, $UserServiceSddl)

$DirectoryAcl = New-Object System.Security.AccessControl.DirectorySecurity
$DirectoryAcl.SetAccessRuleProtection($true, $false)
$DirectoryAcl.SetOwner($Administrators)
$Inherit = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
$NoPropagation = [System.Security.AccessControl.PropagationFlags]::None
$FullControl = [System.Security.AccessControl.FileSystemRights]::FullControl
$Modify = [System.Security.AccessControl.FileSystemRights]::Modify
foreach ($Identity in @($System, $Administrators)) {
    $DirectoryAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        $Identity, $FullControl, $Inherit, $NoPropagation, $Allow
    )))
}
$DirectoryAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
    $ServiceIdentity, $Modify, $Inherit, $NoPropagation, $Allow
)))
Set-Acl -LiteralPath $DataRoot -AclObject $DirectoryAcl

Copy-Item -LiteralPath $TokenSource -Destination $TokenTarget -Force

$Acl = New-Object System.Security.AccessControl.FileSecurity
$Acl.SetAccessRuleProtection($true, $false)
$Acl.SetOwner($Administrators)
foreach ($Identity in @($System, $Administrators)) {
    $Acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        $Identity, $FullControl, $Allow
    )))
}
$Acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
    $ServiceIdentity, $Modify, $Allow
)))
Set-Acl -LiteralPath $TokenTarget -AclObject $Acl

if (-not (Test-Path -LiteralPath $LogTarget -PathType Leaf)) {
    New-Item -ItemType File -Path $LogTarget | Out-Null
}
$LogAcl = New-Object System.Security.AccessControl.FileSecurity
$LogAcl.SetAccessRuleProtection($true, $false)
$LogAcl.SetOwner($Administrators)
foreach ($Identity in @($System, $Administrators)) {
    $LogAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        $Identity, $FullControl, $Allow
    )))
}
$LogAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
    $ServiceIdentity, [System.Security.AccessControl.FileSystemRights]::Modify, $Allow
)))
Set-Acl -LiteralPath $LogTarget -AclObject $LogAcl

# A pre-applied image stages this script below ProgramData. Remove the staged
# token after the protected service copy exists. Read-only removable install
# media remains untouched.
$SetupRootFullPath = [System.IO.Path]::GetFullPath($PSScriptRoot).TrimEnd('\') + '\'
$DataRootFullPath = [System.IO.Path]::GetFullPath($DataRoot).TrimEnd('\') + '\'
$TokenSourceFullPath = [System.IO.Path]::GetFullPath($TokenSource)
$TokenTargetFullPath = [System.IO.Path]::GetFullPath($TokenTarget)
if ($SetupRootFullPath.StartsWith($DataRootFullPath, [System.StringComparison]::OrdinalIgnoreCase) -and
    -not [string]::Equals($TokenSourceFullPath, $TokenTargetFullPath, [System.StringComparison]::OrdinalIgnoreCase)) {
    Remove-Item -LiteralPath $TokenSource -Force
}

if (-not (Get-NetFirewallRule -DisplayName 'LSW Guest Agent' -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -DisplayName 'LSW Guest Agent' -Direction Inbound -Action Allow -Protocol TCP -LocalPort __LSW_AGENT_GUEST_PORT__ -RemoteAddress 10.0.2.2 | Out-Null
}

Set-LswSetupStage 'applying-profile'
& (Join-Path $PSScriptRoot 'apply-profile.ps1')
Set-LswSetupStage 'starting-agent'
Invoke-Sc @('start', $ServiceName)
$StartedService = Get-Service -Name $ServiceName
$StartedService.WaitForStatus(
    [System.ServiceProcess.ServiceControllerStatus]::Running,
    [TimeSpan]::FromSeconds(30)
)
# Catch an immediate post-start failure before Windows Setup reports
# success. Configured recovery does not begin until five seconds later.
Start-Sleep -Milliseconds 500
$StartedService.Refresh()
if ($StartedService.Status -ne 'Running') {
    throw 'LSWAgent did not remain running after SCM started it.'
}
Set-LswSetupStage 'waiting-for-oobe'
"#
    .replace("__LSW_AGENT_GUEST_PORT__", &AGENT_GUEST_PORT.to_string())
    .replace(
        "__LSW_LICENSE_HELPER_PORT__",
        &LICENSE_HELPER_GUEST_PORT.to_string(),
    )
    .replace(
        "__LSW_USER_HELPER_PORT__",
        &USER_HELPER_GUEST_PORT.to_string(),
    )
    .replace("__LSW_SETUP_ACCOUNT__", SETUP_ACCOUNT_NAME)
}

fn profile_script(manifest: &InstanceManifest) -> Result<String> {
    let plan = CustomizationPlan::for_profile(manifest.spec.profile)?;
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
    Ok(format!(
        "$ErrorActionPreference = 'Stop'\r\n$OfflineProfileMarker = Join-Path $PSScriptRoot '{OFFLINE_PROFILE_MARKER_NAME}'\r\nif (Test-Path -LiteralPath $OfflineProfileMarker -PathType Leaf) {{\r\n    Write-Host 'LSW profile was already applied offline by WinPE.'\r\n    return\r\n}}\r\nWrite-Host 'Applying LSW {} profile.'\r\n{}\r\n{}\r\n",
        manifest.spec.profile, removal, compact
    ))
}

fn seed_readme(manifest: &InstanceManifest, options: &InstallSeedOptions) -> String {
    format!(
        "LSW installation seed\r\n\r\nInstance: {}\r\nProfile: {}\r\nLocale: {}\r\n\r\nThis seed contains no Windows image, product key, or activation data.\r\nThe answer file records the user's prior license acceptance and completes OOBE without automatic logon. A random one-shot local account is removed before setup is marked complete.\r\n{}\r\n",
        manifest.spec.name,
        manifest.spec.profile,
        options.locale,
        if options.agent_binary.is_some() {
            "lsw-agent.exe is included and will be installed as a boot-time Windows service during specialize."
        } else {
            "lsw-agent.exe is not included. Copy a Windows x64 agent build to lsw\\lsw-agent.exe before installation."
        }
    )
}

fn generate_setup_account_password() -> Result<String> {
    let mut random = [0_u8; 24];
    getrandom::getrandom(&mut random).map_err(|error| {
        LswError::Io(std::io::Error::other(format!(
            "the operating system random source failed: {error}"
        )))
    })?;

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut password = String::with_capacity(53);
    password.push_str("LsW!9");
    for byte in random {
        password.push(HEX[(byte >> 4) as usize] as char);
        password.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(password)
}

fn unattend_password_value(password: &str) -> String {
    let mut bytes = Vec::with_capacity((password.len() + "Password".len()) * 2);
    for code_unit in password.encode_utf16().chain("Password".encode_utf16()) {
        bytes.extend_from_slice(&code_unit.to_le_bytes());
    }
    base64_encode(&bytes)
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
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

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{InstanceSpec, NetworkMode, WindowsProfile};

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
        assert!(installer.contains("--license-helper --token-file"));
        assert!(installer.contains("$LicenseScriptSource"));
        assert!(installer.contains("$LicenseScriptTarget"));
        assert!(installer.contains(
            "Copy-Item -LiteralPath $LicenseScriptSource -Destination $LicenseScriptTarget -Force"
        ));
        assert!(installer.contains(&format!("--listen 127.0.0.1:{LICENSE_HELPER_GUEST_PORT}")));
        assert!(installer.contains(&format!("--listen 127.0.0.1:{USER_HELPER_GUEST_PORT}")));
        assert!(installer.contains(&format!("-LocalPort {AGENT_GUEST_PORT}")));
        assert!(installer.contains("$LogTarget = Join-Path $DataRoot 'agent.log'"));
        assert!(installer.contains("Set-Acl -LiteralPath $LogTarget"));
        assert!(installer.contains("'start=', 'demand'"));
        assert!(installer.contains("'obj=', 'LocalSystem'"));
        assert!(installer.contains("$LicenseServiceSddl"));
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
        assert!(installer.contains("Remove-Item -LiteralPath $TokenSource -Force"));
        assert!(installer.contains("$SetupRootFullPath.StartsWith($DataRootFullPath"));
        assert!(installer.contains("net.exe user \"LSWSetup\" /delete"));
        assert!(installer.contains("/v AutoAdminLogon /t REG_SZ /d 0 /f"));
        assert!(installer.contains("DefaultUserName DefaultDomainName DefaultPassword"));
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
        assert!(profile.contains("already applied offline by WinPE"));
        assert!(profile.contains("    return\r\n"));
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
