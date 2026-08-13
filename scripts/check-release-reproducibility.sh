#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace_root"

for required_command in cmp mkdir mktemp rm sed sha256sum; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done

repro_root=$(mktemp -d -- "${TMPDIR:-/tmp}/lsw-release-repro.XXXXXX")
cleanup_release_repro() {
    rm -rf -- "$repro_root"
}
trap cleanup_release_repro EXIT HUP INT TERM
mkdir -p -- "$repro_root/first" "$repro_root/second"
first_target="$repro_root/first-target"
second_target="$repro_root/second-target"

CARGO_TARGET_DIR="$first_target" LSW_DIST_DIR="$repro_root/first" \
    scripts/build-release.sh >"$repro_root/first.out"
CARGO_TARGET_DIR="$second_target" LSW_DIST_DIR="$repro_root/second" \
    scripts/build-release.sh >"$repro_root/second.out"
first_archive=$(sed -n '1p' "$repro_root/first.out")
second_archive=$(sed -n '1p' "$repro_root/second.out")
if [ ! -f "$first_archive" ] || [ ! -f "$second_archive" ]; then
    echo "error: a release packaging pass did not produce an archive" >&2
    exit 1
fi

if ! cmp -s "$first_archive" "$second_archive"; then
    echo "error: two independent clean release builds were not byte-for-byte deterministic" >&2
    sha256sum "$first_archive" "$second_archive" >&2
    exit 1
fi

scripts/verify-release.sh "$first_archive" >/dev/null
scripts/verify-release.sh "$second_archive" >/dev/null
echo "Two independent clean release builds are byte-for-byte deterministic."
sha256sum "$first_archive"
