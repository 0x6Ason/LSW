#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
lsw=${LSW_E2E_LSW:-"$workspace_root/target/release/lsw"}
lswd=${LSW_E2E_LSWD:-"$workspace_root/target/release/lswd"}
iso=${LSW_WINDOWS_ISO:-}
iso_sha256=${LSW_WINDOWS_ISO_SHA256:-}
agent=${LSW_WINDOWS_AGENT:-"$workspace_root/target/x86_64-pc-windows-gnu/release/lsw-agent.exe"}
edition=${LSW_WINDOWS_EDITION:-pro}
root_base=${LSW_E2E_ROOT_BASE:-/tmp}
artifact_dir=${LSW_E2E_ARTIFACT_DIR:-}
active_root_file=${LSW_PROFILE_COMPARISON_ACTIVE_ROOT_FILE:-}
slim_audit_dir=${LSW_SLIM_PROFILE_AUDIT_DIR:-}
slim_host_bytes=${LSW_SLIM_HOST_ALLOCATED_BYTES:-}
candidate_sha=${LSW_E2E_CANDIDATE_SHA:-}
instance="windows-kvm-profile-vanilla-$$"
daemon_pid=
daemon_pid_file=
keepalive_pid=
instance_removed=0
checker_staged=0
measurement_tmp=
guest_checker='C:\ProgramData\LSW\profile\lsw-e2e-profile-measure.ps1'

for required_command in awk chmod du grep id kill mkdir mktemp mv python3 realpath rm rmdir setsid sha256sum sleep stat timeout; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done
for required_file in "$lsw" "$lswd" "$agent" "$iso"; do
    if [ ! -f "$required_file" ]; then
        echo "error: profile comparison input is missing: $required_file" >&2
        exit 1
    fi
done
if [ ! -x "$lsw" ] || [ ! -x "$lswd" ]; then
    echo "error: profile comparison requires executable lsw and lswd binaries" >&2
    exit 1
fi
case "$iso_sha256" in
    *[!0-9a-f]*|'') echo "error: Windows ISO SHA-256 must be lowercase hexadecimal" >&2; exit 1 ;;
esac
case "$candidate_sha" in
    *[!0-9a-f]*|'') echo "error: candidate SHA must be lowercase hexadecimal" >&2; exit 1 ;;
esac
if [ "${#candidate_sha}" -ne 40 ]; then
    echo "error: candidate SHA must contain exactly 40 hexadecimal characters" >&2
    exit 1
fi
if [ "${#iso_sha256}" -ne 64 ] \
    || [ "$(sha256sum -- "$iso" | awk '{ print $1 }')" != "$iso_sha256" ]; then
    echo "error: profile comparison ISO does not match its exact SHA-256" >&2
    exit 1
fi
case "$slim_host_bytes" in
    ''|*[!0-9]*) echo "error: slim host allocation is not a byte count" >&2; exit 1 ;;
esac
runner_uid=$(id -u)
if [ ! -d "$root_base" ] || [ -L "$root_base" ] \
    || [ "$(stat -c %a -- "$root_base")" != 700 ] \
    || [ "$(stat -c %u -- "$root_base")" != "$runner_uid" ]; then
    echo "error: profile comparison root must be a real mode-0700 directory" >&2
    exit 1
fi
root_base=$(realpath -e -- "$root_base")
if [ ! -d "$artifact_dir" ] || [ -L "$artifact_dir" ] \
    || [ ! -d "$slim_audit_dir" ] || [ -L "$slim_audit_dir" ]; then
    echo "error: profile comparison artifact inputs must be real directories" >&2
    exit 1
fi
artifact_dir=$(realpath -e -- "$artifact_dir")
slim_audit_dir=$(realpath -e -- "$slim_audit_dir")
if [ "$(stat -c %a -- "$artifact_dir")" != 700 ] \
    || [ "$(stat -c %u -- "$artifact_dir")" != "$runner_uid" ] \
    || [ "$(stat -c %a -- "$slim_audit_dir")" != 700 ] \
    || [ "$(stat -c %u -- "$slim_audit_dir")" != "$runner_uid" ]; then
    echo "error: profile comparison artifacts must be private and runner-owned" >&2
    exit 1
fi
case "$active_root_file" in
    /*) ;;
    *) echo "error: profile comparison active-root file must be absolute" >&2; exit 1 ;;
esac
if [ -e "$active_root_file" ] || [ -L "$active_root_file" ]; then
    echo "error: profile comparison active-root file already exists" >&2
    exit 1
fi
active_parent=${active_root_file%/*}
if [ ! -d "$active_parent" ] || [ -L "$active_parent" ]; then
    echo "error: profile comparison active-root parent must be real" >&2
    exit 1
fi
if [ "$(stat -c %a -- "$active_parent")" != 700 ] \
    || [ "$(stat -c %u -- "$active_parent")" != "$runner_uid" ]; then
    echo "error: profile comparison active-root parent must be private and runner-owned" >&2
    exit 1
fi
for output in \
    "$artifact_dir/profile-audits-vanilla" \
    "$artifact_dir/profile-vanilla" \
    "$artifact_dir/profile-comparison.json"
do
    if [ -e "$output" ] || [ -L "$output" ]; then
        echo "error: profile comparison output already exists: $output" >&2
        exit 1
    fi
done

comparison_root=$(mktemp -d -- "$root_base/lsw-profile.XXXXXX")
case "$comparison_root" in
    "$root_base"/lsw-profile.??????) ;;
    *) echo "error: mktemp returned an unexpected profile root" >&2; exit 1 ;;
esac
chmod 700 "$comparison_root"
if ! printf '%s\n' "$comparison_root" >"$active_root_file" \
    || ! chmod 600 "$active_root_file"; then
    rm -f -- "$active_root_file"
    rmdir -- "$comparison_root"
    echo "error: profile comparison could not record its private root" >&2
    exit 1
fi
export LSW_STATE_DIR="$comparison_root/state"
export LSW_WINDOWS_AGENT="$agent"
export LSW_DAEMON="$comparison_root/lswd-autospawn-disabled"
mkdir -p -- "$LSW_STATE_DIR"
chmod 700 "$LSW_STATE_DIR"
daemon_pid_file="$comparison_root/lswd.pid"

terminate_keepalive() {
    case "$keepalive_pid" in
        ''|*[!0-9]*) ;;
        *)
            kill -TERM "-$keepalive_pid" 2>/dev/null || :
            wait "$keepalive_pid" 2>/dev/null || :
            if kill -0 "-$keepalive_pid" 2>/dev/null; then
                echo "error: profile comparison keepalive process group survived cleanup" >&2
                return 1
            fi
            ;;
    esac
    keepalive_pid=
}

terminate_daemon() {
    case "$daemon_pid" in
        ''|*[!0-9]*) rm -f -- "$daemon_pid_file"; return 0 ;;
    esac
    kill -TERM "-$daemon_pid" 2>/dev/null || :
    attempt=0
    while kill -0 "-$daemon_pid" 2>/dev/null && [ "$attempt" -lt 100 ]; do
        attempt=$((attempt + 1))
        sleep 0.1
    done
    if kill -0 "-$daemon_pid" 2>/dev/null; then
        kill -KILL "-$daemon_pid" 2>/dev/null || :
    fi
    wait "$daemon_pid" 2>/dev/null || :
    if kill -0 "-$daemon_pid" 2>/dev/null; then
        echo "error: profile comparison daemon survived cleanup" >&2
        return 1
    fi
    daemon_pid=
    rm -f -- "$daemon_pid_file"
}

cleanup_comparison() {
    status=$?
    trap - EXIT HUP INT TERM
    cleanup_failed=0
    if [ -n "$measurement_tmp" ]; then
        rm -f -- "$measurement_tmp"
        measurement_tmp=
    fi
    if [ "$checker_staged" -eq 1 ] && [ -n "$daemon_pid" ]; then
        "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -NonInteractive \
            -Command "Remove-Item -LiteralPath '$guest_checker' -Force -ErrorAction SilentlyContinue" \
            >/dev/null 2>&1 || :
    fi
    terminate_keepalive || cleanup_failed=1
    if [ "$instance_removed" -ne 1 ] \
        && [ -f "$LSW_STATE_DIR/instances/$instance/instance.lsw" ] \
        && [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
        timeout 120s "$lsw" stop "$instance" --force >/dev/null 2>&1 || cleanup_failed=1
        timeout 60s "$lsw" remove "$instance" >/dev/null 2>&1 || cleanup_failed=1
    fi
    terminate_daemon || cleanup_failed=1
    sh "$workspace_root/scripts/cleanup-windows-e2e-root.sh" \
        "$root_base" "$active_root_file" || cleanup_failed=1
    if [ "$cleanup_failed" -ne 0 ] && [ "$status" -eq 0 ]; then
        status=1
    fi
    exit "$status"
}
trap cleanup_comparison EXIT
trap 'exit 130' HUP INT TERM

LSW_DAEMON_IDLE_SECONDS=3600 setsid "$lswd" >"$comparison_root/lswd.log" 2>&1 &
daemon_pid=$!
printf '%s\n' "$daemon_pid" >"$daemon_pid_file"
chmod 600 "$daemon_pid_file"
daemon_ready=0
attempt=0
while [ "$attempt" -lt 100 ]; do
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        echo "error: profile comparison daemon exited before readiness" >&2
        exit 1
    fi
    if "$lsw" daemon status 2>/dev/null | grep -F 'lswd is ready at ' >/dev/null; then
        daemon_ready=1
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
if [ "$daemon_ready" -ne 1 ]; then
    echo "error: profile comparison daemon did not become ready" >&2
    exit 1
fi
export LSW_COMPARE_DAEMON_PID="$daemon_pid"
export LSW_COMPARE_LSW="$lsw"
# Variables in the helper body intentionally expand in that child shell.
# shellcheck disable=SC2016
setsid sh -c '
    while kill -0 "$LSW_COMPARE_DAEMON_PID" 2>/dev/null; do
        sleep 300
        kill -0 "$LSW_COMPARE_DAEMON_PID" 2>/dev/null || exit 0
        "$LSW_COMPARE_LSW" daemon status >/dev/null
    done
' >"$comparison_root/keepalive.log" 2>&1 &
keepalive_pid=$!

"$lsw" install "$instance" \
    --iso "$iso" \
    --edition "$edition" \
    --profile vanilla \
    --accept-windows-license \
    --defer-user-setup \
    --agent "$agent"

vanilla_audit_dir="$artifact_dir/profile-audits-vanilla"
mkdir -p -- "$vanilla_audit_dir"
chmod 700 "$vanilla_audit_dir"
measure_profile() {
    phase=$1
    destination="$vanilla_audit_dir/$phase.json"
    temporary="$destination.tmp.$$"
    measurement_tmp=$temporary
    "$lsw" push "$instance" "$workspace_root/scripts/measure-windows-profile.ps1" \
        "$guest_checker" >/dev/null
    checker_staged=1
    set +e
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -NonInteractive \
        -ExecutionPolicy Bypass -File "$guest_checker" -Profile vanilla -SettleSeconds 30 \
        >"$temporary"
    measure_status=$?
    set -e
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -NonInteractive \
        -Command "Remove-Item -LiteralPath '$guest_checker' -Force -ErrorAction Stop" >/dev/null
    checker_staged=0
    if [ "$measure_status" -ne 0 ] || [ ! -s "$temporary" ] \
        || [ "$(stat -c %s -- "$temporary")" -gt 8388608 ]; then
        echo "error: vanilla profile measurement failed during $phase" >&2
        exit 1
    fi
    chmod 600 "$temporary"
    mv -- "$temporary" "$destination"
    measurement_tmp=
}

measure_profile boot-1
for phase in boot-2 boot-3; do
    timeout 600s "$lsw" shutdown "$instance"
    marker="LSW_PROFILE_${phase}_$$"
    output=$(timeout 660s "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile \
        -Command "[Console]::Out.Write('$marker')")
    if [ "$output" != "$marker" ]; then
        echo "error: vanilla profile did not cold-start for $phase" >&2
        exit 1
    fi
    measure_profile "$phase"
done

"$lsw" trim "$instance" >/dev/null
vanilla_host_bytes=$(du -B1 "$LSW_STATE_DIR/instances/$instance/disk.qcow2" |
    awk '{ print $1; exit }')
case "$vanilla_host_bytes" in
    ''|*[!0-9]*) echo "error: vanilla host allocation is not numeric" >&2; exit 1 ;;
esac
"$lsw" pull "$instance" --recursive 'C:\ProgramData\LSW\profile' \
    "$artifact_dir/profile-vanilla" >/dev/null
chmod -R go-rwx "$artifact_dir/profile-vanilla"
timeout 600s "$lsw" shutdown "$instance"
"$lsw" compact "$instance" >/dev/null
"$lsw" remove "$instance"
instance_removed=1

python3 "$workspace_root/scripts/compare-windows-profile-audits.py" \
    --slim-dir "$slim_audit_dir" \
    --vanilla-dir "$vanilla_audit_dir" \
    --candidate-sha "$candidate_sha" \
    --iso-sha256 "$iso_sha256" \
    --slim-host-bytes "$slim_host_bytes" \
    --vanilla-host-bytes "$vanilla_host_bytes" \
    --output "$artifact_dir/profile-comparison.json"

terminate_keepalive
terminate_daemon
sh "$workspace_root/scripts/cleanup-windows-e2e-root.sh" \
    "$root_base" "$active_root_file"
trap - EXIT HUP INT TERM
echo "Same-ISO vanilla versus slim profile comparison passed and removed its test instance."
