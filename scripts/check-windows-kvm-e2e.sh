#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu
workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
lsw=${LSW_E2E_LSW:-"$workspace_root/target/release/lsw"}
lswd=${LSW_E2E_LSWD:-"$workspace_root/target/release/lswd"}
lswg=${LSW_E2E_LSWG:-"$workspace_root/target/release/lswg"}
iso=${LSW_WINDOWS_ISO:-}
edition=${LSW_WINDOWS_EDITION:-pro}
profile=${LSW_WINDOWS_PROFILE:-slim}
agent=${LSW_WINDOWS_AGENT:-"$workspace_root/target/x86_64-pc-windows-gnu/release/lsw-agent.exe"}
timeout_seconds=${LSW_E2E_TIMEOUT_SECONDS:-18000}
# Cold Windows boots use a bounded ten-minute agent wait. Give the external
# command guard one additional minute so it never preempts the product timeout.
agent_boot_timeout_seconds=660
root_base=${LSW_E2E_ROOT_BASE:-/tmp}
artifact_dir=${LSW_E2E_ARTIFACT_DIR:-}
active_root_file=${LSW_E2E_ACTIVE_ROOT_FILE:-}
keep_state=${LSW_E2E_KEEP_STATE:-0}
gui_handoff=${LSW_E2E_GUI_HANDOFF:-0}
candidate_sha=${LSW_E2E_CANDIDATE_SHA:-${GITHUB_SHA:-}}
expected_iso_sha256=${LSW_WINDOWS_ISO_SHA256:-}
e2e_no_viewer=${LSW_E2E_NO_VIEWER:-1}
for required_command in awk chmod cmp date du grep kill mkdir mktemp mv python3 rm setsid sha256sum sleep timeout tr uname; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done
if [ ! -c /dev/kvm ] || [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
    echo "error: the real Windows gate requires readable and writable /dev/kvm" >&2
    exit 1
fi
if [ -z "$iso" ] || [ ! -f "$iso" ]; then
    echo "error: set LSW_WINDOWS_ISO to a licensed Windows 11 x64 ISO" >&2
    exit 1
fi
if [ ! -f "$agent" ]; then
    echo "error: set LSW_WINDOWS_AGENT to the matching lsw-agent.exe" >&2
    exit 1
fi
if [ ! -x "$lsw" ] || [ ! -x "$lswd" ] || [ ! -x "$lswg" ]; then
    echo "error: build release lsw, lswd, and lswg binaries before running this gate" >&2
    exit 1
fi
case "$timeout_seconds" in
    ''|*[!0-9]*)
        echo "error: LSW_E2E_TIMEOUT_SECONDS must be a positive integer" >&2
        exit 1
        ;;
esac
if [ "$timeout_seconds" -eq 0 ]; then
    echo "error: LSW_E2E_TIMEOUT_SECONDS must be greater than zero" >&2
    exit 1
fi
case "$keep_state" in
    0|1) ;;
    *)
        echo "error: LSW_E2E_KEEP_STATE must be 0 or 1" >&2
        exit 1
        ;;
esac
case "$gui_handoff" in
    0|1) ;;
    *)
        echo "error: LSW_E2E_GUI_HANDOFF must be 0 or 1" >&2
        exit 1
        ;;
esac
if [ "$gui_handoff" = 1 ] \
    && ! printf '%s\n' "$candidate_sha" | grep -Eq '^[0-9a-f]{40}$'
then
    echo "error: GUI handoff requires an exact lowercase candidate SHA" >&2
    exit 1
fi
case "$e2e_no_viewer" in
    0|1) ;;
    *)
        echo "error: LSW_E2E_NO_VIEWER must be 0 or 1" >&2
        exit 1
        ;;
esac
if [ ! -d "$root_base" ] || [ -L "$root_base" ]; then
    echo "error: LSW_E2E_ROOT_BASE must be an existing real directory" >&2
    exit 1
fi
root_base=$(CDPATH='' cd -- "$root_base" && pwd)
if [ "$root_base" = / ]; then
    echo "error: LSW_E2E_ROOT_BASE must not be the filesystem root" >&2
    exit 1
fi
if [ -n "$artifact_dir" ]; then
    if [ -e "$artifact_dir" ] && { [ ! -d "$artifact_dir" ] || [ -L "$artifact_dir" ]; }; then
        echo "error: LSW_E2E_ARTIFACT_DIR must be a real directory" >&2
        exit 1
    fi
    mkdir -p -- "$artifact_dir"
    chmod 700 "$artifact_dir"
    artifact_dir=$(CDPATH='' cd -- "$artifact_dir" && pwd)
fi
if [ -n "$active_root_file" ]; then
    case "$active_root_file" in
        /*) ;;
        *)
            echo "error: LSW_E2E_ACTIVE_ROOT_FILE must be an absolute path" >&2
            exit 1
            ;;
    esac
    if [ -e "$active_root_file" ] || [ -L "$active_root_file" ]; then
        echo "error: LSW_E2E_ACTIVE_ROOT_FILE already exists" >&2
        exit 1
    fi
    active_root_parent=${active_root_file%/*}
    if [ ! -d "$active_root_parent" ] || [ -L "$active_root_parent" ]; then
        echo "error: LSW_E2E_ACTIVE_ROOT_FILE parent must be a real directory" >&2
        exit 1
    fi
fi
expected_iso_sha256=$(printf '%s' "$expected_iso_sha256" | tr 'A-F' 'a-f')
if [ -n "$expected_iso_sha256" ]; then
    case "$expected_iso_sha256" in
        *[!0-9a-f]*)
            echo "error: LSW_WINDOWS_ISO_SHA256 must contain 64 hexadecimal characters" >&2
            exit 1
            ;;
    esac
    if [ "${#expected_iso_sha256}" -ne 64 ]; then
        echo "error: LSW_WINDOWS_ISO_SHA256 must contain 64 hexadecimal characters" >&2
        exit 1
    fi
fi
# The inner process owns exact process groups and cleanup. Leave additional
# time for dependency checks and bounded teardown around the full validation
# deadline. Official-media DISM export, mount, and commit can legitimately take
# more than two hours on a dedicated disk without being stalled.
if [ "${LSW_WINDOWS_KVM_E2E_TIMEOUT_ACTIVE:-0}" != 1 ]; then
    LSW_WINDOWS_KVM_E2E_TIMEOUT_ACTIVE=1
    export LSW_WINDOWS_KVM_E2E_TIMEOUT_ACTIVE
    overall_timeout=$((timeout_seconds + 1200))
    # Cleanup can include a bounded guest stop, evidence collection, and removal
    # of a large sparse disk tree. Keep five minutes between TERM and KILL while
    # still finishing well inside the workflow's 360-minute job timeout.
    exec timeout --signal=TERM --kill-after=300s "${overall_timeout}s" "$0" "$@"
fi

iso_sha256=$(sha256sum "$iso" | awk '{ print $1 }')
if [ -n "$expected_iso_sha256" ] && [ "$iso_sha256" != "$expected_iso_sha256" ]; then
    echo "error: Windows ISO SHA-256 does not match LSW_WINDOWS_ISO_SHA256" >&2
    exit 1
fi

e2e_root=$(mktemp -d -- "$root_base/lsw-e2e.XXXXXX")
case "$e2e_root" in
    "$root_base"/lsw-e2e.??????) ;;
    *)
        echo "error: mktemp returned an unexpected E2E directory" >&2
        exit 1
        ;;
esac
profile_audit_dir="$e2e_root/profile-audits"
profile_report_destination=
mkdir -p -- "$profile_audit_dir"
chmod 700 "$profile_audit_dir"
if [ -n "$artifact_dir" ]; then
    profile_audit_dir="$artifact_dir/profile-audits"
    profile_report_destination="$artifact_dir/profile"
    mkdir -p -- "$profile_audit_dir"
    chmod 700 "$profile_audit_dir"
fi
if [ -n "$active_root_file" ]; then
    printf '%s\n' "$e2e_root" >"$active_root_file"
    chmod 600 "$active_root_file"
fi
instance="windows-kvm-e2e-$$"
export LSW_STATE_DIR="$e2e_root/state"
export LSW_WINDOWS_AGENT="$agent"
export LSW_E2E_LSW="$lsw"
export LSW_E2E_INSTANCE="$instance"
autospawn_blocker="$e2e_root/lswd-autospawn-disabled"
cold_daemon_pid_file="$e2e_root/cold-daemon.pid"
daemon_pid_file="$e2e_root/lswd.pid"
export LSW_DAEMON="$autospawn_blocker"
daemon_pid=
daemon_keepalive_pid=
viewer_pid=
sync_pid=
artifacts_collected=0
gui_handoff_complete=0
gui_instance=none
guest_build=unknown
guest_edition=unknown
setup_account_removed=unknown
agent_service_sid=unknown
cold_agent_service_sid=unknown
agent_service_pid=unknown
cold_agent_service_pid=unknown
agent_service_name=unknown
agent_service_start_mode=unknown
agent_service_start_name=unknown
agent_service_state=unknown
cold_interactive_user=unknown
initial_interactive_user=unknown
cached_unattend_removed=unknown
setup_payload_removed=unknown
automatic_logon=unknown
desktop_user_registered=unknown
desktop_user_role=unknown
separate_administrator_created=unknown
windows_sudo_force_new_window=unknown
windows_sudo_policy=unknown
windows_uac_enabled=unknown
clone_identity_isolated=unknown
folder_share_boundaries=unknown
live_folder_verified=unknown
resource_governor_verified=unknown
ovmf_code_sha256=unknown
exec_context_verified=unknown
signal_status=unknown
detached_run_verified=unknown
recursive_transfer_verified=unknown
watch_sync_verified=unknown
ovmf_vars_sha256=unknown
official_iso_sha256=unknown
license_status=unknown
license_helper_start_mode=unknown
license_helper_start_name=unknown
user_helper_start_mode=unknown
user_helper_start_name=unknown
maintenance_helper_start_mode=unknown
maintenance_helper_start_name=unknown
profile_audit_boots=0
profile_revision=not-applicable
profile_host_allocated_bytes=unknown

python3 - "$LSW_STATE_DIR/instances/$instance/run/recovery-vnc.sock" <<'PY'
import os
import sys

path = os.fsencode(sys.argv[1])
if len(path) > 100:
    raise SystemExit(
        f"error: E2E runtime socket path is {len(path)} bytes; "
        "configure a shorter LSW_E2E_ROOT_BASE"
    )
PY

# shellcheck source=scripts/e2e/windows-kvm-common.sh
. "$workspace_root/scripts/e2e/windows-kvm-common.sh"
# shellcheck source=scripts/e2e/windows-kvm-install.sh
. "$workspace_root/scripts/e2e/windows-kvm-install.sh"
# shellcheck source=scripts/e2e/windows-kvm-runtime.sh
. "$workspace_root/scripts/e2e/windows-kvm-runtime.sh"

trap cleanup_e2e EXIT
trap 'exit 130' HUP INT TERM

run_windows_kvm_install_scenario
run_windows_kvm_runtime_scenario
