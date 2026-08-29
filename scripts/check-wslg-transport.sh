#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later

# This probe runs as root in the WSLg system distro. It reports the active
# Weston's effective pixel transport, not merely the transport it inherited.

set -eu

weston_process=
for process_dir in /proc/[0-9]*; do
    cmdline=$(tr '\000' ' ' <"$process_dir/cmdline" 2>/dev/null) || continue
    case "$cmdline" in
        */weston\ *--backend=rdp-backend.so*) ;;
        *) continue ;;
    esac
    [ -z "$weston_process" ] || exit 41
    weston_process=$process_dir
done
[ -n "$weston_process" ] || exit 42

if ! tr '\000' '\n' <"$weston_process/environ" 2>/dev/null \
    | grep -q '^WSL2_SHARED_MEMORY_MOUNT_POINT='
then
    printf '%s\n' copy
    exit 0
fi

probe=
if grep -qs ' /mnt/shared_memory ' /proc/mounts; then
    probe=$(mktemp /mnt/shared_memory/lsw-seamless-e2e.XXXXXX 2>/dev/null || :)
    if [ -n "$probe" ] \
        && printf '%s\n' lsw-wslg-probe >"$probe" 2>/dev/null \
        && [ "$(cat -- "$probe" 2>/dev/null)" = lsw-wslg-probe ]
    then
        rm -f -- "$probe" || exit 43
        printf '%s\n' vail
        exit 0
    fi
    [ -z "$probe" ] || rm -f -- "$probe" 2>/dev/null || :
fi

# WSLg truncates this log for each Weston launch. If VAIL allocation failed,
# the active backend explicitly records its automatic RAIL/copy fallback.
grep -Fq 'RDP backend: use_gfxredir = 0' /mnt/wslg/weston.log \
    2>/dev/null || exit 44
if grep -Fq 'RDP backend: use_gfxredir = 1' /mnt/wslg/weston.log \
    2>/dev/null
then
    exit 45
fi
printf '%s\n' copy
