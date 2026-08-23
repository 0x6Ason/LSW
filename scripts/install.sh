#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

bundle_directory=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
default_prefix=
if [ -n "${HOME:-}" ]; then
    default_prefix="$HOME/.local"
fi
if [ -n "${LSW_INSTALL_PREFIX:-}" ]; then
    install_prefix=$LSW_INSTALL_PREFIX
elif [ -n "$default_prefix" ]; then
    install_prefix=$default_prefix
else
    echo "error: HOME is not set; configure LSW_INSTALL_PREFIX" >&2
    exit 1
fi
installing_default_prefix=0
if [ -n "$default_prefix" ] && [ "$install_prefix" = "$default_prefix" ]; then
    installing_default_prefix=1
fi
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
for required_unit in systemd/lswd.service systemd/lswd.socket; do
    if [ ! -f "$bundle_directory/$required_unit" ]; then
        echo "error: bundle is missing $required_unit" >&2
        exit 1
    fi
done

install -d -m 0755 -- "$binary_directory" "$agent_directory" "$documentation_directory"
install -m 0755 -- "$bundle_directory/lsw" "$binary_directory/lsw"
install -m 0755 -- "$bundle_directory/lswd" "$binary_directory/lswd"
install -m 0644 -- "$bundle_directory/lsw-agent.exe" "$agent_directory/lsw-agent.exe"
install -m 0644 -- "$bundle_directory/LICENSE" "$documentation_directory/LICENSE"

if [ "$installing_default_prefix" -eq 1 ]; then
    systemd_user_directory="$install_prefix/share/systemd/user"
    install -d -m 0755 -- "$systemd_user_directory"
    install -m 0644 -- "$bundle_directory/systemd/lswd.service" \
        "$bundle_directory/systemd/lswd.socket" "$systemd_user_directory/"
fi

echo "Installed LSW into $install_prefix"
echo "Ensure $binary_directory is on PATH, then run: lsw doctor"
if [ "$installing_default_prefix" -eq 1 ]; then
    echo "Optional socket activation: systemctl --user enable --now lswd.socket"
else
    echo "Systemd units were not installed because LSW_INSTALL_PREFIX is non-default."
fi
