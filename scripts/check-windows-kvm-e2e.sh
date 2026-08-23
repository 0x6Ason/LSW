#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
lsw=${LSW_E2E_LSW:-"$workspace_root/target/release/lsw"}
lswd=${LSW_E2E_LSWD:-"$workspace_root/target/release/lswd"}
iso=${LSW_WINDOWS_ISO:-}
edition=${LSW_WINDOWS_EDITION:-pro}
profile=${LSW_WINDOWS_PROFILE:-slim}
agent=${LSW_WINDOWS_AGENT:-"$workspace_root/target/x86_64-pc-windows-gnu/release/lsw-agent.exe"}
timeout_seconds=${LSW_E2E_TIMEOUT_SECONDS:-2700}
root_base=${LSW_E2E_ROOT_BASE:-/tmp}
artifact_dir=${LSW_E2E_ARTIFACT_DIR:-}
keep_state=${LSW_E2E_KEEP_STATE:-0}
expected_iso_sha256=${LSW_WINDOWS_ISO_SHA256:-}
e2e_no_viewer=${LSW_E2E_NO_VIEWER:-1}

for required_command in awk chmod date grep kill mkdir mktemp mv python3 rm setsid sha256sum sleep timeout tr uname; do
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
case "$keep_state" in
    0|1) ;;
    *)
        echo "error: LSW_E2E_KEEP_STATE must be 0 or 1" >&2
        exit 1
        ;;
esac
case "$e2e_no_viewer" in
    0|1) ;;
    *)
        echo "error: LSW_E2E_NO_VIEWER must be 0 or 1" >&2
        exit 1
        ;;
esac
if [ ! -d "$root_base" ] || [ -L "$root_base" ]; then
    echo "error: LSW_E2E_ROOT_BASE must be an existing real directory" >&2
    exit 1
fi
root_base=$(CDPATH='' cd -- "$root_base" && pwd)
if [ "$root_base" = / ]; then
    echo "error: LSW_E2E_ROOT_BASE must not be the filesystem root" >&2
    exit 1
fi
if [ -n "$artifact_dir" ]; then
    if [ -e "$artifact_dir" ] && { [ ! -d "$artifact_dir" ] || [ -L "$artifact_dir" ]; }; then
        echo "error: LSW_E2E_ARTIFACT_DIR must be a real directory" >&2
        exit 1
    fi
    mkdir -p -- "$artifact_dir"
    chmod 700 "$artifact_dir"
    artifact_dir=$(CDPATH='' cd -- "$artifact_dir" && pwd)
fi

expected_iso_sha256=$(printf '%s' "$expected_iso_sha256" | tr 'A-F' 'a-f')
if [ -n "$expected_iso_sha256" ]; then
    case "$expected_iso_sha256" in
        *[!0-9a-f]*)
            echo "error: LSW_WINDOWS_ISO_SHA256 must contain 64 hexadecimal characters" >&2
            exit 1
            ;;
    esac
    if [ "${#expected_iso_sha256}" -ne 64 ]; then
        echo "error: LSW_WINDOWS_ISO_SHA256 must contain 64 hexadecimal characters" >&2
        exit 1
    fi
fi
# The inner process owns exact process groups and cleanup. Leave additional
# time for dependency checks and bounded teardown around the OOBE deadline.
if [ "${LSW_WINDOWS_KVM_E2E_TIMEOUT_ACTIVE:-0}" != 1 ]; then
    LSW_WINDOWS_KVM_E2E_TIMEOUT_ACTIVE=1
    export LSW_WINDOWS_KVM_E2E_TIMEOUT_ACTIVE
    overall_timeout=$((timeout_seconds + 1200))
    # Cleanup can include a bounded guest stop, evidence collection, and removal
    # of a large sparse disk tree. Keep five minutes between TERM and KILL while
    # still finishing well inside the workflow's 90-minute job timeout.
    exec timeout --signal=TERM --kill-after=300s "${overall_timeout}s" "$0" "$@"
fi

iso_sha256=$(sha256sum "$iso" | awk '{ print $1 }')
if [ -n "$expected_iso_sha256" ] && [ "$iso_sha256" != "$expected_iso_sha256" ]; then
    echo "error: Windows ISO SHA-256 does not match LSW_WINDOWS_ISO_SHA256" >&2
    exit 1
fi

e2e_root=$(mktemp -d -- "$root_base/lsw-e2e.XXXXXX")
case "$e2e_root" in
    "$root_base"/lsw-e2e.??????) ;;
    *)
        echo "error: mktemp returned an unexpected E2E directory" >&2
        exit 1
        ;;
esac
instance="windows-kvm-e2e-$$"
export LSW_STATE_DIR="$e2e_root/state"
export LSW_WINDOWS_AGENT="$agent"
export LSW_E2E_LSW="$lsw"
export LSW_E2E_INSTANCE="$instance"
autospawn_blocker="$e2e_root/lswd-autospawn-disabled"
cold_daemon_pid_file="$e2e_root/cold-daemon.pid"
export LSW_DAEMON="$autospawn_blocker"
daemon_pid=
viewer_pid=
artifacts_collected=0
guest_build=unknown
guest_edition=unknown
setup_account_removed=unknown
agent_service_sid=unknown
cold_agent_service_sid=unknown
agent_service_pid=unknown
cold_agent_service_pid=unknown
agent_service_name=unknown
agent_service_start_mode=unknown
agent_service_start_name=unknown
agent_service_state=unknown
cold_interactive_user=unknown
initial_interactive_user=unknown
cached_unattend_removed=unknown
setup_payload_removed=unknown
automatic_logon=unknown
ovmf_code_sha256=unknown
ovmf_vars_sha256=unknown
official_iso_sha256=unknown
license_status=unknown
license_helper_start_mode=unknown
license_helper_start_name=unknown

python3 - "$LSW_STATE_DIR/instances/$instance/run/recovery-vnc.sock" <<'PY'
import os
import sys

path = os.fsencode(sys.argv[1])
if len(path) > 100:
    raise SystemExit(
        f"error: E2E runtime socket path is {len(path)} bytes; "
        "configure a shorter LSW_E2E_ROOT_BASE"
    )
PY

run_conpty_probe() {
    probe_timeout=$1
    probe_marker=$2
    probe_command=$3
    shift 3
    python3 - "$probe_timeout" "$probe_marker" "$probe_command" "$@" <<'PY'
import errno
import fcntl
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time

timeout_seconds = int(sys.argv[1])
marker = os.fsencode(sys.argv[2])
command = os.fsencode(sys.argv[3])
argv = sys.argv[4:]
if not argv:
    raise SystemExit("error: ConPTY probe received no command")

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
process = subprocess.Popen(
    argv,
    stdin=slave,
    stdout=slave,
    stderr=slave,
    close_fds=True,
    start_new_session=True,
)
os.close(slave)
deadline = time.monotonic() + timeout_seconds
transcript = bytearray()
command_sent = False
exit_sent = False
timed_out = False

while True:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        timed_out = True
        break
    ready, _, _ = select.select([master], [], [], min(0.1, remaining))
    if ready:
        try:
            data = os.read(master, 32768)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            data = b""
        if data:
            os.write(sys.stdout.fileno(), data)
            transcript.extend(data)
            if len(transcript) > 1024 * 1024:
                del transcript[: len(transcript) - 1024 * 1024]
            prompt = transcript.find(b"PS C:\\")
            if not command_sent and prompt >= 0 and b">" in transcript[prompt:]:
                os.write(master, command + b"\r")
                command_sent = True
            if command_sent and not exit_sent and marker in transcript:
                os.write(master, b"exit\r")
                exit_sent = True
        elif process.poll() is not None:
            break
    elif process.poll() is not None:
        break

if timed_out:
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    raise SystemExit("error: timed out waiting for the ConPTY probe")

status = process.wait()
if not command_sent:
    raise SystemExit("error: ConPTY shell never produced a PowerShell prompt")
if marker not in transcript:
    raise SystemExit("error: ConPTY shell exited before returning its marker")
if status < 0 or status > 255:
    raise SystemExit(f"error: ConPTY probe returned unsupported status {status}")
raise SystemExit(status)
PY
}

collect_e2e_artifacts() {
    result=$1
    if [ -z "$artifact_dir" ]; then
        return 0
    fi

    metadata="$artifact_dir/attestation.env"
    metadata_tmp="$metadata.tmp-$$"
    {
        printf 'result=%s\n' "$result"
        printf 'commit=%s\n' "${GITHUB_SHA:-unknown}"
        printf 'edition_requested=%s\n' "$edition"
        printf 'edition_installed=%s\n' "$guest_edition"
        printf 'profile_requested=%s\n' "$profile"
        printf 'windows_build=%s\n' "$guest_build"
        printf 'setup_account_removed=%s\n' "$setup_account_removed"
        printf 'agent_service_name=%s\n' "$agent_service_name"
        printf 'agent_service_start_mode=%s\n' "$agent_service_start_mode"
        printf 'agent_service_start_name=%s\n' "$agent_service_start_name"
        printf 'agent_service_state=%s\n' "$agent_service_state"
        printf 'agent_service_sid_initial=%s\n' "$agent_service_sid"
        printf 'agent_service_sid_cold=%s\n' "$cold_agent_service_sid"
        printf 'agent_service_pid_initial=%s\n' "$agent_service_pid"
        printf 'agent_service_pid_cold=%s\n' "$cold_agent_service_pid"
        printf 'cold_interactive_user=%s\n' "$cold_interactive_user"
        printf 'initial_interactive_user=%s\n' "$initial_interactive_user"
        printf 'cached_unattend_removed=%s\n' "$cached_unattend_removed"
        printf 'setup_payload_removed=%s\n' "$setup_payload_removed"
        printf 'automatic_logon=%s\n' "$automatic_logon"
        printf 'iso_sha256=%s\n' "$iso_sha256"
        printf 'official_iso_sha256=%s\n' "$official_iso_sha256"
        printf 'license_status=%s\n' "$license_status"
        printf 'license_helper_start_mode=%s\n' "$license_helper_start_mode"
        printf 'license_helper_start_name=%s\n' "$license_helper_start_name"
        printf 'lsw_sha256=%s\n' "$(sha256sum "$lsw" | awk '{ print $1 }')"
        printf 'lswd_sha256=%s\n' "$(sha256sum "$lswd" | awk '{ print $1 }')"
        printf 'agent_sha256=%s\n' "$(sha256sum "$agent" | awk '{ print $1 }')"
        printf 'ovmf_code_sha256=%s\n' "$ovmf_code_sha256"
        printf 'ovmf_vars_sha256=%s\n' "$ovmf_vars_sha256"
        printf 'kernel=%s\n' "$(uname -srmo)"
        printf 'qemu=%s\n' "$(qemu-system-x86_64 --version 2>/dev/null | awk 'NR == 1 { print; exit }')"
        printf 'swtpm=%s\n' "$(swtpm --version 2>/dev/null | awk 'NR == 1 { print; exit }')"
    } >"$metadata_tmp"
    chmod 600 "$metadata_tmp"
    mv "$metadata_tmp" "$metadata"

    if [ -f "$LSW_STATE_DIR/instances/$instance/instance.lsw" ]; then
        if [ ! -e "$artifact_dir/bench.json" ]; then
            timeout 30s "$lsw" bench "$instance" --json \
                >"$artifact_dir/bench.json" 2>/dev/null || :
            if [ -e "$artifact_dir/bench.json" ]; then
                chmod 600 "$artifact_dir/bench.json"
            fi
        fi
        if [ ! -e "$artifact_dir/diagnose.tar.gz" ]; then
            timeout 30s "$lsw" diagnose "$instance" --bundle \
                --output "$artifact_dir/diagnose.tar.gz" >/dev/null 2>&1 || :
        fi
    fi
    artifacts_collected=1
}

terminate_viewer() {
    case "$viewer_pid" in
        ''|*[!0-9]*) ;;
        *)
            kill -TERM "-$viewer_pid" 2>/dev/null || :
            viewer_attempt=0
            while kill -0 "-$viewer_pid" 2>/dev/null \
                && [ "$viewer_attempt" -lt 50 ]
            do
                viewer_attempt=$((viewer_attempt + 1))
                sleep 0.1
            done
            if kill -0 "-$viewer_pid" 2>/dev/null; then
                kill -KILL "-$viewer_pid" 2>/dev/null || :
                viewer_attempt=0
                while kill -0 "-$viewer_pid" 2>/dev/null \
                    && [ "$viewer_attempt" -lt 20 ]
                do
                    viewer_attempt=$((viewer_attempt + 1))
                    sleep 0.1
                done
            fi
            if kill -0 "-$viewer_pid" 2>/dev/null; then
                echo "error: installation viewer process group $viewer_pid survived cleanup" >&2
                return 1
            fi
            ;;
    esac
    viewer_pid=
}

adopt_cold_daemon_if_present() {
    if [ -n "$daemon_pid" ] || [ ! -f "$cold_daemon_pid_file" ]; then
        return 0
    fi
    candidate_daemon_pid=$(awk 'NR == 1 { print $1 }' "$cold_daemon_pid_file")
    case "$candidate_daemon_pid" in
        ''|*[!0-9]*)
            echo "error: bare lsw recorded an invalid daemon PID" >&2
            return 1
            ;;
    esac
    daemon_pid=$candidate_daemon_pid
    rm -f -- "$cold_daemon_pid_file"
}

assert_daemon_alive() {
    case "$daemon_pid" in
        ''|*[!0-9]*)
            echo "error: the tracked lswd PID is unavailable" >&2
            return 1
            ;;
    esac
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        echo "error: tracked lswd process $daemon_pid exited during the gate" >&2
        return 1
    fi
}

terminate_daemon() {
    case "$daemon_pid" in
        ''|*[!0-9]*)
            daemon_pid=
            return 0
            ;;
    esac

    if ! kill -TERM "-$daemon_pid" 2>/dev/null; then
        kill -TERM "$daemon_pid" 2>/dev/null || :
    fi
    daemon_attempt=0
    while kill -0 "-$daemon_pid" 2>/dev/null \
        && [ "$daemon_attempt" -lt 100 ]
    do
        if [ -r "/proc/$daemon_pid/stat" ] \
            && [ "$(awk '{ print $3 }' "/proc/$daemon_pid/stat")" = Z ]
        then
            wait "$daemon_pid" 2>/dev/null || :
        fi
        daemon_attempt=$((daemon_attempt + 1))
        sleep 0.1
    done
    if kill -0 "-$daemon_pid" 2>/dev/null; then
        kill -KILL "-$daemon_pid" 2>/dev/null || :
        daemon_attempt=0
        while kill -0 "-$daemon_pid" 2>/dev/null \
            && [ "$daemon_attempt" -lt 20 ]
        do
            if [ -r "/proc/$daemon_pid/stat" ] \
                && [ "$(awk '{ print $3 }' "/proc/$daemon_pid/stat")" = Z ]
            then
                wait "$daemon_pid" 2>/dev/null || :
            fi
            daemon_attempt=$((daemon_attempt + 1))
            sleep 0.1
        done
    fi
    if kill -0 "-$daemon_pid" 2>/dev/null; then
        echo "error: lswd process group $daemon_pid survived cleanup" >&2
        return 1
    fi
    wait "$daemon_pid" 2>/dev/null || :
    daemon_pid=
}

assert_stopped_runtime_released() {
    stopped_qemu_pid=$1
    stopped_agent_port=$2

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
    case "$stopped_qemu_pid" in
        ''|*[!0-9]*) ;;
        *)
            if kill -0 "$stopped_qemu_pid" 2>/dev/null; then
                echo "error: QEMU process $stopped_qemu_pid survived guest shutdown" >&2
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

    python3 - "$stopped_agent_port" <<'PY'
import socket
import sys

port = int(sys.argv[1])
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    # A completed agent connection can leave the old host-forward endpoint in
    # TIME_WAIT after QEMU exits. Match QEMU's reusable-listener semantics while
    # still rejecting a port held by a live runtime.
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", port))
PY
}

query_agent_service() {
    # Every command in this gate is expected to inherit the auto-start
    # service's virtual-account token. Query the registered service as well as
    # its actual process owner so a same-name process cannot satisfy the gate.
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$Service=Get-CimInstance -ClassName Win32_Service | Where-Object { $_.Name -ceq "LSWAgent" }; if ($null -eq $Service -or @($Service).Count -ne 1) { exit 51 }; if ($Service.Name -cne "LSWAgent") { exit 52 }; if ($Service.StartMode -ne "Auto") { exit 53 }; if ($Service.State -ne "Running") { exit 54 }; if ($Service.StartName -ine "NT SERVICE\LSWAgent") { exit 55 }; if ([uint32]$Service.ProcessId -eq 0) { exit 56 }; $Process=Get-CimInstance -ClassName Win32_Process | Where-Object { $_.ProcessId -eq [uint32]$Service.ProcessId }; if ($null -eq $Process -or @($Process).Count -ne 1) { exit 57 }; $Owner=Invoke-CimMethod -InputObject $Process -MethodName GetOwnerSid; if ($Owner.ReturnValue -ne 0 -or [string]::IsNullOrWhiteSpace([string]$Owner.Sid)) { exit 58 }; $Expected=([System.Security.Principal.NTAccount]::new("NT SERVICE","LSWAgent")).Translate([System.Security.Principal.SecurityIdentifier]).Value; $Current=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value; if ($Owner.Sid -ne $Expected -or $Current -ne $Expected) { exit 59 }; $CurrentProcess=[System.Diagnostics.Process]::GetCurrentProcess(); if ([uint32]$Process.SessionId -ne 0 -or [uint32]$CurrentProcess.SessionId -ne 0) { exit 60 }; [Console]::Out.Write("LSW_WINDOWS_KVM_SERVICE_OK|$Expected|$($Service.ProcessId)")'
}

read_agent_service_identity() {
    service_output=$(query_agent_service)
    service_marker=$(printf '%s\n' "$service_output" | awk -F'|' '{ print $1 }')
    service_sid=$(printf '%s\n' "$service_output" | awk -F'|' '{ print $2 }')
    service_pid=$(printf '%s\n' "$service_output" | awk -F'|' '{ print $3 }')
    service_field_count=$(printf '%s\n' "$service_output" | awk -F'|' '{ print NF }')
    if [ "$service_marker" != LSW_WINDOWS_KVM_SERVICE_OK ] \
        || [ "$service_field_count" -ne 3 ] \
        || ! printf '%s\n' "$service_sid" | grep -E '^S-1-5-80-[0-9]+(-[0-9]+)+$' >/dev/null
    then
        echo "error: LSWAgent did not report a valid virtual-account process SID" >&2
        return 1
    fi
    case "$service_pid" in
        ''|*[!0-9]*)
            echo "error: LSWAgent did not report a numeric service process ID" >&2
            return 1
            ;;
    esac
    if [ "$service_pid" -eq 0 ]; then
        echo "error: LSWAgent reported service process ID zero" >&2
        return 1
    fi
}

cleanup_e2e() {
    status=$?
    trap - EXIT HUP INT TERM
    cleanup_failed=0

    if ! adopt_cold_daemon_if_present; then
        cleanup_failed=1
    fi

    if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
        timeout 30s "$lsw" stop "$instance" --force >/dev/null 2>&1 || :
    fi

    if ! terminate_viewer; then
        cleanup_failed=1
    fi
    if ! terminate_daemon; then
        cleanup_failed=1
    fi

    if [ "$artifacts_collected" -ne 1 ]; then
        collect_e2e_artifacts "$status" || :
    fi

    if [ "$cleanup_failed" -ne 0 ] && [ "$status" -eq 0 ]; then
        status=1
    fi
    if [ "$keep_state" = 1 ] && [ "$status" -ne 0 ]; then
        echo "LSW Windows/KVM E2E state retained by explicit request at $e2e_root" >&2
    else
        rm -rf -- "$e2e_root"
    fi
    exit "$status"
}
trap cleanup_e2e EXIT
trap 'exit 130' HUP INT TERM

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
setsid "$lswd" >"$e2e_root/lswd.log" 2>&1 &
daemon_pid=$!
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
        --agent "$agent"
else
    "$lsw" install "$instance" \
        --iso "$iso" \
        --edition "$edition" \
        --profile "$profile" \
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
    '$ErrorActionPreference="Stop"; $Marker="C:\ProgramData\LSW\setup-complete.marker"; if (-not (Test-Path -LiteralPath $Marker -PathType Leaf) -or [System.IO.File]::ReadAllText($Marker).Trim() -cne "LSW-SETUP-COMPLETE") { exit 50 }; if ($null -ne (Get-LocalUser -Name "LSWSetup" -ErrorAction SilentlyContinue)) { exit 51 }; $Interactive=[string](Get-CimInstance -ClassName Win32_ComputerSystem).UserName; if (-not [string]::IsNullOrWhiteSpace($Interactive)) { exit 52 }; $Winlogon=Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon"; if ([string]$Winlogon.AutoAdminLogon -eq "1") { exit 53 }; $StoredPassword=$Winlogon.PSObject.Properties["DefaultPassword"]; if ($null -ne $StoredPassword -and -not [string]::IsNullOrEmpty([string]$StoredPassword.Value)) { exit 54 }; foreach ($Path in @("C:\Windows\Panther\unattend.xml", "C:\Windows\Panther\Unattend\unattend.xml", "C:\Windows\Setup\Scripts\SetupComplete.cmd", "C:\ProgramData\LSW\setup")) { if (Test-Path -LiteralPath $Path) { exit 55 } }; [Console]::Out.Write("LSW_WINDOWS_KVM_HEADLESS_SETUP_COMPLETE")')
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
if [ -n "$artifact_dir" ]; then
    "$lsw" bench "$instance" --json >"$artifact_dir/bench.json"
    chmod 600 "$artifact_dir/bench.json"
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

# A daily-use instance has to cold-start from its disk and make the agent and
# daemon available through a bare `lsw` invocation without a console sign-in
# or installation media.
terminate_viewer
terminate_daemon
cold_daemon_wrapper="$e2e_root/cold-daemon-wrapper.sh"
cold_daemon_session="$e2e_root/cold-daemon-session.sh"
# The variables are intentionally expanded later by the generated wrapper.
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'exec setsid "$LSW_E2E_COLD_DAEMON_SESSION" "$@"' \
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
"$lsw" remove "$instance"
if [ -e "$LSW_STATE_DIR/instances/$instance" ]; then
    echo "error: instance directory remained after lsw remove" >&2
    exit 1
fi
terminate_daemon
collect_e2e_artifacts success

echo "Windows/KVM E2E passed: WinPE -> unattended OOBE -> LSWAgent service -> ConPTY -> shutdown -> cold restart -> cleanup."
