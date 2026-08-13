#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: scripts/verify-release.sh DIST_ARCHIVE.tar.gz" >&2
    exit 1
fi

for required_command in awk basename cmp file find gzip grep mktemp python3 rm sed sha256sum sort tar uname uniq; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done

archive=$1
if [ ! -f "$archive" ]; then
    echo "error: archive does not exist: $archive" >&2
    exit 1
fi
archive_directory=$(CDPATH='' cd -- "$(dirname -- "$archive")" && pwd)
archive_name=$(basename -- "$archive")
archive="$archive_directory/$archive_name"
checksum_file="$archive.sha256"

if [ ! -f "$checksum_file" ]; then
    echo "error: checksum sidecar does not exist: $checksum_file" >&2
    exit 1
fi
if ! awk -v archive_name="$archive_name" '
    NR == 1 && length($1) == 64 && $1 ~ /^[0-9a-f]+$/ && $2 == archive_name {
        valid = 1
        next
    }
    { valid = 0; exit }
    END { exit(valid ? 0 : 1) }
' "$checksum_file"; then
    echo "error: checksum sidecar must contain exactly one SHA-256 entry for $archive_name" >&2
    exit 1
fi
expected_checksum=$(awk 'NR == 1 { print $1 }' "$checksum_file")
actual_checksum=$(sha256sum "$archive" | awk '{ print $1 }')
if [ "$actual_checksum" != "$expected_checksum" ]; then
    echo "error: SHA-256 mismatch for $archive_name" >&2
    exit 1
fi

gzip -t "$archive"
verification_directory=$(mktemp -d -- "${TMPDIR:-/tmp}/lsw-release-verify.XXXXXX")
cleanup_release_verification() {
    rm -rf -- "$verification_directory"
}
trap cleanup_release_verification EXIT HUP INT TERM
member_list="$verification_directory/archive-members.txt"
normalized_member_list="$verification_directory/archive-members.normalized.txt"
tar -tzf "$archive" >"$member_list"
if [ ! -s "$member_list" ]; then
    echo "error: release archive is empty" >&2
    exit 1
fi

bundle_name=$(awk -v normalized_member_list="$normalized_member_list" '
    {
        member = $0
        while (sub(/^\.\//, "", member)) { }
        if (member == "" || member ~ /^\//) {
            exit 2
        }
        count = split(member, parts, "/")
        normalized = ""
        for (field_index = 1; field_index <= count; field_index++) {
            if (parts[field_index] == "..") {
                exit 2
            }
            if (parts[field_index] == "" || parts[field_index] == ".") {
                continue
            }
            normalized = normalized == "" \
                ? parts[field_index] \
                : normalized "/" parts[field_index]
        }
        if (normalized == "") {
            exit 2
        }
        print normalized >> normalized_member_list
        split(normalized, normalized_parts, "/")
        if (root == "") {
            root = normalized_parts[1]
        } else if (root != normalized_parts[1]) {
            exit 3
        }
    }
    END {
        if (root == "") {
            exit 4
        }
        print root
    }
' "$member_list") || {
    echo "error: archive members are unsafe or do not share one top-level directory" >&2
    exit 1
}
duplicate_member=$(LC_ALL=C sort "$normalized_member_list" | uniq -d | sed -n '1p')
if [ -n "$duplicate_member" ]; then
    echo "error: archive contains a duplicate member path: $duplicate_member" >&2
    exit 1
fi
case "$bundle_name" in
    lsw-*-linux-x86_64) ;;
    *)
        echo "error: unexpected release bundle name: $bundle_name" >&2
        exit 1
        ;;
esac
if ! tar -tvzf "$archive" | awk '
    substr($0, 1, 1) != "d" && substr($0, 1, 1) != "-" { exit 1 }
'; then
    echo "error: archive contains a link or special filesystem node" >&2
    exit 1
fi

tar --no-same-owner --no-same-permissions -xzf "$archive" -C "$verification_directory"
bundle="$verification_directory/$bundle_name"
unexpected_node=$(find "$bundle" ! -type d ! -type f -print -quit)
if [ -n "$unexpected_node" ]; then
    echo "error: archive contains a link or special filesystem node: $unexpected_node" >&2
    exit 1
fi

for required_file in \
    BUILDINFO.txt CHANGELOG.md LICENSE README.md SOURCE-MANIFEST.sha256 \
    install.sh lsw lsw-agent.exe lswd \
    docs/DEVELOPMENT.md \
    source/Cargo.lock source/Cargo.toml source/LICENSE source/README.md \
    source/crates/lsw-agent/Cargo.toml source/crates/lsw-agent/src/main.rs \
    source/crates/lsw-cli/Cargo.toml source/crates/lsw-cli/src/main.rs \
    source/crates/lsw-core/Cargo.toml source/crates/lsw-core/src/lib.rs \
    source/crates/lsw-daemon/Cargo.toml source/crates/lsw-daemon/src/main.rs \
    source/scripts/build-release.sh source/scripts/build-windows-agent.sh \
    source/scripts/normalize-pe-timestamp.py \
    source/scripts/zig-windows-linker.sh; do
    if [ ! -f "$bundle/$required_file" ]; then
        echo "error: archive is missing $required_file" >&2
        exit 1
    fi
done
for executable in install.sh lsw lswd; do
    if [ ! -x "$bundle/$executable" ]; then
        echo "error: archive entry is not executable: $executable" >&2
        exit 1
    fi
done

release_version=$(sed -n 's/^LSW_VERSION=//p' "$bundle/BUILDINFO.txt")
if [ -z "$release_version" ] || [ "$bundle_name" != "lsw-$release_version-linux-x86_64" ]; then
    echo "error: BUILDINFO version and bundle directory do not agree" >&2
    exit 1
fi
case "$release_version" in
    ''|*[!0-9A-Za-z.+-]*)
        echo "error: BUILDINFO contains an unsafe release version: $release_version" >&2
        exit 1
        ;;
esac
if ! grep -Fx 'LICENSE=GPL-3.0-or-later' "$bundle/BUILDINFO.txt" >/dev/null \
    || ! grep -Fx 'CORRESPONDING_SOURCE=source/' "$bundle/BUILDINFO.txt" >/dev/null \
    || ! grep -Fx 'HOST_TARGET=x86_64-unknown-linux-gnu' "$bundle/BUILDINFO.txt" >/dev/null \
    || ! grep -Fx 'GUEST_AGENT_TARGET=x86_64-pc-windows-gnu' "$bundle/BUILDINFO.txt" >/dev/null; then
    echo "error: BUILDINFO is incomplete or has unexpected release metadata" >&2
    exit 1
fi
if ! cmp -s "$bundle/LICENSE" "$bundle/source/LICENSE"; then
    echo "error: binary bundle and corresponding source contain different license texts" >&2
    exit 1
fi
if ! (cd "$bundle/source" && sha256sum --check ../SOURCE-MANIFEST.sha256 >/dev/null); then
    echo "error: corresponding source is missing or differs from its source manifest" >&2
    exit 1
fi
source_version=$(awk '
    $0 == "[workspace.package]" { in_workspace_package = 1; next }
    /^\[/ { in_workspace_package = 0 }
    in_workspace_package && $1 == "version" && $2 == "=" {
        version = $3
        sub(/^"/, "", version)
        sub(/"$/, "", version)
        print version
        exit
    }
' "$bundle/source/Cargo.toml")
if [ "$source_version" != "$release_version" ]; then
    echo "error: corresponding source version does not match the release binary" >&2
    exit 1
fi
if ! grep -F 'license = "GPL-3.0-or-later"' "$bundle/source/Cargo.toml" >/dev/null; then
    echo "error: corresponding source does not declare GPL-3.0-or-later" >&2
    exit 1
fi

if ! file "$bundle/lsw" | grep -E 'ELF 64-bit LSB.*x86-64' >/dev/null \
    || ! file "$bundle/lswd" | grep -E 'ELF 64-bit LSB.*x86-64' >/dev/null; then
    echo "error: archive host binaries are not Linux x86_64 ELF executables" >&2
    exit 1
fi
if ! file "$bundle/lsw-agent.exe" | grep -E 'PE32\+ executable.*x86-64' >/dev/null; then
    echo "error: archive guest agent is not a Windows x86_64 PE executable" >&2
    exit 1
fi
python3 "$bundle/source/scripts/normalize-pe-timestamp.py" \
    --check "$bundle/lsw-agent.exe"

# Legal media boundaries are also enforced at package verification time. The
# source and release bundle must not acquire an OS installer or VM disk image.
bundled_media=$(find "$bundle" -type f \( \
    -iname '*.esd' -o -iname '*.iso' -o -iname '*.qcow' -o -iname '*.qcow2' \
    -o -iname '*.vdi' -o -iname '*.vhd' -o -iname '*.vhdx' -o -iname '*.vmdk' \
    -o -iname '*.wim' \) -print -quit)
if [ -n "$bundled_media" ]; then
    echo "error: release contains prohibited OS media or a VM disk image: $bundled_media" >&2
    exit 1
fi

if [ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ]; then
    "$bundle/lsw" --version | grep -Fx "lsw $release_version" >/dev/null
    "$bundle/lsw" help >/dev/null
    install_test_prefix="$verification_directory/install-root"
    LSW_INSTALL_PREFIX="$install_test_prefix" "$bundle/install.sh" >/dev/null
    if ! cmp -s "$bundle/lsw" "$install_test_prefix/bin/lsw" \
        || ! cmp -s "$bundle/lswd" "$install_test_prefix/bin/lswd" \
        || ! cmp -s "$bundle/lsw-agent.exe" \
            "$install_test_prefix/libexec/lsw/lsw-agent.exe"; then
        echo "error: install.sh did not reproduce the release binaries" >&2
        exit 1
    fi
    "$install_test_prefix/bin/lsw" --version \
        | grep -Fx "lsw $release_version" >/dev/null
else
    echo "note: skipped Linux x86_64 host-binary smoke tests on this verifier host" >&2
fi

echo "Verified release bundle: $bundle_name"
echo "SHA-256: $actual_checksum"
