// SPDX-License-Identifier: GPL-3.0-or-later

use crate::CustomizationPlan;

pub(super) fn prepare_script(
    edition_index: u32,
    customization: &CustomizationPlan,
    stage_guest_setup: bool,
    workspace_drive: &str,
    prepared_image_name: &str,
    offline_marker_name: &str,
) -> String {
    let appx_removals = customization
        .appx_removals
        .iter()
        .map(|removal| {
            format!(
                "call :remove_appx_if_present \"{}\"\r\nif errorlevel 1 goto :fail_mounted",
                removal.display_name
            )
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    let appx_assertions = customization
        .appx_removals
        .iter()
        .map(|removal| {
            format!(
                "call :assert_appx_absent \"{}\"\r\nif errorlevel 1 goto :fail_mounted",
                removal.display_name
            )
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    let feature_removals = customization
        .optional_feature_removals
        .iter()
        .map(|feature| {
            format!(
                "call :remove_feature_if_present \"{feature}\"\r\nif errorlevel 1 goto :fail_mounted"
            )
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    let feature_assertions = customization
        .optional_feature_removals
        .iter()
        .map(|feature| {
            format!(
                "call :assert_feature_disabled \"{feature}\"\r\nif errorlevel 1 goto :fail_mounted"
            )
        })
        .collect::<Vec<_>>()
        .join("\r\n");

    let guest_setup = if stage_guest_setup {
        GUEST_SETUP
            .replace("__LSW_OFFLINE_MARKER__", offline_marker_name)
            .replace("__LSW_PROFILE_REVISION__", &customization.revision)
    } else {
        String::new()
    };
    let audit = if customization.appx_removals.is_empty()
        && customization.optional_feature_removals.is_empty()
    {
        String::new()
    } else {
        OFFLINE_AUDIT.replace("__LSW_PROFILE_REVISION__", &customization.revision)
    };

    PREPARE_SCRIPT
        .replace("__LSW_WORKSPACE_DRIVE__", workspace_drive)
        .replace("__LSW_PREPARED_IMAGE__", prepared_image_name)
        .replace("__LSW_EDITION_INDEX__", &edition_index.to_string())
        .replace(
            "__LSW_APPX_REMOVALS__",
            if appx_removals.is_empty() {
                "call :status no-appx-removals"
            } else {
                &appx_removals
            },
        )
        .replace(
            "__LSW_FEATURE_REMOVALS__",
            if feature_removals.is_empty() {
                "call :status no-feature-removals"
            } else {
                &feature_removals
            },
        )
        .replace(
            "__LSW_APPX_ASSERTIONS__",
            if appx_assertions.is_empty() {
                "call :status no-appx-assertions"
            } else {
                &appx_assertions
            },
        )
        .replace(
            "__LSW_FEATURE_ASSERTIONS__",
            if feature_assertions.is_empty() {
                "call :status no-feature-assertions"
            } else {
                &feature_assertions
            },
        )
        .replace("__LSW_OFFLINE_AUDIT__", &audit)
        .replace("__LSW_GUEST_SETUP__", &guest_setup)
        .replace(
            "__LSW_APPX_HELPERS__",
            if customization.appx_removals.is_empty() {
                ""
            } else {
                APPX_HELPERS
            },
        )
        .replace(
            "__LSW_FEATURE_HELPERS__",
            if customization.optional_feature_removals.is_empty() {
                ""
            } else {
                FEATURE_HELPERS
            },
        )
}

const GUEST_SETUP: &str = r#"call :status stage-guest-setup
mkdir "%LSW_MOUNT%\ProgramData\LSW\setup" "%LSW_MOUNT%\Windows\Panther" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
xcopy.exe "%LSW_SEED%\payload\lsw\*" "%LSW_MOUNT%\ProgramData\LSW\setup\" /E /H /K /Y /I >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
>"%LSW_MOUNT%\ProgramData\LSW\setup\__LSW_OFFLINE_MARKER__" echo LSW-OFFLINE-PROFILE-APPLIED __LSW_PROFILE_REVISION__
if errorlevel 1 goto :fail_mounted
copy /Y "%LSW_SEED%\lsw\offline-unattend.xml" "%LSW_MOUNT%\Windows\Panther\unattend.xml" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
icacls.exe "%LSW_MOUNT%\ProgramData\LSW\setup" /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
"#;

const OFFLINE_AUDIT: &str = r#"call :status persist-profile-audit
mkdir "%LSW_MOUNT%\ProgramData\LSW\profile" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
copy /Y "%LSW_PACKAGES_BEFORE%" "%LSW_MOUNT%\ProgramData\LSW\profile\offline-provisioned-appx-before.txt" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
copy /Y "%LSW_PACKAGES_AFTER%" "%LSW_MOUNT%\ProgramData\LSW\profile\offline-provisioned-appx-after.txt" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
copy /Y "%LSW_FEATURES_BEFORE%" "%LSW_MOUNT%\ProgramData\LSW\profile\offline-features-before.txt" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
copy /Y "%LSW_FEATURES_AFTER%" "%LSW_MOUNT%\ProgramData\LSW\profile\offline-features-after.txt" >>"%LSW_LOG%" 2>&1
if errorlevel 1 goto :fail_mounted
>"%LSW_MOUNT%\ProgramData\LSW\profile\offline-profile.env" echo schema_version=2
>>"%LSW_MOUNT%\ProgramData\LSW\profile\offline-profile.env" echo revision=__LSW_PROFILE_REVISION__
"#;

const PREPARE_SCRIPT: &str = r#"@echo off
setlocal EnableExtensions EnableDelayedExpansion
set "LSW_SEED=%~d0"
set "LSW_WORK=__LSW_WORKSPACE_DRIVE__"
set "LSW_MOUNT=__LSW_WORKSPACE_DRIVE__\mount"
set "LSW_SCRATCH=__LSW_WORKSPACE_DRIVE__\scratch"
set "LSW_LOG="
set "LSW_PACKAGES_BEFORE=__LSW_WORKSPACE_DRIVE__\logs\provisioned-appx-before.txt"
set "LSW_PACKAGES_AFTER=__LSW_WORKSPACE_DRIVE__\logs\provisioned-appx-after.txt"
set "LSW_FEATURES_BEFORE=__LSW_WORKSPACE_DRIVE__\logs\features-before.txt"
set "LSW_FEATURES_AFTER=__LSW_WORKSPACE_DRIVE__\logs\features-after.txt"
set "LSW_IMAGE=__LSW_WORKSPACE_DRIVE__\__LSW_PREPARED_IMAGE__"
set "LSW_DISM=%SystemRoot%\System32\dism.exe"
set "LSW_STATUS="
for %%D in (C D E F G H I J K L M N O P Q R S T U V W X Y Z) do if exist "%%D:\lsw-status.tag" set "LSW_STATUS=%%D:"
if not defined LSW_STATUS (
    wpeutil.exe shutdown
    exit /b 1
)
set "LSW_LOG=%LSW_STATUS%\dism.log"

call :status initialize-workspace
diskpart.exe /s "%LSW_SEED%\lsw\workspace.diskpart" > X:\lsw-workspace.log 2>&1
if errorlevel 1 goto :fail
mkdir "%LSW_MOUNT%" "%LSW_SCRATCH%" "__LSW_WORKSPACE_DRIVE__\logs" >nul 2>&1
if errorlevel 1 goto :fail

set "LSW_SOURCE="
for %%D in (C D E F G H I J K L M N O P Q R S T U V X Y Z) do (
    if not defined LSW_SOURCE if exist "%%D:\sources\install.wim" set "LSW_SOURCE=%%D:\sources\install.wim"
    if not defined LSW_SOURCE if exist "%%D:\sources\install.esd" set "LSW_SOURCE=%%D:\sources\install.esd"
)
if not defined LSW_SOURCE (
    >>"%LSW_LOG%" echo official media has no sources\install.wim or sources\install.esd
    goto :fail
)

call :status export-image
call :run "%LSW_DISM%" /English /Export-Image /SourceImageFile:"%LSW_SOURCE%" /SourceIndex:__LSW_EDITION_INDEX__ /DestinationImageFile:"%LSW_IMAGE%" /Compress:max /ScratchDir:"%LSW_SCRATCH%" /CheckIntegrity
if errorlevel 1 goto :fail

call :status mount-image
call :run "%LSW_DISM%" /English /Mount-Image /ImageFile:"%LSW_IMAGE%" /Index:1 /MountDir:"%LSW_MOUNT%" /ScratchDir:"%LSW_SCRATCH%" /CheckIntegrity
if errorlevel 1 goto :fail_mounted

call :status inventory-appx-before
"%LSW_DISM%" /English /Image:"%LSW_MOUNT%" /Get-ProvisionedAppxPackages >"%LSW_PACKAGES_BEFORE%" 2>>"%LSW_LOG%"
if errorlevel 1 goto :fail_mounted
call :status inventory-features-before
"%LSW_DISM%" /English /Image:"%LSW_MOUNT%" /Get-Features /Format:Table >"%LSW_FEATURES_BEFORE%" 2>>"%LSW_LOG%"
if errorlevel 1 goto :fail_mounted

__LSW_APPX_REMOVALS__
__LSW_FEATURE_REMOVALS__

call :status inventory-appx-after
"%LSW_DISM%" /English /Image:"%LSW_MOUNT%" /Get-ProvisionedAppxPackages >"%LSW_PACKAGES_AFTER%" 2>>"%LSW_LOG%"
if errorlevel 1 goto :fail_mounted
call :status inventory-features-after
"%LSW_DISM%" /English /Image:"%LSW_MOUNT%" /Get-Features /Format:Table >"%LSW_FEATURES_AFTER%" 2>>"%LSW_LOG%"
if errorlevel 1 goto :fail_mounted
call :status verify-profile
__LSW_APPX_ASSERTIONS__
__LSW_FEATURE_ASSERTIONS__

__LSW_OFFLINE_AUDIT__
__LSW_GUEST_SETUP__
call :status commit-image
call :run "%LSW_DISM%" /English /Unmount-Image /MountDir:"%LSW_MOUNT%" /Commit /CheckIntegrity
if errorlevel 1 goto :fail_mounted
call :retain_log
call :status complete
call :flush_status
wpeutil.exe shutdown
exit /b 0

__LSW_APPX_HELPERS__
__LSW_FEATURE_HELPERS__

:run
>>"%LSW_LOG%" echo LSW-DISM-COMMAND %*
%* >>"%LSW_LOG%" 2>&1
set "LSW_EXIT=!errorlevel!"
if not "!LSW_EXIT!"=="0" >>"%LSW_LOG%" echo command failed with exit code !LSW_EXIT!
exit /b !LSW_EXIT!

:fail_mounted
call :status discard-image
"%LSW_DISM%" /English /Unmount-Image /MountDir:"%LSW_MOUNT%" /Discard >>"%LSW_LOG%" 2>&1

:fail
call :retain_log
call :status failed
call :flush_status
wpeutil.exe shutdown
exit /b 1

:retain_log
exit /b 0

:flush_status
if exist "%SystemRoot%\System32\timeout.exe" (
    timeout.exe /t 2 /nobreak >nul 2>&1
) else (
    ping.exe -n 3 127.0.0.1 >nul 2>&1
)
exit /b 0

:status
>>"%LSW_STATUS%\status.log" echo LSW-WINPE-DISM %*
if defined LSW_LOG >>"%LSW_LOG%" echo LSW-DISM-STAGE %*
exit /b 0
"#;

const APPX_HELPERS: &str = r#":remove_appx_if_present
set "LSW_DISPLAY_NAME=%~1"
set "LSW_MATCHED=0"
for /f "tokens=2 delims=:" %%P in ('findstr.exe /b /c:"PackageName :" "%LSW_PACKAGES_BEFORE%"') do (
    set "LSW_PACKAGE=%%P"
    for /f "tokens=*" %%Q in ("!LSW_PACKAGE!") do set "LSW_PACKAGE=%%Q"
    echo(!LSW_PACKAGE!| findstr.exe /i /b /l /c:"!LSW_DISPLAY_NAME!_" >nul
    if not errorlevel 1 (
        set "LSW_MATCHED=1"
        call :status remove-appx !LSW_DISPLAY_NAME!
        call :run "%LSW_DISM%" /English /Image:"%LSW_MOUNT%" /Remove-ProvisionedAppxPackage /PackageName:!LSW_PACKAGE!
        if errorlevel 1 exit /b 1
    )
)
if "!LSW_MATCHED!"=="0" call :status appx-not-present !LSW_DISPLAY_NAME!
exit /b 0

:assert_appx_absent
set "LSW_DISPLAY_NAME=%~1"
for /f "tokens=2 delims=:" %%P in ('findstr.exe /b /c:"PackageName :" "%LSW_PACKAGES_AFTER%"') do (
    set "LSW_PACKAGE=%%P"
    for /f "tokens=*" %%Q in ("!LSW_PACKAGE!") do set "LSW_PACKAGE=%%Q"
    echo(!LSW_PACKAGE!| findstr.exe /i /b /l /c:"!LSW_DISPLAY_NAME!_" >nul
    if not errorlevel 1 (
        >>"%LSW_LOG%" echo targeted AppX survived removal: !LSW_PACKAGE!
        exit /b 1
    )
)
exit /b 0
"#;

const FEATURE_HELPERS: &str = r#":remove_feature_if_present
set "LSW_FEATURE=%~1"
findstr.exe /i /r /c:"^%LSW_FEATURE%  *|" "%LSW_FEATURES_BEFORE%" >nul
if errorlevel 1 (
    call :status feature-not-present %LSW_FEATURE%
    exit /b 0
)
call :status remove-feature %LSW_FEATURE%
call :run "%LSW_DISM%" /English /Image:"%LSW_MOUNT%" /Disable-Feature /FeatureName:%LSW_FEATURE% /Remove
exit /b %errorlevel%

:assert_feature_disabled
set "LSW_FEATURE=%~1"
findstr.exe /i /r /c:"^%LSW_FEATURE%  *|" "%LSW_FEATURES_AFTER%" | findstr.exe /i /c:"Enabled" /c:"Enable Pending" >nul
if not errorlevel 1 (
    >>"%LSW_LOG%" echo targeted optional feature survived removal: %LSW_FEATURE%
    exit /b 1
)
exit /b 0
"#;

#[cfg(test)]
mod tests {
    use crate::{CustomizationPlan, WindowsProfile};

    use super::*;

    #[test]
    fn slim_offline_script_inventories_removes_and_asserts() {
        let plan = CustomizationPlan::for_profile(WindowsProfile::Slim)
            .expect("slim profile should validate");
        let script = prepare_script(6, &plan, true, "W:", "prepared.wim", "profile.marker");
        assert!(script.contains("/Get-ProvisionedAppxPackages"));
        assert!(script.contains("/Remove-ProvisionedAppxPackage"));
        assert!(script.contains("/Get-Features /Format:Table"));
        assert!(script.contains("call :remove_feature_if_present \"Recall\""));
        assert!(script.contains("/Disable-Feature /FeatureName:%LSW_FEATURE% /Remove"));
        assert!(script.contains(":assert_appx_absent"));
        assert!(script.contains(":assert_feature_disabled"));
        assert!(script.contains("offline-provisioned-appx-before.txt"));
        assert!(script.contains("LSW-OFFLINE-PROFILE-APPLIED slim-v2"));
        assert!(!script.contains("__LSW_"));
    }

    #[test]
    fn vanilla_offline_script_has_no_remove_commands_or_audit_payload() {
        let plan = CustomizationPlan::for_profile(WindowsProfile::Vanilla)
            .expect("vanilla profile should validate");
        let script = prepare_script(1, &plan, false, "W:", "prepared.wim", "profile.marker");
        assert!(!script.contains("/Remove-ProvisionedAppxPackage"));
        assert!(!script.contains("/Disable-Feature"));
        assert!(!script.contains("offline-profile.env"));
        assert!(!script.contains("__LSW_"));
    }
}
