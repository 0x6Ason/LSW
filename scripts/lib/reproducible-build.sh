# SPDX-License-Identifier: GPL-3.0-or-later
# shellcheck shell=sh
# Helpers shared by the release builder and the guest-agent cross-builder so
# both compile with the same path remapping and describe the same source.

# Appends the --remap-path-prefix that turns the absolute source root into
# /lsw inside every compiled artifact. Panic locations and debug paths then
# match between two checkouts, which is what byte-for-byte reproducibility
# of the release binaries requires.
#
# Usage: lsw_export_reproducible_rustflags SOURCE_ROOT
lsw_export_reproducible_rustflags() {
    lsw_remap_flag="--remap-path-prefix=$1=/lsw"
    case " ${RUSTFLAGS:-} " in
        *" $lsw_remap_flag "*) ;;
        *) RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$lsw_remap_flag" ;;
    esac
    export RUSTFLAGS
}

# Prints the commit that SOURCE_ROOT corresponds to: the git HEAD with a
# -dirty suffix when tracked files are modified, the SOURCE_COMMIT file a
# previous release bundle recorded, or "unknown".
#
# Usage: lsw_source_commit SOURCE_ROOT
lsw_source_commit() {
    if git -C "$1" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        lsw_commit=$(git -C "$1" rev-parse HEAD)
        if [ -n "$(git -C "$1" status --porcelain --untracked-files=no)" ]; then
            lsw_commit="$lsw_commit-dirty"
        fi
        printf '%s\n' "$lsw_commit"
    elif [ -f "$1/SOURCE_COMMIT" ]; then
        sed -n '1p' "$1/SOURCE_COMMIT"
    else
        echo unknown
    fi
}

# Whether SOURCE_ROOT carries a vendored dependency tree that cargo can use
# without network access.
#
# Usage: lsw_source_is_vendored SOURCE_ROOT
lsw_source_is_vendored() {
    [ -f "$1/.cargo/config.toml" ] \
        && grep -Fx 'directory = "vendor"' "$1/.cargo/config.toml" >/dev/null 2>&1
}
