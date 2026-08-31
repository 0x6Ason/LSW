// SPDX-License-Identifier: GPL-3.0-or-later

use super::unattend::SETUP_ACCOUNT_NAME;
use super::InstallSeedOptions;
use crate::{
    InstanceManifest, AGENT_GUEST_PORT, LICENSE_HELPER_GUEST_PORT, MAINTENANCE_HELPER_GUEST_PORT,
    USER_HELPER_GUEST_PORT,
};

pub(super) fn install_agent_script() -> String {
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
# Explicit hibernation remains available, but a reusable VM must not carry the
# kernel and removable-media cache across an ordinary shutdown.
& reg.exe add 'HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Power' /v HiberbootEnabled /t REG_DWORD /d 0 /f | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw 'Windows Fast Startup could not be disabled.'
}
Set-LswSetupStage 'installing-agent'
$SetupCompleteContents = @'
@echo off
setlocal EnableExtensions
>"%ProgramData%\LSW\setup-progress.marker.tmp" echo cleanup
move /y "%ProgramData%\LSW\setup-progress.marker.tmp" "%ProgramData%\LSW\setup-progress.marker" >nul
reg.exe add "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon" /v AutoAdminLogon /t REG_SZ /d 0 /f >nul
if errorlevel 1 exit /b 70
for %%V in (DefaultUserName DefaultDomainName DefaultPassword) do reg.exe delete "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon" /v %%V /f >nul 2>&1
reg.exe add "HKLM\SOFTWARE\Policies\Microsoft\Windows\OOBE" /v DisablePrivacyExperience /t REG_DWORD /d 1 /f >nul
if errorlevel 1 exit /b 74
reg.exe add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE" /v LaunchUserOOBE /t REG_DWORD /d 0 /f >nul
if errorlevel 1 exit /b 75
reg.exe add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE" /v DefaultAccountAction /t REG_DWORD /d 0 /f >nul
if errorlevel 1 exit /b 76
for %%V in (DefaultAccountSAMName DefaultAccountSID) do reg.exe delete "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE" /v %%V /f >nul 2>&1
net.exe user "__LSW_SETUP_ACCOUNT__" /delete >nul 2>&1
if errorlevel 1 exit /b 71
net.exe user "defaultuser0" /delete >nul 2>&1
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
$MaintenanceServiceName = 'LSWMaintenanceHelper'
$MaintenanceServiceDisplayName = 'LSW Windows Storage Maintenance Helper'
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
foreach ($ServiceToStop in @($ServiceName, $LicenseServiceName, $UserServiceName, $MaintenanceServiceName)) {
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

$MaintenanceCommand = ('"{0}" --maintenance-helper --token-file "{1}" --listen 127.0.0.1:__LSW_MAINTENANCE_HELPER_PORT__' -f $AgentTarget, $TokenTarget)
$ScMaintenanceCommand = ConvertTo-ScBinaryPathArgument -Command $MaintenanceCommand
$ExistingMaintenanceService = Get-Service -Name $MaintenanceServiceName -ErrorAction SilentlyContinue
if ($null -eq $ExistingMaintenanceService) {
    Invoke-Sc @(
        'create', $MaintenanceServiceName,
        'binPath=', $ScMaintenanceCommand,
        'DisplayName=', $MaintenanceServiceDisplayName,
        'start=', 'demand',
        'obj=', 'LocalSystem'
    )
}
Invoke-Sc @(
    'config', $MaintenanceServiceName,
    'binPath=', $ScMaintenanceCommand,
    'DisplayName=', $MaintenanceServiceDisplayName,
    'start=', 'demand',
    'obj=', 'LocalSystem'
)
Invoke-Sc @('sidtype', $MaintenanceServiceName, 'unrestricted')
Invoke-Sc @('description', $MaintenanceServiceName, 'Performs authenticated fixed Windows maintenance operations and retains an active live-folder mapping.')
$MaintenanceServiceSddl = 'D:(A;;CCLCSWRPWPDTLOCRSDRCWDWO;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPLOCRRC;;;{0})' -f $ServiceIdentity.Value
Invoke-Sc @('sdset', $MaintenanceServiceName, $MaintenanceServiceSddl)

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
Set-LswSetupStage 'applying-profile'
& (Join-Path $PSScriptRoot 'apply-profile.ps1')
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
    .replace(
        "__LSW_MAINTENANCE_HELPER_PORT__",
        &MAINTENANCE_HELPER_GUEST_PORT.to_string(),
    )
    .replace("__LSW_SETUP_ACCOUNT__", SETUP_ACCOUNT_NAME)
}

pub(super) fn seed_readme(manifest: &InstanceManifest, options: &InstallSeedOptions) -> String {
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
