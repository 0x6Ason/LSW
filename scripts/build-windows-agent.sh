#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace_root"

zig_binary=${LSW_ZIG:-zig}
if ! command -v "$zig_binary" >/dev/null 2>&1; then
    echo "error: Zig was not found; set LSW_ZIG to its executable" >&2
    exit 1
fi

if ! rustc --print target-list | rg -x 'x86_64-pc-windows-gnu' >/dev/null; then
    echo "error: this Rust toolchain does not support x86_64-pc-windows-gnu" >&2
    exit 1
fi
rust_sysroot=$(rustc --print sysroot)
if [ ! -d "$rust_sysroot/lib/rustlib/x86_64-pc-windows-gnu/lib" ]; then
    echo "error: the x86_64-pc-windows-gnu Rust standard library is not installed" >&2
    echo "install it with: rustup target add x86_64-pc-windows-gnu" >&2
    exit 1
fi

export LSW_ZIG="$zig_binary"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$workspace_root/scripts/zig-windows-linker.sh"
# Zig's PE linker cannot consume rustc's embedded ThinLTO objects. The agent
# remains optimized; only cross-language link-time optimization is disabled.
export CARGO_PROFILE_RELEASE_LTO=false
: "${ZIG_LOCAL_CACHE_DIR:=$workspace_root/target/zig-cache/local}"
: "${ZIG_GLOBAL_CACHE_DIR:=$workspace_root/target/zig-cache/global}"
export ZIG_LOCAL_CACHE_DIR ZIG_GLOBAL_CACHE_DIR
mkdir -p "$ZIG_LOCAL_CACHE_DIR" "$ZIG_GLOBAL_CACHE_DIR"
cargo build --locked --release --target x86_64-pc-windows-gnu --bin lsw-agent

agent="$workspace_root/target/x86_64-pc-windows-gnu/release/lsw-agent.exe"
if [ ! -f "$agent" ]; then
    echo "error: Cargo completed without producing $agent" >&2
    exit 1
fi
echo "$agent"
