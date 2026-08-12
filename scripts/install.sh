#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

bundle_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
install_prefix=${LSW_INSTALL_PREFIX:-"${HOME:?HOME is not set}/.local"}
binary_directory="$install_prefix/bin"
agent_directory="$install_prefix/libexec/lsw"
documentation_directory="$install_prefix/share/doc/lsw"

if [ "$(uname -s)" != "Linux" ] || [ "$(uname -m)" != "x86_64" ]; then
    echo "error: this beta bundle supports Linux x86_64 hosts only" >&2
    exit 1
fi

for required_file in lsw lswd lsw-agent.exe LICENSE; do
    if [ ! -f "$bundle_directory/$required_file" ]; then
        echo "error: bundle is missing $required_file" >&2
        exit 1
    fi
done

install -d -m 0755 "$binary_directory" "$agent_directory" "$documentation_directory"
install -m 0755 "$bundle_directory/lsw" "$binary_directory/lsw"
install -m 0755 "$bundle_directory/lswd" "$binary_directory/lswd"
install -m 0644 "$bundle_directory/lsw-agent.exe" "$agent_directory/lsw-agent.exe"
install -m 0644 "$bundle_directory/LICENSE" "$documentation_directory/LICENSE"

echo "Installed LSW into $install_prefix"
echo "Ensure $binary_directory is on PATH, then run: lsw doctor"
