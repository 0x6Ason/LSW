#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: scripts/verify-release.sh DIST_ARCHIVE.tar.gz" >&2
    exit 1
fi

archive=$1
if [ ! -f "$archive" ]; then
    echo "error: archive does not exist: $archive" >&2
    exit 1
fi

archive_directory=$(CDPATH= cd -- "$(dirname -- "$archive")" && pwd)
archive_name=$(basename -- "$archive")
if [ -f "$archive.sha256" ]; then
    (
        cd "$archive_directory"
        sha256sum --check "$archive_name.sha256"
    )
fi

verification_directory=$(mktemp -d /tmp/lsw-release-verify.XXXXXX)
cleanup_release_verification() {
    rm -rf -- "$verification_directory"
}
trap cleanup_release_verification EXIT INT TERM

tar -xzf "$archive" -C "$verification_directory"
bundle=$(find "$verification_directory" -mindepth 1 -maxdepth 1 -type d)
if [ -z "$bundle" ] || [ ! -x "$bundle/lsw" ] || [ ! -x "$bundle/lswd" ]; then
    echo "error: archive does not contain executable lsw and lswd binaries" >&2
    exit 1
fi
if ! file "$bundle/lsw-agent.exe" | rg 'PE32\+ executable.*x86-64' >/dev/null; then
    echo "error: archive guest agent is not a Windows x86_64 PE executable" >&2
    exit 1
fi

"$bundle/lsw" --version
"$bundle/lsw" help >/dev/null
echo "Verified release bundle: $bundle"
