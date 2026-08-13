#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace_root"

cargo_target_directory=${CARGO_TARGET_DIR:-"$workspace_root/target"}
case "$cargo_target_directory" in
    /*) ;;
    *) cargo_target_directory="$workspace_root/$cargo_target_directory" ;;
esac
export CARGO_TARGET_DIR="$cargo_target_directory"

for required_command in cargo grep mkdir python3 rustc; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done

windows_target=x86_64-pc-windows-gnu
if ! rustc --print target-list | grep -Fx "$windows_target" >/dev/null; then
    echo "error: this Rust toolchain does not support $windows_target" >&2
    exit 1
fi
rust_sysroot=$(rustc --print sysroot)
if [ ! -d "$rust_sysroot/lib/rustlib/$windows_target/lib" ]; then
    echo "error: the $windows_target Rust standard library is not installed" >&2
    echo "install it with: rustup target add $windows_target" >&2
    exit 1
fi

windows_linker=${LSW_WINDOWS_LINKER:-auto}
zig_binary=${LSW_ZIG:-zig}
mingw_binary=${LSW_MINGW_CC:-x86_64-w64-mingw32-gcc}

if [ "$windows_linker" = auto ]; then
    if command -v "$zig_binary" >/dev/null 2>&1; then
        windows_linker=zig
    elif command -v "$mingw_binary" >/dev/null 2>&1; then
        windows_linker=mingw
    else
        echo "error: neither Zig nor a MinGW x86_64 compiler was found" >&2
        echo "set LSW_WINDOWS_LINKER to zig or mingw after installing one" >&2
        exit 1
    fi
fi

case "$windows_linker" in
    zig)
        if ! command -v "$zig_binary" >/dev/null 2>&1; then
            echo "error: Zig was not found; set LSW_ZIG to its executable" >&2
            exit 1
        fi
        export LSW_ZIG="$zig_binary"
        export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$workspace_root/scripts/zig-windows-linker.sh"
        # Zig's PE linker cannot consume rustc's embedded ThinLTO objects. The
        # agent remains optimized; only cross-language LTO is disabled.
        export CARGO_PROFILE_RELEASE_LTO=false
        : "${ZIG_LOCAL_CACHE_DIR:=$cargo_target_directory/zig-cache/local}"
        : "${ZIG_GLOBAL_CACHE_DIR:=$cargo_target_directory/zig-cache/global}"
        export ZIG_LOCAL_CACHE_DIR ZIG_GLOBAL_CACHE_DIR
        mkdir -p -- "$ZIG_LOCAL_CACHE_DIR" "$ZIG_GLOBAL_CACHE_DIR"
        ;;
    mingw)
        if ! command -v "$mingw_binary" >/dev/null 2>&1; then
            echo "error: the MinGW compiler was not found: $mingw_binary" >&2
            echo "set LSW_MINGW_CC to an x86_64-w64-mingw32-gcc-compatible executable" >&2
            exit 1
        fi
        export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$mingw_binary"
        ;;
    *)
        echo "error: LSW_WINDOWS_LINKER must be auto, zig, or mingw" >&2
        exit 1
        ;;
esac

echo "Building lsw-agent with the $windows_linker linker" >&2
cargo build --locked --release --target "$windows_target" --bin lsw-agent

agent="$cargo_target_directory/$windows_target/release/lsw-agent.exe"
if [ ! -f "$agent" ]; then
    echo "error: Cargo completed without producing $agent" >&2
    exit 1
fi
python3 "$workspace_root/scripts/normalize-pe-timestamp.py" "$agent"
echo "$agent"
