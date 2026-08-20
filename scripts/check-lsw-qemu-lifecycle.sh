#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
probe="$workspace_root/scripts/lsw-qemu-lifecycle.py"
lsw=${LSW_CLI:-"$workspace_root/target/debug/lsw"}
lswd=${LSW_DAEMON:-"$workspace_root/target/debug/lswd"}

if ! command -v timeout >/dev/null 2>&1; then
    echo "error: required command timeout was not found" >&2
    exit 1
fi

# The daemon, two QEMU launches, every QMP wait, and cleanup are bounded as one
# operation. The inner process owns the exact child PIDs and cleanup trap.
if [ "${LSW_QEMU_LIFECYCLE_TIMEOUT_ACTIVE:-0}" != 1 ]; then
    LSW_QEMU_LIFECYCLE_TIMEOUT_ACTIVE=1
    export LSW_QEMU_LIFECYCLE_TIMEOUT_ACTIVE
    exec timeout --signal=TERM --kill-after=15s 150s "$0" "$@"
fi

for required_command in cat chmod cut grep kill mkdir mktemp python3 rm setsid sleep; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done
for executable in "$lsw" "$lswd"; do
    if [ ! -x "$executable" ]; then
        echo "error: required executable was not found: $executable" >&2
        exit 1
    fi
done
if [ ! -f "$probe" ]; then
    echo "error: lifecycle probe was not found at $probe" >&2
    exit 1
fi

# Keep QMP, swtpm, and recovery-VNC pathname sockets well below sockaddr_un's
# platform limit even when the runner exports a long general-purpose TMPDIR.
lifecycle_base=${LSW_QEMU_LIFECYCLE_TMPDIR:-/tmp}
lifecycle_root=$(mktemp -d -- "$lifecycle_base/lsw-product-qemu.XXXXXX")
case "$lifecycle_root" in
    /*/lsw-product-qemu.??????) ;;
    *)
        echo "error: mktemp returned an unexpected lifecycle directory" >&2
        exit 1
        ;;
esac
state_root="$lifecycle_root/state"
instance_name='lsw-qemu-product-smoke'
instance_dir="$state_root/instances/$instance_name"
daemon_pid=

cleanup_lifecycle() {
    cleanup_status=$?
    trap - EXIT HUP INT TERM

    if [ -n "$daemon_pid" ]; then
        if kill -0 "$daemon_pid" 2>/dev/null; then
            timeout 10s "$lsw" stop "$instance_name" --force >/dev/null 2>&1 || :
        fi

        # lswd is a private session leader. QEMU and swtpm inherit this process
        # group, so cleanup remains exact even if lswd dies before supervising
        # its children. Never discover cleanup targets by name or a broad glob.
        kill -TERM "-$daemon_pid" 2>/dev/null || :
        cleanup_attempt=0
        while kill -0 "-$daemon_pid" 2>/dev/null \
            && [ "$cleanup_attempt" -lt 50 ]
        do
            cleanup_attempt=$((cleanup_attempt + 1))
            sleep 0.1
        done
        if kill -0 "-$daemon_pid" 2>/dev/null; then
            kill -KILL "-$daemon_pid" 2>/dev/null || :
        fi
        wait "$daemon_pid" 2>/dev/null || :
    fi

    if [ "$cleanup_status" -ne 0 ]; then
        for diagnostic_log in \
            "$lifecycle_root/lswd.log" \
            "$state_root/lswd.log" \
            "$instance_dir/helper.log" \
            "$instance_dir/qemu.log"
        do
            if [ -s "$diagnostic_log" ]; then
                echo "LSW lifecycle diagnostic from $diagnostic_log:" >&2
                cat "$diagnostic_log" >&2
            fi
        done
    fi
    rm -rf -- "$lifecycle_root"
    exit "$cleanup_status"
}
trap cleanup_lifecycle EXIT
trap 'exit 130' HUP INT TERM

mkdir -p -- "$state_root"
chmod 700 "$state_root"
if ! python3 "$probe" probe-unix "$lifecycle_root/unix-probe.sock"; then
    if [ "${LSW_QEMU_LIFECYCLE_REQUIRE:-0}" = 1 ]; then
        echo "error: pathname Unix sockets are required for the product lifecycle gate" >&2
        exit 1
    fi
    echo "LSW product lifecycle smoke skipped: pathname Unix sockets are unavailable."
    echo "Run scripts/check-qemu-smoke.sh for the transport-adapted QEMU gate."
    exit 0
fi

for required_runtime in qemu-img qemu-system-x86_64 swtpm; do
    if ! command -v "$required_runtime" >/dev/null 2>&1; then
        echo "error: required runtime $required_runtime was not found" >&2
        exit 1
    fi
done
qemu_img=$(command -v qemu-img)
qemu_system=$(command -v qemu-system-x86_64)
swtpm=$(command -v swtpm)

published_port=$(python3 "$probe" allocate-port)
python3 - "$lifecycle_root/windows-placeholder.iso" <<'PY'
import sys

with open(sys.argv[1], "wb") as media:
    media.write(b"LSW media-free QEMU lifecycle placeholder\n")
    media.truncate(64 * 1024)
PY

export LSW_STATE_DIR="$state_root"
"$lsw" create "$instance_name" \
    --iso "$lifecycle_root/windows-placeholder.iso" \
    --profile vanilla \
    --cpus 1 \
    --memory 512 \
    --disk 8 \
    --network nat \
    --publish "$published_port:8080" \
    --accept-license \
    --allow-unsupported-requirements \
    > "$lifecycle_root/create.out"
"$lsw" prepare "$instance_name" --execute > "$lifecycle_root/prepare.out"
"$qemu_img" check -q -f qcow2 "$instance_dir/disk.qcow2"
"$lsw" plan "$instance_name" > "$lifecycle_root/install-plan.out"

control_port=$(grep -F 'control_port=' "$instance_dir/instance.lsw" | cut -d= -f2)
case "$control_port" in
    ''|*[!0-9]*)
        echo "error: manifest did not contain a numeric control port" >&2
        exit 1
        ;;
esac

assert_plan_contains() {
    if ! grep -F -- "$1" "$lifecycle_root/install-plan.out" >/dev/null; then
        echo "error: QemuPlanner output did not contain: $1" >&2
        exit 1
    fi
}

assert_plan_contains 'if=pflash,format=raw,readonly=on,file='
assert_plan_contains "qemu:   $qemu_system"
assert_plan_contains "if=pflash,format=raw,file=$instance_dir/OVMF_VARS.fd"
assert_plan_contains "file=$instance_dir/disk.qcow2,if=none,id=system,format=qcow2"
assert_plan_contains 'nvme,drive=system,serial=lsw-system'
assert_plan_contains 'e1000e,netdev=net0'
assert_plan_contains 'emulator,id=tpm0,chardev=chrtpm'
assert_plan_contains 'tpm-tis,tpmdev=tpm0'
assert_plan_contains "user,id=net0,restrict=off,hostfwd=tcp:127.0.0.1:$control_port-:5040,hostfwd=tcp:127.0.0.1:$published_port-:8080"
assert_plan_contains "unix:$instance_dir/run/qmp.sock,server=on,wait=off"
assert_plan_contains "helper: $swtpm socket --tpm2"

# Give this test an isolated process group so its exact daemon/helper/QEMU tree
# can always be reclaimed by the cleanup trap.
setsid "$lswd" > "$lifecycle_root/lswd.log" 2>&1 &
daemon_pid=$!
daemon_socket="$state_root/run/lswd.sock"
daemon_ready=0
attempt=0
while [ "$attempt" -lt 100 ]; do
    if [ -S "$daemon_socket" ]; then
        daemon_ready=1
        break
    fi
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.05
done
if [ "$daemon_ready" -ne 1 ]; then
    echo "error: lswd did not create its control socket" >&2
    exit 1
fi

assert_status_contains() {
    "$lsw" status "$instance_name" > "$lifecycle_root/status.out"
    for expected_status in "$@"; do
        if ! grep -F -- "$expected_status" "$lifecycle_root/status.out" >/dev/null; then
            echo "error: status output did not contain: $expected_status" >&2
            cat "$lifecycle_root/status.out" >&2
            exit 1
        fi
    done
}

# The placeholder is intentionally not Windows media. OVMF remains alive, so
# every host-side product lifecycle path can still be exercised truthfully.
"$lsw" install "$instance_name" --without-agent > "$lifecycle_root/install.out"
assert_status_contains 'STATE=installing' 'QMP=running' 'ACTIVE=true'
python3 "$probe" ports bound "$control_port" "$published_port"
python3 "$probe" qmp-tpm "$instance_dir/run/qmp.sock"

"$lsw" stop "$instance_name" --force > "$lifecycle_root/install-stop.out"
assert_status_contains 'STATE=stopped' 'QMP=unavailable' 'ACTIVE=false'
python3 "$probe" ports released "$control_port" "$published_port"

"$lsw" start "$instance_name" > "$lifecycle_root/start.out"
assert_status_contains 'STATE=running' 'QMP=running' 'ACTIVE=true'
python3 "$probe" ports bound "$control_port" "$published_port"

"$lsw" suspend "$instance_name" > "$lifecycle_root/suspend.out"
assert_status_contains 'STATE=suspended' 'QMP=paused' 'ACTIVE=true'
"$lsw" resume "$instance_name" > "$lifecycle_root/resume.out"
assert_status_contains 'STATE=running' 'QMP=running' 'ACTIVE=true'

"$lsw" stop "$instance_name" --force > "$lifecycle_root/run-stop.out"
assert_status_contains 'STATE=stopped' 'QMP=unavailable' 'ACTIVE=false'
python3 "$probe" ports released "$control_port" "$published_port"

echo "LSW manifest/preparation/QemuPlanner/daemon lifecycle smoke passed."
echo "Validated OVMF, NVMe, e1000e, vTPM, loopback hostfwd, and QMP stop/cont/quit."
