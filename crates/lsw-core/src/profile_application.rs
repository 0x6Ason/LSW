// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{CustomizationPlan, ProductUninstaller, RegistryHive, Result, WindowsProfile};

pub(crate) fn profile_script(profile: WindowsProfile, offline_marker_name: &str) -> Result<String> {
    let plan = CustomizationPlan::for_profile(profile)?;
    if profile == WindowsProfile::Vanilla || profile == WindowsProfile::Secure {
        return Ok(format!(
            "$ErrorActionPreference = 'Stop'\r\nWrite-Host 'LSW {} leaves the Windows image unchanged.'\r\n",
            plan.revision
        ));
    }

    let script = SLIM_PROFILE_SCRIPT
        .replace("__LSW_PROFILE__", &profile.to_string())
        .replace("__LSW_PROFILE_REVISION__", &plan.revision)
        .replace("__LSW_OFFLINE_MARKER__", offline_marker_name)
        .replace("__LSW_APPX_TARGETS__", &appx_targets(&plan))
        .replace("__LSW_FEATURE_TARGETS__", &feature_targets(&plan))
        .replace("__LSW_SERVICE_TARGETS__", &service_targets(&plan))
        .replace(
            "__LSW_MACHINE_POLICIES__",
            &registry_targets(&plan, RegistryHive::Machine),
        )
        .replace(
            "__LSW_DEFAULT_USER_POLICIES__",
            &registry_targets(&plan, RegistryHive::DefaultUser),
        )
        .replace(
            "__LSW_UNINSTALL_ONEDRIVE__",
            if plan
                .product_uninstallers
                .contains(&ProductUninstaller::OneDrive)
            {
                "$true"
            } else {
                "$false"
            },
        )
        .replace(
            "__LSW_COMPACT_OS__",
            if plan.compact_os { "$true" } else { "$false" },
        )
        .replace('\n', "\r\n");
    Ok(script)
}

fn appx_targets(plan: &CustomizationPlan) -> String {
    plan.appx_removals
        .iter()
        .map(|removal| {
            let families = removal
                .package_family_names
                .iter()
                .map(|family| format!("'{family}'"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "    [pscustomobject]@{{ DisplayName = '{}'; Families = @({families}) }}",
                removal.display_name
            )
        })
        .collect::<Vec<_>>()
        .join(",\n")
}

fn feature_targets(plan: &CustomizationPlan) -> String {
    plan.optional_feature_removals
        .iter()
        .map(|feature| format!("    '{feature}'"))
        .collect::<Vec<_>>()
        .join(",\n")
}

fn service_targets(plan: &CustomizationPlan) -> String {
    plan.service_policies
        .iter()
        .map(|service| {
            format!(
                "    [pscustomobject]@{{ Name = '{}'; Startup = '{}' }}",
                service.name,
                service.startup.powershell_name()
            )
        })
        .collect::<Vec<_>>()
        .join(",\n")
}

fn registry_targets(plan: &CustomizationPlan, hive: RegistryHive) -> String {
    plan.registry_policies
        .iter()
        .filter(|policy| policy.hive == hive)
        .map(|policy| {
            format!(
                "    [pscustomobject]@{{ Path = '{}'; Name = '{}'; Value = {} }}",
                policy.path, policy.name, policy.value
            )
        })
        .collect::<Vec<_>>()
        .join(",\n")
}

const SLIM_PROFILE_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$ProfileName = '__LSW_PROFILE__'
$ProfileRevision = '__LSW_PROFILE_REVISION__'
$OfflineProfileMarker = Join-Path $PSScriptRoot '__LSW_OFFLINE_MARKER__'
$ReportRoot = Join-Path $env:ProgramData 'LSW\profile'
$AppxTargets = @(
__LSW_APPX_TARGETS__
)
$FeatureTargets = @(
__LSW_FEATURE_TARGETS__
)
$ServiceTargets = @(
__LSW_SERVICE_TARGETS__
)
$MachinePolicies = @(
__LSW_MACHINE_POLICIES__
)
$DefaultUserPolicies = @(
__LSW_DEFAULT_USER_POLICIES__
)
$UninstallOneDrive = __LSW_UNINSTALL_ONEDRIVE__
$EnableCompactOs = __LSW_COMPACT_OS__
$Operations = New-Object 'System.Collections.Generic.List[object]'

New-Item -ItemType Directory -Path $ReportRoot -Force | Out-Null

function Add-LswOperation {
    param([string]$Kind, [string]$Target, [string]$Result, [string]$Detail)
    $Operations.Add([pscustomobject]@{
        kind = $Kind
        target = $Target
        result = $Result
        detail = $Detail
    })
}

function Get-LswProvisionedApps {
    @(Get-AppxProvisionedPackage -Online | Sort-Object DisplayName, PackageName |
        Select-Object DisplayName, PackageName)
}

function Get-LswInstalledApps {
    @(Get-AppxPackage -AllUsers | Sort-Object Name, PackageFullName |
        Select-Object Name, PackageFullName, PackageFamilyName, NonRemovable)
}

function Get-LswFeatureInventory {
    $Inventory = @()
    foreach ($Target in $FeatureTargets) {
        $Feature = Get-WindowsOptionalFeature -Online -FeatureName $Target -ErrorAction SilentlyContinue
        if ($null -eq $Feature) {
            $Inventory += [pscustomobject]@{ FeatureName = $Target; State = 'NotPresent' }
        } else {
            $Inventory += [pscustomobject]@{ FeatureName = $Feature.FeatureName; State = [string]$Feature.State }
        }
    }
    @($Inventory)
}

function Get-LswServiceInventory {
    $Inventory = @()
    foreach ($Target in $ServiceTargets) {
        $Service = Get-CimInstance Win32_Service -Filter ("Name='" + $Target.Name + "'") -ErrorAction SilentlyContinue
        if ($null -eq $Service) {
            $Inventory += [pscustomobject]@{ Name = $Target.Name; State = 'NotPresent'; StartMode = 'NotPresent' }
        } else {
            $Inventory += [pscustomobject]@{ Name = $Service.Name; State = $Service.State; StartMode = $Service.StartMode }
        }
    }
    @($Inventory)
}

function Get-LswStartupInventory {
    $Inventory = @()
    foreach ($Path in @(
        'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\Run',
        'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run'
    )) {
        if (Test-Path -LiteralPath $Path) {
            $Properties = Get-ItemProperty -LiteralPath $Path
            foreach ($Name in @('OneDrive', 'OneDriveSetup')) {
                if ($null -ne $Properties.PSObject.Properties[$Name]) {
                    $Inventory += [pscustomobject]@{ Path = $Path; Name = $Name; Value = [string]$Properties.$Name }
                }
            }
        }
    }
    @($Inventory)
}

function Test-LswInstalledAppMatch {
    param($Package, $Target)
    if ($Package.Name -ieq $Target.DisplayName) {
        return $true
    }
    foreach ($Family in $Target.Families) {
        if ($Package.PackageFamilyName -ieq $Family) {
            return $true
        }
    }
    return $false
}

function Remove-LswAppxTargets {
    foreach ($Target in $AppxTargets) {
        $Matched = $false
        $Provisioned = @(Get-AppxProvisionedPackage -Online | Where-Object {
            $_.DisplayName -ieq $Target.DisplayName
        })
        foreach ($Package in $Provisioned) {
            $Matched = $true
            Remove-AppxProvisionedPackage -Online -PackageName $Package.PackageName -AllUsers | Out-Null
            Add-LswOperation 'provisioned-appx' $Target.DisplayName 'removed' $Package.PackageName
        }

        $Installed = @(Get-AppxPackage -AllUsers | Where-Object {
            Test-LswInstalledAppMatch $_ $Target
        })
        foreach ($Package in $Installed) {
            $Matched = $true
            Remove-AppxPackage -Package $Package.PackageFullName -AllUsers -Confirm:$false
            Add-LswOperation 'installed-appx' $Target.DisplayName 'removed' $Package.PackageFullName
        }
        if (-not $Matched) {
            Add-LswOperation 'appx' $Target.DisplayName 'not-applicable' 'not present in this Windows build'
        }
    }

    $RemainingProvisioned = Get-LswProvisionedApps
    $RemainingInstalled = Get-LswInstalledApps
    foreach ($Target in $AppxTargets) {
        if (@($RemainingProvisioned | Where-Object { $_.DisplayName -ieq $Target.DisplayName }).Count -ne 0) {
            throw ('Targeted provisioned AppX survived removal: ' + $Target.DisplayName)
        }
        if (@($RemainingInstalled | Where-Object { Test-LswInstalledAppMatch $_ $Target }).Count -ne 0) {
            throw ('Targeted installed AppX survived removal: ' + $Target.DisplayName)
        }
    }
}

function Remove-LswOptionalFeatures {
    foreach ($Target in $FeatureTargets) {
        $Feature = Get-WindowsOptionalFeature -Online -FeatureName $Target -ErrorAction SilentlyContinue
        if ($null -eq $Feature) {
            Add-LswOperation 'optional-feature' $Target 'not-applicable' 'not present in this Windows build'
            continue
        }
        if ([string]$Feature.State -ne 'DisabledWithPayloadRemoved') {
            Disable-WindowsOptionalFeature -Online -FeatureName $Target -Remove -NoRestart | Out-Null
            Add-LswOperation 'optional-feature' $Target 'removed' ([string]$Feature.State)
        } else {
            Add-LswOperation 'optional-feature' $Target 'already-applied' ([string]$Feature.State)
        }
        $After = Get-WindowsOptionalFeature -Online -FeatureName $Target -ErrorAction SilentlyContinue
        if ($null -ne $After -and [string]$After.State -ne 'DisabledWithPayloadRemoved') {
            throw ('Targeted optional-feature payload survived removal: ' + $Target)
        }
    }
}

function Remove-LswOneDriveStartupHooks {
    param([string]$HiveRoot)
    foreach ($Suffix in @(
        'Software\Microsoft\Windows\CurrentVersion\Run',
        'Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run'
    )) {
        $Path = $HiveRoot + '\' + $Suffix
        if (Test-Path -LiteralPath $Path) {
            foreach ($Name in @('OneDrive', 'OneDriveSetup')) {
                if ($null -ne (Get-ItemProperty -LiteralPath $Path -Name $Name -ErrorAction SilentlyContinue)) {
                    Remove-ItemProperty -LiteralPath $Path -Name $Name -Force
                    Add-LswOperation 'startup' ($Path + '\' + $Name) 'removed' 'OneDrive startup hook'
                }
            }
        }
    }
}

function Remove-LswOneDrive {
    if (-not $UninstallOneDrive) {
        return
    }
    Get-Process -Name OneDrive -ErrorAction SilentlyContinue | Stop-Process -Force
    $Setup = @(
        (Join-Path $env:SystemRoot 'SysWOW64\OneDriveSetup.exe'),
        (Join-Path $env:SystemRoot 'System32\OneDriveSetup.exe')
    ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
    if ($null -ne $Setup) {
        $Process = Start-Process -FilePath $Setup -ArgumentList '/uninstall' -Wait -PassThru
        if ($Process.ExitCode -ne 0) {
            throw ("OneDriveSetup /uninstall failed with exit code " + $Process.ExitCode)
        }
        Add-LswOperation 'product' 'OneDrive' 'removed' $Setup
    } else {
        Add-LswOperation 'product' 'OneDrive' 'not-applicable' 'supported uninstaller is absent'
    }
    Remove-LswOneDriveStartupHooks 'Registry::HKEY_LOCAL_MACHINE'
    if ($null -ne (Get-Process -Name OneDrive -ErrorAction SilentlyContinue)) {
        throw 'OneDrive remained running after uninstall.'
    }
    foreach ($Root in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
        if ($null -ne $Root -and (Test-Path -LiteralPath (Join-Path $Root 'Microsoft OneDrive\OneDrive.exe') -PathType Leaf)) {
            throw ('OneDrive client survived uninstall under ' + $Root)
        }
    }
}

function Set-LswServicePolicies {
    foreach ($Target in $ServiceTargets) {
        $Service = Get-Service -Name $Target.Name -ErrorAction SilentlyContinue
        if ($null -eq $Service) {
            Add-LswOperation 'service' $Target.Name 'not-applicable' 'not present in this Windows build'
            continue
        }
        if ($Target.Startup -eq 'Disabled' -and $Service.Status -ne 'Stopped') {
            Stop-Service -Name $Target.Name -Force -ErrorAction Stop
        }
        Set-Service -Name $Target.Name -StartupType $Target.Startup
        $After = Get-CimInstance Win32_Service -Filter ("Name='" + $Target.Name + "'")
        if ($Target.Startup -eq 'Disabled') {
            if ($After.StartMode -ne 'Disabled' -or $After.State -ne 'Stopped') {
                throw ('Disabled service policy did not apply: ' + $Target.Name)
            }
        } elseif ($After.StartMode -ne 'Manual') {
            throw ('Demand-start service policy did not apply: ' + $Target.Name)
        }
        Add-LswOperation 'service' $Target.Name 'configured' ($After.StartMode + '/' + $After.State)
    }
}

function Set-LswRegistryPolicies {
    param([string]$HiveRoot, [object[]]$Policies)
    foreach ($Target in $Policies) {
        $Path = $HiveRoot + '\' + $Target.Path
        New-Item -Path $Path -Force | Out-Null
        New-ItemProperty -Path $Path -Name $Target.Name -PropertyType DWord -Value $Target.Value -Force | Out-Null
        $Property = Get-ItemProperty -LiteralPath $Path -Name $Target.Name
        $Readback = $Property.PSObject.Properties[$Target.Name].Value
        if ([uint32]$Readback -ne [uint32]$Target.Value) {
            throw ('Registry policy readback failed: ' + $Path + '\' + $Target.Name)
        }
        Add-LswOperation 'registry-policy' ($Path + '\' + $Target.Name) 'configured' ([string]$Target.Value)
    }
}

function Set-LswDefaultUserPolicies {
    $HiveFile = Join-Path $env:SystemDrive 'Users\Default\NTUSER.DAT'
    if (-not (Test-Path -LiteralPath $HiveFile -PathType Leaf)) {
        throw 'Default User registry hive is missing.'
    }
    & reg.exe load 'HKU\LSWDefaultProfile' $HiveFile | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw ('Could not load Default User registry hive; reg.exe exited ' + $LASTEXITCODE)
    }
    try {
        Set-LswRegistryPolicies 'Registry::HKEY_USERS\LSWDefaultProfile' $DefaultUserPolicies
        Remove-LswOneDriveStartupHooks 'Registry::HKEY_USERS\LSWDefaultProfile'
    } finally {
        [GC]::Collect()
        [GC]::WaitForPendingFinalizers()
        & reg.exe unload 'HKU\LSWDefaultProfile' | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw ('Could not unload Default User registry hive; reg.exe exited ' + $LASTEXITCODE)
        }
    }
}

function Enable-LswCompactOs {
    if (-not $EnableCompactOs) {
        return
    }
    & compact.exe /CompactOS:always
    if ($LASTEXITCODE -ne 0) {
        throw ("CompactOS failed with exit code " + $LASTEXITCODE)
    }
    Add-LswOperation 'compact-os' 'C:' 'configured' 'CompactOS always'
}

Write-Host ('Applying LSW ' + $ProfileRevision + ' profile.')
$Before = [pscustomobject]@{
    provisioned_appx = @(Get-LswProvisionedApps)
    installed_appx = @(Get-LswInstalledApps)
    optional_features = @(Get-LswFeatureInventory)
    services = @(Get-LswServiceInventory)
    startup_entries = @(Get-LswStartupInventory)
}
$Before | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $ReportRoot 'before.json') -Encoding UTF8

$Failure = $null
try {
    if (Test-Path -LiteralPath $OfflineProfileMarker -PathType Leaf) {
        Add-LswOperation 'offline-profile' $ProfileRevision 'detected' 'WinPE servicing marker is present'
    } else {
        Add-LswOperation 'offline-profile' $ProfileRevision 'missing' 'online fallback and validation required'
    }
    Remove-LswAppxTargets
    Remove-LswOptionalFeatures
    Remove-LswOneDrive
    Set-LswServicePolicies
    Set-LswRegistryPolicies 'Registry::HKEY_LOCAL_MACHINE' $MachinePolicies
    Set-LswDefaultUserPolicies
    Enable-LswCompactOs
} catch {
    $Failure = $_.Exception.Message
    Add-LswOperation 'profile' $ProfileRevision 'failed' $Failure
}

$After = [pscustomobject]@{
    provisioned_appx = @(Get-LswProvisionedApps)
    installed_appx = @(Get-LswInstalledApps)
    optional_features = @(Get-LswFeatureInventory)
    services = @(Get-LswServiceInventory)
    startup_entries = @(Get-LswStartupInventory)
}
$After | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $ReportRoot 'after.json') -Encoding UTF8
$Report = [pscustomobject]@{
    schema_version = 2
    profile = $ProfileName
    revision = $ProfileRevision
    applied_at_utc = [DateTime]::UtcNow.ToString('o')
    offline_marker = (Test-Path -LiteralPath $OfflineProfileMarker -PathType Leaf)
    outcome = $(if ($null -eq $Failure) { 'success' } else { 'failed' })
    targets = [pscustomobject]@{
        appx = @($AppxTargets)
        optional_features = @($FeatureTargets)
        services = @($ServiceTargets)
        machine_policies = @($MachinePolicies)
        default_user_policies = @($DefaultUserPolicies)
        uninstall_onedrive = $UninstallOneDrive
        compact_os = $EnableCompactOs
    }
    operations = @($Operations)
}
$Report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $ReportRoot 'report.json') -Encoding UTF8
if ($null -ne $Failure) {
    throw $Failure
}
"#;

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use std::io::Write;
    #[cfg(windows)]
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn slim_script_is_inventory_driven_bounded_and_audited() {
        let script = profile_script(WindowsProfile::Slim, "offline.marker")
            .expect("slim script should generate");
        assert!(script.contains("Get-AppxProvisionedPackage -Online"));
        assert!(script.contains("Get-AppxPackage -AllUsers"));
        assert!(script.contains("Remove-AppxPackage -Package"));
        assert!(script.contains("Disable-WindowsOptionalFeature"));
        assert!(script.contains("OneDriveSetup.exe"));
        assert!(script.contains("Set-Service"));
        assert!(script.contains("LSWDefaultProfile"));
        assert!(script.contains("before.json"));
        assert!(script.contains("after.json"));
        assert!(script.contains("report.json"));
        assert!(script.contains("machine_policies = @($MachinePolicies)"));
        assert!(script.contains("Targeted installed AppX survived removal"));
        assert!(!script.contains('*'));
    }

    #[cfg(windows)]
    #[test]
    fn slim_script_parses_with_inbox_windows_powershell() {
        let script = profile_script(WindowsProfile::Slim, "offline.marker")
            .expect("slim script should generate");
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$Source=[Console]::In.ReadToEnd(); [scriptblock]::Create($Source) | Out-Null",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("inbox Windows PowerShell should start");
        child
            .stdin
            .take()
            .expect("PowerShell stdin should be piped")
            .write_all(script.as_bytes())
            .expect("generated script should reach PowerShell");
        let output = child
            .wait_with_output()
            .expect("PowerShell parser should finish");
        assert!(
            output.status.success(),
            "generated script did not parse: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn vanilla_script_has_no_mutation_commands() {
        let script = profile_script(WindowsProfile::Vanilla, "offline.marker")
            .expect("vanilla script should generate");
        for forbidden in [
            "Remove-Appx",
            "Disable-WindowsOptionalFeature",
            "Set-Service",
            "New-ItemProperty",
            "CompactOS:always",
        ] {
            assert!(!script.contains(forbidden));
        }
    }
}
