#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace_root"

for required_command in awk cargo rustc rg tar gzip sha256sum install mktemp; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done

cargo build --locked --release --bin lsw --bin lswd
windows_agent=$("$workspace_root/scripts/build-windows-agent.sh")
release_version=$("$workspace_root/target/release/lsw" --version | awk '{print $2}')
bundle_name="lsw-$release_version-linux-x86_64"
distribution_directory="$workspace_root/dist"
bundle_directory="$distribution_directory/$bundle_name"
archive="$distribution_directory/$bundle_name.tar.gz"

mkdir -p "$distribution_directory"
if [ -e "$bundle_directory" ] || [ -e "$archive" ] || [ -e "$archive.sha256" ]; then
    echo "error: release output for $bundle_name already exists; move it aside first" >&2
    exit 1
fi

staging_directory=$(mktemp -d "$distribution_directory/.lsw-release.XXXXXX")
cleanup_release_staging() {
    rm -rf -- "$staging_directory"
}
trap cleanup_release_staging EXIT INT TERM
staged_bundle="$staging_directory/$bundle_name"
install -d -m 0755 "$staged_bundle/docs" "$staged_bundle/source"
install -m 0755 target/release/lsw "$staged_bundle/lsw"
install -m 0755 target/release/lswd "$staged_bundle/lswd"
install -m 0644 "$windows_agent" "$staged_bundle/lsw-agent.exe"
install -m 0755 scripts/install.sh "$staged_bundle/install.sh"
install -m 0644 README.md CHANGELOG.md LICENSE "$staged_bundle/"
install -m 0644 docs/ARCHITECTURE.md docs/BETA.md docs/LEGAL_BOUNDARIES.md \
    docs/REFERENCES.md docs/SECURITY.md "$staged_bundle/docs/"
tar -cf - .github .gitignore Cargo.lock Cargo.toml CHANGELOG.md LICENSE README.md \
    crates docs rustfmt.toml scripts | tar -xf - -C "$staged_bundle/source"
printf 'LSW_VERSION=%s\nLICENSE=GPL-3.0-or-later\nCORRESPONDING_SOURCE=source/\nHOST_TARGET=x86_64-unknown-linux-gnu\nGUEST_AGENT_TARGET=x86_64-pc-windows-gnu\n' \
    "$release_version" >"$staged_bundle/BUILDINFO.txt"

mv "$staged_bundle" "$bundle_directory"
tar --sort=name --mtime='UTC 2020-01-01' --owner=0 --group=0 --numeric-owner \
    -C "$distribution_directory" -cf - "$bundle_name" | gzip -n >"$archive"
(
    cd "$distribution_directory"
    sha256sum "$bundle_name.tar.gz" >"$bundle_name.tar.gz.sha256"
)

echo "$archive"
echo "$archive.sha256"
