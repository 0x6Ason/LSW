#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
lsw=${LSW_E2E_LSW:-"$workspace_root/target/debug/lsw"}
lswd=${LSW_E2E_LSWD:-"$workspace_root/target/debug/lswd"}

for executable in "$lsw" "$lswd"; do
    if [ ! -x "$executable" ]; then
        echo "error: build lsw and lswd before running the socket activation check" >&2
        exit 1
    fi
done
if ! command -v systemd-socket-activate >/dev/null 2>&1; then
    echo "error: systemd-socket-activate is required" >&2
    exit 1
fi

activation_root=$(mktemp -d)
socket_path="$activation_root/run/lswd.sock"
activation_pid=
cleanup() {
    if [ -n "$activation_pid" ]; then
        kill -TERM "-$activation_pid" 2>/dev/null \
            || kill -TERM "$activation_pid" 2>/dev/null \
            || :
        wait "$activation_pid" 2>/dev/null || :
    fi
    rm -rf -- "$activation_root"
}
trap cleanup EXIT HUP INT TERM

mkdir -m 0700 -- "$activation_root/run"
(
    umask 077
    exec systemd-socket-activate \
        --fdname=lswd \
        --listen="$socket_path" \
        --setenv="LSW_STATE_DIR=$activation_root" \
        --setenv="LSW_DAEMON_SOCKET=$socket_path" \
        "$lswd"
) >"$activation_root/activate.log" 2>&1 &
activation_pid=$!

attempt=0
while [ ! -S "$socket_path" ] && [ "$attempt" -lt 100 ]; do
    sleep 0.05
    attempt=$((attempt + 1))
done
if [ ! -S "$socket_path" ]; then
    echo "error: activation helper did not create its socket" >&2
    cat "$activation_root/activate.log" >&2
    exit 1
fi
# systemd-socket-activate is a protocol test helper and does not expose the
# SocketMode= setting used by the shipped unit.
chmod 0600 -- "$socket_path"

status=$(LSW_STATE_DIR="$activation_root" LSW_DAEMON_SOCKET="$socket_path" \
    "$lsw" daemon status)
if ! printf '%s\n' "$status" | grep -F "lswd is ready at $socket_path" >/dev/null; then
    echo "error: activated lswd did not answer the daemon status request" >&2
    printf '%s\n' "$status" >&2
    cat "$activation_root/activate.log" >&2
    exit 1
fi

attempt=0
while ! grep -F "lswd listening on $socket_path" "$activation_root/activate.log" >/dev/null \
    && [ "$attempt" -lt 100 ]; do
    sleep 0.05
    attempt=$((attempt + 1))
done
if ! grep -F "lswd listening on $socket_path" "$activation_root/activate.log" >/dev/null; then
    echo "error: activated lswd did not accept the inherited socket" >&2
    cat "$activation_root/activate.log" >&2
    exit 1
fi

echo "LSW systemd socket activation smoke passed."
