#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
helper="$workspace_root/scripts/qemu-smoke.py"

if ! command -v timeout >/dev/null 2>&1; then
    echo "error: required command timeout was not found" >&2
    exit 1
fi

# Bound every wait, including cleanup of a wedged QEMU process. The inner
# process owns the cleanup trap and is terminated as a group by GNU timeout.
if [ "${LSW_QEMU_SMOKE_TIMEOUT_ACTIVE:-0}" != 1 ]; then
    LSW_QEMU_SMOKE_TIMEOUT_ACTIVE=1
    export LSW_QEMU_SMOKE_TIMEOUT_ACTIVE
    exec timeout --signal=TERM --kill-after=10s 90s "$0" "$@"
fi

if [ -n "${LSW_QEMU_STAGE_ROOT:-}" ]; then
    stage_root=${LSW_QEMU_STAGE_ROOT%/}
    if [ ! -d "$stage_root" ]; then
        echo "error: LSW_QEMU_STAGE_ROOT is not a directory: $stage_root" >&2
        exit 1
    fi
    PATH="$stage_root/usr/bin:$PATH"
    LD_LIBRARY_PATH="$stage_root/usr/lib/x86_64-linux-gnu:$stage_root/usr/lib/x86_64-linux-gnu/swtpm:$stage_root/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    QEMU_MODULE_DIR="$stage_root/usr/lib/x86_64-linux-gnu/qemu"
    export PATH LD_LIBRARY_PATH QEMU_MODULE_DIR
    : "${LSW_QEMU_SYSTEM:=$stage_root/usr/bin/qemu-system-x86_64}"
    : "${LSW_QEMU_IMG:=$stage_root/usr/bin/qemu-img}"
    : "${LSW_SWTPM:=$stage_root/usr/bin/swtpm}"
    : "${LSW_QEMU_DATA_DIR:=$stage_root/usr/share/qemu}"
    : "${LSW_OVMF_CODE:=$stage_root/usr/share/OVMF/OVMF_CODE_4M.fd}"
    : "${LSW_OVMF_VARS:=$stage_root/usr/share/OVMF/OVMF_VARS_4M.fd}"
    if [ -r "$stage_root/usr/lib/ipxe/qemu/efi-e1000e.rom" ]; then
        : "${LSW_QEMU_NIC_ROM:=$stage_root/usr/lib/ipxe/qemu/efi-e1000e.rom}"
    fi
fi

qemu_system=${LSW_QEMU_SYSTEM:-qemu-system-x86_64}
qemu_img=${LSW_QEMU_IMG:-qemu-img}
swtpm=${LSW_SWTPM:-swtpm}
qemu_data=${LSW_QEMU_DATA_DIR:-}
nic_rom=${LSW_QEMU_NIC_ROM:-}
PYTHONDONTWRITEBYTECODE=1
export PYTHONDONTWRITEBYTECODE

for required_command in cat cp kill mkdir mktemp python3 rm; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done
for executable in "$qemu_system" "$qemu_img" "$swtpm"; do
    if ! command -v "$executable" >/dev/null 2>&1; then
        echo "error: required command $executable was not found" >&2
        exit 1
    fi
done
if [ ! -f "$helper" ]; then
    echo "error: QEMU smoke helper was not found at $helper" >&2
    exit 1
fi
if [ -n "$qemu_data" ] && [ ! -d "$qemu_data" ]; then
    echo "error: QEMU data directory is not a directory: $qemu_data" >&2
    exit 1
fi
if [ -n "$nic_rom" ] && [ ! -r "$nic_rom" ]; then
    echo "error: configured QEMU NIC ROM is not readable: $nic_rom" >&2
    exit 1
fi

smoke_root=$(mktemp -d -- "${TMPDIR:-/tmp}/lsw-qemu-smoke.XXXXXX")
smoke_helper_pid=
cleanup_qemu_smoke() {
    cleanup_status=$?
    trap - EXIT HUP INT TERM
    if [ -n "$smoke_helper_pid" ] && kill -0 "$smoke_helper_pid" 2>/dev/null; then
        kill "$smoke_helper_pid" 2>/dev/null || :
        wait "$smoke_helper_pid" 2>/dev/null || :
    fi
    if [ "$cleanup_status" -ne 0 ]; then
        for diagnostic_log in \
            "$smoke_root/qemu.log" \
            "$smoke_root/swtpm.log" \
            "$smoke_root/swtpm.stdout"
        do
            if [ -s "$diagnostic_log" ]; then
                echo "QEMU smoke diagnostic from $diagnostic_log:" >&2
                cat "$diagnostic_log" >&2
            fi
        done
    fi
    rm -rf -- "$smoke_root"
    exit "$cleanup_status"
}
trap cleanup_qemu_smoke EXIT
trap 'exit 130' HUP INT TERM

select_ovmf_pair() {
    if [ -r "$1" ] && [ -r "$2" ]; then
        ovmf_code=$1
        ovmf_vars=$2
        return 0
    fi
    return 1
}

if [ -n "${LSW_OVMF_CODE:-}" ] || [ -n "${LSW_OVMF_VARS:-}" ]; then
    if [ -z "${LSW_OVMF_CODE:-}" ] || [ -z "${LSW_OVMF_VARS:-}" ]; then
        echo "error: LSW_OVMF_CODE and LSW_OVMF_VARS must be set together" >&2
        exit 1
    fi
    if ! select_ovmf_pair "$LSW_OVMF_CODE" "$LSW_OVMF_VARS"; then
        echo "error: configured OVMF code or variable template is not readable" >&2
        exit 1
    fi
elif select_ovmf_pair \
    /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_VARS_4M.fd; then
    :
elif select_ovmf_pair \
    /usr/share/OVMF/OVMF_CODE.fd /usr/share/OVMF/OVMF_VARS.fd; then
    :
elif select_ovmf_pair \
    /usr/share/edk2/x64/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_VARS.fd; then
    :
elif select_ovmf_pair \
    /usr/share/edk2/ovmf/OVMF_CODE.fd /usr/share/edk2/ovmf/OVMF_VARS.fd; then
    :
else
    echo "error: no matching OVMF code and variable-store template was found" >&2
    exit 1
fi

mkdir -p -- "$smoke_root/swtpm-state"
cp -- "$ovmf_vars" "$smoke_root/OVMF_VARS.fd"

timeout 10s "$qemu_img" create -q -f qcow2 "$smoke_root/disk.qcow2" 64M
timeout 10s "$qemu_img" check -q -f qcow2 "$smoke_root/disk.qcow2"

python3 "$helper" \
    --qemu "$qemu_system" \
    --swtpm "$swtpm" \
    --qemu-data "$qemu_data" \
    --nic-rom "$nic_rom" \
    --ovmf-code "$ovmf_code" \
    --ovmf-vars "$smoke_root/OVMF_VARS.fd" \
    --disk "$smoke_root/disk.qcow2" \
    --swtpm-state "$smoke_root/swtpm-state" \
    --swtpm-log "$smoke_root/swtpm.log" \
    --swtpm-stdout "$smoke_root/swtpm.stdout" \
    --qemu-log "$smoke_root/qemu.log" \
    --seconds 40 &
smoke_helper_pid=$!
if wait "$smoke_helper_pid"; then
    smoke_helper_pid=
else
    helper_status=$?
    smoke_helper_pid=
    exit "$helper_status"
fi

echo "qemu-img, swtpm, TCG, OVMF, vTPM, QMP, and loopback hostfwd smoke checks passed."
