#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace_root"

for required_command in bash dash head shellcheck; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done

for script in scripts/*.sh; do
    case "$(head -n 1 "$script")" in
        '#!/usr/bin/env bash'|'#!/bin/bash')
            bash -n "$script"
            shellcheck --external-sources --shell=bash "$script"
            ;;
        '#!/bin/sh'|'#!/usr/bin/env sh')
            dash -n "$script"
            shellcheck --external-sources --shell=sh "$script"
            ;;
        *)
            echo "error: unsupported or missing shell shebang: $script" >&2
            exit 1
            ;;
    esac
done

# The e2e libraries are sourced by the gate drivers rather than executed, so
# they carry a `shell=sh` directive instead of a shebang and are only linted
# through the driver's --external-sources above. Lint each one directly as
# well: a stray `//` comment line parses as a command, breaks every driver
# that sources the file, and would otherwise surface only on the dedicated
# Windows/KVM runner.
for library in scripts/e2e/*.sh; do
    case "$(head -n 1 "$library")" in
        '# SPDX-License-Identifier: '*) ;;
        *)
            echo "error: sourced library must start with a '# SPDX' comment: $library" >&2
            exit 1
            ;;
    esac
    if grep -n '^[[:space:]]*//' "$library"; then
        echo "error: '//' is not a comment in sh: $library" >&2
        exit 1
    fi
    dash -n "$library"
    # Variables a library assigns for its driver or sibling libraries look
    # unused in isolation (SC2034); the driver lint above covers their use.
    shellcheck --external-sources --shell=sh --exclude=SC2034 "$library"
done

echo "Shell syntax and ShellCheck passed."
