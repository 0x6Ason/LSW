#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
set -eu

if [ "$#" -ne 6 ]; then
    echo "usage: run-windows-slim-profile-audit.sh LSW INSTANCE PHASE OUTPUT SETTLE_SECONDS REPORT_DESTINATION" >&2
    exit 2
fi

lsw=$1
instance=$2
phase=$3
output=$4
settle_seconds=$5
report_destination=$6
workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
checker="$workspace_root/scripts/check-windows-slim-profile.ps1"
guest_checker='C:\ProgramData\LSW\profile\lsw-e2e-slim-profile-check.ps1'
checker_staged=0

for required_command in cat chmod dirname mv python3 rm; do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: required command $required_command was not found" >&2
        exit 1
    fi
done
if [ ! -x "$lsw" ] || [ ! -f "$checker" ]; then
    echo "error: the LSW binary or slim-profile checker is unavailable" >&2
    exit 1
fi
case "$phase" in
    boot-1|boot-2|boot-3) ;;
    *) echo "error: profile audit phase must be boot-1, boot-2, or boot-3" >&2; exit 1 ;;
esac
case "$settle_seconds" in
    ''|*[!0-9]*) echo "error: settle seconds must be an integer" >&2; exit 1 ;;
esac
if [ "$settle_seconds" -gt 300 ]; then
    echo "error: settle seconds must not exceed 300" >&2
    exit 1
fi
case "$output" in
    /*) ;;
    *) echo "error: profile audit output must be an absolute path" >&2; exit 1 ;;
esac
output_parent=$(dirname -- "$output")
if [ ! -d "$output_parent" ] || [ -L "$output_parent" ] || [ -e "$output" ] || [ -L "$output" ]; then
    echo "error: profile audit output must be new beneath a real directory" >&2
    exit 1
fi
output_tmp="$output.tmp.$$"
if [ -e "$output_tmp" ] || [ -L "$output_tmp" ]; then
    echo "error: profile audit temporary output already exists" >&2
    exit 1
fi

cleanup_checker() {
    rm -f -- "$output_tmp"
    if [ "$checker_staged" -eq 1 ]; then
        "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -NonInteractive \
            -Command "Remove-Item -LiteralPath '$guest_checker' -Force -ErrorAction SilentlyContinue" \
            >/dev/null 2>&1 || :
    fi
}
trap cleanup_checker EXIT
trap 'exit 130' HUP INT TERM

"$lsw" push "$instance" "$checker" "$guest_checker" >/dev/null
checker_staged=1
set +e
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -NonInteractive \
    -ExecutionPolicy Bypass -File "$guest_checker" -SettleSeconds "$settle_seconds" \
    >"$output_tmp"
audit_status=$?
set -e
"$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -NonInteractive \
    -Command "Remove-Item -LiteralPath '$guest_checker' -Force -ErrorAction Stop" >/dev/null
checker_staged=0
if [ "$audit_status" -ne 0 ]; then
    cat "$output_tmp" >&2
    echo "error: Windows slim-profile audit failed during $phase" >&2
    exit 1
fi

python3 - "$output_tmp" "$phase" <<'PY'
import json
from pathlib import Path
import sys

path = Path(sys.argv[1])
phase = sys.argv[2]
try:
    result = json.loads(path.read_text(encoding="utf-8-sig"))
except (OSError, UnicodeError, json.JSONDecodeError) as error:
    raise SystemExit(f"error: invalid slim-profile audit JSON: {error}")
if result.get("schema_version") != 1:
    raise SystemExit("error: slim-profile audit schema is not version 1")
if result.get("profile") != "slim" or result.get("revision") != "slim-v2":
    raise SystemExit("error: slim-profile audit identity is not slim-v2")
if result.get("outcome") != "passed":
    raise SystemExit("error: slim-profile audit did not pass")
for field in (
    "process_count",
    "committed_bytes",
    "working_set_bytes",
    "system_volume_used_bytes",
    "provisioned_appx_count",
    "installed_appx_count",
    "targeted_appx_count",
    "targeted_service_count",
    "policy_count",
):
    value = result.get(field)
    if not isinstance(value, int) or value < 0:
        raise SystemExit(f"error: slim-profile audit field {field} is invalid")
if result["targeted_appx_count"] < 40 or result["targeted_service_count"] < 10:
    raise SystemExit("error: slim-profile audit target set is unexpectedly small")
if result["policy_count"] < 20:
    raise SystemExit("error: slim-profile audit policy set is unexpectedly small")
print(
    f"Slim-v2 {phase} audit passed: {result['process_count']} processes, "
    f"{result['committed_bytes']} committed bytes, "
    f"{result['system_volume_used_bytes']} system-volume bytes used."
)
PY

chmod 600 "$output_tmp"
mv -- "$output_tmp" "$output"

if [ -n "$report_destination" ]; then
    case "$report_destination" in
        /*) ;;
        *) echo "error: profile report destination must be absolute" >&2; exit 1 ;;
    esac
    report_parent=$(dirname -- "$report_destination")
    if [ ! -d "$report_parent" ] || [ -L "$report_parent" ] \
        || [ -e "$report_destination" ] || [ -L "$report_destination" ]; then
        echo "error: profile report destination must be new beneath a real directory" >&2
        exit 1
    fi
    "$lsw" pull "$instance" --recursive 'C:\ProgramData\LSW\profile' \
        "$report_destination" >/dev/null
    chmod -R go-rwx "$report_destination"
fi

trap - EXIT HUP INT TERM
