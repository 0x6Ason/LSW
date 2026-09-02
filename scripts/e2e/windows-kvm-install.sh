// SPDX-License-Identifier: GPL-3.0-or-later
# shellcheck shell=sh
# Official-media installation, guest identity, and policy scenario.
# shellcheck disable=SC2154

run_windows_kvm_install_scenario() {
doctor_output=$("$lsw" doctor)
printf '%s\n' "$doctor_output"
if [ -n "$artifact_dir" ]; then
    printf '%s\n' "$doctor_output" >"$artifact_dir/doctor.txt"
    chmod 600 "$artifact_dir/doctor.txt"
fi
if ! printf '%s\n' "$doctor_output" | grep -E '^  KVM:[[:space:]]+yes$' >/dev/null; then
    echo "error: lsw doctor did not confirm KVM acceleration" >&2
    exit 1
fi
for capability in \
    '  QEMU:        ' \
    '  qemu-img:    ' \
    '  swtpm:       ' \
    '  wimlib:      ' \
    '  xorriso:     ' \
    '  7z:          ' \
    '  OVMF code:   ' \
    '  OVMF vars:   '
do
    capability_value=$(printf '%s\n' "$doctor_output" |
        awk -v prefix="$capability" 'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }')
    if [ -z "$capability_value" ] || [ "$capability_value" = 'not found' ]; then
        echo "error: trusted E2E runner is missing ${capability#  }" >&2
        exit 1
    fi
done
ovmf_code_path=$(printf '%s\n' "$doctor_output" |
    awk -v prefix='  OVMF code:   ' 'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }')
ovmf_vars_path=$(printf '%s\n' "$doctor_output" |
    awk -v prefix='  OVMF vars:   ' 'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }')
ovmf_code_sha256=$(sha256sum -- "$ovmf_code_path" | awk '{ print $1 }')
ovmf_vars_sha256=$(sha256sum -- "$ovmf_vars_path" | awk '{ print $1 }')

media_output=
media_status=1
for media_retry_delay in 0 15 30 60; do
    if [ "$media_retry_delay" -ne 0 ]; then
        echo "warning: Microsoft ISO resolution failed; retrying in ${media_retry_delay}s" >&2
        sleep "$media_retry_delay"
    fi
    set +e
    media_output=$(timeout 60s "$lsw" media published-sha256 --language English)
    media_status=$?
    set -e
    if [ "$media_status" -eq 0 ]; then
        break
    fi
done
if [ "$media_status" -ne 0 ]; then
    echo "error: Microsoft ISO resolution failed after bounded retries" >&2
    exit 1
fi
official_iso_sha256=$(printf '%s\n' "$media_output" |
    awk -F= '$1 == "SHA256" { print tolower($2); exit }')
if [ "$official_iso_sha256" != "$iso_sha256" ]; then
    echo "error: provisioned ISO does not match Microsoft's current published SHA-256" >&2
    exit 1
fi
if [ "$e2e_no_viewer" != 1 ]; then
    viewer_value=$(printf '%s\n' "$doctor_output" |
        awk -v prefix='  viewer:      ' 'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }')
    if [ -z "$viewer_value" ] || [ "$viewer_value" = 'not found' ]; then
        echo "error: trusted E2E runner is missing remote-viewer" >&2
        exit 1
    fi
fi
mkdir -p -- "$LSW_STATE_DIR"
chmod 700 "$LSW_STATE_DIR"
# WinPE preparation and apply run directly from the installer and therefore do
# not count as daemon-owned work. Keep this explicitly tracked gate daemon
# alive across those bounded phases; the cold-start path below still uses the
# product's default 30-second idle configuration.
LSW_DAEMON_IDLE_SECONDS=3600 setsid "$lswd" >"$e2e_root/lswd.log" 2>&1 &
daemon_pid=$!
printf '%s\n' "$daemon_pid" >"$daemon_pid_file"
chmod 600 "$daemon_pid_file"
daemon_ready=0
daemon_attempt=0
while [ "$daemon_attempt" -lt 100 ]; do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        echo "error: lswd exited before becoming ready" >&2
        exit 1
    fi
    if "$lsw" daemon status 2>/dev/null | grep -F 'lswd is ready at ' >/dev/null; then
        daemon_ready=1
        break
    fi
    daemon_attempt=$((daemon_attempt + 1))
    sleep 0.1
done
if [ "$daemon_ready" -ne 1 ]; then
    echo "error: lswd did not become ready within ten seconds" >&2
    exit 1
fi

# Preparing and applying an official image can legitimately exceed lswd's
# one-hour configurable idle ceiling on slower disks. Keep the one explicitly
# tracked exact-candidate daemon active without enabling autospawn; teardown
# stops this private process group before the cold-start daemon test.
daemon_keepalive="$e2e_root/daemon-keepalive.sh"
# The variables are intentionally expanded later by the generated helper.
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'while kill -0 "$LSW_E2E_DAEMON_PID" 2>/dev/null; do' \
    '    sleep 300' \
    '    kill -0 "$LSW_E2E_DAEMON_PID" 2>/dev/null || exit 1' \
    '    "$LSW_E2E_LSW" daemon status >/dev/null' \
    'done' \
    >"$daemon_keepalive"
chmod 700 "$daemon_keepalive"
export LSW_E2E_DAEMON_PID="$daemon_pid"
setsid "$daemon_keepalive" >"$e2e_root/lswd-keepalive.log" 2>&1 &
daemon_keepalive_pid=$!

viewer_option=
if [ "$e2e_no_viewer" != 1 ]; then
    viewer_option=--viewer
    viewer_command=${LSW_INSTALL_VIEWER:-$(command -v remote-viewer || :)}
    if [ -z "$viewer_command" ] || [ ! -x "$viewer_command" ]; then
        echo "error: remote-viewer is required unless LSW_E2E_NO_VIEWER=1" >&2
        exit 1
    fi
    viewer_pid_file="$e2e_root/viewer.pid"
    viewer_wrapper="$e2e_root/viewer-wrapper.sh"
    viewer_session="$e2e_root/viewer-session.sh"
    # The variables are intentionally expanded later by the generated wrapper.
    # shellcheck disable=SC2016
    printf '%s\n' \
        '#!/bin/sh' \
        'set -eu' \
        'exec setsid "$LSW_E2E_VIEWER_SESSION" "$@"' \
        >"$viewer_wrapper"
    # Record the PID only after setsid has established the session/process
    # group, so teardown never adopts the short-lived outer wrapper.
    # shellcheck disable=SC2016
    printf '%s\n' \
        '#!/bin/sh' \
        'set -eu' \
        'printf "%s\n" "$$" >"$LSW_E2E_VIEWER_PID_FILE"' \
        'exec "$LSW_E2E_VIEWER_COMMAND" "$@"' \
        >"$viewer_session"
    chmod 700 "$viewer_wrapper" "$viewer_session"
    export LSW_E2E_VIEWER_COMMAND="$viewer_command"
    export LSW_E2E_VIEWER_PID_FILE="$viewer_pid_file"
    export LSW_E2E_VIEWER_SESSION="$viewer_session"
    export LSW_INSTALL_VIEWER="$viewer_wrapper"
fi

if [ -z "$viewer_option" ]; then
    "$lsw" install "$instance" \
        --iso "$iso" \
        --edition "$edition" \
        --profile "$profile" \
        --accept-windows-license \
        --defer-user-setup \
        --agent "$agent"
else
    "$lsw" install "$instance" \
        --iso "$iso" \
        --edition "$edition" \
        --profile "$profile" \
        --accept-windows-license \
        --defer-user-setup \
        --agent "$agent" \
        "$viewer_option"

    viewer_ready=0
    viewer_attempt=0
    while [ "$viewer_attempt" -lt 50 ]; do
        if [ -f "$viewer_pid_file" ]; then
            viewer_pid=$(awk 'NR == 1 { print $1 }' "$viewer_pid_file")
            case "$viewer_pid" in
                ''|*[!0-9]*) viewer_pid= ;;
                *)
                    if kill -0 "-$viewer_pid" 2>/dev/null; then
                        viewer_ready=1
                        break
                    fi
                    ;;
            esac
        fi
        viewer_attempt=$((viewer_attempt + 1))
        sleep 0.1
    done
    if [ "$viewer_ready" -ne 1 ]; then
        echo "error: installation viewer did not remain running" >&2
        exit 1
    fi
fi
assert_daemon_alive
assert_daemon_keepalive_alive

for removed_transient in \
    seed \
    winpe-seed \
    winpe-apply-seed \
    run/winpe-workspace.qcow2 \
    run/winpe-control-root \
    run/winpe-control.iso
do
    if [ -e "$LSW_STATE_DIR/instances/$instance/$removed_transient" ]; then
        echo "error: WinPE transient remained after successful install: $removed_transient" >&2
        exit 1
    fi
done
if ! grep -F 'LSW-WINPE-DISM complete' \
    "$LSW_STATE_DIR/instances/$instance/run/winpe-prepare-status/status.log" >/dev/null
then
    echo "error: WinPE prepare completion marker was not retained" >&2
    exit 1
fi
if ! grep -F 'LSW-WINPE-DISM apply-complete' \
    "$LSW_STATE_DIR/instances/$instance/run/winpe-apply-status/status.log" >/dev/null
then
    echo "error: WinPE apply completion marker was not retained" >&2
    exit 1
fi
if grep -F 'LSW-WINPE-DISM failed' \
    "$LSW_STATE_DIR/instances/$instance/run/winpe-prepare-status/status.log" \
    "$LSW_STATE_DIR/instances/$instance/run/winpe-apply-status/status.log" >/dev/null
then
    echo "error: WinPE failure marker appeared in a successful install" >&2
    exit 1
fi

license_output=$(timeout 120s "$lsw" license status "$instance")
license_status=$(printf '%s\n' "$license_output" |
    tr -d '\r' |
    awk -F= '$1 == "STATUS" { print $2; exit }')
case "$license_status" in
    licensed|unlicensed) ;;
    *)
        echo "error: Windows WMI license status did not return a stable state" >&2
        exit 1
        ;;
esac
if [ "$license_status" = unlicensed ] \
    && [ ! -f "$LSW_STATE_DIR/instances/$instance/activation-notice-shown" ]
then
    echo "error: unactivated install did not record its one-time notice" >&2
    exit 1
fi

license_helper_output=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$Deadline=[DateTime]::UtcNow.AddSeconds(30); do { $Service=Get-CimInstance -ClassName Win32_Service -Filter "Name = '\''LSWLicenseHelper'\''"; if ($null -ne $Service -and $Service.State -eq "Stopped") { break }; Start-Sleep -Milliseconds 100 } while ([DateTime]::UtcNow -lt $Deadline); if ($null -eq $Service) { exit 61 }; if ($Service.StartMode -ne "Manual") { exit 62 }; if ($Service.StartName -ine "LocalSystem") { exit 63 }; if ($Service.PathName -notlike "*--license-helper*" -or $Service.PathName -like "*ProductKey*") { exit 64 }; [Console]::Out.Write("$($Service.StartMode)|$($Service.StartName)")'
)
license_helper_start_mode=$(printf '%s\n' "$license_helper_output" | awk -F'|' '{ print $1 }')
license_helper_start_name=$(printf '%s\n' "$license_helper_output" | awk -F'|' '{ print $2 }')
if [ "$license_helper_start_mode" != Manual ] || [ "$license_helper_start_name" != LocalSystem ]; then
    echo "error: activation helper is not a stopped demand-start LocalSystem service" >&2
    exit 1
fi

guest_build=$("$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    '[Console]::Out.Write((Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion").CurrentBuildNumber)')
guest_edition=$("$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    '[Console]::Out.Write((Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion").EditionID)')
case "$guest_build" in
    ''|*[!0-9]*)
        echo "error: guest did not report a numeric Windows build" >&2
        exit 1
        ;;
esac
if [ "$guest_build" -lt 22000 ]; then
    echo "error: installed guest build $guest_build is older than Windows 11" >&2
    exit 1
fi
if [ -z "$guest_edition" ]; then
    echo "error: installed guest did not report EditionID" >&2
    exit 1
fi
read_agent_service_identity
agent_service_sid=$service_sid
agent_service_pid=$service_pid
agent_service_name=LSWAgent
agent_service_start_mode=Auto
agent_service_start_name='NT SERVICE\LSWAgent'
agent_service_state=Running

headless_marker='LSW_WINDOWS_KVM_HEADLESS_SETUP_COMPLETE'
set +e
# PowerShell expands its own variables in the guest.
# shellcheck disable=SC2016
headless_output=$("$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    '$ErrorActionPreference="Stop"; $Marker="C:\ProgramData\LSW\setup-complete.marker"; if (-not (Test-Path -LiteralPath $Marker -PathType Leaf) -or [System.IO.File]::ReadAllText($Marker).Trim() -cne "LSW-SETUP-COMPLETE") { exit 50 }; foreach ($Name in @("LSWSetup", "defaultuser0")) { if ($null -ne (Get-LocalUser -Name $Name -ErrorAction SilentlyContinue)) { exit 51 } }; $Interactive=[string](Get-CimInstance -ClassName Win32_ComputerSystem).UserName; if (-not [string]::IsNullOrWhiteSpace($Interactive)) { exit 52 }; $Winlogon=Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon"; if ([string]$Winlogon.AutoAdminLogon -eq "1") { exit 53 }; $StoredPassword=$Winlogon.PSObject.Properties["DefaultPassword"]; if ($null -ne $StoredPassword -and -not [string]::IsNullOrEmpty([string]$StoredPassword.Value)) { exit 54 }; foreach ($Path in @("C:\Windows\Panther\unattend.xml", "C:\Windows\Panther\Unattend\unattend.xml", "C:\Windows\Setup\Scripts\SetupComplete.cmd", "C:\ProgramData\LSW\setup")) { if (Test-Path -LiteralPath $Path) { exit 55 } }; $Oobe=Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE"; if ([int]$Oobe.LaunchUserOOBE -ne 0 -or $null -ne $Oobe.PSObject.Properties["DefaultAccountSAMName"]) { exit 56 }; $Privacy=Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Policies\Microsoft\Windows\OOBE"; if ([int]$Privacy.DisablePrivacyExperience -ne 1) { exit 57 }; [Console]::Out.Write("LSW_WINDOWS_KVM_HEADLESS_SETUP_COMPLETE")')
headless_status=$?
set -e
if [ "$headless_status" -ne 0 ] || [ "$headless_output" != "$headless_marker" ]; then
    echo "error: unattended setup did not remove its account, cached answer file, or staging payload" >&2
    exit 1
fi
setup_account_removed=true
initial_interactive_user=none
cached_unattend_removed=true
setup_payload_removed=true
automatic_logon=false
audit_slim_profile boot-1

pid_file="$LSW_STATE_DIR/instances/$instance/run/qemu.pid"
clone_instance="${instance}-clone"
clone_source_pid=$(awk 'NR == 1 { print $1 }' "$pid_file")
clone_agent_port=$(
    "$lsw" show "$instance" |
        awk -v prefix='agent host port:      ' \
            'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }'
)
"$lsw" shutdown "$instance"
assert_stopped_runtime_released "$clone_source_pid" "$clone_agent_port"
"$lsw" image seal "$instance"
base_image_key=$(awk -F= '$1 == "base_image_key" { print $2; exit }' \
    "$LSW_STATE_DIR/instances/$instance/instance.lsw")
base_disk="$LSW_STATE_DIR/images/$base_image_key/base.qcow2"
base_disk_writable=$(find "$base_disk" -maxdepth 0 -perm /222 -print -quit 2>/dev/null || true)
if [ -z "$base_image_key" ] || [ ! -f "$base_disk" ] || [ -n "$base_disk_writable" ]; then
    echo "error: sealed linked-clone base is absent or writable" >&2
    exit 1
fi
"$lsw" image verify "$base_image_key"
"$lsw" clone "$instance" "$clone_instance"
source_token_sha=$(sha256sum "$LSW_STATE_DIR/instances/$instance/agent.token" | awk '{ print $1 }')
clone_token_sha=$(sha256sum "$LSW_STATE_DIR/instances/$clone_instance/agent.token" | awk '{ print $1 }')
if [ "$source_token_sha" = "$clone_token_sha" ]; then
    echo "error: linked clone reused the source agent secret" >&2
    exit 1
fi
clone_base_image_key=$(awk -F= '$1 == "base_image_key" { print $2; exit }' \
    "$LSW_STATE_DIR/instances/$clone_instance/instance.lsw")
if [ "$clone_base_image_key" != "$base_image_key" ]; then
    echo "error: linked clone did not retain the verified base key" >&2
    exit 1
fi
clone_backing=$(qemu-img info --output=json \
    "$LSW_STATE_DIR/instances/$clone_instance/disk.qcow2" |
    python3 -c 'import json,sys; print(json.load(sys.stdin).get("backing-filename", ""))')
if [ -z "$clone_backing" ] \
    || [ "$(realpath -e -- "$clone_backing")" != "$(realpath -e -- "$base_disk")" ]; then
    echo "error: linked clone does not reference its exact sealed base" >&2
    exit 1
fi
clone_identity=$(
    timeout "${agent_boot_timeout_seconds}s" "$lsw" exec "$clone_instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '[Console]::Out.Write([IO.File]::ReadAllText("C:\ProgramData\LSW\instance.name").Trim())'
)
if [ "$clone_identity" != "$clone_instance" ]; then
    echo "error: linked clone did not rotate to its private instance identity" >&2
    exit 1
fi
clone_pid=$(awk 'NR == 1 { print $1 }' \
    "$LSW_STATE_DIR/instances/$clone_instance/run/qemu.pid")
clone_port=$(awk -F= '$1 == "control_port" { print $2; exit }' \
    "$LSW_STATE_DIR/instances/$clone_instance/instance.lsw")
"$lsw" shutdown "$clone_instance"
assert_stopped_runtime_released "$clone_pid" "$clone_port" "$clone_instance"
"$lsw" remove "$clone_instance"
if [ -e "$LSW_STATE_DIR/instances/$clone_instance" ]; then
    echo "error: linked clone remained after removal" >&2
    exit 1
fi
resume_marker=LSW_CLONE_SOURCE_RESUME_OK
resume_output=$(timeout "${agent_boot_timeout_seconds}s" "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile \
    -Command "[Console]::Out.Write('$resume_marker')")
if [ "$resume_output" != "$resume_marker" ]; then
    echo "error: sealed source did not resume after linked-clone validation" >&2
    exit 1
fi
clone_identity_isolated=true
audit_slim_profile boot-2

desktop_user='lsw-e2e-user'
desktop_password="Lsw!$(tr -d '-' </proc/sys/kernel/random/uuid)9a"
if ! printf '%s\n' "$desktop_password" | timeout 120s "$lsw" user setup "$instance" \
    --username "$desktop_user" --password-stdin; then
    desktop_password=
    unset desktop_password
    echo "error: native Windows desktop-user registration failed" >&2
    exit 1
fi
desktop_password=
unset desktop_password
desktop_user_output=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$ErrorActionPreference="Stop"; $User=Get-LocalUser -Name "lsw-e2e-user"; if (-not $User.Enabled) { exit 70 }; $Administrators=Get-LocalGroup -SID "S-1-5-32-544"; if (Get-LocalGroupMember -Group $Administrators | Where-Object { $_.SID -eq $User.SID }) { exit 71 }; $Users=Get-LocalGroup -SID "S-1-5-32-545"; if (-not (Get-LocalGroupMember -Group $Users | Where-Object { $_.SID -eq $User.SID })) { exit 73 }; $Winlogon=Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon"; if ([string]$Winlogon.AutoAdminLogon -eq "1") { exit 72 }; [Console]::Out.Write($User.Name)'
)
if [ "$desktop_user_output" != "$desktop_user" ]; then
    echo "error: registered desktop user is absent, disabled, administrative, or configured for AutoLogon" >&2
    exit 1
fi
if ! grep -F "default_user=$desktop_user" \
    "$LSW_STATE_DIR/instances/$instance/instance.lsw" >/dev/null; then
    echo "error: registered desktop user was not persisted as the default identity" >&2
    exit 1
fi
if ! grep -F 'default_user_role=standard' \
    "$LSW_STATE_DIR/instances/$instance/instance.lsw" >/dev/null; then
    echo "error: initial standard desktop-user role was not persisted" >&2
    exit 1
fi
set +e
gui_without_login_output=$(timeout 60s "$lsw" run "$instance" --gui -- notepad.exe 2>&1)
gui_without_login_status=$?
set -e
if [ "$gui_without_login_status" -eq 0 ] \
    || ! printf '%s\n' "$gui_without_login_output" | grep -F 'sign in once' >/dev/null
then
    printf '%s\n' "$gui_without_login_output" >&2
    echo "error: GUI launch did not fail closed with an actionable sign-in requirement" >&2
    exit 1
fi
"$lsw" user promote "$instance"
promoted_user_output=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$User=Get-LocalUser -Name "lsw-e2e-user"; $Administrators=Get-LocalGroup -SID "S-1-5-32-544"; if (-not (Get-LocalGroupMember -Group $Administrators | Where-Object { $_.SID -eq $User.SID })) { exit 83 }; [Console]::Out.Write("administrator")'
)
if [ "$promoted_user_output" != administrator ] \
    || ! grep -F 'default_user_role=administrator' \
        "$LSW_STATE_DIR/instances/$instance/instance.lsw" >/dev/null
then
    echo "error: desktop-user promotion did not update Windows and the manifest" >&2
    exit 1
fi
"$lsw" user demote "$instance"
demoted_user_output=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$User=Get-LocalUser -Name "lsw-e2e-user"; $Administrators=Get-LocalGroup -SID "S-1-5-32-544"; if (Get-LocalGroupMember -Group $Administrators | Where-Object { $_.SID -eq $User.SID }) { exit 84 }; [Console]::Out.Write("standard")'
)
if [ "$demoted_user_output" != standard ] \
    || ! grep -F 'default_user_role=standard' \
        "$LSW_STATE_DIR/instances/$instance/instance.lsw" >/dev/null
then
    echo "error: desktop-user demotion did not update Windows and the manifest" >&2
    exit 1
fi
"$lsw" user promote "$instance"
if ! grep -F 'default_user_role=administrator' \
    "$LSW_STATE_DIR/instances/$instance/instance.lsw" >/dev/null; then
    echo "error: final desktop-user administrator role was not persisted" >&2
    exit 1
fi
desktop_user_role=administrator
secondary_admin='lsw-e2e-admin'
secondary_admin_password="Lsw!$(tr -d '-' </proc/sys/kernel/random/uuid)8b"
if ! printf '%s\n' "$secondary_admin_password" | timeout 120s "$lsw" user add "$instance" \
    --username "$secondary_admin" --password-stdin --administrator; then
    secondary_admin_password=
    unset secondary_admin_password
    echo "error: separate Windows administrator creation failed" >&2
    exit 1
fi
secondary_admin_password=
unset secondary_admin_password
secondary_admin_output=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$User=Get-LocalUser -Name "lsw-e2e-admin"; $Administrators=Get-LocalGroup -SID "S-1-5-32-544"; if (-not $User.Enabled) { exit 88 }; if (-not (Get-LocalGroupMember -Group $Administrators | Where-Object { $_.SID -eq $User.SID })) { exit 89 }; [Console]::Out.Write($User.Name)'
)
if [ "$secondary_admin_output" != "$secondary_admin" ] \
    || ! grep -F "default_user=$desktop_user" \
        "$LSW_STATE_DIR/instances/$instance/instance.lsw" >/dev/null \
    || grep -F "$secondary_admin" \
        "$LSW_STATE_DIR/instances/$instance/instance.lsw" >/dev/null
then
    echo "error: separate administrator changed the default desktop identity" >&2
    exit 1
fi
separate_administrator_created=true
sudo_status_before=$("$lsw" sudo status "$instance")
if printf '%s\n' "$sudo_status_before" | grep -Fx 'Windows sudo: unavailable' >/dev/null; then
    echo "error: installed Windows 11 image does not provide native sudo" >&2
    exit 1
fi
if ! printf '%s\n' "$sudo_status_before" \
    | grep -Fx 'System policy: not configured' >/dev/null; then
    echo "error: disposable Windows guest unexpectedly has a managed sudo policy" >&2
    exit 1
fi
"$lsw" sudo enable "$instance"
sudo_enabled_status=$("$lsw" sudo status "$instance")
if ! printf '%s\n' "$sudo_enabled_status" | grep -Fx 'Windows sudo: new window' >/dev/null \
    || ! printf '%s\n' "$sudo_enabled_status" \
        | grep -Fx 'Configured mode: new window' >/dev/null \
    || ! printf '%s\n' "$sudo_enabled_status" \
        | grep -Fx 'UAC consent: required for elevation' >/dev/null
then
    echo "error: Windows sudo did not report the safe new-window configuration" >&2
    exit 1
fi
sudo_registry_output=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$ErrorActionPreference="Stop"; $Sudo=[int](Get-ItemPropertyValue -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Sudo" -Name Enabled); $Uac=[int](Get-ItemPropertyValue -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System" -Name EnableLUA); $Policy=Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Policies\Microsoft\Windows\Sudo" -ErrorAction SilentlyContinue; if ($null -ne $Policy -and $null -ne $Policy.PSObject.Properties["Enabled"]) { exit 85 }; if ($Sudo -ne 1) { exit 86 }; if ($Uac -ne 1) { exit 87 }; [Console]::Out.Write("$Sudo|$Uac")'
)
if [ "$sudo_registry_output" != '1|1' ]; then
    echo "error: Windows sudo registry mode or UAC state is incorrect" >&2
    exit 1
fi
"$lsw" sudo disable "$instance"
sudo_disabled_mode=$(
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '[Console]::Out.Write((Get-ItemPropertyValue -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Sudo" -Name Enabled))'
)
if [ "$sudo_disabled_mode" != 0 ]; then
    echo "error: Windows sudo disable was not reversible" >&2
    exit 1
fi
"$lsw" sudo enable "$instance"
windows_sudo_force_new_window=true
windows_sudo_policy=unmanaged
windows_uac_enabled=true
user_helper_output=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$Deadline=[DateTime]::UtcNow.AddSeconds(30); do { $Service=Get-CimInstance -ClassName Win32_Service -Filter "Name = '\''LSWUserHelper'\''"; if ($null -ne $Service -and $Service.State -eq "Stopped") { break }; Start-Sleep -Milliseconds 100 } while ([DateTime]::UtcNow -lt $Deadline); if ($null -eq $Service) { exit 73 }; if ($Service.State -ne "Stopped") { exit 74 }; if ($Service.StartMode -ne "Manual") { exit 75 }; if ($Service.StartName -ine "LocalSystem") { exit 76 }; if ($Service.PathName -notlike "*--user-helper*") { exit 77 }; [Console]::Out.Write("$($Service.StartMode)|$($Service.StartName)")'
)
user_helper_start_mode=$(printf '%s\n' "$user_helper_output" | awk -F'|' '{ print $1 }')
user_helper_start_name=$(printf '%s\n' "$user_helper_output" | awk -F'|' '{ print $2 }')
if [ "$user_helper_start_mode" != Manual ] || [ "$user_helper_start_name" != LocalSystem ]; then
    echo "error: account helper is not a stopped demand-start LocalSystem service" >&2
    exit 1
fi
maintenance_helper_output=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$Deadline=[DateTime]::UtcNow.AddSeconds(30); do { $Service=Get-CimInstance -ClassName Win32_Service -Filter "Name = '\''LSWMaintenanceHelper'\''"; if ($null -ne $Service -and $Service.State -eq "Stopped") { break }; Start-Sleep -Milliseconds 100 } while ([DateTime]::UtcNow -lt $Deadline); if ($null -eq $Service) { exit 78 }; if ($Service.State -ne "Stopped") { exit 79 }; if ($Service.StartMode -ne "Manual") { exit 80 }; if ($Service.StartName -ine "LocalSystem") { exit 81 }; if ($Service.PathName -notlike "*--maintenance-helper*") { exit 82 }; [Console]::Out.Write("$($Service.StartMode)|$($Service.StartName)")'
)
maintenance_helper_start_mode=$(printf '%s\n' "$maintenance_helper_output" | awk -F'|' '{ print $1 }')
maintenance_helper_start_name=$(printf '%s\n' "$maintenance_helper_output" | awk -F'|' '{ print $2 }')
if [ "$maintenance_helper_start_mode" != Manual ] \
    || [ "$maintenance_helper_start_name" != LocalSystem ]; then
    echo "error: maintenance helper is not a stopped demand-start LocalSystem service" >&2
    exit 1
fi
if grep -R -F --exclude='*.qcow2' --exclude='OVMF_VARS.fd' \
    'Lsw!' "$LSW_STATE_DIR/instances/$instance" >/dev/null 2>&1; then
    echo "error: desktop password material entered LSW metadata, seeds, or logs" >&2
    exit 1
fi
desktop_user_registered=true
edition_normalized=$(printf '%s' "$edition" | tr '[:upper:]' '[:lower:]')
case "$edition_normalized" in
    pro|professional)
        if [ "$guest_edition" != Professional ]; then
            echo "error: requested Windows Pro but the guest reported EditionID=$guest_edition" >&2
            exit 1
        fi
        ;;
    enterprise|education)
        expected_edition=$(printf '%s' "$edition_normalized" |
            awk '{ print toupper(substr($0, 1, 1)) substr($0, 2) }')
        if [ "$guest_edition" != "$expected_edition" ]; then
            echo "error: requested Windows $expected_edition but the guest reported EditionID=$guest_edition" >&2
            exit 1
        fi
        ;;
    home|core)
        case "$guest_edition" in
            Core|CoreN|CoreSingleLanguage|CoreCountrySpecific) ;;
            *)
                echo "error: requested Windows Home but the guest reported EditionID=$guest_edition" >&2
                exit 1
                ;;
        esac
        ;;
esac
}
