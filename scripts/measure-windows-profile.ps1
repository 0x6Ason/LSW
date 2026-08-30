# SPDX-License-Identifier: GPL-3.0-or-later

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('vanilla', 'slim')]
    [string]$Profile,
    [ValidateRange(0, 300)]
    [int]$SettleSeconds = 0,
    [string]$ReportRoot = 'C:\ProgramData\LSW\profile'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Assert-LswMeasurement {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) {
        throw $Message
    }
}

$ReportPath = Join-Path $ReportRoot 'report.json'
$AfterPath = Join-Path $ReportRoot 'after.json'
Assert-LswMeasurement (Test-Path -LiteralPath $ReportPath -PathType Leaf) 'Profile report is absent.'
Assert-LswMeasurement (Test-Path -LiteralPath $AfterPath -PathType Leaf) 'Profile after-inventory is absent.'
$Report = Get-Content -LiteralPath $ReportPath -Raw | ConvertFrom-Json
$After = Get-Content -LiteralPath $AfterPath -Raw | ConvertFrom-Json
$ExpectedRevision = if ($Profile -eq 'slim') { 'slim-v2' } else { 'vanilla-v2' }
Assert-LswMeasurement ([int]$Report.schema_version -eq 2) 'Profile report schema is not version 2.'
Assert-LswMeasurement ([string]$Report.profile -ceq $Profile) 'Profile report name does not match.'
Assert-LswMeasurement ([string]$Report.revision -ceq $ExpectedRevision) 'Profile report revision does not match.'
Assert-LswMeasurement ([string]$Report.outcome -ceq 'success') 'Profile report did not succeed.'
Assert-LswMeasurement ($null -ne $After.all_optional_features) 'Complete optional-feature inventory is absent.'
Assert-LswMeasurement ($null -ne $After.all_services) 'Complete service inventory is absent.'
Assert-LswMeasurement ($null -ne $After.all_startup_entries) 'Complete startup inventory is absent.'
if ($Profile -eq 'vanilla') {
    $ExpectedTargetProperties = @(
        'appx',
        'optional_features',
        'services',
        'machine_policies',
        'default_user_policies',
        'uninstall_onedrive',
        'compact_os'
    )
    foreach ($Property in $ExpectedTargetProperties) {
        Assert-LswMeasurement ($Report.targets.PSObject.Properties.Name -contains $Property) `
            "Vanilla profile target field is absent: $Property"
    }
    foreach ($Collection in @(
        $Report.targets.appx,
        $Report.targets.optional_features,
        $Report.targets.services,
        $Report.targets.machine_policies,
        $Report.targets.default_user_policies
    )) {
        Assert-LswMeasurement (@($Collection).Count -eq 0) 'Vanilla profile contains mutation targets.'
    }
    Assert-LswMeasurement (-not [bool]$Report.targets.uninstall_onedrive) `
        'Vanilla profile requests OneDrive removal.'
    Assert-LswMeasurement (-not [bool]$Report.targets.compact_os) `
        'Vanilla profile requests CompactOS.'
    $Operations = @($Report.operations)
    Assert-LswMeasurement ($Operations.Count -eq 1) 'Vanilla profile operation report is ambiguous.'
    Assert-LswMeasurement (
        [string]$Operations[0].kind -ceq 'profile' -and
        [string]$Operations[0].result -ceq 'unchanged'
    ) 'Vanilla profile reports a mutating operation.'
}

if ($SettleSeconds -ne 0) {
    Start-Sleep -Seconds $SettleSeconds
}

$OperatingSystem = Get-CimInstance Win32_OperatingSystem
$ComputerSystem = Get-CimInstance Win32_ComputerSystem
$SystemVolume = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='C:'"
$Processes = @(Get-Process | Where-Object { $_.Id -ne 0 } | Sort-Object ProcessName, Id)
$Services = @(Get-CimInstance Win32_Service | Sort-Object Name)
$CommittedBytes = [uint64](Get-Counter '\Memory\Committed Bytes').CounterSamples[0].CookedValue
$WorkingSetBytes = [uint64](($Processes | Measure-Object -Property WorkingSet64 -Sum).Sum)
$Uac = Get-ItemProperty -LiteralPath 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
$Defender = Get-MpComputerStatus

$Result = [ordered]@{
    schema_version = 1
    profile = $Profile
    revision = $ExpectedRevision
    collected_at_utc = [DateTime]::UtcNow.ToString('o')
    last_boot_utc = $OperatingSystem.LastBootUpTime.ToUniversalTime().ToString('o')
    windows_build = [string]$OperatingSystem.BuildNumber
    edition = [string](Get-ItemProperty -LiteralPath 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion').EditionID
    total_physical_bytes = [uint64]$ComputerSystem.TotalPhysicalMemory
    process_count = $Processes.Count
    processes = @($Processes | Select-Object ProcessName, Id, WorkingSet64)
    committed_bytes = $CommittedBytes
    working_set_bytes = $WorkingSetBytes
    system_volume_used_bytes = [uint64]($SystemVolume.Size - $SystemVolume.FreeSpace)
    provisioned_appx_count = @($After.provisioned_appx).Count
    installed_appx_count = @($After.installed_appx).Count
    provisioned_appx = @($After.provisioned_appx)
    installed_appx = @($After.installed_appx)
    optional_features = @($After.all_optional_features)
    running_services = @($Services | Where-Object { $_.State -eq 'Running' } |
        Select-Object Name, StartMode)
    startup_entries = @($After.all_startup_entries)
    uac_enabled = ([int]$Uac.EnableLUA -eq 1)
    defender_enabled = [bool]$Defender.AMServiceEnabled
    report_sha256 = (Get-FileHash -LiteralPath $ReportPath -Algorithm SHA256).Hash.ToLowerInvariant()
    outcome = 'passed'
}
Assert-LswMeasurement ([bool]$Result.uac_enabled) 'UAC is disabled.'
Assert-LswMeasurement ([bool]$Result.defender_enabled) 'Microsoft Defender is disabled.'
[Console]::Out.Write(($Result | ConvertTo-Json -Depth 8 -Compress))
