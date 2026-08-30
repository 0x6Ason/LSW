#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later

# Final native-Linux Slice 4 gate. It consumes only the stopped linked clone
# produced by check-windows-kvm-e2e.sh in the same job, owns that clone, and
# removes it on every bounded exit path.

set -eu

overall_timeout_seconds=${LSW_NATIVE_GUI_E2E_TIMEOUT_SECONDS:-1800}
workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
lsw=${LSW_E2E_LSW:-"$workspace_root/target/release/lsw"}
lswd=${LSW_E2E_LSWD:-"$workspace_root/target/release/lswd"}
lswg=${LSW_E2E_LSWG:-"$workspace_root/target/release/lswg"}
agent=${LSW_WINDOWS_AGENT:-"$workspace_root/target/x86_64-pc-windows-gnu/release/lsw-agent.exe"}
active_root_file=${LSW_E2E_ACTIVE_ROOT_FILE:-}
artifact_dir=${LSW_E2E_ARTIFACT_DIR:-}
expected_sha=${LSW_E2E_CANDIDATE_SHA:-${GITHUB_SHA:-}}
root_base=${LSW_E2E_ROOT_BASE:-}

case "$overall_timeout_seconds" in
    ''|*[!0-9]*)
        echo "error: LSW_NATIVE_GUI_E2E_TIMEOUT_SECONDS must be a positive integer" >&2
        exit 1
        ;;
esac
if [ "$overall_timeout_seconds" -eq 0 ]; then
    echo "error: LSW_NATIVE_GUI_E2E_TIMEOUT_SECONDS must be greater than zero" >&2
    exit 1
fi
if [ "${LSW_NATIVE_GUI_E2E_TIMEOUT_ACTIVE:-0}" != 1 ]; then
    LSW_NATIVE_GUI_E2E_TIMEOUT_ACTIVE=1
    export LSW_NATIVE_GUI_E2E_TIMEOUT_ACTIVE
    exec timeout --signal=TERM --kill-after=300s \
        "$((overall_timeout_seconds + 300))s" sh "$0" "$@"
fi
if [ "$#" -ne 0 ]; then
    echo "usage: scripts/check-windows-native-wayland-e2e.sh" >&2
    exit 1
fi

if grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null \
    || [ -n "${WSL_DISTRO_NAME:-}" ]
then
    echo "error: the final GUI gate requires native Linux; WSLg is compatibility evidence only" >&2
    exit 1
fi
for required_command in \
    awk chmod convert date env find grep grim id identify jq kill mkdir mv \
    python3 realpath rm setsid sha256sum sleep stat sway swaymsg tail timeout tr wtype
do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: native Wayland runner command was not found: $required_command" >&2
        exit 1
    fi
done
if [ ! -x "$lsw" ] || [ ! -x "$lswd" ] || [ ! -x "$lswg" ] || [ ! -f "$agent" ]; then
    echo "error: the exact candidate lsw, lswd, lswg, and Windows agent are required" >&2
    exit 1
fi
if ! printf '%s\n' "$expected_sha" | grep -Eq '^[0-9a-f]{40}$'; then
    echo "error: LSW_E2E_CANDIDATE_SHA must be the exact lowercase commit SHA" >&2
    exit 1
fi
if [ -z "$root_base" ] || [ -z "$active_root_file" ] || [ -z "$artifact_dir" ]; then
    echo "error: native GUI handoff paths were not configured" >&2
    exit 1
fi

pass() {
    printf 'CHECK %s=pass\n' "$1"
}

fail() {
    echo "error: $*" >&2
    exit 1
}

handoff_field() {
    handoff_key=$1
    awk -F= -v key="$handoff_key" '
        $1 == key {
            count++
            value = substr($0, length($1) + 2)
        }
        END {
            if (count == 1 && value != "") {
                print value
                exit 0
            }
            exit 1
        }
    ' "$handoff_metadata"
}

canonical_root_base=$(realpath -e -- "$root_base") \
    || fail "could not resolve LSW_E2E_ROOT_BASE"
[ "$canonical_root_base" = "$root_base" ] \
    || fail "LSW_E2E_ROOT_BASE must be canonical"
if [ -L "$active_root_file" ] || [ ! -f "$active_root_file" ] \
    || [ "$(stat -c %u -- "$active_root_file")" != "$(id -u)" ] \
    || [ "$(stat -c %a -- "$active_root_file")" != 600 ]
then
    fail "the active E2E root record must be one private regular file"
fi
active_root_lines=$(awk 'END { print NR }' "$active_root_file")
[ "$active_root_lines" -eq 1 ] || fail "the active E2E root record is ambiguous"
handoff_root=$(awk 'NR == 1 { print; exit }' "$active_root_file")
handoff_parent=${handoff_root%/*}
handoff_name=${handoff_root##*/}
[ "$handoff_parent" = "$canonical_root_base" ] \
    || fail "the GUI handoff root escaped LSW_E2E_ROOT_BASE"
printf '%s\n' "$handoff_name" | grep -Eq '^lsw-e2e\.[[:alnum:]]{6}$' \
    || fail "the GUI handoff root name is invalid"
[ "$(realpath -e -- "$handoff_root")" = "$handoff_root" ] \
    || fail "the GUI handoff root must be canonical"
if [ -L "$handoff_root" ] || [ ! -d "$handoff_root" ] \
    || [ "$(stat -c %u -- "$handoff_root")" != "$(id -u)" ] \
    || [ "$(stat -c %a -- "$handoff_root")" != 700 ]
then
    fail "the GUI handoff root must be an owned mode-0700 real directory"
fi

handoff_metadata="$handoff_root/gui-handoff.env"
if [ -L "$handoff_metadata" ] || [ ! -f "$handoff_metadata" ] \
    || [ "$(stat -c %u -- "$handoff_metadata")" != "$(id -u)" ] \
    || [ "$(stat -c %a -- "$handoff_metadata")" != 600 ] \
    || [ "$(stat -c %s -- "$handoff_metadata")" -gt 16384 ]
then
    fail "the GUI handoff metadata must be one bounded private regular file"
fi
[ "$(awk 'END { print NR }' "$handoff_metadata")" -eq 10 ] \
    || fail "the GUI handoff metadata has an unexpected field count"
handoff_version=$(handoff_field version) || fail "the GUI handoff has no unique version"
handoff_sha=$(handoff_field candidate_sha) || fail "the GUI handoff has no unique candidate SHA"
handoff_source=$(handoff_field source) || fail "the GUI handoff has no unique source"
state_dir=$(handoff_field state_dir) || fail "the GUI handoff has no unique state directory"
instance=$(handoff_field instance) || fail "the GUI handoff has no unique instance"
login_secret=$(handoff_field login_secret) || fail "the GUI handoff has no unique login secret"
handoff_lsw_sha=$(handoff_field lsw_sha256) || fail "the GUI handoff has no unique CLI hash"
handoff_lswd_sha=$(handoff_field lswd_sha256) || fail "the GUI handoff has no unique daemon hash"
handoff_lswg_sha=$(handoff_field lswg_sha256) || fail "the GUI handoff has no unique lswg hash"
handoff_agent_sha=$(handoff_field agent_sha256) || fail "the GUI handoff has no unique agent hash"

[ "$handoff_version" = 1 ] || fail "unsupported GUI handoff version"
[ "$handoff_sha" = "$expected_sha" ] || fail "the GUI handoff commit is not the candidate"
[ "$handoff_source" = real-install-linked-clone ] \
    || fail "the GUI handoff was not derived from the real install"
[ "$state_dir" = "$handoff_root/state" ] \
    || fail "the GUI handoff state directory changed"
[ "$login_secret" = "$handoff_root/gui-login.secret" ] \
    || fail "the GUI handoff login-secret path changed"
printf '%s\n' "$instance" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$' \
    || fail "the GUI handoff instance name is invalid"
for handoff_hash in "$handoff_lsw_sha" "$handoff_lswd_sha" "$handoff_lswg_sha" "$handoff_agent_sha"; do
    printf '%s\n' "$handoff_hash" | grep -Eq '^[0-9a-f]{64}$' \
        || fail "the GUI handoff contains an invalid binary hash"
done
[ "$handoff_lsw_sha" = "$(sha256sum "$lsw" | awk '{ print $1 }')" ] \
    || fail "the GUI handoff CLI does not match the candidate"
[ "$handoff_lswd_sha" = "$(sha256sum "$lswd" | awk '{ print $1 }')" ] \
    || fail "the GUI handoff daemon does not match the candidate"
[ "$handoff_lswg_sha" = "$(sha256sum "$lswg" | awk '{ print $1 }')" ] \
    || fail "the GUI handoff lswg does not match the candidate"
[ "$handoff_agent_sha" = "$(sha256sum "$agent" | awk '{ print $1 }')" ] \
    || fail "the GUI handoff agent does not match the candidate"
if [ -L "$login_secret" ] || [ ! -f "$login_secret" ] \
    || [ "$(stat -c %u -- "$login_secret")" != "$(id -u)" ] \
    || [ "$(stat -c %a -- "$login_secret")" != 600 ] \
    || [ "$(stat -c %s -- "$login_secret")" -gt 256 ]
then
    fail "the GUI login secret must be one bounded private regular file"
fi
instance_manifest="$state_dir/instances/$instance/instance.lsw"
if [ -L "$instance_manifest" ] || [ ! -f "$instance_manifest" ]; then
    fail "the stopped GUI clone manifest is missing"
fi
grep -Fx 'state=stopped' "$instance_manifest" >/dev/null \
    || fail "the GUI handoff clone is not stopped"
grep -Fx 'default_user=lsw-e2e-gui' "$instance_manifest" >/dev/null \
    || fail "the GUI handoff user is not registered"
grep -Fx 'default_user_role=administrator' "$instance_manifest" >/dev/null \
    || fail "the GUI handoff user is not the explicit test administrator"
grep -E '^base_image_key=[0-9a-f]{64}$' "$instance_manifest" >/dev/null \
    || fail "the GUI handoff is not a sealed linked clone"
pass real_install_handoff

export LSW_STATE_DIR="$state_dir"
export LSW_WINDOWS_AGENT="$agent"
export LSWG="$lswg"
export LSWG_SHA256="$handoff_lswg_sha"
autospawn_blocker="$handoff_root/lswd-autospawn-disabled"
export LSW_DAEMON="$autospawn_blocker"
daemon_pid=
presenter_pid=
sway_pid=
guest_root_created=0
matrix_complete=0
window_title=
guest_root=
guest_cleanup_signal=

terminate_process_group() {
    terminate_pid=$1
    case "$terminate_pid" in
        ''|*[!0-9]*) return 0 ;;
    esac
    kill -TERM "-$terminate_pid" 2>/dev/null || kill -TERM "$terminate_pid" 2>/dev/null || :
    terminate_attempt=0
    while kill -0 "-$terminate_pid" 2>/dev/null && [ "$terminate_attempt" -lt 50 ]; do
        terminate_attempt=$((terminate_attempt + 1))
        sleep 0.1
    done
    if kill -0 "-$terminate_pid" 2>/dev/null; then
        kill -KILL "-$terminate_pid" 2>/dev/null || :
    fi
    wait "$terminate_pid" 2>/dev/null || :
}

run_lsw() {
    run_seconds=$1
    shift
    timeout --signal=TERM --kill-after=3s "${run_seconds}s" "$lsw" "$@"
}

instance_is_running() {
    run_lsw 8 status "$instance" 2>/dev/null | grep -Fx 'STATE=running' >/dev/null
}

remove_guest_root() {
    if [ "$guest_root_created" -ne 1 ] || ! instance_is_running; then
        return 0
    fi
    case "$guest_root" in
        'C:\Users\Public\Documents\LSW-Native-Wayland-E2E-'*) ;;
        *) return 1 ;;
    esac
    cleanup_script="\$ErrorActionPreference='Stop'; \$Root=[IO.Path]::GetFullPath('$guest_root'); \$Signal=[IO.Path]::GetFullPath('$guest_cleanup_signal'); \$Marker=[IO.Path]::GetFullPath('$guest_marker'); \$ExpectedSignal=[IO.Path]::Combine(\$Root,'cleanup.signal'); \$ExpectedMarker=[IO.Path]::Combine(\$Root,'main.tsv'); if (\$Signal -cne \$ExpectedSignal -or \$Marker -cne \$ExpectedMarker) { throw 'native Wayland cleanup paths changed' }; if (Test-Path -LiteralPath \$Root) { \$Directory=Get-Item -LiteralPath \$Root -Force; if (\$Directory.FullName -cne \$Root -or ((\$Directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'native Wayland guest root must be one real directory' }; [IO.File]::WriteAllText(\$Signal,'cleanup',[Text.UTF8Encoding]::new(\$false)); \$Closed=\$false; \$Deadline=[DateTime]::UtcNow.AddSeconds(10); do { if (Test-Path -LiteralPath \$Marker -PathType Leaf) { \$MarkerItem=Get-Item -LiteralPath \$Marker -Force; if (\$MarkerItem.FullName -cne \$Marker -or ((\$MarkerItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'native Wayland marker must be one real file' }; \$Lines=@(Get-Content -LiteralPath \$Marker -ErrorAction Stop); if (@(\$Lines | Where-Object { \$_ -match '\tclosed\$' }).Count -gt 0) { \$Closed=\$true; break }; \$PidLine=@(\$Lines | Where-Object { \$_ -match '\tprocess-id=([1-9][0-9]*)\$' } | Select-Object -First 1); if (\$PidLine.Count -gt 0) { [void](\$PidLine[0] -match '\tprocess-id=([1-9][0-9]*)\$'); if (\$null -eq (Get-Process -Id ([int]\$Matches[1]) -ErrorAction SilentlyContinue)) { \$Closed=\$true; break } } }; Start-Sleep -Milliseconds 100 } while ([DateTime]::UtcNow -lt \$Deadline); if (-not \$Closed) { throw 'native Wayland fixture did not release its files' }; \$Stack=[Collections.Generic.Stack[string]]::new(); \$Stack.Push(\$Root); while (\$Stack.Count -gt 0) { foreach (\$Entry in [IO.Directory]::EnumerateFileSystemEntries(\$Stack.Pop())) { \$Attributes=[IO.File]::GetAttributes(\$Entry); if ((\$Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'native Wayland cleanup tree contains a reparse point' }; if ((\$Attributes -band [IO.FileAttributes]::Directory) -ne 0) { \$Stack.Push(\$Entry) } } }; for (\$Attempt=0; \$Attempt -lt 30; \$Attempt++) { try { Remove-Item -LiteralPath \$Root -Recurse -Force -ErrorAction Stop; break } catch { if (\$Attempt -eq 29) { throw }; Start-Sleep -Milliseconds 200 } } }; if (Test-Path -LiteralPath \$Root) { throw 'native Wayland guest root remained' }"
    if run_lsw 20 exec "$instance" -- powershell.exe -NoLogo -NoProfile \
        -NonInteractive -Command "$cleanup_script" >/dev/null 2>&1
    then
        guest_root_created=0
        return 0
    fi
    return 1
}

cleanup() {
    cleanup_status=$?
    trap - EXIT HUP INT TERM
    cleanup_failed=0

    if ! remove_guest_root; then
        echo "warning: native Wayland guest artifacts could not be removed" >&2
        cleanup_failed=1
    fi
    terminate_process_group "$presenter_pid"
    presenter_pid=
    terminate_process_group "$sway_pid"
    sway_pid=

    if [ -f "$state_dir/instances/$instance/instance.lsw" ]; then
        cleanup_qemu_pid=
        if [ -f "$state_dir/instances/$instance/run/qemu.pid" ]; then
            cleanup_qemu_pid=$(awk 'NR == 1 { print $1; exit }' \
                "$state_dir/instances/$instance/run/qemu.pid")
        fi
        if instance_is_running; then
            timeout 180s "$lsw" shutdown "$instance" >/dev/null 2>&1 || \
                timeout 30s "$lsw" stop "$instance" --force >/dev/null 2>&1 || :
        fi
        cleanup_deadline=$(( $(date +%s) + 180 ))
        while [ "$(date +%s)" -lt "$cleanup_deadline" ]; do
            if run_lsw 5 status "$instance" 2>/dev/null | grep -Fx 'STATE=stopped' >/dev/null; then
                break
            fi
            sleep 1
        done
        if instance_is_running; then
            echo "warning: graceful GUI shutdown timed out; forcing the test VM off" >&2
            run_lsw 30 stop "$instance" --force >/dev/null 2>&1 || :
            force_deadline=$(( $(date +%s) + 30 ))
            while [ "$(date +%s)" -lt "$force_deadline" ]; do
                if ! instance_is_running; then
                    break
                fi
                sleep 1
            done
        fi
        if instance_is_running \
            || ! run_lsw 30 remove "$instance" >/dev/null 2>&1
        then
            echo "warning: final GUI gate could not remove instance $instance" >&2
            cleanup_failed=1
        fi
    fi
    rm -f -- "$login_secret"
    terminate_process_group "$daemon_pid"
    daemon_pid=

    case "${cleanup_qemu_pid:-}" in
        ''|*[!0-9]*) ;;
        *)
            cleanup_qemu_attempt=0
            while kill -0 "$cleanup_qemu_pid" 2>/dev/null \
                && [ "$cleanup_qemu_attempt" -lt 50 ]
            do
                cleanup_qemu_attempt=$((cleanup_qemu_attempt + 1))
                sleep 0.1
            done
            if kill -0 "$cleanup_qemu_pid" 2>/dev/null; then
                echo "warning: final GUI QEMU process $cleanup_qemu_pid survived cleanup" >&2
                cleanup_failed=1
            fi
            ;;
    esac

    if [ -e "$state_dir/instances/$instance" ]; then
        echo "warning: final GUI instance directory remained after cleanup" >&2
        cleanup_failed=1
    fi
    if [ "$cleanup_failed" -ne 0 ] && [ "$cleanup_status" -eq 0 ]; then
        cleanup_status=1
    fi
    if [ "$cleanup_status" -eq 0 ] && [ "$matrix_complete" -eq 1 ]; then
        printf 'RESULT=pass instance=%s candidate_sha=%s compositor=sway-headless\n' \
            "$instance" "$expected_sha"
    fi
    exit "$cleanup_status"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

mkdir -p -- "$artifact_dir"
chmod 700 -- "$artifact_dir"
setsid "$lswd" >"$artifact_dir/native-lswd.log" 2>&1 &
daemon_pid=$!
daemon_deadline=$(( $(date +%s) + 20 ))
while [ "$(date +%s)" -lt "$daemon_deadline" ]; do
    if run_lsw 3 status "$instance" >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        tail -n 80 "$artifact_dir/native-lswd.log" >&2 || :
        fail "the exact candidate daemon exited before handoff validation"
    fi
    sleep 0.2
done
kill -0 "$daemon_pid" 2>/dev/null || fail "the exact candidate daemon is not running"
pass candidate_daemon

run_lsw 30 start "$instance" >/dev/null || fail "could not start the stopped GUI clone"
agent_deadline=$(( $(date +%s) + 660 ))
while [ "$(date +%s)" -lt "$agent_deadline" ]; do
    instance_status=$(run_lsw 8 status "$instance" 2>/dev/null || :)
    if printf '%s\n' "$instance_status" | grep -Fx 'STATE=running' >/dev/null \
        && printf '%s\n' "$instance_status" | grep -Fx 'AGENT=ready' >/dev/null
    then
        break
    fi
    sleep 2
done
instance_status=$(run_lsw 8 status "$instance") || fail "could not query the GUI clone"
printf '%s\n' "$instance_status" | grep -Fx 'STATE=running' >/dev/null \
    || fail "the GUI clone did not remain running"
printf '%s\n' "$instance_status" | grep -Fx 'AGENT=ready' >/dev/null \
    || fail "the GUI clone agent did not become ready"
vnc_socket="$state_dir/instances/$instance/run/recovery-vnc.sock"
vnc_deadline=$(( $(date +%s) + 20 ))
while [ "$(date +%s)" -lt "$vnc_deadline" ] && [ ! -S "$vnc_socket" ]; do
    sleep 0.2
done
[ -S "$vnc_socket" ] || fail "the GUI clone has no private recovery VNC socket"

IFS= read -r gui_password <"$login_secret" || fail "could not read the GUI login secret"
printf '%s\n' "$gui_password" | LC_ALL=C grep -Eq '^[ -~]{1,127}$' \
    || fail "the GUI login secret is not bounded ASCII"

probe_explorer() {
    # PowerShell expands its own session variables inside the guest.
    # shellcheck disable=SC2016
    explorer_output=$(run_lsw 8 exec "$instance" -- powershell.exe -NoLogo -NoProfile \
        -NonInteractive -Command \
        '$Sessions=@(Get-Process -Name explorer -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -gt 0 } | Select-Object -ExpandProperty SessionId -Unique); if ($Sessions.Count -eq 1) { [Console]::Out.Write("SIGNED_IN_SESSION="+$Sessions[0]) } else { [Console]::Out.Write("SIGNED_IN_SESSION=unavailable") }' \
        2>/dev/null || :)
    printf '%s\n' "$explorer_output" | tr -d '\r' | grep -Eq '^SIGNED_IN_SESSION=[1-9][0-9]*$'
}

login_attempt=1
signed_in=0
while [ "$login_attempt" -le 3 ]; do
    if [ "$login_attempt" -eq 1 ]; then
        printf '%s\n' "$gui_password" | python3 "$workspace_root/scripts/qemu-vnc-login.py" \
            "$vnc_socket" || fail "could not send the private initial login sequence"
    else
        printf '%s\n' "$gui_password" | python3 "$workspace_root/scripts/qemu-vnc-login.py" \
            "$vnc_socket" --secure-attention \
            || fail "could not send the private secure-attention login sequence"
    fi
    login_deadline=$(( $(date +%s) + 90 ))
    while [ "$(date +%s)" -lt "$login_deadline" ]; do
        if probe_explorer; then
            signed_in=1
            break
        fi
        sleep 2
    done
    [ "$signed_in" -eq 0 ] || break
    login_attempt=$((login_attempt + 1))
done
gui_password=
unset gui_password
rm -f -- "$login_secret"
[ "$signed_in" -eq 1 ] || fail "the private VNC login did not create one Explorer session"
pass private_console_login

native_root="$handoff_root/native-wayland"
if [ -e "$native_root" ] || [ -L "$native_root" ]; then
    fail "refusing to replace an existing native Wayland root"
fi
mkdir -m 700 -- "$native_root"
mkdir -m 700 -- "$native_root/runtime"
runtime_dir="$native_root/runtime"
sway_config="$native_root/sway.conf"
run_id="$(date +%s)-$$"
window_title="LSW Seamless Fixture $run_id - LSW"
{
    printf 'xwayland disable\n'
    printf 'default_border none\n'
    printf 'default_floating_border none\n'
    printf 'focus_follows_mouse no\n'
    printf 'mouse_warping none\n'
    printf 'seat seat0 fallback true\n'
    printf 'output * resolution 1280x720\n'
    printf 'for_window [title="^LSW Seamless Fixture .* - LSW$"] floating enable, border none\n'
} >"$sway_config"
chmod 600 "$sway_config"
setsid env -u DISPLAY -u WAYLAND_DISPLAY \
    XDG_RUNTIME_DIR="$runtime_dir" \
    WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_LIBINPUT_NO_DEVICES=1 \
    WLR_RENDERER=pixman \
    sway --config "$sway_config" --debug >"$artifact_dir/native-sway.log" 2>&1 &
sway_pid=$!

sway_deadline=$(( $(date +%s) + 20 ))
wayland_socket=
sway_socket=
while [ "$(date +%s)" -lt "$sway_deadline" ]; do
    if ! kill -0 "$sway_pid" 2>/dev/null; then
        tail -n 120 "$artifact_dir/native-sway.log" >&2 || :
        fail "the private native Sway compositor exited during startup"
    fi
    wayland_socket=$(find "$runtime_dir" -maxdepth 1 -type s -name 'wayland-*' -print -quit)
    sway_socket=$(find "$runtime_dir" -maxdepth 1 -type s -name 'sway-ipc.*.sock' -print -quit)
    if [ -n "$wayland_socket" ] && [ -n "$sway_socket" ]; then
        break
    fi
    sleep 0.1
done
if [ -z "$wayland_socket" ] || [ -z "$sway_socket" ]; then
    fail "the private native Sway sockets did not appear"
fi
export XDG_RUNTIME_DIR="$runtime_dir"
export WAYLAND_DISPLAY="${wayland_socket##*/}"
export SWAYSOCK="$sway_socket"
unset DISPLAY
sway_outputs=$(swaymsg -s "$sway_socket" -r -t get_outputs) \
    || fail "could not query the private Sway output"
printf '%s\n' "$sway_outputs" | jq -e \
    'length == 1 and .[0].active == true and .[0].rect.width == 1280 and .[0].rect.height == 720' \
    >/dev/null || fail "the private Sway output is not the exact 1280x720 matrix"
pass native_wayland_compositor

sway_command() {
    sway_reply=$(swaymsg -s "$sway_socket" -r -t command "$1") \
        || return 1
    printf '%s\n' "$sway_reply" | jq -e \
        'type == "array" and length > 0 and all(.[]; .success == true)' >/dev/null
}

window_nodes() {
    swaymsg -s "$sway_socket" -r -t get_tree | jq -c --arg title "$window_title" \
        '[.. | objects | select(.type? == "con" and .name? == $title and .shell? == "xdg_shell")]'
}

wait_window() {
    window_deadline=$(( $(date +%s) + 30 ))
    while [ "$(date +%s)" -lt "$window_deadline" ]; do
        window_matches=$(window_nodes 2>/dev/null || printf '[]')
        if [ "$(printf '%s\n' "$window_matches" | jq 'length')" -eq 1 ]; then
            window_node=$(printf '%s\n' "$window_matches" | jq -c '.[0]')
            window_id=$(printf '%s\n' "$window_node" | jq -r '.id')
            return 0
        fi
        sleep 0.1
    done
    return 1
}

wait_no_window() {
    no_window_deadline=$(( $(date +%s) + 15 ))
    while [ "$(date +%s)" -lt "$no_window_deadline" ]; do
        no_window_matches=$(window_nodes 2>/dev/null || printf '[]')
        if [ "$(printf '%s\n' "$no_window_matches" | jq 'length')" -eq 0 ]; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

refresh_window() {
    window_matches=$(window_nodes) || return 1
    [ "$(printf '%s\n' "$window_matches" | jq 'length')" -eq 1 ] || return 1
    window_node=$(printf '%s\n' "$window_matches" | jq -c '.[0]')
    window_id=$(printf '%s\n' "$window_node" | jq -r '.id')
    window_x=$(printf '%s\n' "$window_node" | jq -r '.rect.x')
    window_y=$(printf '%s\n' "$window_node" | jq -r '.rect.y')
    window_width=$(printf '%s\n' "$window_node" | jq -r '.rect.width')
    window_height=$(printf '%s\n' "$window_node" | jq -r '.rect.height')
    for window_value in "$window_id" "$window_x" "$window_y" "$window_width" "$window_height"; do
        printf '%s\n' "$window_value" | grep -Eq '^-?[0-9]+$' || return 1
    done
}

guest_marker_text() {
    run_lsw 5 exec "$instance" -- cmd.exe /d /c type "$guest_marker" 2>/dev/null | tr -d '\r'
}

marker_has_exact() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        '$2 == expected { found=1 } END { exit(found ? 0 : 1) }'
}

marker_exact_count() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        '$2 == expected { count++ } END { print count + 0 }'
}

wait_marker_exact() {
    wait_event=$1
    marker_deadline=$(( $(date +%s) + 30 ))
    while [ "$(date +%s)" -lt "$marker_deadline" ]; do
        marker_text=$(guest_marker_text 2>/dev/null || :)
        if marker_has_exact "$marker_text" "$wait_event"; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

marker_pair_field() {
    marker_prefix=$1
    marker_name=$2
    guest_marker_text | awk -F '\t' -v prefix="$marker_prefix" -v name="$marker_name" '
        index($2, prefix) == 1 {
            count = split(substr($2, length(prefix) + 1), values, ",")
            for (index = 1; index <= count; index++) {
                split(values[index], pair, ":")
                if (pair[1] == name && pair[2] ~ /^[0-9]+$/) {
                    observed = pair[2]
                }
            }
        }
        END { if (observed != "") print observed; else exit 1 }
    '
}

pointer_click() {
    click_x=$1
    click_y=$2
    pointer_button "$click_x" "$click_y" button1
}

pointer_button() {
    button_x=$1
    button_y=$2
    button_name=$3
    sway_command "seat seat0 cursor set $button_x $button_y" \
        && sway_command "seat seat0 cursor press $button_name" \
        && sleep 0.05 \
        && sway_command "seat seat0 cursor release $button_name"
}

capture_window() {
    capture_label=$1
    refresh_window || fail "could not refresh the native window before $capture_label"
    capture_path="$artifact_dir/native-$capture_label.png"
    grim -g "$window_x,$window_y ${window_width}x${window_height}" "$capture_path" \
        || fail "could not capture the native Wayland window for $capture_label"
    capture_dimensions=$(identify -format '%w %h' "$capture_path" 2>/dev/null) \
        || fail "could not inspect the native Wayland capture"
    [ "$capture_dimensions" = "$window_width $window_height" ] \
        || fail "the native Wayland capture geometry changed"
    capture_variance=$(convert "$capture_path" -colorspace RGB \
        -format '%[fx:standard_deviation]' info: 2>/dev/null) \
        || fail "could not measure native Wayland capture variance"
    awk -v value="$capture_variance" 'BEGIN { exit(value > 0.01 ? 0 : 1) }' \
        || fail "the native Wayland capture was visually empty"
    chmod 600 "$capture_path"
}

wait_visual_color() {
    visual_label=$1
    visual_color=$2
    visual_minimum=$3
    visual_deadline=$(( $(date +%s) + 12 ))
    visual_fraction=none
    while [ "$(date +%s)" -lt "$visual_deadline" ]; do
        capture_window "$visual_label"
        visual_fraction=$(convert "$capture_path" -alpha off -fuzz 6% \
            -fill black +opaque "$visual_color" -fill white -opaque "$visual_color" \
            -colorspace gray -format '%[fx:mean]' info: 2>/dev/null \
            || printf none)
        if awk -v observed="$visual_fraction" -v minimum="$visual_minimum" \
            'BEGIN { exit(!(observed + 0 >= minimum + 0)) }'
        then
            return 0
        fi
        sleep 0.2
    done
    fail "$visual_label did not show sentinel $visual_color (fraction=$visual_fraction)"
}

guest_root="C:\\Users\\Public\\Documents\\LSW-Native-Wayland-E2E-$run_id"
guest_fixture="$guest_root\\fixture.ps1"
guest_marker="$guest_root\\main.tsv"
guest_cleanup_signal="$guest_root\\cleanup.signal"
prepare_guest="\$ErrorActionPreference='Stop'; \$Root=[IO.Path]::GetFullPath('$guest_root'); if (Test-Path -LiteralPath \$Root) { throw 'native Wayland guest root already exists' }; [void][IO.Directory]::CreateDirectory(\$Root)"
run_lsw 15 exec "$instance" -- powershell.exe -NoLogo -NoProfile -NonInteractive \
    -Command "$prepare_guest" >/dev/null || fail "could not create the native guest fixture root"
guest_root_created=1
run_lsw 20 cp "$workspace_root/scripts/windows-seamless-fixture.ps1" "$guest_fixture" \
    >/dev/null || fail "could not stage the native Wayland fixture"
pass fixture_staged

start_presenter() {
    presenter_log="$artifact_dir/native-presenter.log"
    setsid env XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
        SWAYSOCK="$sway_socket" LSW_GUI_TRACE=1 \
        "$lsw" run "$instance" --gui -- \
        powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
        -File "$guest_fixture" -MarkerPath "$guest_marker" -RunId "$run_id" \
        -CleanupSignalPath "$guest_cleanup_signal" \
        >>"$presenter_log" 2>&1 &
    presenter_pid=$!
    wait_window || {
        tail -n 120 "$presenter_log" >&2 || :
        fail "the candidate did not create its native Wayland window"
    }
    wait_marker_exact ready || fail "the guest fixture did not become ready"
}

start_presenter
refresh_window || fail "could not inspect the first native Wayland window"
printf '%s\n' "$window_node" | jq -e \
    '.border == "none" and .current_border_width == 0 and .shell == "xdg_shell" and .visible == true' \
    >/dev/null || fail "the first GUI window is not a visible undecorated xdg-shell surface"
capture_window initial
pass first_native_window

probe_x=$(marker_pair_field 'pointer-probe=' x) || fail "the fixture did not report pointer X"
probe_y=$(marker_pair_field 'pointer-probe=' y) || fail "the fixture did not report pointer Y"
pointer_click "$((window_x + probe_x))" "$((window_y + probe_y))" \
    || fail "the native compositor could not inject the first pointer click"
wait_marker_exact left-click || fail "the guest did not receive the native pointer click"
pass native_pointer

refresh_window || fail "could not inspect the native window before the owned modal"
pointer_click "$((window_x + window_width * 56 / 100))" \
    "$((window_y + window_height * 42 / 100))" \
    || fail "the native compositor could not open the guest modal"
wait_marker_exact modal-open || fail "the guest did not receive the modal click"
wait_marker_exact 'modal-kind=ordinary-owned' \
    || fail "the guest modal was not an ordinary owned top-level HWND"
wait_visual_color modal '#7E22CE' 0.01
wtype -k Return || fail "the native compositor could not close the guest modal"
wait_marker_exact modal-close || fail "the guest modal did not close"
pass native_owned_modal

refresh_window || fail "could not inspect the native window before the File menu"
pointer_click "$((window_x + window_width * 4 / 100))" \
    "$((window_y + window_height * 8 / 100))" \
    || fail "the native compositor could not open the File menu"
wait_visual_color file-menu '#EC407A' 0.0005
pointer_click "$((window_x + window_width * 10 / 100))" \
    "$((window_y + window_height * 14 / 100))" \
    || fail "the native compositor could not click the File menu item"
wait_marker_exact file-menu || fail "the guest did not receive the File menu item click"
pass native_file_menu

refresh_window || fail "could not inspect the native window before the context menu"
context_x=$((window_x + window_width * 50 / 100))
context_y=$((window_y + window_height * 75 / 100))
pointer_button "$context_x" "$context_y" button3 \
    || fail "the native compositor could not inject the right click"
wait_marker_exact right-button || fail "the guest did not receive the right click"
wait_visual_color context-menu '#16A34A' 0.0005
pointer_click "$((context_x + 85))" "$((context_y + 15))" \
    || fail "the native compositor could not click the context-menu item"
wait_marker_exact context-menu-item \
    || fail "the guest did not receive the context-menu item click"
pass native_context_menu

refresh_window || fail "could not inspect the native window before middle click"
pointer_button "$((window_x + window_width * 50 / 100))" \
    "$((window_y + window_height * 75 / 100))" button2 \
    || fail "the native compositor could not inject the middle click"
wait_marker_exact middle-button || fail "the guest did not receive the middle click"
pass native_middle_button

wtype -M ctrl c -m ctrl || fail "the native compositor could not inject Ctrl+C"
wait_marker_exact ctrl-c-key-down || fail "the guest did not receive native Ctrl+C"
wtype -M ctrl v -m ctrl || fail "the native compositor could not inject Ctrl+V"
wait_marker_exact ctrl-v-key-down || fail "the guest did not receive native Ctrl+V"
wait_marker_exact ctrl-key-up || fail "the guest did not receive the native Ctrl release"
pass native_keyboard

sway_command 'workspace 2' || fail "could not move focus away from the native window"
wait_marker_exact blur || fail "the guest did not observe native focus loss"
sway_command 'workspace 1' || fail "could not return to the native GUI workspace"
sway_command "[con_id=$window_id] focus" || fail "could not restore native window focus"
wait_marker_exact focus || fail "the guest did not observe native focus recovery"
pass native_focus

refresh_window || fail "could not inspect the native window before caption drag"
caption_y=$(marker_pair_field 'caption-controls=' y) || fail "the fixture did not report caption Y"
move_start_x=$((window_x + window_width / 2))
move_start_y=$((window_y + caption_y))
if ! sway_command "seat seat0 cursor set $move_start_x $move_start_y" \
    || ! sway_command 'seat seat0 cursor press button1'
then
    fail "could not begin the guest-caption move"
fi
sleep 0.3
if ! sway_command "seat seat0 cursor set $((move_start_x + 40)) $((move_start_y - 20))" \
    || ! sway_command 'seat seat0 cursor release button1'
then
    fail "could not complete the guest-caption move"
fi
old_window_x=$window_x
old_window_y=$window_y
sleep 0.5
refresh_window || fail "could not inspect the moved native window"
if [ "$window_x" -eq "$old_window_x" ] && [ "$window_y" -eq "$old_window_y" ]; then
    fail "the guest-caption drag did not move the native Wayland window"
fi
pass guest_caption_move

resize_start_x=$((window_x + window_width - 2))
resize_start_y=$((window_y + window_height - 2))
old_window_width=$window_width
old_window_height=$window_height
if ! sway_command "seat seat0 cursor set $resize_start_x $resize_start_y" \
    || ! sway_command 'seat seat0 cursor press button1'
then
    fail "could not begin the guest-border resize"
fi
sleep 0.3
if ! sway_command "seat seat0 cursor set $((resize_start_x + 60)) $((resize_start_y + 30))" \
    || ! sway_command 'seat seat0 cursor release button1'
then
    fail "could not complete the guest-border resize"
fi
sleep 0.8
refresh_window || fail "could not inspect the resized native window"
if [ "$window_width" -le "$old_window_width" ] || [ "$window_height" -le "$old_window_height" ]; then
    fail "the guest-border drag did not resize the native Wayland window"
fi
capture_window resized
pass guest_border_resize

maximize_offset=$(marker_pair_field 'caption-controls=' maximize) \
    || fail "the fixture did not report its maximize control"
[ "$maximize_offset" -gt 0 ] || fail "the fixture maximize control is unavailable"
normal_count=$(marker_exact_count "$(guest_marker_text)" 'window-state=Normal')
pointer_click "$((window_x + window_width - maximize_offset))" "$((window_y + caption_y))" \
    || fail "could not click the guest maximize control"
wait_marker_exact 'window-state=Maximized' || fail "the guest window did not maximize"
sleep 0.8
refresh_window || fail "could not inspect the maximized native window"
if [ "$window_width" -ne 1280 ] || [ "$window_height" -ne 720 ]; then
    fail "the maximized native window did not fill the compositor output"
fi
capture_window maximized
pass guest_caption_maximize

pointer_click "$((window_x + window_width - maximize_offset))" "$((window_y + caption_y))" \
    || fail "could not click the guest restore control"
restore_deadline=$(( $(date +%s) + 30 ))
restored=0
while [ "$(date +%s)" -lt "$restore_deadline" ]; do
    restore_text=$(guest_marker_text 2>/dev/null || :)
    restore_count=$(marker_exact_count "$restore_text" 'window-state=Normal')
    if [ "$restore_count" -gt "$normal_count" ]; then
        restored=1
        break
    fi
    sleep 0.2
done
[ "$restored" -eq 1 ] || fail "the guest window did not restore"
pass guest_caption_restore

fixture_pid=$(guest_marker_text | awk -F '\t' '$2 ~ /^process-id=/ { print substr($2, 12); exit }')
fixture_hwnd=$(guest_marker_text | awk -F '\t' '$2 ~ /^window-hwnd=/ { print substr($2, 13); exit }')
for fixture_identity in "$fixture_pid" "$fixture_hwnd"; do
    printf '%s\n' "$fixture_identity" | grep -Eq '^[1-9][0-9]*$' \
        || fail "the fixture did not publish its exact process and HWND"
done
kill -KILL "-$presenter_pid" 2>/dev/null || kill -KILL "$presenter_pid" \
    || fail "could not crash the native presenter"
wait "$presenter_pid" 2>/dev/null || :
presenter_pid=
wait_no_window || fail "the crashed native presenter left a Wayland window"
marker_has_exact "$(guest_marker_text)" closed \
    && fail "the presenter crash closed the retained guest HWND"
start_presenter
reattach_pid=$(guest_marker_text | awk -F '\t' '$2 ~ /^process-id=/ { print substr($2, 12); exit }')
reattach_hwnd=$(guest_marker_text | awk -F '\t' '$2 ~ /^window-hwnd=/ { print substr($2, 13); exit }')
if [ "$reattach_pid" != "$fixture_pid" ] || [ "$reattach_hwnd" != "$fixture_hwnd" ]; then
    fail "native presenter recovery replaced the retained guest identity"
fi
pass native_exact_reattach

refresh_window || fail "could not inspect the recovered native window"
sway_command "[con_id=$window_id] kill" || fail "could not request native host-window close"
wait_marker_exact closed || fail "native host close did not close the guest window"
set +e
wait "$presenter_pid"
presenter_status=$?
set -e
presenter_pid=
[ "$presenter_status" -eq 0 ] || fail "the native presenter returned $presenter_status"
wait_no_window || fail "the native Wayland window remained after close"
pass native_host_close

if ! remove_guest_root; then
    fail "the native GUI fixture could not remove its guest artifacts"
fi
pass guest_artifact_cleanup
matrix_complete=1
