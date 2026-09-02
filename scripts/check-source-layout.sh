#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace_root"

baseline=config/source-size-baseline.txt
default_limit=1000
entrypoint_limit=300

if [ ! -f "$baseline" ]; then
    echo "error: source-size baseline is missing: $baseline" >&2
    exit 1
fi

for required_command in awk git grep mktemp rm rmdir wc; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done

temporary_directory=$(mktemp -d)
tracked_files="$temporary_directory/tracked-files.txt"
cleanup() {
    rm -f -- "$tracked_files"
    rmdir -- "$temporary_directory" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
git ls-files '*.rs' '*.sh' '*.py' '*.ps1' >"$tracked_files"

duplicate=$(
    awk -F '|' '
        /^[[:space:]]*#/ || /^[[:space:]]*$/ { next }
        seen[$1]++ { print $1; exit }
    ' "$baseline"
)
if [ -n "$duplicate" ]; then
    echo "error: duplicate source-size exception: $duplicate" >&2
    exit 1
fi

lookup_limit() {
    awk -F '|' -v target="$1" '
        /^[[:space:]]*#/ || /^[[:space:]]*$/ { next }
        $1 == target { print $2 }
    ' "$baseline"
}

path_limit() {
    case "$1" in
        crates/*/src/main.rs) printf '%s\n' "$entrypoint_limit" ;;
        *) printf '%s\n' "$default_limit" ;;
    esac
}

failed=0
while IFS='|' read -r path limit extra; do
    case "$path" in
        ''|'#'*) continue ;;
    esac
    if [ -n "${extra:-}" ]; then
        echo "error: invalid source-size exception: $path|$limit|$extra" >&2
        failed=1
        continue
    fi
    case "$limit" in
        ''|*[!0-9]*)
            echo "error: invalid source-size limit for $path: $limit" >&2
            failed=1
            continue
            ;;
    esac
    if ! grep -Fqx -- "$path" "$tracked_files"; then
        echo "error: stale or untracked source-size exception: $path" >&2
        failed=1
        continue
    fi
    lines=$(wc -l <"$path")
    lines=$(printf '%s' "$lines" | awk '{$1=$1; print}')
    target_limit=$(path_limit "$path")
    if [ "$lines" -le "$target_limit" ]; then
        echo "error: remove the obsolete source-size exception for $path ($lines lines)" >&2
        failed=1
    elif [ "$lines" -gt "$limit" ]; then
        echo "error: $path grew from its $limit-line Slice 4.5 baseline to $lines lines" >&2
        failed=1
    fi
done <"$baseline"

while IFS= read -r path; do
    [ -n "$path" ] || continue
    limit=$(lookup_limit "$path")
    if [ -n "$limit" ]; then
        continue
    fi
    limit=$(path_limit "$path")
    lines=$(wc -l <"$path")
    lines=$(printf '%s' "$lines" | awk '{$1=$1; print}')
    if [ "$lines" -gt "$limit" ]; then
        echo "error: $path has $lines lines; split it below the $limit-line limit" >&2
        failed=1
    fi
done <"$tracked_files"

if [ "$failed" -ne 0 ]; then
    exit 1
fi

echo "Source layout ratchet passed (sources: at most $default_limit lines; executable entry points: at most $entrypoint_limit lines)."
