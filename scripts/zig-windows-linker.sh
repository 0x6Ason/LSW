#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

zig_binary=${LSW_ZIG:-zig}
linker_arguments=()
for argument in "$@"; do
    case "$argument" in
        -Wl,--disable-auto-image-base|-lgcc|-lgcc_eh|-l:libpthread.a|*libcompiler_builtins*.rlib*)
            # rustc's GNU Windows target adds this PE flag. Zig's bundled LLD
            # does not expose it, and normal executable images do not need it.
            # Zig supplies compiler-rt and the MinGW import libraries, so the
            # equivalent Rust/GCC runtime entries must not be linked twice.
            ;;
        *)
            linker_arguments+=("$argument")
            ;;
    esac
done

exec "$zig_binary" cc -target x86_64-windows-gnu -nostdlib "${linker_arguments[@]}" -lunwind
