#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
lsw=${LSW_E2E_LSW:-"$workspace_root/target/release/lsw"}
lswd=${LSW_E2E_LSWD:-"$workspace_root/target/release/lswd"}
iso=${LSW_WINDOWS_ISO:-}
edition=${LSW_WINDOWS_EDITION:-pro}
agent=${LSW_WINDOWS_AGENT:-"$workspace_root/target/x86_64-pc-windows-gnu/release/lsw-agent.exe"}
timeout_seconds=${LSW_E2E_TIMEOUT_SECONDS:-2700}

for required_command in awk date grep kill mktemp python3 rm sleep; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done
if [ ! -c /dev/kvm ] || [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
    echo "error: the real Windows gate requires readable and writable /dev/kvm" >&2
    exit 1
fi
if [ -z "$iso" ] || [ ! -f "$iso" ]; then
    echo "error: set LSW_WINDOWS_ISO to a licensed Windows 11 x64 ISO" >&2
    exit 1
fi
if [ ! -f "$agent" ]; then
    echo "error: set LSW_WINDOWS_AGENT to the matching lsw-agent.exe" >&2
    exit 1
fi
if [ ! -x "$lsw" ] || [ ! -x "$lswd" ]; then
    echo "error: build release lsw and lswd binaries before running this gate" >&2
    exit 1
fi
case "$timeout_seconds" in
    ''|*[!0-9]*)
        echo "error: LSW_E2E_TIMEOUT_SECONDS must be a positive integer" >&2
        exit 1
        ;;
esac
if [ "$timeout_seconds" -eq 0 ]; then
    echo "error: LSW_E2E_TIMEOUT_SECONDS must be greater than zero" >&2
    exit 1
fi

e2e_root=$(mktemp -d -- "${TMPDIR:-/tmp}/lsw-windows-kvm-e2e.XXXXXX")
instance="windows-kvm-e2e-$$"
export LSW_STATE_DIR="$e2e_root/state"
export LSW_DAEMON="$lswd"
export LSW_WINDOWS_AGENT="$agent"

cleanup_e2e() {
    status=$?
    trap - EXIT HUP INT TERM
    "$lsw" stop "$instance" --force >/dev/null 2>&1 || true
    if [ "$status" -eq 0 ] && [ "${LSW_E2E_KEEP_STATE:-0}" != 1 ]; then
        rm -rf -- "$e2e_root"
    else
        echo "LSW Windows/KVM E2E state retained at $e2e_root" >&2
    fi
    exit "$status"
}
trap cleanup_e2e EXIT HUP INT TERM

"$lsw" doctor
viewer_option=
if [ "${LSW_E2E_NO_VIEWER:-0}" = 1 ]; then
    viewer_option=--no-viewer
fi

# shellcheck disable=SC2086
"$lsw" install "$instance" \
    --iso "$iso" \
    --edition "$edition" \
    --profile standard \
    --agent "$agent" \
    $viewer_option

echo "Complete normal Windows OOBE and the first administrative login in the LSW viewer."
echo "The gate will continue automatically when the guest agent becomes ready."

deadline=$(( $(date +%s) + timeout_seconds ))
agent_ready=0
while [ "$(date +%s)" -lt "$deadline" ]; do
    if "$lsw" status "$instance" 2>/dev/null | grep -Fx 'AGENT=ready' >/dev/null; then
        agent_ready=1
        break
    fi
    sleep 5
done
if [ "$agent_ready" -ne 1 ]; then
    echo "error: Windows guest agent did not become ready before the E2E timeout" >&2
    exit 1
fi

marker="LSW_WINDOWS_KVM_E2E_OK_$$"
output=$("$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
    "Write-Output '$marker'")
printf '%s\n' "$output" | grep -Fx "$marker" >/dev/null

set +e
"$lsw" exec "$instance" -- cmd.exe /d /c exit 37
guest_status=$?
set -e
if [ "$guest_status" -ne 37 ]; then
    echo "error: guest exit code 37 became host exit code $guest_status" >&2
    exit 1
fi

pid_file="$LSW_STATE_DIR/instances/$instance/run/qemu.pid"
qemu_pid=
agent_port=$(
    "$lsw" show "$instance" |
        awk -F: '/^agent host port:/ { value=$2; gsub(/[[:space:]]/, "", value); print value }'
)
if [ -f "$pid_file" ]; then
    qemu_pid=$(awk 'NR == 1 { print $1 }' "$pid_file")
fi
"$lsw" shutdown "$instance"

deadline=$(( $(date +%s) + 300 ))
stopped=0
while [ "$(date +%s)" -lt "$deadline" ]; do
    if "$lsw" status "$instance" 2>/dev/null | grep -Fx 'STATE=stopped' >/dev/null; then
        stopped=1
        break
    fi
    sleep 2
done
if [ "$stopped" -ne 1 ]; then
    echo "error: Windows did not complete graceful shutdown within five minutes" >&2
    exit 1
fi
case "$qemu_pid" in
    ''|*[!0-9]*) ;;
    *)
        if kill -0 "$qemu_pid" 2>/dev/null; then
            echo "error: QEMU process $qemu_pid survived guest shutdown" >&2
            exit 1
        fi
        ;;
esac

for stale in qmp.sock swtpm.sock recovery-vnc.sock qemu.pid; do
    if [ -e "$LSW_STATE_DIR/instances/$instance/run/$stale" ]; then
        echo "error: stale runtime artifact remained after shutdown: $stale" >&2
        exit 1
    fi
done

python3 - "$agent_port" <<'PY'
import socket
import sys

port = int(sys.argv[1])
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    listener.bind(("127.0.0.1", port))
PY

"$lsw" diagnose "$instance" --bundle --output "$e2e_root/diagnose.tar.gz"
"$lsw" remove "$instance"
if [ -e "$LSW_STATE_DIR/instances/$instance" ]; then
    echo "error: instance directory remained after lsw remove" >&2
    exit 1
fi

echo "Windows/KVM E2E passed: Setup -> OOBE -> agent -> command -> exit code -> shutdown -> cleanup."
