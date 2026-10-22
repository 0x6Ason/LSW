#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace_root"

for required_command in cmp find mkdir mktemp rm sed sha256sum tar; do
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

# Pass 1 builds from this checkout. Pass 2 unpacks the archive pass 1
# produced and builds again from the corresponding source it ships, in a
# different directory with an empty Cargo target directory. Identical archives
# prove that the published source rebuilds the published bytes, which is the
# property a downstream verifier needs; two builds of the same checkout would
# only prove that the build is deterministic on this machine.
CARGO_TARGET_DIR="$first_target" LSW_DIST_DIR="$repro_root/first" \
    scripts/build-release.sh >"$repro_root/first.out"
first_archive=$(sed -n '1p' "$repro_root/first.out")
if [ ! -f "$first_archive" ]; then
    echo "error: the first release packaging pass did not produce an archive" >&2
    exit 1
fi

mkdir -p -- "$repro_root/unpacked"
tar --no-same-owner --no-same-permissions -xzf "$first_archive" -C "$repro_root/unpacked"
shipped_source=$(find "$repro_root/unpacked" -mindepth 2 -maxdepth 2 -type d -name source -print -quit)
if [ -z "$shipped_source" ] || [ ! -x "$shipped_source/scripts/build-release.sh" ]; then
    echo "error: the first archive does not carry a buildable corresponding source" >&2
    exit 1
fi
CARGO_TARGET_DIR="$second_target" LSW_DIST_DIR="$repro_root/second" \
    "$shipped_source/scripts/build-release.sh" >"$repro_root/second.out"
second_archive=$(sed -n '1p' "$repro_root/second.out")
if [ ! -f "$second_archive" ]; then
    echo "error: the rebuild from shipped source did not produce an archive" >&2
    exit 1
fi

if ! cmp -s "$first_archive" "$second_archive"; then
    echo "error: rebuilding from the shipped corresponding source did not reproduce the release archive" >&2
    sha256sum "$first_archive" "$second_archive" >&2
    exit 1
fi

scripts/verify-release.sh "$first_archive" >/dev/null
scripts/verify-release.sh "$second_archive" >/dev/null
echo "The shipped corresponding source rebuilds the release archive byte for byte."
sha256sum "$first_archive"
