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

for required_command in awk cargo find grep gzip install mkdir mktemp mv rm rustc sed sha256sum sort tar uname xargs; do
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

# shellcheck source=scripts/lib/reproducible-build.sh
. "$workspace_root/scripts/lib/reproducible-build.sh"

source_commit=$(lsw_source_commit "$workspace_root")
case "$source_commit" in
    *-dirty)
        if [ -n "${LSW_EXPECT_VERSION:-}" ]; then
            echo "error: a tagged release must be built from a clean checkout; tracked files are modified" >&2
            exit 1
        fi
        echo "warning: building from a checkout with modified tracked files ($source_commit)" >&2
        ;;
esac

# A release has no stable output name until the binary reports its version,
# so stage the corresponding source first, build from that snapshot, and only
# then name the bundle. Building from the snapshot, with its vendored
# dependency tree and the source root remapped to /lsw, is what lets a
# rebuild from the shipped source/ directory reproduce the binaries exactly.
distribution_directory=${LSW_DIST_DIR:-"$workspace_root/dist"}
mkdir -p -- "$distribution_directory"
distribution_directory=$(CDPATH='' cd -- "$distribution_directory" && pwd)
staging_directory=$(mktemp -d -- "$distribution_directory/.lsw-release.XXXXXX")
cleanup_release_staging() {
    rm -rf -- "$staging_directory"
}
trap cleanup_release_staging EXIT HUP INT TERM
staged_source="$staging_directory/source"
install -d -m 0755 -- "$staged_source"

if git -C "$workspace_root" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    # Tracked files only, read from the working tree: local edits are kept
    # for development builds, untracked scratch files never ship.
    git -C "$workspace_root" ls-files -z | tar --null -T - -cf - | tar -xf - -C "$staged_source"
else
    # A corresponding-source tree from an earlier bundle has no git history;
    # it carries the fixed file set below and its own SOURCE_COMMIT.
    tar --exclude='*/__pycache__' --exclude='*.pyc' --exclude='*.pyo' \
        -cf - .github .gitattributes .gitignore Cargo.lock Cargo.toml CHANGELOG.md LICENSE \
        README.md ROADMAP.md THIRD_PARTY_NOTICES.md config contrib crates deny.toml docs \
        rust-toolchain.toml rustfmt.toml scripts | \
        tar -xf - -C "$staged_source"
fi
printf '%s\n' "$source_commit" >"$staged_source/SOURCE_COMMIT"
(
    cd "$staged_source"
    mkdir -p -- .cargo
    cargo vendor --locked --versioned-dirs vendor >.cargo/config.toml
)

lsw_export_reproducible_rustflags "$staged_source"
(
    cd "$staged_source"
    cargo build --locked --offline --release --bin lsw --bin lswd --bin lswg
)
linker_info="$staging_directory/windows-linker.txt"
windows_agent=$(LSW_WINDOWS_LINKER_INFO="$linker_info" "$staged_source/scripts/build-windows-agent.sh")
windows_linker=$(sed -n 's/^linker=//p' "$linker_info")
windows_linker_version=$(sed -n 's/^version=//p' "$linker_info")
rustc_version=$(rustc -vV | awk '/^release:/ { print $2 }')
rustc_commit=$(rustc -vV | awk '/^commit-hash:/ { print $2 }')
cargo_version=$(cargo --version | awk '{ print $2 }')
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
bundle_directory="$distribution_directory/$bundle_name"
archive="$distribution_directory/$bundle_name.tar.gz"
checksum="$archive.sha256"

if [ -e "$bundle_directory" ] || [ -e "$archive" ] || [ -e "$checksum" ]; then
    echo "error: release output for $bundle_name already exists; move it aside first" >&2
    exit 1
fi

staged_bundle="$staging_directory/$bundle_name"
staged_tar="$staging_directory/$bundle_name.tar"
staged_archive="$staging_directory/$bundle_name.tar.gz"
staged_checksum="$staged_archive.sha256"
install -d -m 0755 -- "$staged_bundle/docs" "$staged_bundle/systemd"
mv "$staged_source" "$staged_bundle/source"
install -m 0755 -- "$cargo_target_directory/release/lsw" "$staged_bundle/lsw"
install -m 0755 -- "$cargo_target_directory/release/lswd" "$staged_bundle/lswd"
install -m 0755 -- "$cargo_target_directory/release/lswg" "$staged_bundle/lswg"
install -m 0644 -- "$windows_agent" "$staged_bundle/lsw-agent.exe"
lsw_sha256=$(sha256sum -- "$staged_bundle/lsw" | awk '{ print $1; exit }')
lswd_sha256=$(sha256sum -- "$staged_bundle/lswd" | awk '{ print $1; exit }')
lswg_sha256=$(sha256sum -- "$staged_bundle/lswg" | awk '{ print $1; exit }')
agent_sha256=$(sha256sum -- "$staged_bundle/lsw-agent.exe" | awk '{ print $1; exit }')
printf '%s\n' "$lswg_sha256" >"$staged_bundle/lswg.sha256"
install -m 0755 -- scripts/install.sh "$staged_bundle/install.sh"
install -m 0644 -- contrib/systemd/lswd.service contrib/systemd/lswd.socket \
    "$staged_bundle/systemd/"
install -m 0644 -- README.md ROADMAP.md CHANGELOG.md LICENSE THIRD_PARTY_NOTICES.md \
    "$staged_bundle/"
install -m 0644 -- docs/ARCHITECTURE.md docs/BETA.md docs/LEGAL_BOUNDARIES.md \
    docs/DEVELOPMENT.md docs/REFERENCES.md docs/SECURITY.md \
    docs/WINDOWS_KVM_E2E.md "$staged_bundle/docs/"
(
    cd "$staged_bundle/source"
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
    "SOURCE_COMMIT=$source_commit" \
    'HOST_TARGET=x86_64-unknown-linux-gnu' \
    'GUEST_AGENT_TARGET=x86_64-pc-windows-gnu' \
    "RUSTC_VERSION=$rustc_version ($rustc_commit)" \
    "CARGO_VERSION=$cargo_version" \
    "WINDOWS_LINKER=$windows_linker ($windows_linker_version)" \
    'SOURCE_PATH_PREFIX=/lsw' \
    "LSW_SHA256=$lsw_sha256" \
    "LSWD_SHA256=$lswd_sha256" \
    "LSWG_SHA256=$lswg_sha256" \
    "WINDOWS_AGENT_SHA256=$agent_sha256" \
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
