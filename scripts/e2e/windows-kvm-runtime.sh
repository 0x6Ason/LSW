// SPDX-License-Identifier: GPL-3.0-or-later
# shellcheck shell=sh
# ConPTY, transfer, sharing, restart, and release-attestation scenario.
# shellcheck disable=SC2154

run_windows_kvm_runtime_scenario() {
conpty_prefix='LSW_WINDOWS_KVM_CONPTY_'
conpty_suffix="OK_$$"
conpty_marker="$conpty_prefix$conpty_suffix"
conpty_identity="$agent_service_sid|0"
conpty_command=$(printf "\$a='%s'; \$b='%s'; Write-Output (\$a+\$b); \$s=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value; \$i=[System.Diagnostics.Process]::GetCurrentProcess().SessionId; Write-Output (\$s+'|'+\$i)" \
    "$conpty_prefix" "$conpty_suffix")
set +e
conpty_output=$(
    run_conpty_probe 60 "$conpty_marker" "$conpty_command" \
        "$LSW_E2E_LSW" shell "$LSW_E2E_INSTANCE" 2>&1
)
conpty_status=$?
set -e
if [ "$conpty_status" -ne 0 ]; then
    printf '%s\n' "$conpty_output" >&2
    echo "error: live ConPTY probe exited with status $conpty_status" >&2
    exit 1
fi
if printf '%s\n' "$conpty_output" | grep -F 'ConPTY is not available' >/dev/null; then
    echo "error: guest agent fell back to a pipe shell instead of ConPTY" >&2
    exit 1
fi
if ! printf '%s\n' "$conpty_output" | tr -d '\r' | grep -F "$conpty_marker" >/dev/null; then
    echo "error: live ConPTY probe did not return its marker" >&2
    exit 1
fi
if ! printf '%s\n' "$conpty_output" | tr -d '\r' | grep -Fx "$conpty_identity" >/dev/null; then
    echo "error: live ConPTY shell did not run as the expected service SID in session 0" >&2
    exit 1
fi

exec_marker="LSW_WINDOWS_KVM_EXEC_OK_$$"
output=$("$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    "Write-Output '$exec_marker'")
if ! printf '%s\n' "$output" | tr -d '\r' | grep -Fx "$exec_marker" >/dev/null; then
    echo "error: lsw exec did not return its marker" >&2
    exit 1
fi

set +e
"$lsw" exec "$instance" -- cmd.exe /d /c exit 37
guest_status=$?
set -e
if [ "$guest_status" -ne 37 ]; then
    echo "error: guest exit code 37 became host exit code $guest_status" >&2
    exit 1
fi

exec_environment="LSW_BETA6_ENV_$$"
# PowerShell, not the host shell, expands $env in the guest command.
# shellcheck disable=SC2016
exec_context=$(
    "$lsw" exec "$instance" \
        --cwd 'C:\Windows\Temp' \
        --env "LSW_E2E_VALUE=$exec_environment" \
        -- powershell.exe -NoLogo -NoProfile -Command \
        '[Console]::Out.WriteLine((Get-Location).Path+"|"+$env:LSW_E2E_VALUE)'
)
if ! printf '%s\n' "$exec_context" | tr -d '\r' \
    | grep -Fxi "C:\Windows\Temp|$exec_environment" >/dev/null
then
    echo "error: exec did not preserve its working directory and environment" >&2
    exit 1
fi
exec_context_verified=true
echo "cwd and environment injection passed."

set +e
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    'Start-Sleep -Seconds 120' >"$e2e_root/signal.stdout" 2>"$e2e_root/signal.stderr" &
signal_pid=$!
sleep 2
kill -TERM "$signal_pid"
wait "$signal_pid"
signal_status=$?
set -e
if [ "$signal_status" -ne 143 ]; then
    echo "error: SIGTERM returned $signal_status instead of exact status 143" >&2
    cat "$e2e_root/signal.stderr" >&2
    exit 1
fi
echo "SIGTERM propagation returned exact status 143."

guest_test_root="C:\ProgramData\LSW\e2e-$$"
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    "if (Test-Path -LiteralPath '$guest_test_root') { throw 'guest test root already exists' }; New-Item -ItemType Directory -Path '$guest_test_root' | Out-Null"
guest_detached_marker="$guest_test_root\detached.txt"
detached_output=$(
    "$lsw" run "$instance" --detach -- powershell.exe -NoLogo -NoProfile -Command \
        "Start-Sleep -Milliseconds 500; Set-Content -LiteralPath '$guest_detached_marker' -Value 'DETACHED_OK' -NoNewline"
)
if ! printf '%s\n' "$detached_output" \
    | grep -E "^Started detached process [1-9][0-9]* in \"$instance\"\.$" >/dev/null
then
    echo "error: detached run did not return a guest process ID" >&2
    exit 1
fi
attempt=0
detached_marker=
while [ "$attempt" -lt 100 ]; do
    detached_marker=$(
        "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
            "if (Test-Path -LiteralPath '$guest_detached_marker') { Get-Content -LiteralPath '$guest_detached_marker' -Raw }" \
            2>/dev/null || :
    )
    if [ "$(printf '%s' "$detached_marker" | tr -d '\r')" = DETACHED_OK ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
if [ "$(printf '%s' "$detached_marker" | tr -d '\r')" != DETACHED_OK ]; then
    echo "error: detached guest process did not survive client disconnect" >&2
    exit 1
fi
detached_run_verified=true
echo "detached run completed after client disconnect."

transfer_source="$e2e_root/transfer-source"
transfer_pull="$e2e_root/transfer-pull"
guest_transfer="$guest_test_root\transfer"
mkdir -p -- "$transfer_source/nested"
printf 'root-file\n' >"$transfer_source/root.txt"
printf 'nested-file\n' >"$transfer_source/nested/file with space.txt"
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    "if (Test-Path -LiteralPath '$guest_transfer') { Remove-Item -LiteralPath '$guest_transfer' -Recurse -Force }"
"$lsw" push "$instance" --recursive "$transfer_source" "$guest_transfer"
"$lsw" pull "$instance" --recursive "$guest_transfer" "$transfer_pull"
cmp "$transfer_source/root.txt" "$transfer_pull/root.txt"
cmp "$transfer_source/nested/file with space.txt" \
    "$transfer_pull/nested/file with space.txt"
recursive_transfer_verified=true
echo "recursive push and pull round-trip passed."

sync_source="$e2e_root/sync-source"
sync_log="$e2e_root/sync.log"
guest_sync="$guest_test_root\sync"
mkdir -p -- "$sync_source"
printf 'sync-one' >"$sync_source/value.txt"
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    "if (Test-Path -LiteralPath '$guest_sync') { Remove-Item -LiteralPath '$guest_sync' -Recurse -Force }"
setsid "$lsw" sync "$instance" --watch "$sync_source" "$guest_sync" \
    >"$sync_log" 2>&1 &
sync_pid=$!
attempt=0
sync_value=
while [ "$attempt" -lt 100 ]; do
    sync_value=$(
        "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
            "if (Test-Path -LiteralPath '$guest_sync\value.txt') { Get-Content -LiteralPath '$guest_sync\value.txt' -Raw }" \
            2>/dev/null || :
    )
    if [ "$(printf '%s' "$sync_value" | tr -d '\r')" = sync-one ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
if [ "$(printf '%s' "$sync_value" | tr -d '\r')" != sync-one ]; then
    echo "error: sync --watch did not complete its initial tree upload" >&2
    cat "$sync_log" >&2
    exit 1
fi
printf 'sync-two' >"$sync_source/value.txt"
mkdir -p -- "$sync_source/new-directory"
printf 'new-file' >"$sync_source/new-directory/new.txt"
attempt=0
sync_value=
sync_new_value=
while [ "$attempt" -lt 100 ]; do
    sync_value=$(
        "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
            "Get-Content -LiteralPath '$guest_sync\value.txt' -Raw" 2>/dev/null || :
    )
    sync_new_value=$(
        "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
            "if (Test-Path -LiteralPath '$guest_sync\new-directory\new.txt') { Get-Content -LiteralPath '$guest_sync\new-directory\new.txt' -Raw }" \
            2>/dev/null || :
    )
    if [ "$(printf '%s' "$sync_value" | tr -d '\r')" = sync-two ] \
        && [ "$(printf '%s' "$sync_new_value" | tr -d '\r')" = new-file ]
    then
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
if [ "$(printf '%s' "$sync_value" | tr -d '\r')" != sync-two ] \
    || [ "$(printf '%s' "$sync_new_value" | tr -d '\r')" != new-file ]
then
    echo "error: sync --watch did not propagate a changed file and new directory" >&2
    cat "$sync_log" >&2
    exit 1
fi
rm -- "$sync_source/value.txt"
sleep 2
sync_value=$(
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        "Get-Content -LiteralPath '$guest_sync\value.txt' -Raw"
)
if [ "$(printf '%s' "$sync_value" | tr -d '\r')" != sync-two ]; then
    echo "error: additive sync unexpectedly deleted the remote file" >&2
    exit 1
fi
watch_sync_verified=true
echo "additive sync --watch passed."
terminate_sync

share_source="$e2e_root/share-source"
guest_share="$guest_test_root\share"
mkdir -p -- "$share_source"
printf 'host-to-guest' >"$share_source/host.txt"
"$lsw" share add "$instance" source "$share_source" "$guest_share" --read-write
"$lsw" share sync "$instance" source
share_value=$(
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        "Get-Content -LiteralPath '$guest_share\host.txt' -Raw"
)
if [ "$(printf '%s' "$share_value" | tr -d '\r')" != host-to-guest ]; then
    echo "error: declarative share did not synchronize host data" >&2
    exit 1
fi
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    "Set-Content -LiteralPath '$guest_share\guest.txt' -Value 'guest-to-host' -NoNewline"
"$lsw" share sync "$instance" source --from-guest
if [ "$(cat "$share_source/guest.txt")" != guest-to-host ]; then
    echo "error: read-write share did not synchronize guest data" >&2
    exit 1
fi
mkdir -p -- "$share_source/escape"
printf 'must-not-escape' >"$share_source/escape/out.txt"
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    "New-Item -ItemType Junction -Path '$guest_share\escape' -Target 'C:\Windows\Temp' | Out-Null"
if "$lsw" share sync "$instance" source >/dev/null 2>&1; then
    echo "error: folder share followed a guest reparse point" >&2
    exit 1
fi
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    "Remove-Item -LiteralPath '$guest_share\escape' -Force"
rm -rf -- "$share_source/escape"
ln -s -- "$share_source/host.txt" "$share_source/link.txt"
if "$lsw" share sync "$instance" source >/dev/null 2>&1; then
    echo "error: folder share followed a host symbolic link" >&2
    exit 1
fi
rm -- "$share_source/link.txt"
"$lsw" share remove "$instance" source
"$lsw" share add "$instance" source "$share_source" "$guest_share" --read-only
"$lsw" share sync "$instance" source
read_only_acl=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" --env "LSW_SHARE_ROOT=$guest_share" -- \
        powershell.exe -NoLogo -NoProfile -Command \
        '$Acl=Get-Acl -LiteralPath $env:LSW_SHARE_ROOT; $Agent=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $Rules=@($Acl.Access); $Inheritance=[Security.AccessControl.InheritanceFlags]"ContainerInherit, ObjectInherit"; $Propagation=[Security.AccessControl.PropagationFlags]::None; $FullSids=@("S-1-5-18","S-1-5-32-544",$Agent); $FullRules=@($Rules | Where-Object { $FullSids -contains $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -and $_.AccessControlType -eq "Allow" -and $_.IsInherited -eq $false -and $_.InheritanceFlags -eq $Inheritance -and $_.PropagationFlags -eq $Propagation -and [int]($_.FileSystemRights) -eq [int]([Security.AccessControl.FileSystemRights]::FullControl) }); $Users=@($Rules | Where-Object { $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -eq "S-1-5-32-545" -and $_.AccessControlType -eq "Allow" -and $_.IsInherited -eq $false -and $_.InheritanceFlags -eq $Inheritance -and $_.PropagationFlags -eq $Propagation -and [int]($_.FileSystemRights) -eq [int]([Security.AccessControl.FileSystemRights]"ReadAndExecute, Synchronize") }); if (-not $Acl.AreAccessRulesProtected -or $Rules.Count -ne 4 -or $FullRules.Count -ne 3 -or $Users.Count -ne 1) { exit 41 }; [Console]::Out.Write("LSW_RO_ACL_OK")'
)
if [ "$read_only_acl" != LSW_RO_ACL_OK ]; then
    echo "error: read-only share did not install its protected guest ACL" >&2
    exit 1
fi
"$lsw" share remove "$instance" source
folder_share_boundaries=true

live_source="$e2e_root/live-source"
mkdir -p -- "$live_source"
printf 'live-host-one' >"$live_source/host.txt"
"$lsw" share "$live_source"
live_mapping=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$Mapping=Get-SmbMapping -LocalPath "L:" -ErrorAction Stop; if ($Mapping.RemotePath -eq "\\10.0.2.4\qemu" -and (Test-Path -LiteralPath "L:\host.txt")) { [Console]::Out.Write("LSW_LIVE_OK") }'
)
if [ "$live_mapping" != LSW_LIVE_OK ]; then
    echo "error: the private QEMU SMB root was not mounted in the agent session as Linux (L:)" >&2
    exit 1
fi
live_helper_state=$(
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$Service=Get-CimInstance -ClassName Win32_Service -Filter "Name = '\''LSWMaintenanceHelper'\''"; [Console]::Out.Write($Service.State)'
)
if [ "$live_helper_state" != Stopped ]; then
    echo "error: live sharing kept the privileged maintenance helper running" >&2
    exit 1
fi
# The restricted service account cannot use the administrator-only
# Get-SmbConnection CIM provider. Requiring both server policies here and then
# proving real I/O below demonstrates that the client negotiated those terms.
live_smb_config="$LSW_STATE_DIR/instances/$instance/run/live-smb/smb.conf"
if [ -L "$live_smb_config" ] || [ ! -f "$live_smb_config" ] || \
   [ "$(stat -c %a -- "$live_smb_config")" != 600 ] || \
   ! grep -Eq '^[[:space:]]*server signing = mandatory[[:space:]]*$' \
       "$live_smb_config" || \
   ! grep -Eq '^[[:space:]]*server smb encrypt = required[[:space:]]*$' \
       "$live_smb_config" || \
   ! grep -Eq '^[[:space:]]*smb encrypt = required[[:space:]]*$' \
       "$live_smb_config"; then
    echo "error: the private live SMB server did not require signing and encryption" >&2
    exit 1
fi
printf 'live-host-two' >"$live_source/host.txt"
live_value=$(
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        "Get-Content -LiteralPath 'L:\host.txt' -Raw"
)
if [ "$(printf '%s' "$live_value" | tr -d '\r')" != live-host-two ]; then
    echo "error: live folder did not expose a host update without synchronization" >&2
    exit 1
fi
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    "Set-Content -LiteralPath 'L:\guest.txt' -Value 'live-guest' -NoNewline"
if [ "$(cat "$live_source/guest.txt")" != live-guest ]; then
    echo "error: live folder did not expose a guest update without synchronization" >&2
    exit 1
fi

printf 'cp-host' >"$e2e_root/cp-host.txt"
"$lsw" cp "$e2e_root/cp-host.txt" "$guest_test_root\cp-host.txt"
"$lsw" cp "$guest_test_root\cp-host.txt" "$e2e_root/cp-return.txt"
if [ "$(cat "$e2e_root/cp-return.txt")" != cp-host ]; then
    echo "error: lsw cp did not infer both transfer directions" >&2
    exit 1
fi

files_bench_json=$("$lsw" bench files "$instance" --json --size-mib 16 --small-files 32)
python3 - "$files_bench_json" <<'PY'
import json
import sys

result = json.loads(sys.argv[1])
if result.get("schema") != 1:
    raise SystemExit("error: file benchmark schema is not version 1")
if result.get("dataset") != {"sequential_mib": 16, "small_files": 32}:
    raise SystemExit("error: file benchmark dimensions were not retained")
if not result.get("guest_local", {}).get("available"):
    raise SystemExit("error: guest-local file benchmark was unavailable")
if not result.get("live_smb", {}).get("available"):
    raise SystemExit("error: live SMB file benchmark was unavailable")
if not result.get("agent_mirror", {}).get("available"):
    raise SystemExit("error: agent-mirror file benchmark was unavailable")
PY

"$lsw" unshare linux
if [ ! -f "$live_source/host.txt" ] || [ ! -f "$live_source/guest.txt" ]; then
    echo "error: unshare removed host files" >&2
    exit 1
fi
if "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    'if (Get-SmbMapping -LocalPath "L:" -ErrorAction SilentlyContinue) { exit 1 }' >/dev/null
then
    :
else
    echo "error: unshare left Linux (L:) mapped in the agent session" >&2
    exit 1
fi
live_folder_verified=true
echo "driverless live folder, inferred copy, and file benchmark passed."

"$lsw" memory reclaim "$instance"
if [ "$(cat "$LSW_STATE_DIR/instances/$instance/run/balloon.target")" != 2048 ]; then
    echo "error: memory governor did not persist its minimum balloon target" >&2
    exit 1
fi
"$lsw" memory restore "$instance"
if [ "$(cat "$LSW_STATE_DIR/instances/$instance/run/balloon.target")" != 4096 ]; then
    echo "error: memory governor did not restore the configured maximum" >&2
    exit 1
fi
"$lsw" trim "$instance" >/dev/null
profile_host_allocated_bytes=$(du -B1 \
    "$base_disk" "$LSW_STATE_DIR/instances/$instance/disk.qcow2" | \
    awk '{ total += $1 } END { print total }')
case "$profile_host_allocated_bytes" in
    ''|*[!0-9]*) echo "error: could not measure allocated profile storage" >&2; exit 1 ;;
esac
hibernate_pid=$(awk 'NR == 1 { print $1 }' \
    "$LSW_STATE_DIR/instances/$instance/run/qemu.pid")
timeout 180s "$lsw" hibernate "$instance"
if ! "$lsw" status "$instance" | grep -Fx 'STATE=hibernated' >/dev/null; then
    echo "error: Windows hibernation did not reach the hibernated state" >&2
    exit 1
fi
if kill -0 "$hibernate_pid" 2>/dev/null; then
    echo "error: QEMU remained resident after Windows hibernation" >&2
    exit 1
fi
hibernate_resume_marker=LSW_HIBERNATE_RESUME_OK
hibernate_resume=$(
    timeout "${agent_boot_timeout_seconds}s" "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        "[Console]::Out.Write('$hibernate_resume_marker')"
)
if [ "$hibernate_resume" != "$hibernate_resume_marker" ]; then
    echo "error: agent did not recover after Windows hibernation" >&2
    exit 1
fi
resource_governor_verified=true
# Keep all guest artifacts below the LSW data root, whose installer-managed
# ACL grants the virtual service account Modify access across restarts. The
# read-only share child separately grants that account FullControl, so removing
# the single run-specific root also proves the protected share ACL is usable.
# PowerShell, not the host shell, expands the cleanup variable.
# shellcheck disable=SC2016
if ! "$lsw" exec "$instance" \
    --env "LSW_CLEANUP_ROOT=$guest_test_root" \
    -- powershell.exe -NoLogo -NoProfile -Command \
    '$ErrorActionPreference="Stop"; $Root=$env:LSW_CLEANUP_ROOT; for ($Attempt=0; $Attempt -lt 40; $Attempt++) { try { if (Test-Path -LiteralPath $Root) { Remove-Item -LiteralPath $Root -Recurse -Force -ErrorAction Stop }; break } catch { if ($Attempt -eq 39) { throw ("guest cleanup failed for {0}: {1}" -f $Root,$_.Exception.Message) }; Start-Sleep -Milliseconds 250 } }; if (Test-Path -LiteralPath $Root) { throw ("guest cleanup left the test root at {0}" -f $Root) }'
then
    echo "error: guest test artifact cleanup failed" >&2
    exit 1
fi

if [ -n "$artifact_dir" ]; then
    "$lsw" bench "$instance" --json >"$artifact_dir/bench.json"
    chmod 600 "$artifact_dir/bench.json"
fi

qemu_pid=
agent_port=$(
    "$lsw" show "$instance" |
        awk -F: '/^agent host port:/ { value=$2; gsub(/[[:space:]]/, "", value); print value }'
)
if [ -f "$pid_file" ]; then
    qemu_pid=$(awk 'NR == 1 { print $1 }' "$pid_file")
fi
case "$qemu_pid" in
    ''|*[!0-9]*)
        echo "error: installed instance did not record a QEMU PID" >&2
        exit 1
        ;;
esac
if ! kill -0 "$qemu_pid" 2>/dev/null; then
    echo "error: installed instance's QEMU process is not alive" >&2
    exit 1
fi
case "$agent_port" in
    ''|*[!0-9]*)
        echo "error: installed instance did not report a numeric agent port" >&2
        exit 1
        ;;
esac
if [ "$agent_port" -lt 1 ] || [ "$agent_port" -gt 65535 ]; then
    echo "error: installed instance reported an out-of-range agent port" >&2
    exit 1
fi
"$lsw" shutdown "$instance"
assert_stopped_runtime_released "$qemu_pid" "$agent_port"
"$lsw" compact "$instance"

# A daily-use instance has to cold-start from its disk and make the agent and
# daemon available through a bare `lsw` invocation without a console sign-in
# or installation media.
terminate_viewer
terminate_daemon
cold_daemon_wrapper="$e2e_root/cold-daemon-wrapper.sh"
cold_daemon_session="$e2e_root/cold-daemon-session.sh"
# DaemonClient already launches the configured program through setsid. Executing
# setsid again here would fork because the wrapper is now a session leader, so
# the child monitored by DaemonClient could exit before the socket is ready.
# The variables are intentionally expanded later by the generated wrapper.
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'exec "$LSW_E2E_COLD_DAEMON_SESSION" "$@"' \
    >"$cold_daemon_wrapper"
# Record the daemon only after it owns its private session/process group.
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'printf "%s\n" "$$" >"$LSW_E2E_COLD_DAEMON_PID_FILE"' \
    'exec "$LSW_E2E_REAL_LSWD" "$@"' \
    >"$cold_daemon_session"
chmod 700 "$cold_daemon_wrapper" "$cold_daemon_session"
export LSW_E2E_COLD_DAEMON_PID_FILE="$cold_daemon_pid_file"
export LSW_E2E_COLD_DAEMON_SESSION="$cold_daemon_session"
export LSW_E2E_REAL_LSWD="$lswd"
export LSW_DAEMON="$cold_daemon_wrapper"
cold_prefix='LSW_WINDOWS_KVM_COLD_START_'
cold_suffix="OK_$$"
cold_marker="$cold_prefix$cold_suffix"
cold_conpty_identity="$agent_service_sid|0"
cold_command=$(printf "\$a='%s'; \$b='%s'; Write-Output (\$a+\$b); \$s=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value; \$i=[System.Diagnostics.Process]::GetCurrentProcess().SessionId; Write-Output (\$s+'|'+\$i)" \
    "$cold_prefix" "$cold_suffix")
set +e
cold_output=$(
    run_conpty_probe 180 "$cold_marker" "$cold_command" "$LSW_E2E_LSW" 2>&1
)
cold_status=$?
set -e
if ! adopt_cold_daemon_if_present; then
    printf '%s\n' "$cold_output" >&2
    exit 1
fi
export LSW_DAEMON="$autospawn_blocker"
if [ "$cold_status" -ne 0 ]; then
    printf '%s\n' "$cold_output" >&2
    echo "error: bare lsw did not restore a working shell after cold boot" >&2
    exit 1
fi
assert_daemon_alive
if printf '%s\n' "$cold_output" | grep -F 'ConPTY is not available' >/dev/null; then
    echo "error: cold-start shell fell back to pipes instead of ConPTY" >&2
    exit 1
fi
if ! printf '%s\n' "$cold_output" | tr -d '\r' | grep -F "$cold_marker" >/dev/null; then
    echo "error: cold-start ConPTY probe did not return its marker" >&2
    exit 1
fi
if ! printf '%s\n' "$cold_output" | tr -d '\r' | grep -Fx "$cold_conpty_identity" >/dev/null; then
    echo "error: cold-start ConPTY shell did not run as the expected service SID in session 0" >&2
    exit 1
fi

read_agent_service_identity
cold_agent_service_sid=$service_sid
cold_agent_service_pid=$service_pid
if [ "$cold_agent_service_sid" != "$agent_service_sid" ]; then
    echo "error: LSWAgent virtual-account process SID changed across cold boot" >&2
    exit 1
fi

# A service-backed cold start must work at the Windows sign-in screen. Reject a
# hidden second login as well as the Winlogon registry shortcuts checked before
# shutdown.
set +e
# PowerShell expands its own variables in the guest.
# shellcheck disable=SC2016
cold_console_output=$("$lsw" exec "$instance" -- \
    powershell.exe -NoLogo -NoProfile -Command \
    '$ErrorActionPreference="Stop"; $ComputerSystem=Get-CimInstance -ClassName Win32_ComputerSystem; if ($null -eq $ComputerSystem -or @($ComputerSystem).Count -ne 1) { exit 64 }; $Interactive=[string]$ComputerSystem.UserName; $Winlogon=Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon"; if ($null -eq $Winlogon) { exit 64 }; if ([string]$Winlogon.AutoAdminLogon -eq "1") { exit 61 }; $StoredPassword=$Winlogon.PSObject.Properties["DefaultPassword"]; if ($null -ne $StoredPassword -and -not [string]::IsNullOrEmpty([string]$StoredPassword.Value)) { exit 62 }; if (-not [string]::IsNullOrWhiteSpace($Interactive)) { exit 63 }; [Console]::Out.Write("LSW_WINDOWS_KVM_NO_COLD_CONSOLE_USER")')
cold_console_status=$?
set -e
if [ "$cold_console_status" -ne 0 ] \
    || [ "$cold_console_output" != LSW_WINDOWS_KVM_NO_COLD_CONSOLE_USER ]
then
    echo "error: cold-start gate detected an interactive login or automatic-logon credential" >&2
    exit 1
fi
cold_interactive_user=none
audit_slim_profile boot-3

cold_qemu_pid=
if [ -f "$pid_file" ]; then
    cold_qemu_pid=$(awk 'NR == 1 { print $1 }' "$pid_file")
fi
case "$cold_qemu_pid" in
    ''|*[!0-9]*)
        echo "error: cold-started instance did not record a QEMU PID" >&2
        exit 1
        ;;
esac
if ! kill -0 "$cold_qemu_pid" 2>/dev/null; then
    echo "error: cold-started QEMU process is not alive" >&2
    exit 1
fi
source_iso=$(
    "$lsw" show "$instance" |
        awk -v prefix='source ISO:           ' \
            'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }'
)
if [ -z "$source_iso" ]; then
    echo "error: lsw show did not report the installed instance's source ISO" >&2
    exit 1
fi
python3 - "/proc/$cold_qemu_pid/cmdline" "$source_iso" \
    "$LSW_STATE_DIR/instances/$instance/seed" <<'PY'
import os
from pathlib import Path
import sys

arguments = Path(sys.argv[1]).read_bytes().split(b"\0")
for forbidden in sys.argv[2:]:
    needle = os.fsencode(forbidden)
    if needle and any(needle in argument for argument in arguments):
        raise SystemExit(
            f"error: cold-started QEMU still attached installation media: {forbidden}"
        )
PY

cold_exec_marker="LSW_WINDOWS_KVM_COLD_EXEC_OK_$$"
cold_exec_output=$(
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        "Write-Output '$cold_exec_marker'"
)
if ! printf '%s\n' "$cold_exec_output" | tr -d '\r' | grep -Fx "$cold_exec_marker" >/dev/null; then
    echo "error: cold-start lsw exec did not return its marker" >&2
    exit 1
fi
assert_daemon_alive

"$lsw" shutdown "$instance"
assert_stopped_runtime_released "$cold_qemu_pid" "$agent_port"

if [ -n "$artifact_dir" ]; then
    timeout 30s "$lsw" diagnose "$instance" --bundle \
        --output "$artifact_dir/diagnose.tar.gz" >/dev/null
    chmod 600 "$artifact_dir/diagnose.tar.gz"
else
    "$lsw" diagnose "$instance" --bundle --output "$e2e_root/diagnose.tar.gz"
fi
if [ "$gui_handoff" = 1 ]; then
    prepare_gui_handoff
else
    "$lsw" remove "$instance"
    if [ -e "$LSW_STATE_DIR/instances/$instance" ]; then
        echo "error: instance directory remained after lsw remove" >&2
        exit 1
    fi
fi
terminate_daemon
collect_e2e_artifacts success

if [ "$gui_handoff" = 1 ]; then
    echo "Windows/KVM E2E passed and produced a stopped real-install linked clone for native GUI validation."
else
    echo "Windows/KVM E2E passed: WinPE -> unattended OOBE -> LSWAgent service -> ConPTY -> shutdown -> cold restart -> cleanup."
fi
}
