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

echo "Shell syntax and ShellCheck passed."
