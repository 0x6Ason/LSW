#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workspace_root"

cargo_target_directory=${CARGO_TARGET_DIR:-"$workspace_root/target"}
case "$cargo_target_directory" in
    /*) ;;
    *) cargo_target_directory="$workspace_root/$cargo_target_directory" ;;
esac
export CARGO_TARGET_DIR="$cargo_target_directory"

for required_command in awk cargo find grep gzip install mkdir mktemp mv rm rustc sha256sum sort tar uname xargs; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done
if ! tar --version 2>/dev/null | grep -F 'GNU tar' >/dev/null; then
    echo "error: release packaging currently requires GNU tar" >&2
    exit 1
fi
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
    echo "error: this beta release builder supports Linux x86_64 hosts only" >&2
    exit 1
fi
rust_host=$(rustc -vV | awk '/^host:/ { print $2 }')
if [ "$rust_host" != x86_64-unknown-linux-gnu ]; then
    echo "error: the release builder requires the x86_64-unknown-linux-gnu Rust host" >&2
    echo "current Rust host: ${rust_host:-unknown}" >&2
    exit 1
fi

release_epoch=${SOURCE_DATE_EPOCH:-1577836800}
case "$release_epoch" in
    ''|*[!0-9]*)
        echo "error: SOURCE_DATE_EPOCH must be a non-negative integer" >&2
        exit 1
        ;;
esac
SOURCE_DATE_EPOCH=$release_epoch
export SOURCE_DATE_EPOCH

cargo build --locked --release --bin lsw --bin lswd
windows_agent=$("$workspace_root/scripts/build-windows-agent.sh")
version_output=$("$cargo_target_directory/release/lsw" --version)
version_field_count=$(printf '%s\n' "$version_output" | awk '{ print NF }')
version_program=$(printf '%s\n' "$version_output" | awk '{ print $1 }')
release_version=$(printf '%s\n' "$version_output" | awk '{ print $2 }')
if [ "$version_field_count" -ne 2 ] || [ "$version_program" != lsw ]; then
    echo "error: unexpected version output: $version_output" >&2
    exit 1
fi
case "$release_version" in
    ''|*[!0-9A-Za-z.+-]*)
        echo "error: release version contains unsafe characters: $release_version" >&2
        exit 1
        ;;
esac

if [ -n "${LSW_EXPECT_VERSION:-}" ]; then
    expected_version=${LSW_EXPECT_VERSION#v}
    if [ "$expected_version" != "$release_version" ]; then
        echo "error: requested version $LSW_EXPECT_VERSION does not match lsw $release_version" >&2
        exit 1
    fi
fi

bundle_name="lsw-$release_version-linux-x86_64"
distribution_directory=${LSW_DIST_DIR:-"$workspace_root/dist"}
mkdir -p -- "$distribution_directory"
distribution_directory=$(CDPATH='' cd -- "$distribution_directory" && pwd)
bundle_directory="$distribution_directory/$bundle_name"
archive="$distribution_directory/$bundle_name.tar.gz"
checksum="$archive.sha256"

if [ -e "$bundle_directory" ] || [ -e "$archive" ] || [ -e "$checksum" ]; then
    echo "error: release output for $bundle_name already exists; move it aside first" >&2
    exit 1
fi

staging_directory=$(mktemp -d -- "$distribution_directory/.lsw-release.XXXXXX")
cleanup_release_staging() {
    rm -rf -- "$staging_directory"
}
trap cleanup_release_staging EXIT HUP INT TERM

staged_bundle="$staging_directory/$bundle_name"
staged_tar="$staging_directory/$bundle_name.tar"
staged_archive="$staging_directory/$bundle_name.tar.gz"
staged_checksum="$staged_archive.sha256"
install -d -m 0755 -- "$staged_bundle/docs" "$staged_bundle/source"
install -m 0755 -- "$cargo_target_directory/release/lsw" "$staged_bundle/lsw"
install -m 0755 -- "$cargo_target_directory/release/lswd" "$staged_bundle/lswd"
install -m 0644 -- "$windows_agent" "$staged_bundle/lsw-agent.exe"
install -m 0755 -- scripts/install.sh "$staged_bundle/install.sh"
install -m 0644 -- README.md CHANGELOG.md LICENSE THIRD_PARTY_NOTICES.md "$staged_bundle/"
install -m 0644 -- docs/ARCHITECTURE.md docs/BETA.md docs/LEGAL_BOUNDARIES.md \
    docs/DEVELOPMENT.md docs/REFERENCES.md docs/SECURITY.md \
    docs/WINDOWS_KVM_E2E.md "$staged_bundle/docs/"
tar --exclude='*/__pycache__' --exclude='*.pyc' --exclude='*.pyo' \
    -cf - .github .gitignore Cargo.lock Cargo.toml CHANGELOG.md LICENSE README.md \
    THIRD_PARTY_NOTICES.md crates docs rustfmt.toml scripts | \
    tar -xf - -C "$staged_bundle/source"
(
    cd "$staged_bundle/source"
    mkdir -p -- .cargo
    cargo vendor --locked --versioned-dirs vendor >.cargo/config.toml
    find . -type f \
        ! -path '*/__pycache__/*' \
        ! -name '*.pyc' \
        ! -name '*.pyo' \
        -print0 | LC_ALL=C sort -z | xargs -0 sha256sum
) >"$staged_bundle/SOURCE-MANIFEST.sha256"
printf '%s\n' \
    "LSW_VERSION=$release_version" \
    'LICENSE=GPL-3.0-or-later' \
    'CORRESPONDING_SOURCE=source/' \
    'HOST_TARGET=x86_64-unknown-linux-gnu' \
    'GUEST_AGENT_TARGET=x86_64-pc-windows-gnu' \
    "SOURCE_DATE_EPOCH=$release_epoch" >"$staged_bundle/BUILDINFO.txt"

tar --sort=name --mtime="@$release_epoch" --owner=0 --group=0 --numeric-owner \
    --mode='u+rwX,go+rX,go-w' \
    -C "$staging_directory" -cf "$staged_tar" "$bundle_name"
gzip -n "$staged_tar"
(
    cd "$staging_directory"
    sha256sum "$bundle_name.tar.gz" >"$bundle_name.tar.gz.sha256"
)

# Each move is within the distribution filesystem, so completed files become
# visible atomically and an interrupted build never leaves a partial archive.
mv "$staged_bundle" "$bundle_directory"
mv "$staged_archive" "$archive"
mv "$staged_checksum" "$checksum"

echo "$archive"
echo "$checksum"
