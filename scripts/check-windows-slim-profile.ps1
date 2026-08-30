# SPDX-License-Identifier: GPL-3.0-or-later

[CmdletBinding()]
param(
    [string]$ReportRoot = 'C:\ProgramData\LSW\profile',
    [ValidateRange(0, 300)]
    [int]$SettleSeconds = 0
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Assert-LswProfile {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) {
        throw $Message
    }
}

function Read-LswJson {
    param([string]$Name)
    $Path = Join-Path $ReportRoot $Name
    Assert-LswProfile (Test-Path -LiteralPath $Path -PathType Leaf) ("Missing profile artifact: " + $Path)
    Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}

function Test-LswAppPresent {
    param($Inventory, [string]$Name)
    if (@($Inventory.provisioned_appx | Where-Object { $_.DisplayName -ieq $Name }).Count -ne 0) {
        return $true
    }
    return @($Inventory.installed_appx | Where-Object { $_.Name -ieq $Name }).Count -ne 0
}

$Before = Read-LswJson 'before.json'
$After = Read-LswJson 'after.json'
$Report = Read-LswJson 'report.json'

Assert-LswProfile ([int]$Report.schema_version -eq 2) 'Profile report schema is not version 2.'
Assert-LswProfile ([string]$Report.profile -ceq 'slim') 'Profile report is not for slim.'
Assert-LswProfile ([string]$Report.revision -ceq 'slim-v2') 'Profile report revision is not slim-v2.'
Assert-LswProfile ([string]$Report.outcome -ceq 'success') 'Profile application did not report success.'
Assert-LswProfile ([bool]$Report.offline_marker) 'The slim-v2 WinPE servicing marker is absent.'

$AppxTargets = @($Report.targets.appx)
$FeatureTargets = @($Report.targets.optional_features)
$ServiceTargets = @($Report.targets.services)
$MachinePolicies = @($Report.targets.machine_policies)
$DefaultUserPolicies = @($Report.targets.default_user_policies)
$Operations = @($Report.operations)

Assert-LswProfile ($AppxTargets.Count -ge 40) 'The slim-v2 AppX allowlist is unexpectedly small.'
Assert-LswProfile ($FeatureTargets.Count -eq 1 -and [string]$FeatureTargets[0] -ceq 'Recall') 'Recall is not the exact optional-feature target.'
Assert-LswProfile ($ServiceTargets.Count -ge 10) 'The slim-v2 service policy set is unexpectedly small.'
Assert-LswProfile (($MachinePolicies.Count + $DefaultUserPolicies.Count) -ge 20) 'The slim-v2 policy set is unexpectedly small.'
Assert-LswProfile (@($Operations | Where-Object { $_.result -eq 'failed' }).Count -eq 0) 'A profile operation reported failure.'

foreach ($Target in $AppxTargets) {
    $TargetOperations = @($Operations | Where-Object {
        $_.target -ieq $Target.DisplayName -and $_.kind -in @('appx', 'installed-appx', 'provisioned-appx')
    })
    Assert-LswProfile ($TargetOperations.Count -ne 0) ("AppX target has no audit operation: " + $Target.DisplayName)
    Assert-LswProfile (@($After.provisioned_appx | Where-Object {
        $_.DisplayName -ieq $Target.DisplayName
    }).Count -eq 0) ("Targeted provisioned AppX survived: " + $Target.DisplayName)
    Assert-LswProfile (@($After.installed_appx | Where-Object {
        $_.Name -ieq $Target.DisplayName -or $Target.Families -icontains $_.PackageFamilyName
    }).Count -eq 0) ("Targeted installed AppX survived: " + $Target.DisplayName)
}

foreach ($Target in $FeatureTargets) {
    $Feature = @($After.optional_features | Where-Object { $_.FeatureName -ieq $Target })
    Assert-LswProfile ($Feature.Count -eq 1) ("Optional-feature audit is missing: " + $Target)
    Assert-LswProfile ([string]$Feature[0].State -eq 'DisabledWithPayloadRemoved') ("Optional-feature payload survived: " + $Target)
}

foreach ($Target in $ServiceTargets) {
    $Service = Get-CimInstance Win32_Service -Filter ("Name='" + $Target.Name + "'") -ErrorAction SilentlyContinue
    if ($null -eq $Service) {
        continue
    }
    if ($Target.Startup -eq 'Disabled') {
        Assert-LswProfile ($Service.StartMode -eq 'Disabled') ("Service is not disabled: " + $Target.Name)
        Assert-LswProfile ($Service.State -eq 'Stopped') ("Disabled service is running: " + $Target.Name)
    } else {
        Assert-LswProfile ($Service.StartMode -eq 'Manual') ("Demand service is not manual: " + $Target.Name)
    }
}

foreach ($Target in $MachinePolicies) {
    $Path = 'Registry::HKEY_LOCAL_MACHINE\' + $Target.Path
    $Property = Get-ItemProperty -LiteralPath $Path -Name $Target.Name -ErrorAction Stop
    $Value = $Property.PSObject.Properties[$Target.Name].Value
    Assert-LswProfile ([uint32]$Value -eq [uint32]$Target.Value) ("Machine policy readback failed: " + $Path + '\' + $Target.Name)
}

$RegistryOperations = @($Operations | Where-Object { $_.kind -eq 'registry-policy' })
Assert-LswProfile ($RegistryOperations.Count -eq ($MachinePolicies.Count + $DefaultUserPolicies.Count)) 'Not every declarative registry policy was audited.'

Assert-LswProfile ([bool]$Report.targets.uninstall_onedrive) 'OneDrive uninstallation is not part of the report contract.'
Assert-LswProfile (@($Operations | Where-Object { $_.kind -eq 'product' -and $_.target -eq 'OneDrive' }).Count -eq 1) 'OneDrive has no product-uninstaller audit operation.'
Assert-LswProfile (@($After.startup_entries).Count -eq 0) 'A OneDrive startup entry survived.'
Assert-LswProfile ($null -eq (Get-Process -Name OneDrive -ErrorAction SilentlyContinue)) 'OneDrive is running.'
foreach ($Root in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
    if ($null -ne $Root) {
        Assert-LswProfile (-not (Test-Path -LiteralPath (Join-Path $Root 'Microsoft OneDrive\OneDrive.exe') -PathType Leaf)) ("OneDrive client survived under " + $Root)
    }
}

foreach ($Name in @(
    'Microsoft.DesktopAppInstaller',
    'Microsoft.Paint',
    'Microsoft.ScreenSketch',
    'Microsoft.Windows.Photos',
    'Microsoft.WindowsCalculator',
    'Microsoft.WindowsNotepad',
    'Microsoft.WindowsStore',
    'Microsoft.WindowsTerminal'
)) {
    Assert-LswProfile (Test-LswAppPresent $After $Name) ("Protected AppX is absent: " + $Name)
}

foreach ($Name in @(
    'AppXSvc', 'BITS', 'ClipSVC', 'InstallService', 'msiserver', 'TrustedInstaller',
    'UsoSvc', 'WaaSMedicSvc', 'WinDefend', 'Winmgmt', 'wuauserv'
)) {
    $Service = Get-CimInstance Win32_Service -Filter ("Name='" + $Name + "'") -ErrorAction SilentlyContinue
    Assert-LswProfile ($null -ne $Service) ("Protected service is absent: " + $Name)
    Assert-LswProfile ($Service.StartMode -ne 'Disabled') ("Protected service is disabled: " + $Name)
}

$Uac = Get-ItemProperty -LiteralPath 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
Assert-LswProfile ([int]$Uac.EnableLUA -eq 1) 'UAC was disabled.'
foreach ($Command in @('compact.exe', 'conhost.exe', 'dism.exe', 'msiexec.exe', 'powershell.exe')) {
    Assert-LswProfile ($null -ne (Get-Command $Command -ErrorAction SilentlyContinue)) ("Protected command is absent: " + $Command)
}
$Defender = Get-MpComputerStatus
Assert-LswProfile ([bool]$Defender.AMServiceEnabled) 'Microsoft Defender antimalware service is not enabled.'

$EdgePath = Join-Path ${env:ProgramFiles(x86)} 'Microsoft\Edge\Application\msedge.exe'
Assert-LswProfile (Test-Path -LiteralPath $EdgePath -PathType Leaf) 'Microsoft Edge is absent.'
$WebViewRoot = Join-Path ${env:ProgramFiles(x86)} 'Microsoft\EdgeWebView\Application'
$WebView = @(Get-ChildItem -LiteralPath $WebViewRoot -Filter msedgewebview2.exe -File -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1)
Assert-LswProfile ($WebView.Count -eq 1) 'WebView2 runtime is absent.'

if ($SettleSeconds -ne 0) {
    Start-Sleep -Seconds $SettleSeconds
}
$OperatingSystem = Get-CimInstance Win32_OperatingSystem
$SystemVolume = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='C:'"
$Processes = @(Get-Process | Where-Object { $_.Id -ne 0 })
$CommittedBytes = [uint64](Get-Counter '\Memory\Committed Bytes').CounterSamples[0].CookedValue
$WorkingSetBytes = [uint64](($Processes | Measure-Object -Property WorkingSet64 -Sum).Sum)
$Startup = @(Get-CimInstance Win32_StartupCommand | Sort-Object Name, Location | Select-Object Name, Command, Location, User)
$Result = [ordered]@{
    schema_version = 1
    profile = 'slim'
    revision = [string]$Report.revision
    windows_build = [string]$OperatingSystem.BuildNumber
    process_count = $Processes.Count
    committed_bytes = $CommittedBytes
    working_set_bytes = $WorkingSetBytes
    system_volume_used_bytes = [uint64]($SystemVolume.Size - $SystemVolume.FreeSpace)
    provisioned_appx_count = @($After.provisioned_appx).Count
    installed_appx_count = @($After.installed_appx).Count
    startup_entry_count = $Startup.Count
    startup_entries = $Startup
    targeted_appx_count = $AppxTargets.Count
    targeted_service_count = $ServiceTargets.Count
    policy_count = $MachinePolicies.Count + $DefaultUserPolicies.Count
    outcome = 'passed'
}
[Console]::Out.Write(($Result | ConvertTo-Json -Depth 8 -Compress))
