#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: cleanup-windows-e2e-root.sh ROOT_BASE ACTIVE_ROOT_FILE" >&2
    exit 2
fi

root_base=$1
active_root_file=$2
for required_command in awk chmod find grep id kill realpath rm stat; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done
if [ ! -e "$active_root_file" ]; then
    exit 0
fi
if [ -L "$active_root_file" ] || [ ! -f "$active_root_file" ] \
    || [ "$(stat -c %a -- "$active_root_file")" != 600 ] \
    || [ "$(stat -c %s -- "$active_root_file")" -eq 0 ] \
    || [ "$(stat -c %s -- "$active_root_file")" -gt 4096 ] \
    || [ "$(awk 'END { print NR }' "$active_root_file")" != 1 ]; then
    echo "error: refusing an invalid active E2E root record: $active_root_file" >&2
    exit 1
fi
if [ ! -d "$root_base" ] || [ -L "$root_base" ]; then
    echo "error: E2E root base must be a real directory" >&2
    exit 1
fi
canonical_base=$(realpath -e -- "$root_base")
runner_uid=$(id -u)
if [ "$(stat -c %u -- "$canonical_base")" != "$runner_uid" ] \
    || [ "$(stat -c %a -- "$canonical_base")" != 700 ] \
    || [ "$(stat -c %u -- "$active_root_file")" != "$runner_uid" ]; then
    echo "error: E2E cleanup inputs must be private and owned by the runner account" >&2
    exit 1
fi
recorded_root=$(awk 'NR == 1 { print; exit }' "$active_root_file")
recorded_parent=${recorded_root%/*}
recorded_name=${recorded_root##*/}
if [ "$recorded_parent" != "$canonical_base" ]; then
    echo "error: refusing an E2E path outside the configured root: $recorded_root" >&2
    exit 1
fi
case "$recorded_name" in
    lsw-e2e.*) recorded_suffix=${recorded_name#lsw-e2e.} ;;
    lsw-profile.*) recorded_suffix=${recorded_name#lsw-profile.} ;;
    *) echo "error: refusing an unexpected E2E directory name: $recorded_name" >&2; exit 1 ;;
esac
if ! printf '%s\n' "$recorded_suffix" | grep -Eq '^[[:alnum:]]{6}$'; then
    echo "error: refusing an unexpected E2E directory name: $recorded_name" >&2
    exit 1
fi
if [ -L "$recorded_root" ] || { [ -e "$recorded_root" ] && [ ! -d "$recorded_root" ]; }; then
    echo "error: refusing a non-directory E2E path: $recorded_root" >&2
    exit 1
fi
if [ -d "$recorded_root" ]; then
    if [ "$(stat -c %u -- "$recorded_root")" != "$runner_uid" ] \
        || [ "$(stat -c %a -- "$recorded_root")" != 700 ]; then
        echo "error: refusing a non-private or foreign-owned E2E directory: $recorded_root" >&2
        exit 1
    fi
    daemon_pid_file="$recorded_root/lswd.pid"
    if [ -e "$daemon_pid_file" ] || [ -L "$daemon_pid_file" ]; then
        if [ -L "$daemon_pid_file" ] || [ ! -f "$daemon_pid_file" ] \
            || [ "$(stat -c %a -- "$daemon_pid_file")" != 600 ] \
            || [ "$(stat -c %u -- "$daemon_pid_file")" != "$runner_uid" ] \
            || [ "$(stat -c %s -- "$daemon_pid_file")" -eq 0 ] \
            || [ "$(stat -c %s -- "$daemon_pid_file")" -gt 32 ]; then
            echo "error: refusing an invalid tracked daemon PID file" >&2
            exit 1
        fi
        daemon_pid=$(awk 'NR == 1 { print $1; exit }' "$daemon_pid_file")
        case "$daemon_pid" in
            ''|*[!0-9]*)
                echo "error: refusing an invalid tracked daemon PID" >&2
                exit 1
                ;;
            *)
                if [ "$daemon_pid" -le 1 ]; then
                    echo "error: refusing an invalid tracked daemon PID" >&2
                    exit 1
                fi
                if kill -0 "$daemon_pid" 2>/dev/null; then
                    echo "error: refusing to delete state while test daemon $daemon_pid is alive" >&2
                    exit 1
                fi
                ;;
        esac
    fi
    while IFS= read -r qemu_pid_file; do
        if [ -z "$qemu_pid_file" ]; then
            continue
        fi
        if [ -L "$qemu_pid_file" ] || [ ! -f "$qemu_pid_file" ] \
            || [ "$(stat -c %u -- "$qemu_pid_file")" != "$runner_uid" ] \
            || [ "$(stat -c %s -- "$qemu_pid_file")" -eq 0 ] \
            || [ "$(stat -c %s -- "$qemu_pid_file")" -gt 32 ]; then
            echo "error: refusing an invalid tracked QEMU PID file" >&2
            exit 1
        fi
        qemu_pid=$(awk 'NR == 1 { print $1; exit }' "$qemu_pid_file")
        case "$qemu_pid" in
            ''|*[!0-9]*)
                echo "error: refusing an invalid tracked QEMU PID" >&2
                exit 1
                ;;
            *)
                if [ "$qemu_pid" -le 1 ]; then
                    echo "error: refusing an invalid tracked QEMU PID" >&2
                    exit 1
                fi
                if kill -0 "$qemu_pid" 2>/dev/null; then
                    echo "error: refusing to delete state while test QEMU process $qemu_pid is alive" >&2
                    exit 1
                fi
                ;;
        esac
    done <<EOF
$(find "$recorded_root/state/instances" -mindepth 3 -maxdepth 3 \
    -path '*/run/qemu.pid' -print 2>/dev/null || true)
EOF
    chmod -R u+rwX -- "$recorded_root" || true
    find "$recorded_root" -depth -delete
fi
rm -f -- "$active_root_file"
