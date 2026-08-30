#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later

# Short, signed-in seamless-window acceptance test for an existing instance.
# This script never installs, starts, stops, creates, removes, or deletes a VM.

set -eu

overall_timeout_seconds=${LSW_SEAMLESS_E2E_TIMEOUT_SECONDS:-1800}
expected_guest_agent_sha=${LSW_SEAMLESS_E2E_EXPECTED_AGENT_SHA256:-}
case "$overall_timeout_seconds" in
    ''|*[!0-9]*)
        echo "error: LSW_SEAMLESS_E2E_TIMEOUT_SECONDS must be a positive integer" >&2
        exit 1
        ;;
esac
if [ "$overall_timeout_seconds" -eq 0 ]; then
    echo "error: LSW_SEAMLESS_E2E_TIMEOUT_SECONDS must be greater than zero" >&2
    exit 1
fi
case "$expected_guest_agent_sha" in
    ''|*[!0-9A-Fa-f]*)
        echo "error: LSW_SEAMLESS_E2E_EXPECTED_AGENT_SHA256 must be the exact 64-hex candidate guest-agent SHA" >&2
        exit 1
        ;;
esac
if [ "${#expected_guest_agent_sha}" -ne 64 ]; then
    echo "error: LSW_SEAMLESS_E2E_EXPECTED_AGENT_SHA256 must contain exactly 64 hex digits" >&2
    exit 1
fi

if ! command -v timeout >/dev/null 2>&1; then
    echo "error: GNU timeout is required for the seamless E2E driver" >&2
    exit 1
fi
if [ "${LSW_SEAMLESS_E2E_TIMEOUT_ACTIVE:-0}" != 1 ]; then
    LSW_SEAMLESS_E2E_TIMEOUT_ACTIVE=1
    export LSW_SEAMLESS_E2E_TIMEOUT_ACTIVE
    # The TERM trap performs bounded guest and host cleanup. Give it enough
    # time to finish rather than SIGKILLing it halfway through artifact removal.
    exec timeout --signal=TERM --kill-after=45s "${overall_timeout_seconds}s" sh "$0" "$@"
fi

if [ "$#" -gt 1 ]; then
    echo "usage: scripts/check-windows-seamless-e2e.sh [DEFAULT_INSTANCE]" >&2
    exit 1
fi

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
lsw=${LSW_E2E_LSW:-"$workspace_root/target/release/lsw"}
lswg=${LSW_E2E_LSWG:-"$workspace_root/target/release/lswg"}
fixture_source="$workspace_root/scripts/windows-seamless-fixture.ps1"
host_helper_source="$workspace_root/scripts/windows-seamless-host.ps1"
wslg_transport_probe_source="$workspace_root/scripts/check-wslg-transport.sh"
window_title_prefix='LSW Seamless Fixture '
window_title=
for required_command in \
    awk base64 convert date dirname env grep iconv identify kill mktemp powershell.exe rm rmdir \
    id setsid sha256sum sleep stat tail tr wsl.exe wslpath
do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: $required_command is required for the WSLg Wayland seamless E2E driver" >&2
        echo "error: provision the prerequisite outside this script; it never downloads or installs software" >&2
        exit 1
    fi
done
expected_guest_agent_sha=$(printf '%s' "$expected_guest_agent_sha" | tr 'A-F' 'a-f')
if [ -z "${WSL_DISTRO_NAME:-}" ] || [ -z "${WAYLAND_DISPLAY:-}" ]; then
    echo "error: this driver requires an interactive WSLg Wayland session (WSL_DISTRO_NAME and WAYLAND_DISPLAY)" >&2
    exit 1
fi
if [ ! -x "$lsw" ] || [ ! -x "$lswg" ]; then
    echo "error: LSW_E2E_LSW and LSW_E2E_LSWG must name executable candidate binaries" >&2
    exit 1
fi
candidate_sha256() {
    candidate_value=$(sha256sum -- "$1" | awk '{ print $1; exit }') || {
        echo "error: could not hash the exact Linux $2 candidate" >&2
        return 1
    }
    printf '%s\n' "$candidate_value" | grep -Eq '^[0-9a-f]{64}$' || {
        echo "error: sha256sum returned an invalid Linux $2 candidate hash" >&2
        return 1
    }
    printf '%s\n' "$candidate_value"
}
cli_sha=$(candidate_sha256 "$lsw" lsw) || exit 1
lswg_sha=$(candidate_sha256 "$lswg" lswg) || exit 1
export LSWG="$lswg"
export LSWG_SHA256="$lswg_sha"
if [ ! -f "$fixture_source" ] || [ -L "$fixture_source" ]; then
    echo "error: the seamless fixture must be a regular non-symlink file: $fixture_source" >&2
    exit 1
fi
if [ ! -f "$host_helper_source" ] || [ -L "$host_helper_source" ]; then
    echo "error: the Windows host helper must be a regular non-symlink file: $host_helper_source" >&2
    exit 1
fi
if [ ! -f "$wslg_transport_probe_source" ] || [ -L "$wslg_transport_probe_source" ]; then
    echo "error: the WSLg transport probe must be a regular non-symlink file: $wslg_transport_probe_source" >&2
    exit 1
fi
host_helper_windows=$(wslpath -w "$host_helper_source") || {
    echo "error: could not translate the Windows host helper path" >&2
    exit 1
}

run_lsw() {
    run_lsw_seconds=$1
    shift
    timeout --signal=TERM --kill-after=3s "${run_lsw_seconds}s" "$lsw" "$@"
}

run_host() {
    run_host_seconds=$1
    shift
    timeout --signal=TERM --kill-after=2s "${run_host_seconds}s" "$@"
}

run_windows_host() {
    windows_host_seconds=$1
    windows_host_action=$2
    shift 2
    run_host "$windows_host_seconds" powershell.exe -NoLogo -NoProfile -NonInteractive \
        -ExecutionPolicy Bypass -File "$host_helper_windows" \
        -Action "$windows_host_action" "$@"
}

run_fixture_host() {
    fixture_host_seconds=$1
    fixture_host_action=$2
    shift 2
    run_windows_host "$fixture_host_seconds" "$fixture_host_action" \
        -TitleNeedle "$window_title" -ProcessName msrdc "$@"
}

run_guest_powershell() {
    guest_powershell_seconds=$1
    guest_powershell_script=$2
    guest_powershell_encoded=$(printf '%s' "$guest_powershell_script" \
        | iconv -f UTF-8 -t UTF-16LE \
        | base64 \
        | tr -d '\n')
    run_lsw "$guest_powershell_seconds" exec "$instance" -- \
        powershell.exe -NoLogo -NoProfile -NonInteractive \
        -EncodedCommand "$guest_powershell_encoded" </dev/null
}

pass() {
    printf 'CHECK %s=pass\n' "$1"
}

fail() {
    echo "error: $*" >&2
    exit 1
}

# WSLg's VAIL transport can retain a mounted but unusable virtiofs share after
# a host or WSL failure. Read the active RDP backend's transport identity first,
# then probe the exact system-distro mount before trusting VAIL. Current Weston
# automatically falls back to copy mode when its initial shared-memory
# allocation fails, while retaining the inherited VAIL environment variable;
# accept that fallback only when the current, freshly truncated Weston log
# reports use_gfxredir=0. A copy-mode Weston can legitimately coexist with a
# stale, unused mount and remains a supported fallback.
wslg_transport_probe=$(timeout 8s wsl.exe --system --user root --exec sh -s \
    <"$wslg_transport_probe_source" 2>/dev/null) \
    || fail "WSLg shared-memory transport is unavailable; stop active WSL workloads and run 'wsl --shutdown' before retrying"
case "$wslg_transport_probe" in
    vail|copy) ;;
    *) fail "WSLg transport preflight returned an invalid result" ;;
esac
printf 'WSLG_TRANSPORT=%s\n' "$wslg_transport_probe"

default_status=$(run_lsw 12 status) || fail "could not query the default LSW instance"
default_instance=$(printf '%s\n' "$default_status" | awk -F= '$1 == "instance" { print $2; exit }')
if [ -z "$default_instance" ]; then
    fail "lsw status did not report the default instance"
fi
instance=${1:-$default_instance}
if [ "$instance" != "$default_instance" ]; then
    echo "error: lsw cp addresses only the configured default instance" >&2
    echo "error: requested $instance but the default instance is $default_instance" >&2
    exit 1
fi

instance_status=$(run_lsw 12 status "$instance") || fail "could not query instance $instance"
if ! printf '%s\n' "$instance_status" | grep -Fx 'STATE=running' >/dev/null; then
    fail "instance $instance must already be running; this driver will not start it"
fi
if ! printf '%s\n' "$instance_status" | grep -Fx 'AGENT=ready' >/dev/null; then
    fail "instance $instance must already have a ready guest agent"
fi

instance_details=$(run_lsw 10 show "$instance") \
    || fail "could not inspect the registered Windows desktop identity"
desktop_user=$(printf '%s\n' "$instance_details" | awk -F ': +' \
    '$1 == "default Windows user" { print $2; found=1; exit } END { if (!found) exit 1 }') \
    || fail "instance $instance did not report its registered Windows desktop user"
[ "$desktop_user" != 'not registered' ] \
    || fail "instance $instance has no registered Windows desktop user"

# Normal exec intentionally runs as the restricted LSWAgent service identity and
# cannot authoritatively enumerate WTS users. A unique non-service Explorer
# session is only a bounded shell-readiness heuristic; the first real --gui
# request remains the fail-closed LocalSystem/WTS authority for $desktop_user.
# shellcheck disable=SC2016
signed_in_raw=$(run_guest_powershell 10 \
    '$ErrorActionPreference="Stop"; $Sessions=@(Get-Process -Name explorer | Where-Object { $_.SessionId -gt 0 } | Select-Object -ExpandProperty SessionId -Unique); if ($Sessions.Count -eq 1) { [Console]::Out.Write("SIGNED_IN_SESSION="+$Sessions[0]) } else { [Console]::Out.Write("SIGNED_IN_SESSION=unavailable") }') \
    || fail "instance $instance has no queryable interactive Windows shell"
signed_in_output=$(printf '%s\n' "$signed_in_raw" | tr -d '\r')
case "$signed_in_output" in
    SIGNED_IN_SESSION=[1-9]*) ;;
    *) fail "instance $instance must already expose one interactive Windows shell session" ;;
esac
pass signed_in_user

# The interactive GUI matrix is meaningless if the VM still runs an agent from
# a previous candidate. Hash the exact installed service binary before staging
# any fixture and fail closed on reparse points or path redirection.
# shellcheck disable=SC2016
guest_agent_hash_raw=$(run_guest_powershell 15 \
    '$ErrorActionPreference="Stop"; $Expected=[IO.Path]::GetFullPath("C:\Program Files\LSW\lsw-agent.exe"); $File=Get-Item -LiteralPath $Expected -Force; if ($File.FullName -cne $Expected) { throw "guest agent path changed" }; if (($File.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "guest agent must not be a reparse point" }; $Hash=(Get-FileHash -LiteralPath $Expected -Algorithm SHA256).Hash.ToLowerInvariant(); [Console]::Out.Write("GUEST_AGENT_SHA="+$Hash)') \
    || fail "could not attest the installed guest agent"
guest_agent_hash_output=$(printf '%s\n' "$guest_agent_hash_raw" | tr -d '\r')
guest_agent_sha=$(printf '%s\n' "$guest_agent_hash_output" | awk -F= \
    '$1 == "GUEST_AGENT_SHA" && $2 ~ /^[0-9a-f]+$/ && length($2) == 64 { print $2; exit }')
[ "$guest_agent_sha" = "$expected_guest_agent_sha" ] \
    || fail "guest agent SHA $guest_agent_sha did not match exact candidate $expected_guest_agent_sha"
pass guest_agent_exact_sha

stale_windows=$(run_windows_host 8 Find -TitleNeedle "$window_title_prefix" \
    -ProcessName msrdc 2>/dev/null) \
    || fail "could not inspect Windows for stale WSLg seamless fixture windows"
if [ -n "$stale_windows" ]; then
    fail "a stale WSLg LSW Seamless Fixture host window is already visible; close it before testing"
fi

temporary_base=${TMPDIR:-/tmp}
if [ ! -d "$temporary_base" ] || [ -L "$temporary_base" ]; then
    fail "TMPDIR must be an existing real directory"
fi
host_root=$(mktemp -d -- "$temporary_base/lsw-seamless-e2e.XXXXXX")
case "$host_root" in
    "$temporary_base"/lsw-seamless-e2e.??????) ;;
    *) fail "mktemp returned an unexpected seamless E2E directory" ;;
esac
host_root_uid=$(id -u)
host_root_identity=$(stat -Lc '%d:%i:%u:%a' -- "$host_root") \
    || fail "could not pin the seamless E2E host directory identity"
case "$host_root_identity" in
    *:"$host_root_uid":700) ;;
    *) fail "the seamless E2E host directory must be owned by this user with mode 0700" ;;
esac
exec 9<"$host_root" || fail "could not hold the seamless E2E host directory open"
host_root_fd_path="/proc/$$/fd/9"
host_root_fd_identity=$(stat -Lc '%d:%i:%u:%a' -- "$host_root_fd_path") \
    || fail "could not verify the pinned seamless E2E host directory handle"
[ "$host_root_fd_identity" = "$host_root_identity" ] \
    || fail "the seamless E2E host directory handle changed identity"
host_artifacts=

run_id="$(date +%s)-$$"
window_title="LSW Seamless Fixture $run_id - LSW"
guest_root="C:\\Users\\Public\\Documents\\LSW-Seamless-E2E-$run_id"
guest_fixture="$guest_root\\fixture.ps1"
guest_cleanup_signal="$guest_root\\cleanup.signal"
guest_owner_token_path="$guest_root\\owner.token"
guest_owner_token=$(printf '%s' "$run_id:$host_root:$cli_sha" | sha256sum | awk '{ print $1; exit }')
case "$guest_owner_token" in
    ''|*[!0-9a-f]*) fail "could not create the guest cleanup ownership token" ;;
    *) ;;
esac
[ "${#guest_owner_token}" -eq 64 ] || fail "guest cleanup ownership token is not SHA-256"
marker_main="$guest_root\\main.tsv"
marker_kill="$guest_root\\kill.tsv"
marker_recovery="$guest_root\\recovery.tsv"
marker_animate="$guest_root\\animate.tsv"
marker_error="$guest_root\\presenter-error.tsv"
marker_close="$guest_root\\close-prompt.tsv"
marker_host_close="$guest_root\\host-close.tsv"
marker_initial_max="$guest_root\\initial-maximized.tsv"
released_input_state='input-state=ctrl:0,shift:0,alt:0,left:0,middle:0,right:0,x1:0,x2:0'
initial_sentinel_color='#0C8CDC'
file_menu_color='#EC407A'
context_menu_color='#16A34A'
modal_color='#7E22CE'
resize_color='#F97316'
maximize_color='#DC2626'
close_prompt_color='#FACC15'
presenter_pid=
presenter_log=
window_id=
focus_sink_pid=
focus_sink_window=
focus_sink_title=
guest_root_created=0
cleanup_expected_markers=
matrix_complete=0

register_host_artifact() {
    register_host_name=$1
    case "$register_host_name" in
        ''|*[!A-Za-z0-9._-]*) fail "refusing unexpected host artifact name $register_host_name" ;;
        *) ;;
    esac
    host_artifacts=${host_artifacts:+"$host_artifacts
"}$register_host_name
}

register_fixture_marker() {
    register_marker_name=${1##*\\}
    case "$register_marker_name" in
        main.tsv|kill.tsv|recovery.tsv|animate.tsv|presenter-error.tsv|close-prompt.tsv|host-close.tsv|initial-maximized.tsv) ;;
        *) fail "refusing to register unexpected fixture marker $register_marker_name" ;;
    esac
    case ",$cleanup_expected_markers," in
        *",$register_marker_name,"*) ;;
        *) cleanup_expected_markers=${cleanup_expected_markers:+$cleanup_expected_markers,}$register_marker_name ;;
    esac
}

terminate_presenter() {
    case "$presenter_pid" in
        ''|*[!0-9]*)
            presenter_pid=
            return 0
            ;;
    esac
    if kill -0 "$presenter_pid" 2>/dev/null; then
        kill -TERM "$presenter_pid" 2>/dev/null || :
        terminate_attempt=0
        while kill -0 "$presenter_pid" 2>/dev/null \
            && [ "$terminate_attempt" -lt 30 ]
        do
            terminate_attempt=$((terminate_attempt + 1))
            sleep 0.1
        done
    fi
    if kill -0 "$presenter_pid" 2>/dev/null; then
        kill -KILL "$presenter_pid" 2>/dev/null || :
    fi
    wait "$presenter_pid" 2>/dev/null || :
    presenter_pid=
}

terminate_focus_sink() {
    case "$focus_sink_pid" in
        ''|*[!0-9]*)
            focus_sink_pid=
            focus_sink_window=
            return 0
            ;;
    esac
    case "$focus_sink_window" in
        ''|*[!0-9]*) ;;
        *) run_windows_host 5 Close -Hwnd "$focus_sink_window" \
            -TitleNeedle "$focus_sink_title" -ProcessName powershell \
            -ExactTitle >/dev/null 2>&1 || : ;;
    esac
    if kill -0 "$focus_sink_pid" 2>/dev/null; then
        focus_stop_attempt=0
        while kill -0 "$focus_sink_pid" 2>/dev/null \
            && [ "$focus_stop_attempt" -lt 20 ]
        do
            focus_stop_attempt=$((focus_stop_attempt + 1))
            sleep 0.1
        done
    fi
    if kill -0 "$focus_sink_pid" 2>/dev/null; then
        kill -TERM "$focus_sink_pid" 2>/dev/null || :
        sleep 0.2
    fi
    if kill -0 "$focus_sink_pid" 2>/dev/null; then
        kill -KILL "$focus_sink_pid" 2>/dev/null || :
    fi
    wait "$focus_sink_pid" 2>/dev/null || :
    focus_sink_pid=
    focus_sink_window=
    focus_sink_title=
}

cleanup_guest_root() {
    if [ "$guest_root_created" -ne 1 ]; then
        return 0
    fi
    case "$guest_root" in
        'C:\Users\Public\Documents\LSW-Seamless-E2E-'*) ;;
        *)
            echo "error: refusing cleanup of unexpected guest path $guest_root" >&2
            return 1
            ;;
    esac
    cleanup_status=$(run_lsw 6 status "$instance" 2>/dev/null || :)
    if ! printf '%s\n' "$cleanup_status" | grep -Fx 'STATE=running' >/dev/null; then
        echo "warning: guest test directory remains because the existing instance is no longer running: $guest_root" >&2
        return 1
    fi
    cleanup_script="\$ErrorActionPreference='Stop'; \$Expected=[IO.Path]::GetFullPath('$guest_root'); \$Signal=[IO.Path]::GetFullPath('$guest_cleanup_signal'); \$ExpectedSignal=[IO.Path]::Combine(\$Expected,'cleanup.signal'); \$MarkerCsv='$cleanup_expected_markers'; if (\$Signal -cne \$ExpectedSignal) { throw 'guest cleanup signal path changed' }; if (Test-Path -LiteralPath \$Expected) { \$Directory=Get-Item -LiteralPath \$Expected -Force; if (\$Directory.FullName -cne \$Expected) { throw 'guest cleanup path changed' }; if ((\$Directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'guest cleanup directory must not be a reparse point' }; if (Test-Path -LiteralPath \$Signal) { \$SignalItem=Get-Item -LiteralPath \$Signal -Force; if (\$SignalItem.FullName -cne \$Signal -or \$SignalItem.PSIsContainer -or ((\$SignalItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'guest cleanup signal must be the exact regular file' } }; [IO.File]::WriteAllText(\$Signal,'cleanup',[Text.UTF8Encoding]::new(\$false)); \$MarkerNames=@(); if (-not [string]::IsNullOrEmpty(\$MarkerCsv)) { \$MarkerNames=@(\$MarkerCsv.Split(',')) }; \$Deadline=[DateTime]::UtcNow.AddSeconds(10); do { \$Pending=@(); foreach (\$MarkerName in \$MarkerNames) { if (\$MarkerName -notmatch '^[A-Za-z0-9-]+[.]tsv\$') { throw 'unexpected fixture marker name' }; \$MarkerPath=[IO.Path]::GetFullPath([IO.Path]::Combine(\$Expected,\$MarkerName)); if ([IO.Path]::GetDirectoryName(\$MarkerPath) -cne \$Expected -or -not (Test-Path -LiteralPath \$MarkerPath -PathType Leaf)) { \$Pending += \$MarkerName; continue }; \$Marker=Get-Item -LiteralPath \$MarkerPath -Force; if (\$Marker.FullName -cne \$MarkerPath -or ((\$Marker.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'fixture marker must be the exact regular file' }; \$Lines=@(Get-Content -LiteralPath \$MarkerPath -ErrorAction Stop); if (@(\$Lines | Where-Object { \$_ -match '\tclosed\$' }).Count -gt 0) { continue }; \$PidLine=@(\$Lines | Where-Object { \$_ -match '\tprocess-id=([1-9][0-9]*)\$' } | Select-Object -First 1); if (\$PidLine.Count -eq 0) { \$Pending += \$MarkerName; continue }; [void](\$PidLine[0] -match '\tprocess-id=([1-9][0-9]*)\$'); if (\$null -ne (Get-Process -Id ([int]\$Matches[1]) -ErrorAction SilentlyContinue)) { \$Pending += \$MarkerName } }; if (\$Pending.Count -eq 0) { break }; Start-Sleep -Milliseconds 100 } while ([DateTime]::UtcNow -lt \$Deadline); if (\$Pending.Count -ne 0) { throw ('fixture cleanup signal timed out: '+(\$Pending -join ',')) }; \$Stack=[Collections.Generic.Stack[string]]::new(); \$Stack.Push(\$Expected); while (\$Stack.Count -gt 0) { foreach (\$Entry in [IO.Directory]::EnumerateFileSystemEntries(\$Stack.Pop())) { \$Attributes=[IO.File]::GetAttributes(\$Entry); if ((\$Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'guest cleanup tree contains a reparse point' }; if ((\$Attributes -band [IO.FileAttributes]::Directory) -ne 0) { \$Stack.Push(\$Entry) } } }; for (\$Attempt=0; \$Attempt -lt 30; \$Attempt++) { try { Remove-Item -LiteralPath \$Expected -Recurse -Force -ErrorAction Stop; break } catch { if (\$Attempt -eq 29) { throw }; Start-Sleep -Milliseconds 200 } } }; if (Test-Path -LiteralPath \$Expected) { throw 'guest seamless E2E directory remained after cleanup' }"
    cleanup_script="\$ErrorActionPreference='Stop'; \$OwnershipRoot=[IO.Path]::GetFullPath('$guest_root'); \$Owner=[IO.Path]::GetFullPath('$guest_owner_token_path'); if (\$Owner -cne [IO.Path]::Combine(\$OwnershipRoot,'owner.token')) { throw 'guest owner-token path changed' }; if (Test-Path -LiteralPath \$OwnershipRoot) { if (-not (Test-Path -LiteralPath \$Owner -PathType Leaf)) { throw 'guest cleanup root has no ownership token' }; \$OwnerItem=Get-Item -LiteralPath \$Owner -Force; if (\$OwnerItem.FullName -cne \$Owner -or ((\$OwnerItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or [IO.File]::ReadAllText(\$Owner) -cne '$guest_owner_token') { throw 'guest cleanup root ownership token is invalid' } }; $cleanup_script"
    if run_guest_powershell 20 "$cleanup_script" >/dev/null 2>&1
    then
        guest_root_created=0
        return 0
    fi
    echo "error: could not remove guest seamless E2E directory $guest_root" >&2
    return 1
}

cleanup() {
    cleanup_result=$?
    trap - EXIT HUP INT TERM
    run_windows_host 5 ReleaseAll 2>/dev/null || :
    terminate_focus_sink
    terminate_presenter
    if [ "$cleanup_result" -ne 0 ] && [ -n "$presenter_log" ] && [ -f "$presenter_log" ]; then
        echo "--- seamless presenter log (tail) ---" >&2
        tail -n 100 "$presenter_log" >&2 || :
    fi
    if ! cleanup_guest_root && [ "$cleanup_result" -eq 0 ]; then
        cleanup_result=1
    fi
    host_cleanup_safe=0
    host_cleanup_fd_identity=$(stat -Lc '%d:%i:%u:%a' -- "$host_root_fd_path" 2>/dev/null || :)
    if [ "$host_cleanup_fd_identity" = "$host_root_identity" ]; then
        host_cleanup_safe=1
    fi
    if [ "$host_cleanup_safe" -eq 1 ]; then
        host_cleanup_old_ifs=$IFS
        IFS='
'
        for host_artifact in $host_artifacts
        do
            case "$host_artifact" in
                ''|*[!A-Za-z0-9._-]*)
                    echo "warning: refusing cleanup of unexpected host artifact $host_artifact" >&2
                    cleanup_result=1
                    ;;
                *)
                    if ! rm -f -- "$host_root_fd_path/$host_artifact"; then
                        echo "warning: could not remove exact host artifact $host_artifact" >&2
                        cleanup_result=1
                    fi
                    ;;
            esac
        done
        IFS=$host_cleanup_old_ifs
        host_cleanup_path_identity=
        case "$host_root" in
            "$temporary_base"/lsw-seamless-e2e.??????)
                if [ -d "$host_root" ] && [ ! -L "$host_root" ]; then
                    host_cleanup_path_identity=$(stat -Lc '%d:%i:%u:%a' -- "$host_root" 2>/dev/null || :)
                fi
                ;;
            *) ;;
        esac
        if [ "$host_cleanup_path_identity" != "$host_root_identity" ]; then
            echo "warning: refusing rmdir because the seamless E2E host path identity changed: $host_root" >&2
            cleanup_result=1
        elif ! rmdir -- "$host_root" 2>/dev/null; then
            echo "warning: seamless E2E host directory contains an unexpected artifact: $host_root" >&2
            cleanup_result=1
        fi
    else
        echo "warning: refusing cleanup because the seamless E2E host directory identity changed: $host_root" >&2
        cleanup_result=1
    fi
    exec 9<&-
    if [ "$cleanup_result" -eq 0 ] && [ "$matrix_complete" -eq 1 ]; then
        printf 'RESULT=pass instance=%s cli_sha256=%s guest_agent_sha256=%s\n' \
            "$instance" "$cli_sha" "$guest_agent_sha"
    fi
    exit "$cleanup_result"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

prepare_script="\$ErrorActionPreference='Stop'; \$Path=[IO.Path]::GetFullPath('$guest_root'); \$Owner=[IO.Path]::GetFullPath('$guest_owner_token_path'); if (\$Owner -cne [IO.Path]::Combine(\$Path,'owner.token')) { throw 'guest owner-token path changed' }; if (Test-Path -LiteralPath \$Path) { throw 'guest seamless E2E path already exists' }; \$Directory=New-Item -Path \$Path -ItemType Directory -ErrorAction Stop; if (\$Directory.FullName -cne \$Path -or ((\$Directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'guest seamless E2E directory identity changed' }; [IO.File]::WriteAllText(\$Owner,'$guest_owner_token',[Text.UTF8Encoding]::new(\$false))"
guest_root_created=1
run_guest_powershell 10 "$prepare_script" >/dev/null \
    || fail "could not create the bounded guest fixture directory"
run_lsw 15 cp "$fixture_source" "$guest_fixture" >/dev/null \
    || fail "lsw cp could not stage the seamless fixture in the default instance"
pass fixture_staged

guest_marker_text() {
    marker_read_path=$1
    marker_read_attempt=1
    while [ "$marker_read_attempt" -le 3 ]; do
        if marker_read_raw=$(run_lsw 3 exec "$instance" -- \
            cmd.exe /d /c type "$marker_read_path" </dev/null 2>/dev/null)
        then
            printf '%s\n' "$marker_read_raw" | tr -d '\r'
            return 0
        fi
        marker_read_attempt=$((marker_read_attempt + 1))
        if [ "$marker_read_attempt" -le 3 ]; then
            sleep 0.2
        fi
    done
    return 1
}

marker_has_exact() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        '$2 == expected { found=1 } END { exit(found ? 0 : 1) }'
}

caption_control_field() {
    caption_marker_text=$1
    caption_field=$2
    printf '%s\n' "$caption_marker_text" | awk -F '\t' -v field="$caption_field" '
        $2 ~ /^caption-controls=/ {
            count = split(substr($2, length("caption-controls=") + 1), values, ",")
            for (entry = 1; entry <= count; entry++) {
                split(values[entry], pair, ":")
                if (pair[1] == field) {
                    observed = pair[2]
                }
            }
        }
        END {
            if (observed ~ /^[1-9][0-9]*$/) {
                print observed
                exit 0
            }
            exit 1
        }
    '
}

close_prompt_control_field() {
    close_prompt_marker_text=$1
    close_prompt_field=$2
    printf '%s\n' "$close_prompt_marker_text" | awk -F '\t' -v field="$close_prompt_field" '
        $2 ~ /^close-prompt-controls=/ {
            count = split(substr($2, length("close-prompt-controls=") + 1), values, ",")
            for (entry = 1; entry <= count; entry++) {
                split(values[entry], pair, ":")
                if (pair[1] == field) {
                    observed = pair[2]
                }
            }
        }
        END {
            if (observed ~ /^[0-9]+$/) {
                print observed
                exit 0
            }
            exit 1
        }
    '
}

pointer_probe_field() {
    probe_marker_text=$1
    probe_field=$2
    printf '%s\n' "$probe_marker_text" | awk -F '\t' -v field="$probe_field" '
        $2 ~ /^pointer-probe=/ {
            count = split(substr($2, length("pointer-probe=") + 1), values, ",")
            for (entry = 1; entry <= count; entry++) {
                split(values[entry], pair, ":")
                if (pair[1] == field) {
                    observed = pair[2]
                }
            }
        }
        END {
            if (observed ~ /^[0-9]+$/) {
                print observed
                exit 0
            }
            exit 1
        }
    '
}

marker_has_prefix() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        'index($2, expected) == 1 { found=1 } END { exit(found ? 0 : 1) }'
}

marker_exact_count() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        '$2 == expected { count++ } END { print count + 0 }'
}

marker_prefix_count() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        'index($2, expected) == 1 { count++ } END { print count + 0 }'
}

latest_guest_frame_size() {
    latest_frame_path=$1
    latest_frame_text=$(guest_marker_text "$latest_frame_path") || return 1
    printf '%s\n' "$latest_frame_text" | awk -F '[=x\t]' '
        $2 == "frame-size" && $3 ~ /^[1-9][0-9]*$/ && $4 ~ /^[1-9][0-9]*$/ {
            size=$3 " " $4; found=1
        }
        END { if (found) print size; else exit 1 }
    '
}

assert_marker_before() {
    order_path=$1
    order_first=$2
    order_second=$3
    order_text=$(guest_marker_text "$order_path") \
        || fail "could not read guest marker ordering from $order_path"
    printf '%s\n' "$order_text" | awk -F '\t' -v first="$order_first" -v second="$order_second" '
        $2 == first && first_line == 0 { first_line=NR }
        $2 == second && second_line == 0 { second_line=NR }
        END { exit(!(first_line > 0 && second_line > first_line)) }
    ' || fail "guest marker $order_first did not precede $order_second"
}

wait_marker_exact() {
    wait_marker_path=$1
    wait_marker_expected=$2
    wait_marker_deadline=$(( $(date +%s) + 15 ))
    wait_marker_latest=
    while [ "$(date +%s)" -lt "$wait_marker_deadline" ]; do
        wait_marker_latest=$(guest_marker_text "$wait_marker_path" || :)
        if marker_has_exact "$wait_marker_latest" "$wait_marker_expected"; then
            return 0
        fi
        sleep 0.2
    done
    printf '%s\n' "$wait_marker_latest" >&2
    fail "guest marker did not appear: $wait_marker_expected"
}

wait_marker_prefix() {
    wait_prefix_path=$1
    wait_prefix_expected=$2
    wait_prefix_deadline=$(( $(date +%s) + 15 ))
    wait_prefix_latest=
    while [ "$(date +%s)" -lt "$wait_prefix_deadline" ]; do
        wait_prefix_latest=$(guest_marker_text "$wait_prefix_path" || :)
        if marker_has_prefix "$wait_prefix_latest" "$wait_prefix_expected"; then
            return 0
        fi
        sleep 0.2
    done
    printf '%s\n' "$wait_prefix_latest" >&2
    fail "guest marker prefix did not appear: $wait_prefix_expected"
}

wait_marker_exact_count() {
    wait_count_path=$1
    wait_count_expected=$2
    wait_count_minimum=$3
    wait_count_deadline=$(( $(date +%s) + 15 ))
    wait_count_latest=
    wait_count_observed=0
    while [ "$(date +%s)" -lt "$wait_count_deadline" ]; do
        wait_count_latest=$(guest_marker_text "$wait_count_path" || :)
        wait_count_observed=$(marker_exact_count "$wait_count_latest" "$wait_count_expected")
        if [ "$wait_count_observed" -ge "$wait_count_minimum" ]; then
            return 0
        fi
        sleep 0.2
    done
    printf '%s\n' "$wait_count_latest" >&2
    fail "guest marker $wait_count_expected appeared $wait_count_observed times; expected at least $wait_count_minimum"
}

wait_marker_prefix_count() {
    wait_prefix_count_path=$1
    wait_prefix_count_expected=$2
    wait_prefix_count_minimum=$3
    wait_prefix_count_deadline=$(( $(date +%s) + 15 ))
    wait_prefix_count_latest=
    wait_prefix_count_observed=0
    while [ "$(date +%s)" -lt "$wait_prefix_count_deadline" ]; do
        wait_prefix_count_latest=$(guest_marker_text "$wait_prefix_count_path" || :)
        wait_prefix_count_observed=$(marker_prefix_count \
            "$wait_prefix_count_latest" "$wait_prefix_count_expected")
        if [ "$wait_prefix_count_observed" -ge "$wait_prefix_count_minimum" ]; then
            return 0
        fi
        sleep 0.2
    done
    printf '%s\n' "$wait_prefix_count_latest" >&2
    fail "guest marker prefix $wait_prefix_count_expected appeared $wait_prefix_count_observed times; expected at least $wait_prefix_count_minimum"
}

fixture_process_count() {
    process_marker=$1
    process_id=$(fixture_process_id "$process_marker") || return 1
    process_output_raw=$(run_lsw 3 exec "$instance" -- tasklist.exe \
        /FI "PID eq $process_id" /FO CSV /NH </dev/null 2>/dev/null) || return 1
    process_output=$(printf '%s\n' "$process_output_raw" | tr -d '\r')
    printf '%s\n' "$process_output" | awk -F, -v expected="\"$process_id\"" '
        $1 == "\"powershell.exe\"" && $2 == expected { count++ }
        END { print count + 0 }
    '
}

fixture_process_id() {
    identity_marker=$1
    identity_marker_text=$(guest_marker_text "$identity_marker" || :)
    printf '%s\n' "$identity_marker_text" | awk -F '[=\t]' \
        '$2 == "process-id" && $3 ~ /^[1-9][0-9]*$/ { print $3; found=1; exit } END { if (!found) exit 1 }'
}

fixture_window_handle() {
    identity_marker=$1
    identity_marker_text=$(guest_marker_text "$identity_marker" || :)
    printf '%s\n' "$identity_marker_text" | awk -F '[=\t]' \
        '$2 == "window-hwnd" && $3 ~ /^[1-9][0-9]*$/ { print $3; found=1; exit } END { if (!found) exit 1 }'
}

wait_fixture_count() {
    wait_count_marker=$1
    wait_count_expected=$2
    wait_count_deadline=$(( $(date +%s) + 15 ))
    wait_count_observed=unknown
    while [ "$(date +%s)" -lt "$wait_count_deadline" ]; do
        wait_count_observed=$(fixture_process_count "$wait_count_marker" || printf unknown)
        if [ "$wait_count_observed" = "$wait_count_expected" ]; then
            return 0
        fi
        sleep 0.25
    done
    fail "fixture process count was $wait_count_observed; expected $wait_count_expected"
}

find_fixture_window() {
    # WSLg can dynamically wrap the Wayland title with host-controlled text
    # such as a copy-mode warning and the distro name. The complete production
    # title remains an ordinal substring containing this run-unique, non-secret ID;
    # combine it with the exact msrdc process and exact HWND on every action.
    find_output=$(run_windows_host 8 Find -TitleNeedle "$window_title" \
        -ProcessName msrdc 2>/dev/null) || return 1
    find_count=$(printf '%s\n' "$find_output" | awk -F= \
        '$1 == "HWND" && $2 ~ /^[1-9][0-9]*$/ { count++ } END { print count + 0 }')
    if [ "$find_count" -gt 1 ]; then
        echo "error: more than one visible WSLg fixture HWND matched the exact test title" >&2
        return 1
    fi
    printf '%s\n' "$find_output" | awk -F= \
        '$1 == "HWND" && $2 ~ /^[1-9][0-9]*$/ { print $2; exit }'
}

dump_window_start_diagnostics() {
    echo "--- seamless presenter process ---" >&2
    ps -o pid=,ppid=,state=,etimes=,wchan=,args= -p "$presenter_pid" >&2 || :
    echo "--- seamless presenter log (tail) ---" >&2
    tail -n 100 "$presenter_log" >&2 || :

    diagnostic_marker=$(guest_marker_text "$start_marker" || :)
    echo "--- seamless guest marker ---" >&2
    if [ -n "$diagnostic_marker" ]; then
        printf '%s\n' "$diagnostic_marker" >&2
    else
        echo "unavailable" >&2
    fi

    diagnostic_fixture_pid=$(fixture_process_id "$start_marker" 2>/dev/null || :)
    case "$diagnostic_fixture_pid" in
        [1-9][0-9]*)
            diagnostic_guest_script="\$ErrorActionPreference='Stop'; \$Process=Get-Process -Id $diagnostic_fixture_pid -ErrorAction Stop; [Console]::Out.WriteLine('FIXTURE_PID='+\$Process.Id); [Console]::Out.WriteLine('FIXTURE_SESSION='+\$Process.SessionId); [Console]::Out.WriteLine('FIXTURE_MAIN_HWND='+\$Process.MainWindowHandle); [Console]::Out.WriteLine('FIXTURE_TITLE_BASE64='+[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(\$Process.MainWindowTitle)))"
            echo "--- seamless guest process identity ---" >&2
            run_guest_powershell 8 "$diagnostic_guest_script" >&2 || :
            ;;
        *) ;;
    esac

    echo "--- WSLg host title-prefix HWNDs ---" >&2
    run_windows_host 8 Find -TitleNeedle "$window_title_prefix" \
        -ProcessName msrdc >&2 || :
    echo "--- Windows msrdc process identities ---" >&2
    # PowerShell expands the process properties; POSIX sh must pass them literally.
    # shellcheck disable=SC2016
    run_host 8 powershell.exe -NoLogo -NoProfile -NonInteractive \
        -ExecutionPolicy Bypass -Command \
        '$ErrorActionPreference="Stop"; @(Get-Process -Name msrdc -ErrorAction SilentlyContinue | Sort-Object Id) | ForEach-Object { [Console]::Out.WriteLine("MSRDC_PID="+$_.Id+",SESSION="+$_.SessionId+",MAIN_HWND="+$_.MainWindowHandle+",TITLE_BASE64="+[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($_.MainWindowTitle))) }' \
        >&2 || :
}

wait_fixture_window() {
    # This outer harness bound must exceed the agent's own 15-second HWND
    # discovery deadline plus transport, WTS launch, and cold PowerShell startup.
    # It does not relax the production selector or its fail-closed deadline.
    wait_window_deadline=$(( $(date +%s) + 40 ))
    while [ "$(date +%s)" -lt "$wait_window_deadline" ]; do
        if ! kill -0 "$presenter_pid" 2>/dev/null; then
            dump_window_start_diagnostics
            fail "seamless presenter exited before creating its WSLg Wayland host window"
        fi
        window_id=$(find_fixture_window || :)
        if [ -n "$window_id" ]; then
            return 0
        fi
        sleep 0.2
    done
    dump_window_start_diagnostics
    fail "timed out waiting for the LSW Seamless Fixture WSLg Wayland host window"
}

wait_no_fixture_window() {
    wait_gone_deadline=$(( $(date +%s) + 10 ))
    wait_gone_observation=unavailable
    while [ "$(date +%s)" -lt "$wait_gone_deadline" ]; do
        if wait_gone_observation=$(find_fixture_window); then
            if [ -z "$wait_gone_observation" ]; then
                return 0
            fi
        fi
        sleep 0.2
    done
    fail "could not prove the LSW Seamless Fixture host window was gone (last observation: $wait_gone_observation)"
}

window_field() {
    field_window=$1
    field_name=$2
    field_output=$(run_fixture_host 8 Query -Hwnd "$field_window") || return 1
    printf '%s\n' "$field_output" | awk -F= -v name="$field_name" \
        '$1 == name { print substr($0, length($1) + 2); found=1; exit } END { if (!found) exit 1 }'
}

activate_window() {
    activate_target=$1
    if [ -n "$focus_sink_window" ] && [ "$activate_target" = "$focus_sink_window" ]; then
        run_windows_host 8 Activate -Hwnd "$activate_target" \
            -TitleNeedle "$focus_sink_title" -ProcessName powershell -ExactTitle \
            || fail "Windows did not focus host HWND $activate_target"
        activate_query=$(run_windows_host 8 Query -Hwnd "$activate_target" \
            -TitleNeedle "$focus_sink_title" -ProcessName powershell -ExactTitle || :)
        activate_observed=$(printf '%s\n' "$activate_query" | awk -F= \
            '$1 == "FOREGROUND" { print $2; exit }')
    else
        run_fixture_host 8 Activate -Hwnd "$activate_target" \
            || fail "Windows did not focus host HWND $activate_target"
        activate_observed=$(window_field "$activate_target" FOREGROUND || :)
    fi
    [ "$activate_observed" = 1 ] \
        || fail "Windows foreground focus did not settle on host HWND $activate_target"
}

move_at() {
    move_window=$1
    move_x=$2
    move_y=$3
    move_attempt=0
    while [ "$move_attempt" -lt 3 ]; do
        if run_fixture_host 8 Pointer -Hwnd "$move_window" \
            -X "$move_x" -Y "$move_y" 2>/dev/null
        then
            return 0
        fi
        move_attempt=$((move_attempt + 1))
        # Non-activating Explorer popup surfaces can temporarily cover an
        # otherwise foreground WSLg proxy. Never forward through them: move
        # away, let the popup dismiss, reactivate the exact HWND, and retry the
        # same WindowFromPoint identity proof.
        run_fixture_host 8 PointerAway -Hwnd "$move_window" >/dev/null 2>&1 \
            || break
        sleep 0.4
        activate_window "$move_window"
    done
    run_fixture_host 8 Pointer -Hwnd "$move_window" -X "$move_x" -Y "$move_y" \
        || fail "could not move the Windows pointer into the unobscured seamless host HWND"
}

button_action() {
    button_name=$1
    run_fixture_host 8 Button -Hwnd "$window_id" -Button "$button_name" \
        || fail "could not send Windows host pointer action $button_name"
}

click_at() {
    click_window=$1
    click_x=$2
    click_y=$3
    # Pointer motion and button input reach winit as distinct asynchronous
    # Wayland events. Prove the Windows cursor landed in the exact proxy first,
    # then leave a bounded settling interval before sending explicit edges.
    # The later Burst gate still exercises rapid pairs without this interval.
    click_attempt=0
    while [ "$click_attempt" -lt 3 ]; do
        if run_fixture_host 8 PointerButton -Hwnd "$click_window" \
            -X "$click_x" -Y "$click_y" -Button Left -DelayMilliseconds 50 \
            2>/dev/null
        then
            return 0
        fi
        click_attempt=$((click_attempt + 1))
        # A non-activating Explorer popup can cover an otherwise foreground
        # WSLg proxy after a taskbar restore. The failed PointerButton sends no
        # edge until both identity checks pass, so dismiss and retry safely.
        run_fixture_host 8 PointerAway -Hwnd "$click_window" >/dev/null 2>&1 \
            || break
        sleep 0.4
        activate_window "$click_window"
    done
    run_fixture_host 8 PointerButton -Hwnd "$click_window" \
        -X "$click_x" -Y "$click_y" -Button Left -DelayMilliseconds 50 \
        || fail "could not click the identity-checked seamless host HWND"
}

click_percent() {
    percent_window=$1
    percent_x=$2
    percent_y=$3
    percent_width=$(window_field "$percent_window" WIDTH) || fail "could not read host window width"
    percent_height=$(window_field "$percent_window" HEIGHT) || fail "could not read host window height"
    click_at "$percent_window" "$((percent_width * percent_x / 100))" \
        "$((percent_height * percent_y / 100))"
}

fit_viewport_values() {
    fit_host_width=$1
    fit_host_height=$2
    fit_guest_width=$3
    fit_guest_height=$4
    if [ "$((fit_host_width * fit_guest_height))" -le \
        "$((fit_host_height * fit_guest_width))" ]
    then
        fit_width=$fit_host_width
        fit_height=$((fit_guest_height * fit_host_width / fit_guest_width))
    else
        fit_width=$((fit_guest_width * fit_host_height / fit_guest_height))
        fit_height=$fit_host_height
    fi
    [ "$fit_width" -gt 0 ] && [ "$fit_height" -gt 0 ] \
        || fail "aspect-fit viewport collapsed to ${fit_width}x${fit_height}"
    fit_x=$(((fit_host_width - fit_width) / 2))
    fit_y=$(((fit_host_height - fit_height) / 2))
    printf '%s %s %s %s\n' "$fit_x" "$fit_y" "$fit_width" "$fit_height"
}

click_guest_point() {
    guest_point_window=$1
    guest_point_width=$2
    guest_point_height=$3
    guest_point_x=$4
    guest_point_y=$5
    guest_point_host_width=$(window_field "$guest_point_window" WIDTH) \
        || fail "could not read host width for a guest-space click"
    guest_point_host_height=$(window_field "$guest_point_window" HEIGHT) \
        || fail "could not read host height for a guest-space click"
    guest_point_fit=$(fit_viewport_values \
        "$guest_point_host_width" "$guest_point_host_height" \
        "$guest_point_width" "$guest_point_height")
    guest_point_view_x=${guest_point_fit%% *}
    guest_point_fit=${guest_point_fit#* }
    guest_point_view_y=${guest_point_fit%% *}
    guest_point_fit=${guest_point_fit#* }
    guest_point_view_width=${guest_point_fit%% *}
    guest_point_view_height=${guest_point_fit#* }
    guest_point_host_x=$((guest_point_view_x \
        + (2 * guest_point_x + 1) * guest_point_view_width / (2 * guest_point_width)))
    guest_point_host_y=$((guest_point_view_y \
        + (2 * guest_point_y + 1) * guest_point_view_height / (2 * guest_point_height)))
    click_at "$guest_point_window" "$guest_point_host_x" "$guest_point_host_y"
}

report_fitted_viewport() {
    fitted_label=$1
    fitted_host_width=$2
    fitted_host_height=$3
    fitted_guest_width=$4
    fitted_guest_height=$5
    fitted_values=$(fit_viewport_values \
        "$fitted_host_width" "$fitted_host_height" \
        "$fitted_guest_width" "$fitted_guest_height")
    fitted_x=${fitted_values%% *}
    fitted_values=${fitted_values#* }
    fitted_y=${fitted_values%% *}
    fitted_values=${fitted_values#* }
    fitted_width=${fitted_values%% *}
    fitted_height=${fitted_values#* }
    printf '%s HOST=%sx%s GUEST=%sx%s VIEWPORT=%s,%s,%sx%s\n' \
        "$fitted_label" "$fitted_host_width" "$fitted_host_height" \
        "$fitted_guest_width" "$fitted_guest_height" \
        "$fitted_x" "$fitted_y" "$fitted_width" "$fitted_height"
}

resize_via_guest_southeast() {
    resize_target_width=$1
    resize_target_height=$2
    resize_target_observed_width=unknown
    resize_target_observed_height=unknown
    for resize_edge_inset in 8 6; do
        resize_current_width=$(window_field "$window_id" WIDTH) \
            || fail "could not read the host width before guest resize"
        resize_current_height=$(window_field "$window_id" HEIGHT) \
            || fail "could not read the host height before guest resize"
        resize_target_delta_x=$((resize_target_width - resize_current_width))
        resize_target_delta_y=$((resize_target_height - resize_current_height))
        [ "$resize_target_delta_x" -ne 0 ] \
            || [ "$resize_target_delta_y" -ne 0 ] \
            || return 0
        if ! run_fixture_host 8 Drag -Hwnd "$window_id" \
            -X "$((resize_current_width - resize_edge_inset))" \
            -Y "$((resize_current_height - resize_edge_inset))" \
            -DeltaX "$resize_target_delta_x" -DeltaY "$resize_target_delta_y" \
            -DelayMilliseconds 300
        then
            continue
        fi
        resize_target_deadline=$(( $(date +%s) + 4 ))
        resize_target_observed_width=$resize_current_width
        resize_target_observed_height=$resize_current_height
        while [ "$(date +%s)" -lt "$resize_target_deadline" ]; do
            resize_target_observed_width=$(window_field "$window_id" WIDTH \
                || printf '%s' "$resize_target_observed_width")
            resize_target_observed_height=$(window_field "$window_id" HEIGHT \
                || printf '%s' "$resize_target_observed_height")
            if integer_near "$resize_target_observed_width" "$resize_target_width" 4 \
                && integer_near "$resize_target_observed_height" "$resize_target_height" 4
            then
                return 0
            fi
            sleep 0.2
        done
    done
    fail "guest southeast resize settled at ${resize_target_observed_width}x${resize_target_observed_height}; expected about ${resize_target_width}x${resize_target_height}"
}

move_percent() {
    move_window=$1
    move_x_percent=$2
    move_y_percent=$3
    move_width=$(window_field "$move_window" WIDTH) || fail "could not read host window width"
    move_height=$(window_field "$move_window" HEIGHT) || fail "could not read host window height"
    move_at "$move_window" "$((move_width * move_x_percent / 100))" \
        "$((move_height * move_y_percent / 100))"
}

capture_window() {
    capture_label=$1
    register_host_artifact "$capture_label.png"
    capture_path="$host_root/$capture_label.png"
    capture_windows=$(wslpath -w "$capture_path") \
        || fail "could not translate the $capture_label capture path"
    run_fixture_host 10 Screenshot -Hwnd "$window_id" -Output "$capture_windows" \
        || fail "the Windows host helper could not capture $capture_label"
    if [ ! -s "$capture_path" ]; then
        fail "ImageMagick produced an empty $capture_label capture"
    fi
}

image_has_color() {
    color_image=$1
    color_value=$2
    color_minimum=$3
    color_fraction=$(run_host 8 convert "$color_image" -alpha off -fuzz 6% \
        -fill black +opaque "$color_value" -fill white -opaque "$color_value" \
        -colorspace gray -format '%[fx:mean]' info: 2>/dev/null) || return 1
    awk -v observed="$color_fraction" -v minimum="$color_minimum" \
        'BEGIN { exit(!(observed + 0 >= minimum + 0)) }'
}

image_has_dimensions() {
    dimension_image=$1
    dimension_width=$2
    dimension_height=$3
    if [ "$dimension_width" -eq 0 ] && [ "$dimension_height" -eq 0 ]; then
        return 0
    fi
    dimension_observed=$(run_host 5 identify -format '%w %h' "$dimension_image" 2>/dev/null) \
        || return 1
    [ "$dimension_observed" = "$dimension_width $dimension_height" ]
}

wait_visual_sentinel() {
    visual_label=$1
    visual_color=$2
    visual_minimum=$3
    visual_width=$4
    visual_height=$5
    visual_deadline=$(( $(date +%s) + 12 ))
    visual_last_fraction=none
    visual_last_dimensions=none
    while [ "$(date +%s)" -lt "$visual_deadline" ]; do
        capture_window "$visual_label"
        visual_last_fraction=$(run_host 8 convert "$capture_path" -alpha off -fuzz 6% \
            -fill black +opaque "$visual_color" -fill white -opaque "$visual_color" \
            -colorspace gray -format '%[fx:mean]' info: 2>/dev/null || printf none)
        visual_last_dimensions=$(run_host 5 identify -format '%w %h' "$capture_path" \
            2>/dev/null || printf none)
        if image_has_color "$capture_path" "$visual_color" "$visual_minimum" \
            && image_has_dimensions "$capture_path" "$visual_width" "$visual_height"
        then
            return 0
        fi
        sleep 0.2
    done
    fail "$visual_label did not show sentinel $visual_color (fraction=$visual_last_fraction, dimensions=$visual_last_dimensions)"
}

wait_host_frame_change() {
    frame_label=$1
    capture_window "$frame_label-initial"
    frame_initial=$(run_host 5 identify -format '%#' "$capture_path" 2>/dev/null) \
        || fail "could not hash the initial $frame_label frame"
    frame_deadline=$(( $(date +%s) + 10 ))
    frame_previous=$frame_initial
    frame_transitions=0
    while [ "$(date +%s)" -lt "$frame_deadline" ]; do
        sleep 0.08
        capture_window "$frame_label-next"
        frame_observed=$(run_host 5 identify -format '%#' "$capture_path" 2>/dev/null) \
            || fail "could not hash a later $frame_label frame"
        if [ "$frame_observed" != "$frame_previous" ]; then
            frame_transitions=$((frame_transitions + 1))
            frame_previous=$frame_observed
            if [ "$frame_transitions" -ge 3 ]; then
                return 0
            fi
        fi
    done
    fail "$frame_label produced only $frame_transitions host-frame transitions"
}

wait_window_state() {
    state_window=$1
    state_name=$2
    state_expected=$3
    state_deadline=$(( $(date +%s) + 10 ))
    state_observed=unavailable
    while [ "$(date +%s)" -lt "$state_deadline" ]; do
        state_observed=$(window_field "$state_window" "$state_name" || printf unavailable)
        if [ "$state_observed" = "$state_expected" ]; then
            return 0
        fi
        sleep 0.2
    done
    fail "Windows host HWND state $state_name was $state_observed; expected $state_expected"
}

integer_near() {
    near_observed=$1
    near_expected=$2
    near_tolerance=$3
    [ "$near_observed" -ge "$((near_expected - near_tolerance))" ] \
        && [ "$near_observed" -le "$((near_expected + near_tolerance))" ]
}

assert_guest_only_chrome() {
    chrome_window=$1
    chrome_query=$(run_fixture_host 8 Query -Hwnd "$chrome_window") \
        || fail "could not inspect the WSLg host HWND"
    for chrome_required in \
        PROCESS=msrdc VISIBLE=1 CLOAKED=0 OWNER=0 HAS_CAPTION=0 \
        HAS_DLG_MODAL_FRAME=0 HAS_CLIENT_EDGE=0
    do
        if ! printf '%s\n' "$chrome_query" | grep -Fx "$chrome_required" >/dev/null; then
            printf '%s\n' "$chrome_query" >&2
            fail "WSLg host HWND violated the guest-only chrome invariant: $chrome_required"
        fi
    done

    chrome_marker=$(guest_marker_text "$marker_main") \
        || fail "could not read the guest frame-size marker"
    marker_has_exact "$chrome_marker" 'frame-size-source=dwm' \
        || fail "guest frame-size marker did not come from DWM visible bounds"
    chrome_size=$(printf '%s\n' "$chrome_marker" | awk -F '[=x\t]' \
        '$2 == "frame-size" && $3 ~ /^[1-9][0-9]*$/ && $4 ~ /^[1-9][0-9]*$/ { size=$3 " " $4 } END { print size }')
    [ -n "$chrome_size" ] || fail "guest fixture did not report its exact WGC frame size"
    chrome_expected_width=${chrome_size% *}
    chrome_expected_height=${chrome_size#* }
    chrome_width=$(window_field "$chrome_window" WIDTH) \
        || fail "could not read WSLg host client width"
    chrome_height=$(window_field "$chrome_window" HEIGHT) \
        || fail "could not read WSLg host client height"
    if [ "$chrome_width" -ne "$chrome_expected_width" ] \
        || [ "$chrome_height" -ne "$chrome_expected_height" ]
    then
        fail "WSLg host client ${chrome_width}x${chrome_height} added outer chrome around guest frame ${chrome_expected_width}x${chrome_expected_height}"
    fi

    chrome_window_left=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "WINDOW_LEFT" && $2 ~ /^-?[0-9]+$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg GetWindowRect left edge"
    chrome_window_top=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "WINDOW_TOP" && $2 ~ /^-?[0-9]+$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg GetWindowRect top edge"
    chrome_window_width=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "WINDOW_WIDTH" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg GetWindowRect width"
    chrome_window_height=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "WINDOW_HEIGHT" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg GetWindowRect height"
    chrome_dwm_left=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "DWM_LEFT" && $2 ~ /^-?[0-9]+$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg DWM left edge"
    chrome_dwm_top=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "DWM_TOP" && $2 ~ /^-?[0-9]+$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg DWM top edge"
    chrome_dwm_width=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "DWM_WIDTH" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg DWM width"
    chrome_dwm_height=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "DWM_HEIGHT" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg DWM height"
    chrome_style=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "STYLE" && $2 ~ /^0x[0-9A-F]{16}$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read the exact WSLg host style"
    chrome_exstyle=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "EXSTYLE" && $2 ~ /^0x[0-9A-F]{16}$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read the exact WSLg host extended style"
    chrome_window_edge=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "HAS_WINDOW_EDGE" && $2 ~ /^[01]$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read the WSLg WS_EX_WINDOWEDGE state"
    chrome_session=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "SESSION" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "WSLg proxy was not owned by an interactive Windows session"

    # DWM extended bounds describe visible pixels. Equality with GetClientRect,
    # plus absence of caption/edge styles above, proves the host did not add a
    # second visible title bar around the already captured guest DWM frame.
    if [ "$chrome_dwm_width" -ne "$chrome_width" ] \
        || [ "$chrome_dwm_height" -ne "$chrome_height" ]
    then
        printf '%s\n' "$chrome_query" >&2
        fail "WSLg DWM frame ${chrome_dwm_width}x${chrome_dwm_height} differs from client ${chrome_width}x${chrome_height}; a visible outer frame remains"
    fi
    chrome_inset_left=$((chrome_dwm_left - chrome_window_left))
    chrome_inset_top=$((chrome_dwm_top - chrome_window_top))
    chrome_inset_right=$((chrome_window_left + chrome_window_width - chrome_dwm_left - chrome_dwm_width))
    chrome_inset_bottom=$((chrome_window_top + chrome_window_height - chrome_dwm_top - chrome_dwm_height))
    for chrome_inset in \
        "$chrome_inset_left" "$chrome_inset_top" "$chrome_inset_right" "$chrome_inset_bottom"
    do
        if [ "$chrome_inset" -lt 0 ] || [ "$chrome_inset" -gt 64 ]; then
            printf '%s\n' "$chrome_query" >&2
            fail "WSLg outer/DWM inset $chrome_inset exceeded the bounded invisible resize border"
        fi
    done
    # WSLg RAIL currently sets WS_EX_WINDOWEDGE on its proxy even when it owns
    # zero DWM-visible pixels. Record that bit, but decide visual chrome from
    # the exact DWM/client geometry rather than the nominal style alone.
    printf 'CHROME SESSION=%s STYLE=%s EXSTYLE=%s WINDOW_EDGE=%s WINDOW=%sx%s DWM=%sx%s CLIENT=%sx%s INSETS=%s,%s,%s,%s\n' \
        "$chrome_session" "$chrome_style" "$chrome_exstyle" "$chrome_window_edge" \
        "$chrome_window_width" "$chrome_window_height" \
        "$chrome_dwm_width" "$chrome_dwm_height" "$chrome_width" "$chrome_height" \
        "$chrome_inset_left" "$chrome_inset_top" "$chrome_inset_right" "$chrome_inset_bottom"
    capture_window guest-only-chrome
    chrome_capture_dimensions=$(run_host 5 identify -format '%w %h' "$capture_path" 2>/dev/null) \
        || fail "could not inspect the guest-only chrome capture dimensions"
    [ "$chrome_capture_dimensions" = "$chrome_size" ] \
        || fail "host capture dimensions $chrome_capture_dimensions differ from guest frame $chrome_size"
}

start_focus_sink() {
    focus_sink_title="LSW Seamless Focus Sink $run_id"
    register_host_artifact focus-sink.log
    setsid powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass \
        -File "$host_helper_windows" -Action FocusSink -TitleNeedle "$focus_sink_title" \
        >"$host_root/focus-sink.log" 2>&1 &
    focus_sink_pid=$!
    focus_sink_deadline=$(( $(date +%s) + 8 ))
    while [ "$(date +%s)" -lt "$focus_sink_deadline" ]; do
        if ! kill -0 "$focus_sink_pid" 2>/dev/null; then
            tail -n 30 "$host_root/focus-sink.log" >&2 || :
            fail "Windows focus sink exited before its window appeared"
        fi
        focus_sink_find=$(run_windows_host 8 Find -TitleNeedle "$focus_sink_title" \
            -ProcessName powershell -ExactTitle 2>/dev/null || :)
        focus_sink_window=$(printf '%s\n' "$focus_sink_find" | awk -F= \
            '$1 == "HWND" && $2 ~ /^[1-9][0-9]*$/ { print $2; exit }')
        if [ -n "$focus_sink_window" ]; then
            activate_window "$focus_sink_window"
            return 0
        fi
        sleep 0.2
    done
    fail "timed out waiting for the native Windows focus sink"
}

start_presenter() {
    start_marker=$1
    start_label=$2
    start_mode=${3:-normal}
    start_status=$(run_lsw 6 status "$instance") \
        || fail "could not recheck instance $instance before $start_label"
    if ! printf '%s\n' "$start_status" | grep -Fx 'STATE=running' >/dev/null; then
        fail "instance $instance stopped before $start_label; this driver will not start it"
    fi
    if ! printf '%s\n' "$start_status" | grep -Fx 'AGENT=ready' >/dev/null; then
        fail "guest agent stopped being ready before $start_label"
    fi
    start_existing_window=$(find_fixture_window) \
        || fail "could not inspect Windows for an existing fixture before $start_label"
    if [ -n "$start_existing_window" ]; then
        fail "refusing to start $start_label while another fixture window is visible"
    fi
    case "$start_mode" in
        normal|animate|close-prompt|maximized) ;;
        *) fail "unknown seamless fixture mode $start_mode" ;;
    esac
    register_fixture_marker "$start_marker"
    register_host_artifact "presenter-$start_label.log"
    presenter_log="$host_root/presenter-$start_label.log"
    case "$start_mode" in
        normal)
            setsid "$lsw" run "$instance" --gui -- \
                powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
                -File "$guest_fixture" -MarkerPath "$start_marker" -RunId "$run_id" \
                -CleanupSignalPath "$guest_cleanup_signal" \
                >"$presenter_log" 2>&1 &
            ;;
        animate)
            setsid "$lsw" run "$instance" --gui -- \
                powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
                -File "$guest_fixture" -MarkerPath "$start_marker" -RunId "$run_id" \
                -CleanupSignalPath "$guest_cleanup_signal" -Animate \
                >"$presenter_log" 2>&1 &
            ;;
        close-prompt)
            setsid "$lsw" run "$instance" --gui -- \
                powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
                -File "$guest_fixture" -MarkerPath "$start_marker" -RunId "$run_id" \
                -CleanupSignalPath "$guest_cleanup_signal" -ClosePrompt \
                >"$presenter_log" 2>&1 &
            ;;
        maximized)
            setsid "$lsw" run "$instance" --gui -- \
                powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
                -File "$guest_fixture" -MarkerPath "$start_marker" -RunId "$run_id" \
                -CleanupSignalPath "$guest_cleanup_signal" -StartMaximized \
                >"$presenter_log" 2>&1 &
            ;;
        *) fail "unknown seamless fixture mode $start_mode" ;;
    esac
    presenter_pid=$!
    wait_fixture_window
    wait_marker_exact "$start_marker" "$released_input_state"
    wait_marker_exact "$start_marker" ready
    wait_marker_exact "$start_marker" 'frame-size-source=dwm'
    wait_marker_prefix "$start_marker" 'frame-size='
    wait_marker_prefix "$start_marker" 'window-hwnd='
    wait_marker_prefix "$start_marker" 'pointer-probe='
    assert_marker_before "$start_marker" "$released_input_state" ready
    if [ "$start_mode" = animate ]; then
        wait_marker_exact "$start_marker" animation-ready
    fi
    activate_window "$window_id"
    wait_fixture_count "$start_marker" 1
    refresh_pointer_epoch "after starting the $start_label presenter"
    ensure_pointer_ready "$start_marker" "$start_label"
}

refresh_pointer_epoch() {
    refresh_context=$1
    if [ "$wslg_transport_probe" = copy ]; then
        # Copy-mode WSLg can defer a RAIL proxy's CursorLeft until after the
        # next synthetic input edge. Hand both foreground focus and the pointer
        # to a distinct identity-checked native HWND, destroy that HWND, then
        # return focus to the exact WSLg HWND. Guest WinForms activation is not
        # the proof: host focus loss releases injected input without changing
        # the guest desktop's foreground window.
        start_focus_sink
        run_windows_host 8 Pointer -Hwnd "$focus_sink_window" \
            -TitleNeedle "$focus_sink_title" -ProcessName powershell -ExactTitle \
            -X 20 -Y 20 \
            || fail "could not move the pointer into the native focus sink $refresh_context"
        sleep 0.4
        terminate_focus_sink
        activate_window "$window_id"
        sleep 0.4
        # A corner point can belong to the Windows RAIL proxy's invisible
        # resize border without being accepted as the Wayland surface's first
        # enter coordinate. Enter through an unobscured interior point first.
        move_percent "$window_id" 80 75
        sleep 0.4
    else
        run_fixture_host 8 PointerAway -Hwnd "$window_id" \
            || fail "could not leave the host window $refresh_context"
        sleep 0.25
    fi
}

ensure_pointer_ready() {
    ready_marker=$1
    ready_label=$2
    ready_text=$(guest_marker_text "$ready_marker") \
        || fail "could not read the $ready_label pointer-probe marker"
    ready_x=$(pointer_probe_field "$ready_text" x) \
        || fail "$ready_label did not report a valid pointer-probe X coordinate"
    ready_y=$(pointer_probe_field "$ready_text" y) \
        || fail "$ready_label did not report a valid pointer-probe Y coordinate"
    ready_guest_frame=$(latest_guest_frame_size "$ready_marker") \
        || fail "could not read the $ready_label guest frame for pointer mapping"
    ready_guest_width=${ready_guest_frame%% *}
    ready_guest_height=${ready_guest_frame#* }
    ready_before=$(marker_exact_count "$ready_text" left-click)
    ready_attempt=1
    while [ "$ready_attempt" -le 4 ]; do
        click_guest_point "$window_id" "$ready_guest_width" "$ready_guest_height" \
            "$ready_x" "$ready_y"
        ready_deadline=$(( $(date +%s) + 3 ))
        while [ "$(date +%s)" -lt "$ready_deadline" ]; do
            ready_latest=$(guest_marker_text "$ready_marker" || :)
            ready_observed=$(marker_exact_count "$ready_latest" left-click)
            if [ "$ready_observed" -gt "$ready_before" ]; then
                printf 'POINTER_HANDSHAKE SESSION=%s ATTEMPTS=%s\n' \
                    "$ready_label" "$ready_attempt"
                return 0
            fi
            sleep 0.2
        done
        ready_attempt=$((ready_attempt + 1))
        if [ "$ready_attempt" -le 4 ]; then
            refresh_pointer_epoch "while establishing $ready_label pointer input"
        fi
    done
    fail "$ready_label did not receive the bounded pointer-input handshake"
}

place_fixture_via_guest_caption() {
    placement_label=$1
    placement_marker=${2:-$marker_main}
    placement_query=$(run_fixture_host 8 Query -Hwnd "$window_id") \
        || fail "could not inspect the fixture placement $placement_label"
    placement_x=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "X" { print $2; exit }')
    placement_y=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "Y" { print $2; exit }')
    placement_width=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WIDTH" { print $2; exit }')
    placement_height=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "HEIGHT" { print $2; exit }')
    placement_work_left=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WORK_LEFT" { print $2; exit }')
    placement_work_top=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WORK_TOP" { print $2; exit }')
    placement_work_width=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WORK_WIDTH" { print $2; exit }')
    placement_work_height=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WORK_HEIGHT" { print $2; exit }')
    for placement_value in \
        "$placement_x" "$placement_y" "$placement_width" "$placement_height" \
        "$placement_work_left" "$placement_work_top" \
        "$placement_work_width" "$placement_work_height"
    do
        printf '%s\n' "$placement_value" | grep -Eq '^-?[0-9]+$' \
            || fail "the Windows work-area query was invalid $placement_label"
    done
    [ "$placement_width" -le "$placement_work_width" ] \
        && [ "$placement_height" -le "$placement_work_height" ] \
        || fail "the seamless HWND cannot fit in its Windows monitor work area"
    placement_target_x=$((placement_work_left + (placement_work_width - placement_width) / 2))
    placement_target_y=$((placement_work_top + (placement_work_height - placement_height) / 2))
    placement_delta_x=$((placement_target_x - placement_x))
    placement_delta_y=$((placement_target_y - placement_y))
    if [ "$placement_delta_x" -ge -2 ] && [ "$placement_delta_x" -le 2 ] \
        && [ "$placement_delta_y" -ge -2 ] && [ "$placement_delta_y" -le 2 ]
    then
        return 0
    fi

    # Move through the guest's real HTCAPTION and the presenter's compositor
    # drag. Direct SetWindowPos on a WSLg RAIL HWND can move the Windows proxy
    # without moving the Wayland surface origin, invalidating pointer mapping.
    ensure_pointer_ready "$placement_marker" "pre-placement-$placement_label"
    run_fixture_host 10 Drag -Hwnd "$window_id" \
        -X "$((placement_width / 2))" -Y "$caption_control_y" \
        -DeltaX "$placement_delta_x" -DeltaY "$placement_delta_y" \
        -DelayMilliseconds 300 \
        || fail "could not move the fixture through its guest caption $placement_label"
    placement_deadline=$(( $(date +%s) + 8 ))
    placement_observed_x=$placement_x
    placement_observed_y=$placement_y
    while [ "$(date +%s)" -lt "$placement_deadline" ]; do
        placement_observed_x=$(window_field "$window_id" X || printf '%s' "$placement_observed_x")
        placement_observed_y=$(window_field "$window_id" Y || printf '%s' "$placement_observed_y")
        if integer_near "$placement_observed_x" "$placement_target_x" 24 \
            && integer_near "$placement_observed_y" "$placement_target_y" 24
        then
            activate_window "$window_id"
            refresh_pointer_epoch "after guest-caption placement $placement_label"
            return 0
        fi
        sleep 0.2
    done
    fail "guest-caption placement settled at ${placement_observed_x},${placement_observed_y}; expected about ${placement_target_x},${placement_target_y} $placement_label"
}

test_guest_resize_direction() {
    resize_direction=$1
    place_fixture_via_guest_caption "before-$resize_direction-resize"
    ensure_pointer_ready "$marker_main" "pre-$resize_direction-resize"
    resize_marker_text=$(guest_marker_text "$marker_main") \
        || fail "could not read pre-$resize_direction-resize markers"
    resize_marker_before=$(marker_prefix_count "$resize_marker_text" 'resize=')
    resize_x_before=$(window_field "$window_id" X) \
        || fail "could not read pre-$resize_direction-resize X"
    resize_y_before=$(window_field "$window_id" Y) \
        || fail "could not read pre-$resize_direction-resize Y"
    resize_width_before=$(window_field "$window_id" WIDTH) \
        || fail "could not read pre-$resize_direction-resize width"
    resize_height_before=$(window_field "$window_id" HEIGHT) \
        || fail "could not read pre-$resize_direction-resize height"
    resize_right_before=$((resize_x_before + resize_width_before))
    resize_bottom_before=$((resize_y_before + resize_height_before))
    # Exercise the companion's guest border instead of its caption or client
    # area. The fixture proves the top corners at two pixels are native
    # HTTOPLEFT/HTTOPRIGHT. Eight pixels is used on the remaining edges, where
    # the companion's DPI-aware inner affordance avoids stealing caption
    # controls. refresh_pointer_epoch provides the separate outside-to-inside
    # transition required by copy-mode WSLg.
    resize_pointer_inset=8
    resize_left_inset=6
    resize_top_corner_inset=2

    case "$resize_direction" in
        north)
            resize_pointer_x=$((resize_width_before / 2)); resize_pointer_y=$resize_pointer_inset
            resize_delta_x=0; resize_delta_y=-35
            ;;
        northeast)
            resize_pointer_x=$((resize_width_before - resize_top_corner_inset)); resize_pointer_y=$resize_top_corner_inset
            resize_delta_x=45; resize_delta_y=-35
            ;;
        east)
            resize_pointer_x=$((resize_width_before - resize_pointer_inset)); resize_pointer_y=$((resize_height_before / 2))
            resize_delta_x=45; resize_delta_y=0
            ;;
        southeast)
            resize_pointer_x=$((resize_width_before - resize_pointer_inset)); resize_pointer_y=$((resize_height_before - resize_pointer_inset))
            resize_delta_x=45; resize_delta_y=35
            ;;
        south)
            resize_pointer_x=$((resize_width_before / 2)); resize_pointer_y=$((resize_height_before - resize_pointer_inset))
            resize_delta_x=0; resize_delta_y=35
            ;;
        southwest)
            resize_pointer_x=$resize_left_inset; resize_pointer_y=$((resize_height_before - resize_pointer_inset))
            resize_delta_x=-45; resize_delta_y=35
            ;;
        west)
            resize_pointer_x=$resize_left_inset; resize_pointer_y=$((resize_height_before / 2))
            resize_delta_x=-45; resize_delta_y=0
            ;;
        northwest)
            resize_pointer_x=$resize_top_corner_inset; resize_pointer_y=$resize_top_corner_inset
            resize_delta_x=-45; resize_delta_y=-35
            ;;
        *) fail "unknown guest resize direction $resize_direction" ;;
    esac

    run_fixture_host 8 Drag -Hwnd "$window_id" \
        -X "$resize_pointer_x" -Y "$resize_pointer_y" \
        -DeltaX "$resize_delta_x" -DeltaY "$resize_delta_y" \
        -DelayMilliseconds 300 \
        || fail "could not drag the guest $resize_direction resize border"

    resize_deadline=$(( $(date +%s) + 8 ))
    resize_changed=0
    while [ "$(date +%s)" -lt "$resize_deadline" ]; do
        resize_x_after=$(window_field "$window_id" X || printf '%s' "$resize_x_before")
        resize_y_after=$(window_field "$window_id" Y || printf '%s' "$resize_y_before")
        resize_width_after=$(window_field "$window_id" WIDTH || printf '%s' "$resize_width_before")
        resize_height_after=$(window_field "$window_id" HEIGHT || printf '%s' "$resize_height_before")
        resize_right_after=$((resize_x_after + resize_width_after))
        resize_bottom_after=$((resize_y_after + resize_height_after))
        resize_x_delta=$((resize_x_after - resize_x_before))
        resize_y_delta=$((resize_y_after - resize_y_before))
        resize_right_delta=$((resize_right_after - resize_right_before))
        resize_bottom_delta=$((resize_bottom_after - resize_bottom_before))
        if case "$resize_direction" in
            north)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" -35 18 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
            northeast)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 45 18 \
                    && integer_near "$resize_y_delta" -35 18 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
            east)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 45 18 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
            southeast)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 45 18 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 35 18 ;;
            south)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 35 18 ;;
            southwest)
                integer_near "$resize_x_delta" -45 18 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 35 18 ;;
            west)
                integer_near "$resize_x_delta" -45 18 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
            northwest)
                integer_near "$resize_x_delta" -45 18 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" -35 18 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
        esac
        then
            resize_changed=1
            break
        fi
        sleep 0.2
    done
    [ "$resize_changed" -eq 1 ] \
        || fail "dragging the guest $resize_direction border changed host ${resize_x_before},${resize_y_before},${resize_width_before}x${resize_height_before} to ${resize_x_after},${resize_y_after},${resize_width_after}x${resize_height_after} (edge deltas ${resize_x_delta},${resize_y_delta},${resize_right_delta},${resize_bottom_delta})"
    wait_marker_prefix_count "$marker_main" 'resize=' "$((resize_marker_before + 1))"
    wait_visual_sentinel "edge-resize-$resize_direction" "$maximize_color" 0.003 \
        "$resize_width_after" "$resize_height_after"
    pass "guest_border_${resize_direction}_resize"
    refresh_pointer_epoch "after the guest $resize_direction resize"
    ensure_pointer_ready "$marker_main" "pre-$resize_direction-reset"

    # Each direction starts from the same guest-visible geometry. Cumulatively
    # growing all eight edges can place a normal Windows window beyond the
    # 1280x800 guest desktop, which tests an unreachable edge rather than the
    # direction mapping. Reverse the same proven guest edge to restore the
    # exact baseline; direct SetWindowPos on the WSLg proxy is not trusted.
    resize_reset_text=$(guest_marker_text "$marker_main") \
        || fail "could not read pre-$resize_direction-reset markers"
    resize_reset_before=$(marker_exact_count "$resize_reset_text" 'frame-size=900x650')
    case "$resize_direction" in
        north)
            resize_reset_x=$((resize_width_after / 2)); resize_reset_y=$resize_pointer_inset ;;
        northeast)
            resize_reset_x=$((resize_width_after - resize_top_corner_inset)); resize_reset_y=$resize_top_corner_inset ;;
        east)
            resize_reset_x=$((resize_width_after - resize_pointer_inset)); resize_reset_y=$((resize_height_after / 2)) ;;
        southeast)
            resize_reset_x=$((resize_width_after - resize_pointer_inset)); resize_reset_y=$((resize_height_after - resize_pointer_inset)) ;;
        south)
            resize_reset_x=$((resize_width_after / 2)); resize_reset_y=$((resize_height_after - resize_pointer_inset)) ;;
        southwest)
            resize_reset_x=$resize_left_inset; resize_reset_y=$((resize_height_after - resize_pointer_inset)) ;;
        west)
            resize_reset_x=$resize_left_inset; resize_reset_y=$((resize_height_after / 2)) ;;
        northwest)
            resize_reset_x=$resize_top_corner_inset; resize_reset_y=$resize_top_corner_inset ;;
        *) fail "unknown guest reset direction $resize_direction" ;;
    esac
    run_fixture_host 8 Drag -Hwnd "$window_id" \
        -X "$resize_reset_x" -Y "$resize_reset_y" \
        -DeltaX "$((-resize_delta_x))" -DeltaY "$((-resize_delta_y))" \
        -DelayMilliseconds 300 \
        || fail "could not reverse the guest $resize_direction resize border"
    resize_reset_deadline=$(( $(date +%s) + 8 ))
    resize_reset_complete=0
    while [ "$(date +%s)" -lt "$resize_reset_deadline" ]; do
        resize_reset_x_after=$(window_field "$window_id" X || printf '%s' "$resize_x_after")
        resize_reset_y_after=$(window_field "$window_id" Y || printf '%s' "$resize_y_after")
        resize_reset_width_after=$(window_field "$window_id" WIDTH || printf '%s' "$resize_width_after")
        resize_reset_height_after=$(window_field "$window_id" HEIGHT || printf '%s' "$resize_height_after")
        if integer_near "$resize_reset_x_after" "$resize_x_before" 4 \
            && integer_near "$resize_reset_y_after" "$resize_y_before" 4 \
            && integer_near "$resize_reset_width_after" "$resize_width_before" 4 \
            && integer_near "$resize_reset_height_after" "$resize_height_before" 4
        then
            resize_reset_complete=1
            break
        fi
        sleep 0.2
    done
    [ "$resize_reset_complete" -eq 1 ] \
        || fail "reversing the guest $resize_direction border restored ${resize_reset_x_after},${resize_reset_y_after},${resize_reset_width_after}x${resize_reset_height_after}; expected about ${resize_x_before},${resize_y_before},${resize_width_before}x${resize_height_before}"
    wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((resize_reset_before + 1))"
    # WinForms can publish the HWND size before WGC emits its matching Ready
    # frame. Prove that the reset visual crossed the complete guest-to-host
    # pipeline before using the next edge coordinate; the agent deliberately
    # rejects stale capture geometry instead of guessing a screen position.
    wait_visual_sentinel "edge-resize-$resize_direction-reset" "$resize_color" 0.005 \
        "$resize_reset_width_after" "$resize_reset_height_after"
    activate_window "$window_id"
    refresh_pointer_epoch "after resetting the guest $resize_direction resize"
}

wait_presenter_exit() {
    exit_deadline=$(( $(date +%s) + 12 ))
    while kill -0 "$presenter_pid" 2>/dev/null \
        && [ "$(date +%s)" -lt "$exit_deadline" ]
    do
        sleep 0.2
    done
    if kill -0 "$presenter_pid" 2>/dev/null; then
        fail "seamless presenter did not exit within twelve seconds"
    fi
    if wait "$presenter_pid"; then
        presenter_status=0
    else
        presenter_status=$?
    fi
    presenter_pid=
}

# Session 1: complete interactive behavior and graceful guest-caption close.
start_presenter "$marker_main" main
pass pointer_input_handshake
assert_guest_only_chrome "$window_id"
pass guest_only_windows_chrome
wait_marker_exact "$marker_main" 'dpi=96x96'
wait_marker_prefix "$marker_main" 'hit-test='
wait_marker_prefix "$marker_main" 'caption-controls='
hit_test_marker=$(guest_marker_text "$marker_main") \
    || fail "could not read guest hit-test diagnostics"
printf '%s\n' "$hit_test_marker" | awk -F '\t' \
    '$2 ~ /^hit-test=/ { print "HIT_TEST " $2; exit }'
caption_control_y=$(caption_control_field "$hit_test_marker" y) \
    || fail "guest fixture did not report a native caption-control Y coordinate"
caption_minimize_offset=$(caption_control_field "$hit_test_marker" minimize) \
    || fail "guest fixture did not report a native minimize-button coordinate"
caption_maximize_offset=$(caption_control_field "$hit_test_marker" maximize) \
    || fail "guest fixture did not report a native maximize-button coordinate"
caption_close_offset=$(caption_control_field "$hit_test_marker" close) \
    || fail "guest fixture did not report a native close-button coordinate"
printf 'CAPTION_CONTROLS y=%s minimize=%s maximize=%s close=%s\n' \
    "$caption_control_y" "$caption_minimize_offset" \
    "$caption_maximize_offset" "$caption_close_offset"
pass slice4_96_dpi_gate
initial_width=$(window_field "$window_id" WIDTH) || fail "could not read initial window width"
initial_height=$(window_field "$window_id" HEIGHT) || fail "could not read initial window height"
wait_marker_exact "$marker_main" 'initial-frame-ready'
wait_visual_sentinel initial "$initial_sentinel_color" 0.01 "$initial_width" "$initial_height"
pass first_window_frame
pass initial_visual_sentinel

click_percent "$window_id" 56 42
wait_marker_exact "$marker_main" modal-open
wait_marker_prefix "$marker_main" 'modal-window='
wait_marker_exact "$marker_main" 'modal-kind=ordinary-owned'
modal_marker_text=$(guest_marker_text "$marker_main") \
    || fail "could not read the modal HWND diagnostics"
printf '%s\n' "$modal_marker_text" | awk -F '\t' \
    '$2 ~ /^modal-window=/ { print "MODAL " $2; exit }'
wait_visual_sentinel modal "$modal_color" 0.01 "$initial_width" "$initial_height"
run_fixture_host 8 Chord -Hwnd "$window_id" -Key ENTER \
    || fail "could not close the guest modal with Enter"
wait_marker_exact "$marker_main" modal-close
pass modal_visible
pass modal_open_close

resize_marker_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-resize markers"
resize_marker_before=$(marker_exact_count "$resize_marker_text" 'frame-size=900x650')
resize_sentinel_before=$(marker_exact_count "$resize_marker_text" 'resize-sentinel=visible')
# Window placement is shell state and can persist near any monitor edge between
# local runs. Move through the guest caption so both the RAIL HWND and Wayland
# surface share a bounded work-area position before growing the guest border.
place_fixture_via_guest_caption "before-initial-resize"
ensure_pointer_ready "$marker_main" "pre-initial-resize"
resize_via_guest_southeast 900 650
wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((resize_marker_before + 1))"
wait_marker_exact_count "$marker_main" 'resize-sentinel=visible' "$((resize_sentinel_before + 1))"
wait_marker_exact "$marker_main" 'frame-size=900x650'
wait_visual_sentinel resized "$resize_color" 0.005 900 650
pass resize
pass resize_frame_ready
resize_reset_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-resize-reset markers"
resize_reset_before=$(marker_exact_count \
    "$resize_reset_text" "frame-size=${initial_width}x${initial_height}")
resize_via_guest_southeast "$initial_width" "$initial_height"
wait_marker_exact_count "$marker_main" \
    "frame-size=${initial_width}x${initial_height}" "$((resize_reset_before + 1))"
# The guest size marker can arrive before the corresponding frame crosses the
# agent/presenter pipeline. Prove the host is showing the reset-sized visual
# before the next pointer event so a stale aspect-fit transform cannot redirect
# the first post-resize click.
wait_visual_sentinel resize-reset "$initial_sentinel_color" 0.01 \
    "$initial_width" "$initial_height"
pass resize_reset_frame_ready

left_click_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-click markers"
left_click_before=$(marker_exact_count "$left_click_text" left-click)
click_percent "$window_id" 12 42
wait_marker_exact_count "$marker_main" left-click "$((left_click_before + 1))"
pass left_click
pass first_pointer_before_fixture_keyboard_input

click_percent "$window_id" 4 8
wait_visual_sentinel file-menu-open "$file_menu_color" 0.0005 \
    "$initial_width" "$initial_height"
click_percent "$window_id" 10 14
wait_marker_exact "$marker_main" file-menu
pass file_menu_visible
pass file_menu_item

move_percent "$window_id" 50 75
button_action Right
wait_marker_exact "$marker_main" right-button
wait_visual_sentinel context-menu-open "$context_menu_color" 0.0005 \
    "$initial_width" "$initial_height"
run_fixture_host 8 MoveRelative -Hwnd "$window_id" -DeltaX 85 -DeltaY 15 \
    || fail "could not move into the guest context menu"
button_action Left
wait_marker_exact "$marker_main" context-menu-item
pass right_click
pass context_menu_visible
pass context_menu_item
pass right_pointer_before_fixture_keyboard_input

# Rapid down/up pairs are a release gate, not a throughput benchmark. The old
# state-polling presenter could miss both edges when they arrived between polls,
# which made File, right-click, and typing appear randomly dead.
rapid_left_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-burst left-click markers"
rapid_left_before=$(marker_exact_count "$rapid_left_text" left-click)
move_percent "$window_id" 12 42
run_fixture_host 15 Burst -Hwnd "$window_id" -Button Left \
    -Repeat 100 -DelayMilliseconds 8 \
    || fail "could not send the rapid left-button edge burst"
wait_marker_exact_count "$marker_main" left-click "$((rapid_left_before + 100))"
sleep 0.5
rapid_left_final_text=$(guest_marker_text "$marker_main") \
    || fail "could not read settled rapid left-button markers"
rapid_left_final=$(marker_exact_count "$rapid_left_final_text" left-click)
[ "$rapid_left_final" -eq "$((rapid_left_before + 100))" ] \
    || fail "rapid left-button burst delivered $((rapid_left_final - rapid_left_before)) edges; expected exactly 100"
pass rapid_left_button_edges

rapid_right_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-burst right-button markers"
rapid_right_before=$(marker_exact_count "$rapid_right_text" right-button)
rapid_right_up_before=$(marker_exact_count "$rapid_right_text" right-button-up)
move_percent "$window_id" 50 75
run_fixture_host 20 Burst -Hwnd "$window_id" -Button Right \
    -Repeat 100 -DelayMilliseconds 25 \
    || fail "could not send the rapid right-button edge burst"
wait_marker_exact_count "$marker_main" right-button "$((rapid_right_before + 100))"
wait_marker_exact_count "$marker_main" right-button-up "$((rapid_right_up_before + 100))"
sleep 0.5
rapid_right_final_text=$(guest_marker_text "$marker_main") \
    || fail "could not read settled rapid right-button markers"
rapid_right_final=$(marker_exact_count "$rapid_right_final_text" right-button)
rapid_right_up_final=$(marker_exact_count "$rapid_right_final_text" right-button-up)
[ "$rapid_right_final" -eq "$((rapid_right_before + 100))" ] \
    || fail "rapid right-button burst delivered $((rapid_right_final - rapid_right_before)) down edges; expected exactly 100"
[ "$rapid_right_up_final" -eq "$((rapid_right_up_before + 100))" ] \
    || fail "rapid right-button burst delivered $((rapid_right_up_final - rapid_right_up_before)) up edges; expected exactly 100"
pass rapid_right_button_edges

move_percent "$window_id" 50 75
button_action Middle
wait_marker_exact "$marker_main" middle-button
pass middle_click

move_percent "$window_id" 50 75
button_action WheelUp
wait_marker_prefix "$marker_main" 'wheel='
pass wheel

move_percent "$window_id" 50 75
run_fixture_host 8 KeyDown -Hwnd "$window_id" -Key CTRL \
    || fail "could not hold Ctrl for the focus-release test"
button_action LeftDown
wait_marker_exact "$marker_main" 'input-held=ctrl,left'
start_focus_sink
wait_marker_exact "$marker_main" 'input-released=all'
run_windows_host 5 ReleaseAll 2>/dev/null || :
terminate_focus_sink
activate_window "$window_id"
click_percent "$window_id" 80 42
wait_marker_exact "$marker_main" focus-recovered
pass focus_loss_releases_input
pass focus_reactivation

rapid_key_text=$(awk 'BEGIN { for (counter = 0; counter < 100; counter++) printf "z" }')
click_percent "$window_id" 72 24
run_fixture_host 8 Type -Hwnd "$window_id" -Text "$rapid_key_text" \
    || fail "could not send the rapid Unicode key edge burst"
click_percent "$window_id" 34 42
rapid_key_base64=$(printf '%s' "$rapid_key_text" | base64 | tr -d '\n')
wait_marker_exact "$marker_main" "text-base64=$rapid_key_base64"
click_percent "$window_id" 34 42
wait_marker_exact "$marker_main" 'text-base64='
pass rapid_keyboard_edges
pass rapid_keyboard_text_cleared

click_percent "$window_id" 23 24
wait_marker_exact "$marker_main" source-selected
run_fixture_host 8 Chord -Hwnd "$window_id" -Key CTRL+C \
    || fail "could not send Ctrl+C to the seamless window"
clipboard_text=$(printf '%s' 'alpha beta' | base64 | tr -d '\n')
wait_marker_exact "$marker_main" ctrl-c-key-down
wait_marker_exact "$marker_main" "clipboard-copy-base64=$clipboard_text"
click_percent "$window_id" 72 24
run_fixture_host 8 Chord -Hwnd "$window_id" -Key CTRL+V \
    || fail "could not send Ctrl+V to the seamless window"
wait_marker_exact "$marker_main" ctrl-v-key-down
wait_marker_exact "$marker_main" "clipboard-paste-base64=$clipboard_text"
run_fixture_host 8 Type -Hwnd "$window_id" -Text ' typed' \
    || fail "could not type into the seamless window"
click_percent "$window_id" 34 42
expected_text=$(printf '%s' 'alpha beta typed' | base64 | tr -d '\n')
wait_marker_exact "$marker_main" "text-base64=$expected_text"
pass typing
pass guest_clipboard_ctrl_cv

resize_matrix_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-matrix-resize markers"
resize_matrix_before=$(marker_exact_count "$resize_matrix_text" 'frame-size=900x650')
resize_via_guest_southeast 900 650
wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((resize_matrix_before + 1))"

move_before_x=$(window_field "$window_id" X) || fail "could not read pre-drag X position"
move_before_y=$(window_field "$window_id" Y) || fail "could not read pre-drag Y position"
move_width=$(window_field "$window_id" WIDTH) || fail "could not read pre-drag width"
move_height=$(window_field "$window_id" HEIGHT) || fail "could not read pre-drag height"
run_fixture_host 8 Drag -Hwnd "$window_id" \
    -X "$((move_width * 55 / 100))" -Y "$((move_height / 40))" \
    -DeltaX 90 -DeltaY 70 -DelayMilliseconds 300 \
    || fail "could not drag the guest caption"
move_deadline=$(( $(date +%s) + 8 ))
move_changed=0
while [ "$(date +%s)" -lt "$move_deadline" ]; do
    move_after_x=$(window_field "$window_id" X || printf '%s' "$move_before_x")
    move_after_y=$(window_field "$window_id" Y || printf '%s' "$move_before_y")
    move_after_width=$(window_field "$window_id" WIDTH || printf '%s' "$move_width")
    move_after_height=$(window_field "$window_id" HEIGHT || printf '%s' "$move_height")
    move_delta_x=$((move_after_x - move_before_x))
    move_delta_y=$((move_after_y - move_before_y))
    if integer_near "$move_delta_x" 90 20 \
        && integer_near "$move_delta_y" 70 20 \
        && integer_near "$move_after_width" "$move_width" 4 \
        && integer_near "$move_after_height" "$move_height" 4
    then
        move_changed=1
        break
    fi
    sleep 0.2
done
if [ "$move_changed" -ne 1 ]; then
    fail "guest caption drag produced delta ${move_delta_x},${move_delta_y} and size ${move_after_width}x${move_after_height}; expected about +90,+70 with size ${move_width}x${move_height}"
fi
pass guest_caption_move

control_width=$(window_field "$window_id" WIDTH) || fail "could not read caption-control width"
click_at "$window_id" "$((control_width - caption_minimize_offset))" "$caption_control_y"
wait_window_state "$window_id" ICONIC 1
pass guest_caption_minimize
activate_window "$window_id"
wait_window_state "$window_id" ICONIC 0
# A synthetic SC_RESTORE can leave WSLg's new RAIL input mapping stale even
# after the HWND is visible. Reproduce the physical taskbar pointer leave on
# the restored proxy before entering it for the next guest-caption action.
run_fixture_host 8 PointerAway -Hwnd "$window_id" \
    || fail "could not move the Windows pointer outside the restored proxy"
sleep 0.7
# Current WSLg rebuilds the outer RAIL proxy on the first synthetic re-entry
# and its Weston pointer seat on the next leave/enter. Use an inert fixture
# background click for the first epoch; keep it in the center of the client so
# a guest-caption move cannot place the probe behind an always-on-top taskbar.
# No product behavior is asserted from that click, and the following caption
# action remains the input proof.
click_percent "$window_id" 50 50
run_fixture_host 8 PointerAway -Hwnd "$window_id" \
    || fail "could not complete the restored-proxy pointer re-entry"
sleep 0.7
pass guest_caption_unminimize

control_width=$(window_field "$window_id" WIDTH) || fail "could not read maximize-control width"
control_guest_frame=$(latest_guest_frame_size "$marker_main") \
    || fail "could not read the pre-maximize guest DWM frame"
control_guest_width=${control_guest_frame%% *}
control_guest_height=${control_guest_frame#* }
max_marker_text=$(guest_marker_text "$marker_main") || fail "could not read pre-maximize markers"
max_marker_before=$(marker_prefix_count "$max_marker_text" 'max-ready=')
max_state_before=$(marker_exact_count "$max_marker_text" 'window-state=Maximized')
click_guest_point "$window_id" "$control_guest_width" "$control_guest_height" \
    "$((control_guest_width - caption_maximize_offset))" "$caption_control_y"
wait_window_state "$window_id" ZOOMED 1
wait_marker_prefix_count "$marker_main" 'max-ready=' "$((max_marker_before + 1))"
wait_marker_exact_count "$marker_main" 'window-state=Maximized' "$((max_state_before + 1))"
maximized_width=$(window_field "$window_id" WIDTH) || fail "could not read maximized width"
maximized_height=$(window_field "$window_id" HEIGHT) || fail "could not read maximized height"
maximized_guest_frame=$(latest_guest_frame_size "$marker_main") \
    || fail "could not read the guest-caption maximized DWM frame"
maximized_guest_width=${maximized_guest_frame%% *}
maximized_guest_height=${maximized_guest_frame#* }
report_fitted_viewport MAXIMIZED_VIEWPORT \
    "$maximized_width" "$maximized_height" \
    "$maximized_guest_width" "$maximized_guest_height"
wait_visual_sentinel maximized "$maximize_color" 0.003 \
    "$maximized_width" "$maximized_height"
pass guest_caption_maximize
pass guest_window_state_maximized
pass maximize_frame_ready

restore_marker_text=$(guest_marker_text "$marker_main") || fail "could not read pre-restore markers"
restore_marker_before=$(marker_exact_count "$restore_marker_text" 'frame-size=900x650')
restore_state_before=$(marker_exact_count "$restore_marker_text" 'window-state=Normal')
control_width=$(window_field "$window_id" WIDTH) || fail "could not read restore-control width"
click_guest_point "$window_id" "$maximized_guest_width" "$maximized_guest_height" \
    "$((maximized_guest_width - caption_maximize_offset))" "$caption_control_y"
wait_window_state "$window_id" ZOOMED 0
wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((restore_marker_before + 1))"
wait_marker_exact_count "$marker_main" 'window-state=Normal' "$((restore_state_before + 1))"
wait_visual_sentinel restored "$resize_color" 0.005 900 650
pass guest_caption_restore
pass guest_window_state_restored
pass restore_frame_ready

# A native Windows shell/taskbar maximize is an outer-host state transition.
# It must travel back through winit and the protocol to the guest HWND, then a
# native restore must recover the exact pre-maximize visible frame.
native_max_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-native-maximize markers"
native_max_state_before=$(marker_exact_count "$native_max_text" 'window-state=Maximized')
native_max_ready_before=$(marker_prefix_count "$native_max_text" 'max-ready=')
native_max_frame_before=$(marker_prefix_count "$native_max_text" 'frame-size=')
run_fixture_host 8 Maximize -Hwnd "$window_id" \
    || fail "could not maximize the seamless HWND through the Windows host"
wait_window_state "$window_id" ZOOMED 1
wait_marker_exact_count "$marker_main" 'window-state=Maximized' "$((native_max_state_before + 1))"
wait_marker_prefix_count "$marker_main" 'max-ready=' "$((native_max_ready_before + 1))"
wait_marker_prefix_count "$marker_main" 'frame-size=' "$((native_max_frame_before + 1))"
native_max_width=$(window_field "$window_id" WIDTH) \
    || fail "could not read native-maximized host width"
native_max_height=$(window_field "$window_id" HEIGHT) \
    || fail "could not read native-maximized host height"
native_max_guest_frame=$(latest_guest_frame_size "$marker_main") \
    || fail "could not read the native-maximized guest DWM frame"
native_max_guest_width=${native_max_guest_frame%% *}
native_max_guest_height=${native_max_guest_frame#* }
report_fitted_viewport NATIVE_MAXIMIZE_VIEWPORT \
    "$native_max_width" "$native_max_height" \
    "$native_max_guest_width" "$native_max_guest_height"
wait_visual_sentinel native-maximized "$maximize_color" 0.003 \
    "$native_max_width" "$native_max_height"
pass native_host_maximize_to_guest
pass native_host_maximize_frame_ready

native_restore_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-native-restore markers"
native_restore_state_before=$(marker_exact_count "$native_restore_text" 'window-state=Normal')
native_restore_frame_before=$(marker_exact_count "$native_restore_text" 'frame-size=900x650')
run_fixture_host 8 Restore -Hwnd "$window_id" \
    || fail "could not restore the seamless HWND through the Windows host"
wait_window_state "$window_id" ZOOMED 0
wait_marker_exact_count "$marker_main" 'window-state=Normal' "$((native_restore_state_before + 1))"
wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((native_restore_frame_before + 1))"
native_restore_width=$(window_field "$window_id" WIDTH) \
    || fail "could not read native-restored host width"
native_restore_height=$(window_field "$window_id" HEIGHT) \
    || fail "could not read native-restored host height"
[ "$native_restore_width" -eq 900 ] && [ "$native_restore_height" -eq 650 ] \
    || fail "native restore returned ${native_restore_width}x${native_restore_height}, expected 900x650"
wait_visual_sentinel native-restored "$resize_color" 0.005 900 650
pass native_host_restore_to_guest
pass native_host_restore_frame_ready

# Guest-caption move and two maximize/restore paths deliberately changed the
# proxy placement. Each direction uses the same guest-caption placement helper
# before testing independently from a guest-visible 900x650 baseline.
activate_window "$window_id"
refresh_pointer_epoch "before the first guest border resize"

for resize_direction in north northeast east southeast south southwest west northwest
do
    test_guest_resize_direction "$resize_direction"
done
matrix_final_width=$(window_field "$window_id" WIDTH) \
    || fail "could not read final edge-resize width"
matrix_final_height=$(window_field "$window_id" HEIGHT) \
    || fail "could not read final edge-resize height"
matrix_guest_frame=$(latest_guest_frame_size "$marker_main") \
    || fail "could not read final edge-resize guest frame"
matrix_guest_width=${matrix_guest_frame%% *}
matrix_guest_height=${matrix_guest_frame#* }
if ! integer_near "$matrix_final_width" "$matrix_guest_width" 4 \
    || ! integer_near "$matrix_final_height" "$matrix_guest_height" 4
then
    fail "final edge-resize host ${matrix_final_width}x${matrix_final_height} did not match guest ${matrix_guest_width}x${matrix_guest_height}"
fi
if ! integer_near "$matrix_final_width" 900 4 \
    || ! integer_near "$matrix_final_height" 650 4
then
    fail "edge-resize reset finished at ${matrix_final_width}x${matrix_final_height}, expected about 900x650"
fi
wait_visual_sentinel edge-resize-final "$resize_color" 0.003 \
    "$matrix_final_width" "$matrix_final_height"
pass guest_border_all_directions
pass guest_border_resize_frame_ready

control_width=$(window_field "$window_id" WIDTH) || fail "could not read close-control width"
click_at "$window_id" "$((control_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_main" clipboard-restored
wait_marker_exact "$marker_main" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "guest-caption close returned presenter status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_main" 0
pass guest_caption_close

# Session 2: a hard host crash must release injected state while preserving the
# exact guest process/HWND for an identical-request reattach.
start_presenter "$marker_kill" kill
kill_process_before=$(fixture_process_id "$marker_kill") \
    || fail "could not read the pre-crash fixture PID"
kill_hwnd_before=$(fixture_window_handle "$marker_kill") \
    || fail "could not read the pre-crash guest HWND"
move_percent "$window_id" 50 75
run_fixture_host 8 KeyDown -Hwnd "$window_id" -Key CTRL \
    || fail "could not hold Ctrl for the presenter-crash test"
button_action LeftDown
wait_marker_exact "$marker_kill" 'input-held=ctrl,left'
kill -KILL "$presenter_pid" || fail "could not SIGKILL the seamless presenter"
wait_presenter_exit
if [ "$presenter_status" -eq 0 ]; then
    fail "SIGKILLed presenter unexpectedly returned success"
fi
wait_marker_exact "$marker_kill" 'input-released=all'
run_windows_host 5 ReleaseAll 2>/dev/null || :
wait_no_fixture_window
wait_fixture_count "$marker_kill" 1
kill_survival_text=$(guest_marker_text "$marker_kill") \
    || fail "could not read post-crash guest markers"
if marker_has_exact "$kill_survival_text" closed \
    || marker_has_exact "$kill_survival_text" clipboard-restored
then
    fail "SIGKILL incorrectly closed the recoverable guest HWND"
fi
pass presenter_sigkill_input_release
pass presenter_sigkill_guest_preserved

start_presenter "$marker_kill" kill-reattach
kill_process_after=$(fixture_process_id "$marker_kill") \
    || fail "could not read the reattached fixture PID"
kill_hwnd_after=$(fixture_window_handle "$marker_kill") \
    || fail "could not read the reattached guest HWND"
[ "$kill_process_after" = "$kill_process_before" ] \
    || fail "crash recovery launched PID $kill_process_after instead of reattaching PID $kill_process_before"
[ "$kill_hwnd_after" = "$kill_hwnd_before" ] \
    || fail "crash recovery captured HWND $kill_hwnd_after instead of reattaching HWND $kill_hwnd_before"
kill_reattach_text=$(guest_marker_text "$marker_kill") \
    || fail "could not read reattach identity markers"
[ "$(marker_prefix_count "$kill_reattach_text" 'process-id=')" -eq 1 ] \
    || fail "reattach restarted the fixture process"
[ "$(marker_prefix_count "$kill_reattach_text" 'window-hwnd=')" -eq 1 ] \
    || fail "reattach recreated the guest window"
pass presenter_sigkill_exact_reattach

kill_close_width=$(window_field "$window_id" WIDTH) || fail "could not read reattach close width"
click_at "$window_id" "$((kill_close_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_kill" clipboard-restored
wait_marker_exact "$marker_kill" closed
wait_presenter_exit
[ "$presenter_status" -eq 0 ] || fail "reattached presenter returned status $presenter_status"
wait_no_fixture_window
wait_fixture_count "$marker_kill" 0
pass presenter_sigkill_recovery_cleanup

# Session 3: plain typing after the crash proves Ctrl/mouse state was released.
start_presenter "$marker_recovery" recovery
pass guest_global_input_released_after_sigkill
click_percent "$window_id" 72 24
run_fixture_host 8 Type -Hwnd "$window_id" -Text plain \
    || fail "could not type after presenter crash recovery"
click_percent "$window_id" 34 42
recovery_text=$(printf '%s' plain | base64 | tr -d '\n')
wait_marker_exact "$marker_recovery" "text-base64=$recovery_text"
pass stuck_modifiers_released

control_width=$(window_field "$window_id" WIDTH) || fail "could not read recovery close width"
click_at "$window_id" "$((control_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_recovery" clipboard-restored
wait_marker_exact "$marker_recovery" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    fail "recovery presenter returned status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_recovery" 0
pass presenter_reconnect

# Session 4: continuous full-client repaint and successive guest-border resizes
# must not starve input or close.
start_presenter "$marker_animate" animate animate
wait_host_frame_change animation-live
pass animated_frames_change
click_percent "$window_id" 12 42
wait_marker_exact "$marker_animate" left-click
click_percent "$window_id" 72 24
run_fixture_host 8 Type -Hwnd "$window_id" -Text stress \
    || fail "could not type while the guest continuously repainted"
click_percent "$window_id" 34 42
stress_text=$(printf '%s' stress | base64 | tr -d '\n')
wait_marker_exact "$marker_animate" "text-base64=$stress_text"

place_fixture_via_guest_caption "before-animated-resize" "$marker_animate"
ensure_pointer_ready "$marker_animate" "pre-animated-resize-1024x720"
resize_via_guest_southeast 1024 720
ensure_pointer_ready "$marker_animate" "pre-animated-resize-820x600"
resize_via_guest_southeast 820 600
ensure_pointer_ready "$marker_animate" "pre-animated-resize-960x680"
resize_via_guest_southeast 960 680
wait_marker_exact "$marker_animate" 'frame-size=960x680'
wait_visual_sentinel animated-resized "$maximize_color" 0.003 960 680
wait_host_frame_change animation-post-resize
click_at "$window_id" 95 220
wait_marker_exact_count "$marker_animate" left-click 2
pass animated_repaint_input
pass animated_frames_change_after_resize
pass rapid_resize_input

control_width=$(window_field "$window_id" WIDTH) || fail "could not read animated close width"
click_at "$window_id" "$((control_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_animate" clipboard-restored
wait_marker_exact "$marker_animate" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "animated presenter returned status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_animate" 0
pass animated_repaint_close

# Session 5: a local presenter-construction error must close the shared socket
# even though its reader thread owns a cloned descriptor. The recoverable guest
# HWND stays alive and an identical request must reattach it without relaunching.
presenter_log="$host_root/presenter-error.log"
register_host_artifact presenter-error.log
register_fixture_marker "$marker_error"
setsid env WAYLAND_DISPLAY="lsw-e2e-unreachable-$run_id" "$lsw" run "$instance" --gui -- \
    powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
    -File "$guest_fixture" -MarkerPath "$marker_error" -RunId "$run_id" \
    -CleanupSignalPath "$guest_cleanup_signal" \
    >"$presenter_log" 2>&1 &
presenter_pid=$!
wait_marker_exact "$marker_error" "$released_input_state"
wait_marker_exact "$marker_error" ready
wait_marker_exact "$marker_error" 'frame-size-source=dwm'
wait_marker_prefix "$marker_error" 'window-hwnd='
error_process_before=$(fixture_process_id "$marker_error") \
    || fail "could not read presenter-error fixture PID"
error_hwnd_before=$(fixture_window_handle "$marker_error") \
    || fail "could not read presenter-error guest HWND"
wait_presenter_exit
if [ "$presenter_status" -eq 0 ]; then
    fail "presenter with an unreachable Wayland display unexpectedly succeeded"
fi
wait_no_fixture_window
wait_fixture_count "$marker_error" 1
pass presenter_error_socket_shutdown
pass presenter_error_guest_preserved

start_presenter "$marker_error" presenter-error-reattach
error_process_after=$(fixture_process_id "$marker_error") \
    || fail "could not read presenter-error reattach PID"
error_hwnd_after=$(fixture_window_handle "$marker_error") \
    || fail "could not read presenter-error reattach HWND"
[ "$error_process_after" = "$error_process_before" ] \
    || fail "presenter-error recovery relaunched the fixture process"
[ "$error_hwnd_after" = "$error_hwnd_before" ] \
    || fail "presenter-error recovery recreated the guest HWND"
error_identity_text=$(guest_marker_text "$marker_error") \
    || fail "could not read presenter-error recovery markers"
[ "$(marker_prefix_count "$error_identity_text" 'process-id=')" -eq 1 ] \
    || fail "presenter-error recovery rewrote the fixture marker"
[ "$(marker_prefix_count "$error_identity_text" 'window-hwnd=')" -eq 1 ] \
    || fail "presenter-error recovery reran the fixture Shown handler"
error_close_width=$(window_field "$window_id" WIDTH) || fail "could not read presenter-error close width"
click_at "$window_id" "$((error_close_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_error" clipboard-restored
wait_marker_exact "$marker_error" closed
wait_presenter_exit
[ "$presenter_status" -eq 0 ] || fail "presenter-error recovery returned status $presenter_status"
wait_no_fixture_window
wait_fixture_count "$marker_error" 0
pass presenter_error_exact_reattach
pass presenter_error_guest_cleanup

# Session 6: a native guest-caption close must leave an owned unsaved-change
# prompt visible and interactive. Cancel keeps the session alive; Discard exits.
start_presenter "$marker_close" close-prompt close-prompt
close_width=$(window_field "$window_id" WIDTH) || fail "could not read close-prompt width"
close_height=$(window_field "$window_id" HEIGHT) || fail "could not read close-prompt height"
click_at "$window_id" "$((close_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact_count "$marker_close" close-prompt-open 1
wait_marker_prefix_count "$marker_close" 'close-prompt-controls=' 1
wait_visual_sentinel close-prompt-cancel "$close_prompt_color" 0.01 \
    "$close_width" "$close_height"
close_prompt_text=$(guest_marker_text "$marker_close") \
    || fail "could not read close-prompt controls"
cancel_x=$(close_prompt_control_field "$close_prompt_text" cancel-x) \
    || fail "could not read close-prompt Cancel x-coordinate"
cancel_y=$(close_prompt_control_field "$close_prompt_text" cancel-y) \
    || fail "could not read close-prompt Cancel y-coordinate"
click_at "$window_id" "$cancel_x" "$cancel_y"
wait_marker_exact "$marker_close" close-prompt-cancel
wait_fixture_count "$marker_close" 1
click_percent "$window_id" 12 42
wait_marker_exact "$marker_close" left-click
pass native_close_prompt_cancel

dirty_process_before=$(fixture_process_id "$marker_close") \
    || fail "could not read dirty fixture PID before presenter crash"
dirty_hwnd_before=$(fixture_window_handle "$marker_close") \
    || fail "could not read dirty guest HWND before presenter crash"
kill -KILL "$presenter_pid" || fail "could not SIGKILL the dirty-window presenter"
run_windows_host 5 ReleaseAll 2>/dev/null || :
wait_presenter_exit
[ "$presenter_status" -ne 0 ] || fail "SIGKILLed dirty presenter unexpectedly returned success"
wait_no_fixture_window
wait_fixture_count "$marker_close" 1
dirty_survival_text=$(guest_marker_text "$marker_close") \
    || fail "could not read dirty-window survival markers"
if marker_has_exact "$dirty_survival_text" closed \
    || marker_has_exact "$dirty_survival_text" clipboard-restored
then
    fail "presenter crash discarded the simulated dirty guest HWND"
fi

start_presenter "$marker_close" close-prompt-reattach close-prompt
dirty_process_after=$(fixture_process_id "$marker_close") \
    || fail "could not read dirty fixture PID after reattach"
dirty_hwnd_after=$(fixture_window_handle "$marker_close") \
    || fail "could not read dirty guest HWND after reattach"
[ "$dirty_process_after" = "$dirty_process_before" ] \
    || fail "dirty recovery relaunched PID $dirty_process_after instead of $dirty_process_before"
[ "$dirty_hwnd_after" = "$dirty_hwnd_before" ] \
    || fail "dirty recovery replaced HWND $dirty_hwnd_before with $dirty_hwnd_after"
dirty_reattach_text=$(guest_marker_text "$marker_close") \
    || fail "could not read dirty reattach markers"
[ "$(marker_exact_count "$dirty_reattach_text" close-prompt-cancel)" -eq 1 ] \
    || fail "dirty recovery lost or duplicated the prior Cancel state"
[ "$(marker_prefix_count "$dirty_reattach_text" 'window-hwnd=')" -eq 1 ] \
    || fail "dirty recovery reran the fixture Shown handler"
pass dirty_window_exact_reattach

close_width=$(window_field "$window_id" WIDTH) || fail "could not reread close-prompt width"
close_height=$(window_field "$window_id" HEIGHT) || fail "could not reread close-prompt height"
click_at "$window_id" "$((close_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact_count "$marker_close" close-prompt-open 2
wait_marker_prefix_count "$marker_close" 'close-prompt-controls=' 2
wait_visual_sentinel close-prompt-discard "$close_prompt_color" 0.01 \
    "$close_width" "$close_height"
close_prompt_text=$(guest_marker_text "$marker_close") \
    || fail "could not reread close-prompt controls"
discard_x=$(close_prompt_control_field "$close_prompt_text" discard-x) \
    || fail "could not read close-prompt Discard x-coordinate"
discard_y=$(close_prompt_control_field "$close_prompt_text" discard-y) \
    || fail "could not read close-prompt Discard y-coordinate"
click_at "$window_id" "$discard_x" "$discard_y"
wait_marker_exact "$marker_close" close-prompt-discard
wait_marker_exact "$marker_close" clipboard-restored
wait_marker_exact "$marker_close" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "native close-prompt presenter returned status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_close" 0
pass native_close_prompt_discard
pass native_close_prompt_cleanup
pass guest_clipboard_restored

# Session 7: a Windows host close of the WSLg Wayland proxy must use the protocol
# close path, release forwarded input, and wait for guest acknowledgement.
start_presenter "$marker_host_close" host-close
move_percent "$window_id" 50 75
host_close_input_text=$(guest_marker_text "$marker_host_close") \
    || fail "could not read pre-close input markers"
host_close_ctrl_before=$(marker_exact_count "$host_close_input_text" ctrl-key-down)
host_close_left_before=$(marker_exact_count "$host_close_input_text" left-button-down)
host_close_ctrl_up_before=$(marker_exact_count "$host_close_input_text" ctrl-key-up)
host_close_left_up_before=$(marker_exact_count "$host_close_input_text" left-button-up)
run_fixture_host 8 CloseWithHeldInput -Hwnd "$window_id" \
    || fail "could not hold input and request the Windows host-window close"
wait_marker_exact_count "$marker_host_close" ctrl-key-down "$((host_close_ctrl_before + 1))"
wait_marker_exact_count "$marker_host_close" left-button-down "$((host_close_left_before + 1))"
wait_marker_exact_count "$marker_host_close" ctrl-key-up "$((host_close_ctrl_up_before + 1))"
wait_marker_exact_count "$marker_host_close" left-button-up "$((host_close_left_up_before + 1))"
wait_marker_exact "$marker_host_close" "closing-$released_input_state"
run_windows_host 5 ReleaseAll 2>/dev/null || :
wait_marker_exact "$marker_host_close" clipboard-restored
wait_marker_exact "$marker_host_close" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "Windows host-window close returned presenter status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_host_close" 0
pass host_window_manager_close
pass host_close_releases_input

# Session 8: an application that remembers a maximized guest state must make
# the new host window maximized, and its visible Restore button must idempotently
# restore both sides rather than blindly toggling an already divergent host.
start_presenter "$marker_initial_max" initial-maximized maximized
wait_marker_exact "$marker_initial_max" 'window-state=Maximized'
wait_window_state "$window_id" ZOOMED 1
pass initial_guest_maximize_sync
initial_restore_text=$(guest_marker_text "$marker_initial_max") \
    || fail "could not read initial-maximized markers"
initial_restore_before=$(marker_exact_count "$initial_restore_text" 'window-state=Normal')
initial_restore_frame_before=$(marker_prefix_count "$initial_restore_text" 'frame-size=')
initial_max_width=$(window_field "$window_id" WIDTH) || fail "could not read initial-maximized width"
initial_max_height=$(window_field "$window_id" HEIGHT) || fail "could not read initial-maximized height"
initial_max_guest_frame=$(latest_guest_frame_size "$marker_initial_max") \
    || fail "could not read the initial-maximized guest DWM frame"
initial_max_guest_width=${initial_max_guest_frame%% *}
initial_max_guest_height=${initial_max_guest_frame#* }
report_fitted_viewport INITIAL_MAXIMIZE_VIEWPORT \
    "$initial_max_width" "$initial_max_height" \
    "$initial_max_guest_width" "$initial_max_guest_height"
click_guest_point "$window_id" "$initial_max_guest_width" "$initial_max_guest_height" \
    "$((initial_max_guest_width - caption_maximize_offset))" "$caption_control_y"
wait_window_state "$window_id" ZOOMED 0
wait_marker_exact_count "$marker_initial_max" 'window-state=Normal' \
    "$((initial_restore_before + 1))"
wait_marker_prefix_count "$marker_initial_max" 'frame-size=' \
    "$((initial_restore_frame_before + 1))"
pass initial_guest_restore_sync
initial_restore_guest_frame=$(latest_guest_frame_size "$marker_initial_max") \
    || fail "could not read the initial-restored guest DWM frame"
initial_restore_guest_width=${initial_restore_guest_frame%% *}
initial_restore_guest_height=${initial_restore_guest_frame#* }
click_guest_point "$window_id" "$initial_restore_guest_width" "$initial_restore_guest_height" \
    "$((initial_restore_guest_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_initial_max" clipboard-restored
wait_marker_exact "$marker_initial_max" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "initial-maximized presenter returned status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_initial_max" 0
pass explicit_maximize_restore_state

if ! cleanup_guest_root; then
    fail "guest fixture cleanup failed"
fi
pass guest_artifact_cleanup
matrix_complete=1
