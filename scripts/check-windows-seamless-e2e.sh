#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later

# Short, signed-in seamless-window acceptance test for an existing instance.
# This script never installs, starts, stops, creates, removes, or deletes a VM.

set -eu

overall_timeout_seconds=${LSW_SEAMLESS_E2E_TIMEOUT_SECONDS:-1800}
expected_guest_agent_sha=${LSW_SEAMLESS_E2E_EXPECTED_AGENT_SHA256:-}
case "$overall_timeout_seconds" in
    ''|*[!0-9]*)
        echo "error: LSW_SEAMLESS_E2E_TIMEOUT_SECONDS must be a positive integer" >&2
        exit 1
        ;;
esac
if [ "$overall_timeout_seconds" -eq 0 ]; then
    echo "error: LSW_SEAMLESS_E2E_TIMEOUT_SECONDS must be greater than zero" >&2
    exit 1
fi
case "$expected_guest_agent_sha" in
    ''|*[!0-9A-Fa-f]*)
        echo "error: LSW_SEAMLESS_E2E_EXPECTED_AGENT_SHA256 must be the exact 64-hex candidate guest-agent SHA" >&2
        exit 1
        ;;
esac
if [ "${#expected_guest_agent_sha}" -ne 64 ]; then
    echo "error: LSW_SEAMLESS_E2E_EXPECTED_AGENT_SHA256 must contain exactly 64 hex digits" >&2
    exit 1
fi

if ! command -v timeout >/dev/null 2>&1; then
    echo "error: GNU timeout is required for the seamless E2E driver" >&2
    exit 1
fi
if [ "${LSW_SEAMLESS_E2E_TIMEOUT_ACTIVE:-0}" != 1 ]; then
    LSW_SEAMLESS_E2E_TIMEOUT_ACTIVE=1
    export LSW_SEAMLESS_E2E_TIMEOUT_ACTIVE
    # The TERM trap performs bounded guest and host cleanup. Give it enough
    # time to finish rather than SIGKILLing it halfway through artifact removal.
    exec timeout --signal=TERM --kill-after=45s "${overall_timeout_seconds}s" sh "$0" "$@"
fi

if [ "$#" -gt 1 ]; then
    echo "usage: scripts/check-windows-seamless-e2e.sh [DEFAULT_INSTANCE]" >&2
    exit 1
fi

workspace_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
lsw=${LSW_E2E_LSW:-"$workspace_root/target/release/lsw"}
lswg=${LSW_E2E_LSWG:-"$workspace_root/target/release/lswg"}
fixture_source="$workspace_root/scripts/windows-seamless-fixture.ps1"
host_helper_source="$workspace_root/scripts/windows-seamless-host.ps1"
wslg_transport_probe_source="$workspace_root/scripts/check-wslg-transport.sh"
window_title_prefix='LSW Seamless Fixture '
window_title=
for required_command in \
    awk base64 convert date dirname env grep iconv identify kill mktemp powershell.exe rm rmdir \
    id setsid sha256sum sleep stat tail tr wsl.exe wslpath
do
    if ! command -v "$required_command" >/dev/null 2>&1; then
        echo "error: $required_command is required for the WSLg Wayland seamless E2E driver" >&2
        echo "error: provision the prerequisite outside this script; it never downloads or installs software" >&2
        exit 1
    fi
done
expected_guest_agent_sha=$(printf '%s' "$expected_guest_agent_sha" | tr 'A-F' 'a-f')
if [ -z "${WSL_DISTRO_NAME:-}" ] || [ -z "${WAYLAND_DISPLAY:-}" ]; then
    echo "error: this driver requires an interactive WSLg Wayland session (WSL_DISTRO_NAME and WAYLAND_DISPLAY)" >&2
    exit 1
fi
if [ ! -x "$lsw" ] || [ ! -x "$lswg" ]; then
    echo "error: LSW_E2E_LSW and LSW_E2E_LSWG must name executable candidate binaries" >&2
    exit 1
fi

# shellcheck source=scripts/e2e/windows-seamless-common.sh
. "$workspace_root/scripts/e2e/windows-seamless-common.sh"
# shellcheck source=scripts/e2e/windows-seamless-interaction.sh
. "$workspace_root/scripts/e2e/windows-seamless-interaction.sh"
# shellcheck source=scripts/e2e/windows-seamless-presenter.sh
. "$workspace_root/scripts/e2e/windows-seamless-presenter.sh"

cli_sha=$(candidate_sha256 "$lsw" lsw) || exit 1
lswg_sha=$(candidate_sha256 "$lswg" lswg) || exit 1
export LSWG="$lswg"
export LSWG_SHA256="$lswg_sha"
if [ ! -f "$fixture_source" ] || [ -L "$fixture_source" ]; then
    echo "error: the seamless fixture must be a regular non-symlink file: $fixture_source" >&2
    exit 1
fi
if [ ! -f "$host_helper_source" ] || [ -L "$host_helper_source" ]; then
    echo "error: the Windows host helper must be a regular non-symlink file: $host_helper_source" >&2
    exit 1
fi
if [ ! -f "$wslg_transport_probe_source" ] || [ -L "$wslg_transport_probe_source" ]; then
    echo "error: the WSLg transport probe must be a regular non-symlink file: $wslg_transport_probe_source" >&2
    exit 1
fi
host_helper_windows=$(wslpath -w "$host_helper_source") || {
    echo "error: could not translate the Windows host helper path" >&2
    exit 1
}

# WSLg's VAIL transport can retain a mounted but unusable virtiofs share after
# a host or WSL failure. Read the active RDP backend's transport identity first,
# then probe the exact system-distro mount before trusting VAIL. Current Weston
# automatically falls back to copy mode when its initial shared-memory
# allocation fails, while retaining the inherited VAIL environment variable;
# accept that fallback only when the current, freshly truncated Weston log
# reports use_gfxredir=0. A copy-mode Weston can legitimately coexist with a
# stale, unused mount and remains a supported fallback.
wslg_transport_probe=$(timeout 8s wsl.exe --system --user root --exec sh -s \
    <"$wslg_transport_probe_source" 2>/dev/null) \
    || fail "WSLg shared-memory transport is unavailable; stop active WSL workloads and run 'wsl --shutdown' before retrying"
case "$wslg_transport_probe" in
    vail|copy) ;;
    *) fail "WSLg transport preflight returned an invalid result" ;;
esac
printf 'WSLG_TRANSPORT=%s\n' "$wslg_transport_probe"

default_status=$(run_lsw 12 status) || fail "could not query the default LSW instance"
default_instance=$(printf '%s\n' "$default_status" | awk -F= '$1 == "instance" { print $2; exit }')
if [ -z "$default_instance" ]; then
    fail "lsw status did not report the default instance"
fi
instance=${1:-$default_instance}
if [ "$instance" != "$default_instance" ]; then
    echo "error: lsw cp addresses only the configured default instance" >&2
    echo "error: requested $instance but the default instance is $default_instance" >&2
    exit 1
fi

instance_status=$(run_lsw 12 status "$instance") || fail "could not query instance $instance"
if ! printf '%s\n' "$instance_status" | grep -Fx 'STATE=running' >/dev/null; then
    fail "instance $instance must already be running; this driver will not start it"
fi
if ! printf '%s\n' "$instance_status" | grep -Fx 'AGENT=ready' >/dev/null; then
    fail "instance $instance must already have a ready guest agent"
fi

instance_details=$(run_lsw 10 show "$instance") \
    || fail "could not inspect the registered Windows desktop identity"
desktop_user=$(printf '%s\n' "$instance_details" | awk -F ': +' \
    '$1 == "default Windows user" { print $2; found=1; exit } END { if (!found) exit 1 }') \
    || fail "instance $instance did not report its registered Windows desktop user"
[ "$desktop_user" != 'not registered' ] \
    || fail "instance $instance has no registered Windows desktop user"

# Normal exec intentionally runs as the restricted LSWAgent service identity and
# cannot authoritatively enumerate WTS users. A unique non-service Explorer
# session is only a bounded shell-readiness heuristic; the first real --gui
# request remains the fail-closed LocalSystem/WTS authority for $desktop_user.
# shellcheck disable=SC2016
signed_in_raw=$(run_guest_powershell 10 \
    '$ErrorActionPreference="Stop"; $Sessions=@(Get-Process -Name explorer | Where-Object { $_.SessionId -gt 0 } | Select-Object -ExpandProperty SessionId -Unique); if ($Sessions.Count -eq 1) { [Console]::Out.Write("SIGNED_IN_SESSION="+$Sessions[0]) } else { [Console]::Out.Write("SIGNED_IN_SESSION=unavailable") }') \
    || fail "instance $instance has no queryable interactive Windows shell"
signed_in_output=$(printf '%s\n' "$signed_in_raw" | tr -d '\r')
case "$signed_in_output" in
    SIGNED_IN_SESSION=[1-9]*) ;;
    *) fail "instance $instance must already expose one interactive Windows shell session" ;;
esac
pass signed_in_user

# The interactive GUI matrix is meaningless if the VM still runs an agent from
# a previous candidate. Hash the exact installed service binary before staging
# any fixture and fail closed on reparse points or path redirection.
# shellcheck disable=SC2016
guest_agent_hash_raw=$(run_guest_powershell 15 \
    '$ErrorActionPreference="Stop"; $Expected=[IO.Path]::GetFullPath("C:\Program Files\LSW\lsw-agent.exe"); $File=Get-Item -LiteralPath $Expected -Force; if ($File.FullName -cne $Expected) { throw "guest agent path changed" }; if (($File.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "guest agent must not be a reparse point" }; $Hash=(Get-FileHash -LiteralPath $Expected -Algorithm SHA256).Hash.ToLowerInvariant(); [Console]::Out.Write("GUEST_AGENT_SHA="+$Hash)') \
    || fail "could not attest the installed guest agent"
guest_agent_hash_output=$(printf '%s\n' "$guest_agent_hash_raw" | tr -d '\r')
guest_agent_sha=$(printf '%s\n' "$guest_agent_hash_output" | awk -F= \
    '$1 == "GUEST_AGENT_SHA" && $2 ~ /^[0-9a-f]+$/ && length($2) == 64 { print $2; exit }')
[ "$guest_agent_sha" = "$expected_guest_agent_sha" ] \
    || fail "guest agent SHA $guest_agent_sha did not match exact candidate $expected_guest_agent_sha"
pass guest_agent_exact_sha

stale_windows=$(run_windows_host 8 Find -TitleNeedle "$window_title_prefix" \
    -ProcessName msrdc 2>/dev/null) \
    || fail "could not inspect Windows for stale WSLg seamless fixture windows"
if [ -n "$stale_windows" ]; then
    fail "a stale WSLg LSW Seamless Fixture host window is already visible; close it before testing"
fi

temporary_base=${TMPDIR:-/tmp}
if [ ! -d "$temporary_base" ] || [ -L "$temporary_base" ]; then
    fail "TMPDIR must be an existing real directory"
fi
host_root=$(mktemp -d -- "$temporary_base/lsw-seamless-e2e.XXXXXX")
case "$host_root" in
    "$temporary_base"/lsw-seamless-e2e.??????) ;;
    *) fail "mktemp returned an unexpected seamless E2E directory" ;;
esac
host_root_uid=$(id -u)
host_root_identity=$(stat -Lc '%d:%i:%u:%a' -- "$host_root") \
    || fail "could not pin the seamless E2E host directory identity"
case "$host_root_identity" in
    *:"$host_root_uid":700) ;;
    *) fail "the seamless E2E host directory must be owned by this user with mode 0700" ;;
esac
exec 9<"$host_root" || fail "could not hold the seamless E2E host directory open"
host_root_fd_path="/proc/$$/fd/9"
host_root_fd_identity=$(stat -Lc '%d:%i:%u:%a' -- "$host_root_fd_path") \
    || fail "could not verify the pinned seamless E2E host directory handle"
[ "$host_root_fd_identity" = "$host_root_identity" ] \
    || fail "the seamless E2E host directory handle changed identity"
host_artifacts=

run_id="$(date +%s)-$$"
window_title="LSW Seamless Fixture $run_id - LSW"
guest_root="C:\\Users\\Public\\Documents\\LSW-Seamless-E2E-$run_id"
guest_fixture="$guest_root\\fixture.ps1"
guest_cleanup_signal="$guest_root\\cleanup.signal"
guest_owner_token_path="$guest_root\\owner.token"
guest_owner_token=$(printf '%s' "$run_id:$host_root:$cli_sha" | sha256sum | awk '{ print $1; exit }')
case "$guest_owner_token" in
    ''|*[!0-9a-f]*) fail "could not create the guest cleanup ownership token" ;;
    *) ;;
esac
[ "${#guest_owner_token}" -eq 64 ] || fail "guest cleanup ownership token is not SHA-256"
marker_main="$guest_root\\main.tsv"
marker_kill="$guest_root\\kill.tsv"
marker_recovery="$guest_root\\recovery.tsv"
marker_animate="$guest_root\\animate.tsv"
marker_error="$guest_root\\presenter-error.tsv"
marker_close="$guest_root\\close-prompt.tsv"
marker_host_close="$guest_root\\host-close.tsv"
marker_initial_max="$guest_root\\initial-maximized.tsv"
released_input_state='input-state=ctrl:0,shift:0,alt:0,left:0,middle:0,right:0,x1:0,x2:0'
initial_sentinel_color='#0C8CDC'
file_menu_color='#EC407A'
context_menu_color='#16A34A'
modal_color='#7E22CE'
resize_color='#F97316'
maximize_color='#DC2626'
close_prompt_color='#FACC15'
presenter_pid=
presenter_log=
window_id=
focus_sink_pid=
focus_sink_window=
focus_sink_title=
guest_root_created=0
cleanup_expected_markers=
matrix_complete=0

# Session 1: complete interactive behavior and graceful guest-caption close.
start_presenter "$marker_main" main
pass pointer_input_handshake
assert_guest_only_chrome "$window_id"
pass guest_only_windows_chrome
wait_marker_exact "$marker_main" 'dpi=96x96'
wait_marker_prefix "$marker_main" 'hit-test='
wait_marker_prefix "$marker_main" 'caption-controls='
hit_test_marker=$(guest_marker_text "$marker_main") \
    || fail "could not read guest hit-test diagnostics"
printf '%s\n' "$hit_test_marker" | awk -F '\t' \
    '$2 ~ /^hit-test=/ { print "HIT_TEST " $2; exit }'
caption_control_y=$(caption_control_field "$hit_test_marker" y) \
    || fail "guest fixture did not report a native caption-control Y coordinate"
caption_minimize_offset=$(caption_control_field "$hit_test_marker" minimize) \
    || fail "guest fixture did not report a native minimize-button coordinate"
caption_maximize_offset=$(caption_control_field "$hit_test_marker" maximize) \
    || fail "guest fixture did not report a native maximize-button coordinate"
caption_close_offset=$(caption_control_field "$hit_test_marker" close) \
    || fail "guest fixture did not report a native close-button coordinate"
printf 'CAPTION_CONTROLS y=%s minimize=%s maximize=%s close=%s\n' \
    "$caption_control_y" "$caption_minimize_offset" \
    "$caption_maximize_offset" "$caption_close_offset"
pass slice4_96_dpi_gate
initial_width=$(window_field "$window_id" WIDTH) || fail "could not read initial window width"
initial_height=$(window_field "$window_id" HEIGHT) || fail "could not read initial window height"
wait_marker_exact "$marker_main" 'initial-frame-ready'
wait_visual_sentinel initial "$initial_sentinel_color" 0.01 "$initial_width" "$initial_height"
pass first_window_frame
pass initial_visual_sentinel

click_percent "$window_id" 56 42
wait_marker_exact "$marker_main" modal-open
wait_marker_prefix "$marker_main" 'modal-window='
wait_marker_exact "$marker_main" 'modal-kind=ordinary-owned'
modal_marker_text=$(guest_marker_text "$marker_main") \
    || fail "could not read the modal HWND diagnostics"
printf '%s\n' "$modal_marker_text" | awk -F '\t' \
    '$2 ~ /^modal-window=/ { print "MODAL " $2; exit }'
wait_visual_sentinel modal "$modal_color" 0.01 "$initial_width" "$initial_height"
run_fixture_host 8 Chord -Hwnd "$window_id" -Key ENTER \
    || fail "could not close the guest modal with Enter"
wait_marker_exact "$marker_main" modal-close
pass modal_visible
pass modal_open_close

resize_marker_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-resize markers"
resize_marker_before=$(marker_exact_count "$resize_marker_text" 'frame-size=900x650')
resize_sentinel_before=$(marker_exact_count "$resize_marker_text" 'resize-sentinel=visible')
# Window placement is shell state and can persist near any monitor edge between
# local runs. Move through the guest caption so both the RAIL HWND and Wayland
# surface share a bounded work-area position before growing the guest border.
place_fixture_via_guest_caption "before-initial-resize"
ensure_pointer_ready "$marker_main" "pre-initial-resize"
resize_via_guest_southeast 900 650
wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((resize_marker_before + 1))"
wait_marker_exact_count "$marker_main" 'resize-sentinel=visible' "$((resize_sentinel_before + 1))"
wait_marker_exact "$marker_main" 'frame-size=900x650'
wait_visual_sentinel resized "$resize_color" 0.005 900 650
pass resize
pass resize_frame_ready
resize_reset_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-resize-reset markers"
resize_reset_before=$(marker_exact_count \
    "$resize_reset_text" "frame-size=${initial_width}x${initial_height}")
resize_via_guest_southeast "$initial_width" "$initial_height"
wait_marker_exact_count "$marker_main" \
    "frame-size=${initial_width}x${initial_height}" "$((resize_reset_before + 1))"
# The guest size marker can arrive before the corresponding frame crosses the
# agent/presenter pipeline. Prove the host is showing the reset-sized visual
# before the next pointer event so a stale aspect-fit transform cannot redirect
# the first post-resize click.
wait_visual_sentinel resize-reset "$initial_sentinel_color" 0.01 \
    "$initial_width" "$initial_height"
pass resize_reset_frame_ready

left_click_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-click markers"
left_click_before=$(marker_exact_count "$left_click_text" left-click)
click_percent "$window_id" 12 42
wait_marker_exact_count "$marker_main" left-click "$((left_click_before + 1))"
pass left_click
pass first_pointer_before_fixture_keyboard_input

click_percent "$window_id" 4 8
wait_visual_sentinel file-menu-open "$file_menu_color" 0.0005 \
    "$initial_width" "$initial_height"
click_percent "$window_id" 10 14
wait_marker_exact "$marker_main" file-menu
pass file_menu_visible
pass file_menu_item

move_percent "$window_id" 50 75
button_action Right
wait_marker_exact "$marker_main" right-button
wait_visual_sentinel context-menu-open "$context_menu_color" 0.0005 \
    "$initial_width" "$initial_height"
run_fixture_host 8 MoveRelative -Hwnd "$window_id" -DeltaX 85 -DeltaY 15 \
    || fail "could not move into the guest context menu"
button_action Left
wait_marker_exact "$marker_main" context-menu-item
pass right_click
pass context_menu_visible
pass context_menu_item
pass right_pointer_before_fixture_keyboard_input

# Rapid down/up pairs are a release gate, not a throughput benchmark. The old
# state-polling presenter could miss both edges when they arrived between polls,
# which made File, right-click, and typing appear randomly dead.
rapid_left_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-burst left-click markers"
rapid_left_before=$(marker_exact_count "$rapid_left_text" left-click)
move_percent "$window_id" 12 42
run_fixture_host 15 Burst -Hwnd "$window_id" -Button Left \
    -Repeat 100 -DelayMilliseconds 8 \
    || fail "could not send the rapid left-button edge burst"
wait_marker_exact_count "$marker_main" left-click "$((rapid_left_before + 100))"
sleep 0.5
rapid_left_final_text=$(guest_marker_text "$marker_main") \
    || fail "could not read settled rapid left-button markers"
rapid_left_final=$(marker_exact_count "$rapid_left_final_text" left-click)
[ "$rapid_left_final" -eq "$((rapid_left_before + 100))" ] \
    || fail "rapid left-button burst delivered $((rapid_left_final - rapid_left_before)) edges; expected exactly 100"
pass rapid_left_button_edges

rapid_right_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-burst right-button markers"
rapid_right_before=$(marker_exact_count "$rapid_right_text" right-button)
rapid_right_up_before=$(marker_exact_count "$rapid_right_text" right-button-up)
move_percent "$window_id" 50 75
run_fixture_host 20 Burst -Hwnd "$window_id" -Button Right \
    -Repeat 100 -DelayMilliseconds 25 \
    || fail "could not send the rapid right-button edge burst"
wait_marker_exact_count "$marker_main" right-button "$((rapid_right_before + 100))"
wait_marker_exact_count "$marker_main" right-button-up "$((rapid_right_up_before + 100))"
sleep 0.5
rapid_right_final_text=$(guest_marker_text "$marker_main") \
    || fail "could not read settled rapid right-button markers"
rapid_right_final=$(marker_exact_count "$rapid_right_final_text" right-button)
rapid_right_up_final=$(marker_exact_count "$rapid_right_final_text" right-button-up)
[ "$rapid_right_final" -eq "$((rapid_right_before + 100))" ] \
    || fail "rapid right-button burst delivered $((rapid_right_final - rapid_right_before)) down edges; expected exactly 100"
[ "$rapid_right_up_final" -eq "$((rapid_right_up_before + 100))" ] \
    || fail "rapid right-button burst delivered $((rapid_right_up_final - rapid_right_up_before)) up edges; expected exactly 100"
pass rapid_right_button_edges

move_percent "$window_id" 50 75
button_action Middle
wait_marker_exact "$marker_main" middle-button
pass middle_click

move_percent "$window_id" 50 75
button_action WheelUp
wait_marker_prefix "$marker_main" 'wheel='
pass wheel

move_percent "$window_id" 50 75
run_fixture_host 8 KeyDown -Hwnd "$window_id" -Key CTRL \
    || fail "could not hold Ctrl for the focus-release test"
button_action LeftDown
wait_marker_exact "$marker_main" 'input-held=ctrl,left'
start_focus_sink
wait_marker_exact "$marker_main" 'input-released=all'
run_windows_host 5 ReleaseAll 2>/dev/null || :
terminate_focus_sink
activate_window "$window_id"
click_percent "$window_id" 80 42
wait_marker_exact "$marker_main" focus-recovered
pass focus_loss_releases_input
pass focus_reactivation

rapid_key_text=$(awk 'BEGIN { for (counter = 0; counter < 100; counter++) printf "z" }')
click_percent "$window_id" 72 24
run_fixture_host 8 Type -Hwnd "$window_id" -Text "$rapid_key_text" \
    || fail "could not send the rapid Unicode key edge burst"
click_percent "$window_id" 34 42
rapid_key_base64=$(printf '%s' "$rapid_key_text" | base64 | tr -d '\n')
wait_marker_exact "$marker_main" "text-base64=$rapid_key_base64"
click_percent "$window_id" 34 42
wait_marker_exact "$marker_main" 'text-base64='
pass rapid_keyboard_edges
pass rapid_keyboard_text_cleared

click_percent "$window_id" 23 24
wait_marker_exact "$marker_main" source-selected
run_fixture_host 8 Chord -Hwnd "$window_id" -Key CTRL+C \
    || fail "could not send Ctrl+C to the seamless window"
clipboard_text=$(printf '%s' 'alpha beta' | base64 | tr -d '\n')
wait_marker_exact "$marker_main" ctrl-c-key-down
wait_marker_exact "$marker_main" "clipboard-copy-base64=$clipboard_text"
click_percent "$window_id" 72 24
run_fixture_host 8 Chord -Hwnd "$window_id" -Key CTRL+V \
    || fail "could not send Ctrl+V to the seamless window"
wait_marker_exact "$marker_main" ctrl-v-key-down
wait_marker_exact "$marker_main" "clipboard-paste-base64=$clipboard_text"
run_fixture_host 8 Type -Hwnd "$window_id" -Text ' typed' \
    || fail "could not type into the seamless window"
click_percent "$window_id" 34 42
expected_text=$(printf '%s' 'alpha beta typed' | base64 | tr -d '\n')
wait_marker_exact "$marker_main" "text-base64=$expected_text"
pass typing
pass guest_clipboard_ctrl_cv

resize_matrix_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-matrix-resize markers"
resize_matrix_before=$(marker_exact_count "$resize_matrix_text" 'frame-size=900x650')
resize_via_guest_southeast 900 650
wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((resize_matrix_before + 1))"

move_before_x=$(window_field "$window_id" X) || fail "could not read pre-drag X position"
move_before_y=$(window_field "$window_id" Y) || fail "could not read pre-drag Y position"
move_width=$(window_field "$window_id" WIDTH) || fail "could not read pre-drag width"
move_height=$(window_field "$window_id" HEIGHT) || fail "could not read pre-drag height"
run_fixture_host 8 Drag -Hwnd "$window_id" \
    -X "$((move_width * 55 / 100))" -Y "$((move_height / 40))" \
    -DeltaX 90 -DeltaY 70 -DelayMilliseconds 300 \
    || fail "could not drag the guest caption"
move_deadline=$(( $(date +%s) + 8 ))
move_changed=0
while [ "$(date +%s)" -lt "$move_deadline" ]; do
    move_after_x=$(window_field "$window_id" X || printf '%s' "$move_before_x")
    move_after_y=$(window_field "$window_id" Y || printf '%s' "$move_before_y")
    move_after_width=$(window_field "$window_id" WIDTH || printf '%s' "$move_width")
    move_after_height=$(window_field "$window_id" HEIGHT || printf '%s' "$move_height")
    move_delta_x=$((move_after_x - move_before_x))
    move_delta_y=$((move_after_y - move_before_y))
    if integer_near "$move_delta_x" 90 20 \
        && integer_near "$move_delta_y" 70 20 \
        && integer_near "$move_after_width" "$move_width" 4 \
        && integer_near "$move_after_height" "$move_height" 4
    then
        move_changed=1
        break
    fi
    sleep 0.2
done
if [ "$move_changed" -ne 1 ]; then
    fail "guest caption drag produced delta ${move_delta_x},${move_delta_y} and size ${move_after_width}x${move_after_height}; expected about +90,+70 with size ${move_width}x${move_height}"
fi
pass guest_caption_move

control_width=$(window_field "$window_id" WIDTH) || fail "could not read caption-control width"
click_at "$window_id" "$((control_width - caption_minimize_offset))" "$caption_control_y"
wait_window_state "$window_id" ICONIC 1
pass guest_caption_minimize
activate_window "$window_id"
wait_window_state "$window_id" ICONIC 0
# A synthetic SC_RESTORE can leave WSLg's new RAIL input mapping stale even
# after the HWND is visible. Reproduce the physical taskbar pointer leave on
# the restored proxy before entering it for the next guest-caption action.
run_fixture_host 8 PointerAway -Hwnd "$window_id" \
    || fail "could not move the Windows pointer outside the restored proxy"
sleep 0.7
# Current WSLg rebuilds the outer RAIL proxy on the first synthetic re-entry
# and its Weston pointer seat on the next leave/enter. Use an inert fixture
# background click for the first epoch; keep it in the center of the client so
# a guest-caption move cannot place the probe behind an always-on-top taskbar.
# No product behavior is asserted from that click, and the following caption
# action remains the input proof.
click_percent "$window_id" 50 50
run_fixture_host 8 PointerAway -Hwnd "$window_id" \
    || fail "could not complete the restored-proxy pointer re-entry"
sleep 0.7
pass guest_caption_unminimize

control_width=$(window_field "$window_id" WIDTH) || fail "could not read maximize-control width"
control_guest_frame=$(latest_guest_frame_size "$marker_main") \
    || fail "could not read the pre-maximize guest DWM frame"
control_guest_width=${control_guest_frame%% *}
control_guest_height=${control_guest_frame#* }
max_marker_text=$(guest_marker_text "$marker_main") || fail "could not read pre-maximize markers"
max_marker_before=$(marker_prefix_count "$max_marker_text" 'max-ready=')
max_state_before=$(marker_exact_count "$max_marker_text" 'window-state=Maximized')
click_guest_point "$window_id" "$control_guest_width" "$control_guest_height" \
    "$((control_guest_width - caption_maximize_offset))" "$caption_control_y"
wait_window_state "$window_id" ZOOMED 1
wait_marker_prefix_count "$marker_main" 'max-ready=' "$((max_marker_before + 1))"
wait_marker_exact_count "$marker_main" 'window-state=Maximized' "$((max_state_before + 1))"
maximized_width=$(window_field "$window_id" WIDTH) || fail "could not read maximized width"
maximized_height=$(window_field "$window_id" HEIGHT) || fail "could not read maximized height"
maximized_guest_frame=$(latest_guest_frame_size "$marker_main") \
    || fail "could not read the guest-caption maximized DWM frame"
maximized_guest_width=${maximized_guest_frame%% *}
maximized_guest_height=${maximized_guest_frame#* }
report_fitted_viewport MAXIMIZED_VIEWPORT \
    "$maximized_width" "$maximized_height" \
    "$maximized_guest_width" "$maximized_guest_height"
wait_visual_sentinel maximized "$maximize_color" 0.003 \
    "$maximized_width" "$maximized_height"
pass guest_caption_maximize
pass guest_window_state_maximized
pass maximize_frame_ready

restore_marker_text=$(guest_marker_text "$marker_main") || fail "could not read pre-restore markers"
restore_marker_before=$(marker_exact_count "$restore_marker_text" 'frame-size=900x650')
restore_state_before=$(marker_exact_count "$restore_marker_text" 'window-state=Normal')
control_width=$(window_field "$window_id" WIDTH) || fail "could not read restore-control width"
click_guest_point "$window_id" "$maximized_guest_width" "$maximized_guest_height" \
    "$((maximized_guest_width - caption_maximize_offset))" "$caption_control_y"
wait_window_state "$window_id" ZOOMED 0
wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((restore_marker_before + 1))"
wait_marker_exact_count "$marker_main" 'window-state=Normal' "$((restore_state_before + 1))"
wait_visual_sentinel restored "$resize_color" 0.005 900 650
pass guest_caption_restore
pass guest_window_state_restored
pass restore_frame_ready

# A native Windows shell/taskbar maximize is an outer-host state transition.
# It must travel back through winit and the protocol to the guest HWND, then a
# native restore must recover the exact pre-maximize visible frame.
native_max_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-native-maximize markers"
native_max_state_before=$(marker_exact_count "$native_max_text" 'window-state=Maximized')
native_max_ready_before=$(marker_prefix_count "$native_max_text" 'max-ready=')
native_max_frame_before=$(marker_prefix_count "$native_max_text" 'frame-size=')
run_fixture_host 8 Maximize -Hwnd "$window_id" \
    || fail "could not maximize the seamless HWND through the Windows host"
wait_window_state "$window_id" ZOOMED 1
wait_marker_exact_count "$marker_main" 'window-state=Maximized' "$((native_max_state_before + 1))"
wait_marker_prefix_count "$marker_main" 'max-ready=' "$((native_max_ready_before + 1))"
wait_marker_prefix_count "$marker_main" 'frame-size=' "$((native_max_frame_before + 1))"
native_max_width=$(window_field "$window_id" WIDTH) \
    || fail "could not read native-maximized host width"
native_max_height=$(window_field "$window_id" HEIGHT) \
    || fail "could not read native-maximized host height"
native_max_guest_frame=$(latest_guest_frame_size "$marker_main") \
    || fail "could not read the native-maximized guest DWM frame"
native_max_guest_width=${native_max_guest_frame%% *}
native_max_guest_height=${native_max_guest_frame#* }
report_fitted_viewport NATIVE_MAXIMIZE_VIEWPORT \
    "$native_max_width" "$native_max_height" \
    "$native_max_guest_width" "$native_max_guest_height"
wait_visual_sentinel native-maximized "$maximize_color" 0.003 \
    "$native_max_width" "$native_max_height"
pass native_host_maximize_to_guest
pass native_host_maximize_frame_ready

native_restore_text=$(guest_marker_text "$marker_main") \
    || fail "could not read pre-native-restore markers"
native_restore_state_before=$(marker_exact_count "$native_restore_text" 'window-state=Normal')
native_restore_frame_before=$(marker_exact_count "$native_restore_text" 'frame-size=900x650')
run_fixture_host 8 Restore -Hwnd "$window_id" \
    || fail "could not restore the seamless HWND through the Windows host"
wait_window_state "$window_id" ZOOMED 0
wait_marker_exact_count "$marker_main" 'window-state=Normal' "$((native_restore_state_before + 1))"
wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((native_restore_frame_before + 1))"
native_restore_width=$(window_field "$window_id" WIDTH) \
    || fail "could not read native-restored host width"
native_restore_height=$(window_field "$window_id" HEIGHT) \
    || fail "could not read native-restored host height"
native_restore_error="native restore returned ${native_restore_width}x${native_restore_height}, expected 900x650"
[ "$native_restore_width" -eq 900 ] || fail "$native_restore_error"
[ "$native_restore_height" -eq 650 ] || fail "$native_restore_error"
wait_visual_sentinel native-restored "$resize_color" 0.005 900 650
pass native_host_restore_to_guest
pass native_host_restore_frame_ready

# Guest-caption move and two maximize/restore paths deliberately changed the
# proxy placement. Each direction uses the same guest-caption placement helper
# before testing independently from a guest-visible 900x650 baseline.
activate_window "$window_id"
refresh_pointer_epoch "before the first guest border resize"

for resize_direction in north northeast east southeast south southwest west northwest
do
    test_guest_resize_direction "$resize_direction"
done
matrix_final_width=$(window_field "$window_id" WIDTH) \
    || fail "could not read final edge-resize width"
matrix_final_height=$(window_field "$window_id" HEIGHT) \
    || fail "could not read final edge-resize height"
matrix_guest_frame=$(latest_guest_frame_size "$marker_main") \
    || fail "could not read final edge-resize guest frame"
matrix_guest_width=${matrix_guest_frame%% *}
matrix_guest_height=${matrix_guest_frame#* }
if ! integer_near "$matrix_final_width" "$matrix_guest_width" 4 \
    || ! integer_near "$matrix_final_height" "$matrix_guest_height" 4
then
    fail "final edge-resize host ${matrix_final_width}x${matrix_final_height} did not match guest ${matrix_guest_width}x${matrix_guest_height}"
fi
if ! integer_near "$matrix_final_width" 900 4 \
    || ! integer_near "$matrix_final_height" 650 4
then
    fail "edge-resize reset finished at ${matrix_final_width}x${matrix_final_height}, expected about 900x650"
fi
wait_visual_sentinel edge-resize-final "$resize_color" 0.003 \
    "$matrix_final_width" "$matrix_final_height"
pass guest_border_all_directions
pass guest_border_resize_frame_ready

control_width=$(window_field "$window_id" WIDTH) || fail "could not read close-control width"
click_at "$window_id" "$((control_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_main" clipboard-restored
wait_marker_exact "$marker_main" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "guest-caption close returned presenter status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_main" 0
pass guest_caption_close

# Session 2: a hard host crash must release injected state while preserving the
# exact guest process/HWND for an identical-request reattach.
start_presenter "$marker_kill" kill
kill_process_before=$(fixture_process_id "$marker_kill") \
    || fail "could not read the pre-crash fixture PID"
kill_hwnd_before=$(fixture_window_handle "$marker_kill") \
    || fail "could not read the pre-crash guest HWND"
move_percent "$window_id" 50 75
run_fixture_host 8 KeyDown -Hwnd "$window_id" -Key CTRL \
    || fail "could not hold Ctrl for the presenter-crash test"
button_action LeftDown
wait_marker_exact "$marker_kill" 'input-held=ctrl,left'
kill -KILL "$presenter_pid" || fail "could not SIGKILL the seamless presenter"
wait_presenter_exit
if [ "$presenter_status" -eq 0 ]; then
    fail "SIGKILLed presenter unexpectedly returned success"
fi
wait_marker_exact "$marker_kill" 'input-released=all'
run_windows_host 5 ReleaseAll 2>/dev/null || :
wait_no_fixture_window
wait_fixture_count "$marker_kill" 1
kill_survival_text=$(guest_marker_text "$marker_kill") \
    || fail "could not read post-crash guest markers"
if marker_has_exact "$kill_survival_text" closed \
    || marker_has_exact "$kill_survival_text" clipboard-restored
then
    fail "SIGKILL incorrectly closed the recoverable guest HWND"
fi
pass presenter_sigkill_input_release
pass presenter_sigkill_guest_preserved

start_presenter "$marker_kill" kill-reattach
kill_process_after=$(fixture_process_id "$marker_kill") \
    || fail "could not read the reattached fixture PID"
kill_hwnd_after=$(fixture_window_handle "$marker_kill") \
    || fail "could not read the reattached guest HWND"
[ "$kill_process_after" = "$kill_process_before" ] \
    || fail "crash recovery launched PID $kill_process_after instead of reattaching PID $kill_process_before"
[ "$kill_hwnd_after" = "$kill_hwnd_before" ] \
    || fail "crash recovery captured HWND $kill_hwnd_after instead of reattaching HWND $kill_hwnd_before"
kill_reattach_text=$(guest_marker_text "$marker_kill") \
    || fail "could not read reattach identity markers"
[ "$(marker_prefix_count "$kill_reattach_text" 'process-id=')" -eq 1 ] \
    || fail "reattach restarted the fixture process"
[ "$(marker_prefix_count "$kill_reattach_text" 'window-hwnd=')" -eq 1 ] \
    || fail "reattach recreated the guest window"
pass presenter_sigkill_exact_reattach

kill_close_width=$(window_field "$window_id" WIDTH) || fail "could not read reattach close width"
click_at "$window_id" "$((kill_close_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_kill" clipboard-restored
wait_marker_exact "$marker_kill" closed
wait_presenter_exit
[ "$presenter_status" -eq 0 ] || fail "reattached presenter returned status $presenter_status"
wait_no_fixture_window
wait_fixture_count "$marker_kill" 0
pass presenter_sigkill_recovery_cleanup

# Session 3: plain typing after the crash proves Ctrl/mouse state was released.
start_presenter "$marker_recovery" recovery
pass guest_global_input_released_after_sigkill
click_percent "$window_id" 72 24
run_fixture_host 8 Type -Hwnd "$window_id" -Text plain \
    || fail "could not type after presenter crash recovery"
click_percent "$window_id" 34 42
recovery_text=$(printf '%s' plain | base64 | tr -d '\n')
wait_marker_exact "$marker_recovery" "text-base64=$recovery_text"
pass stuck_modifiers_released

control_width=$(window_field "$window_id" WIDTH) || fail "could not read recovery close width"
click_at "$window_id" "$((control_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_recovery" clipboard-restored
wait_marker_exact "$marker_recovery" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    fail "recovery presenter returned status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_recovery" 0
pass presenter_reconnect

# Session 4: continuous full-client repaint and successive guest-border resizes
# must not starve input or close.
start_presenter "$marker_animate" animate animate
wait_host_frame_change animation-live
pass animated_frames_change
click_percent "$window_id" 12 42
wait_marker_exact "$marker_animate" left-click
click_percent "$window_id" 72 24
run_fixture_host 8 Type -Hwnd "$window_id" -Text stress \
    || fail "could not type while the guest continuously repainted"
click_percent "$window_id" 34 42
stress_text=$(printf '%s' stress | base64 | tr -d '\n')
wait_marker_exact "$marker_animate" "text-base64=$stress_text"

place_fixture_via_guest_caption "before-animated-resize" "$marker_animate"
ensure_pointer_ready "$marker_animate" "pre-animated-resize-1024x720"
resize_via_guest_southeast 1024 720
ensure_pointer_ready "$marker_animate" "pre-animated-resize-820x600"
resize_via_guest_southeast 820 600
ensure_pointer_ready "$marker_animate" "pre-animated-resize-960x680"
resize_via_guest_southeast 960 680
wait_marker_exact "$marker_animate" 'frame-size=960x680'
wait_visual_sentinel animated-resized "$maximize_color" 0.003 960 680
wait_host_frame_change animation-post-resize
click_at "$window_id" 95 220
wait_marker_exact_count "$marker_animate" left-click 2
pass animated_repaint_input
pass animated_frames_change_after_resize
pass rapid_resize_input

control_width=$(window_field "$window_id" WIDTH) || fail "could not read animated close width"
click_at "$window_id" "$((control_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_animate" clipboard-restored
wait_marker_exact "$marker_animate" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "animated presenter returned status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_animate" 0
pass animated_repaint_close

# Session 5: a local presenter-construction error must close the shared socket
# even though its reader thread owns a cloned descriptor. The recoverable guest
# HWND stays alive and an identical request must reattach it without relaunching.
presenter_log="$host_root/presenter-error.log"
register_host_artifact presenter-error.log
register_fixture_marker "$marker_error"
setsid env WAYLAND_DISPLAY="lsw-e2e-unreachable-$run_id" "$lsw" run "$instance" --gui -- \
    powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
    -File "$guest_fixture" -MarkerPath "$marker_error" -RunId "$run_id" \
    -CleanupSignalPath "$guest_cleanup_signal" \
    >"$presenter_log" 2>&1 &
presenter_pid=$!
wait_marker_exact "$marker_error" "$released_input_state"
wait_marker_exact "$marker_error" ready
wait_marker_exact "$marker_error" 'frame-size-source=dwm'
wait_marker_prefix "$marker_error" 'window-hwnd='
error_process_before=$(fixture_process_id "$marker_error") \
    || fail "could not read presenter-error fixture PID"
error_hwnd_before=$(fixture_window_handle "$marker_error") \
    || fail "could not read presenter-error guest HWND"
wait_presenter_exit
if [ "$presenter_status" -eq 0 ]; then
    fail "presenter with an unreachable Wayland display unexpectedly succeeded"
fi
wait_no_fixture_window
wait_fixture_count "$marker_error" 1
pass presenter_error_socket_shutdown
pass presenter_error_guest_preserved

start_presenter "$marker_error" presenter-error-reattach
error_process_after=$(fixture_process_id "$marker_error") \
    || fail "could not read presenter-error reattach PID"
error_hwnd_after=$(fixture_window_handle "$marker_error") \
    || fail "could not read presenter-error reattach HWND"
[ "$error_process_after" = "$error_process_before" ] \
    || fail "presenter-error recovery relaunched the fixture process"
[ "$error_hwnd_after" = "$error_hwnd_before" ] \
    || fail "presenter-error recovery recreated the guest HWND"
error_identity_text=$(guest_marker_text "$marker_error") \
    || fail "could not read presenter-error recovery markers"
[ "$(marker_prefix_count "$error_identity_text" 'process-id=')" -eq 1 ] \
    || fail "presenter-error recovery rewrote the fixture marker"
[ "$(marker_prefix_count "$error_identity_text" 'window-hwnd=')" -eq 1 ] \
    || fail "presenter-error recovery reran the fixture Shown handler"
error_close_width=$(window_field "$window_id" WIDTH) || fail "could not read presenter-error close width"
click_at "$window_id" "$((error_close_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_error" clipboard-restored
wait_marker_exact "$marker_error" closed
wait_presenter_exit
[ "$presenter_status" -eq 0 ] || fail "presenter-error recovery returned status $presenter_status"
wait_no_fixture_window
wait_fixture_count "$marker_error" 0
pass presenter_error_exact_reattach
pass presenter_error_guest_cleanup

# Session 6: a native guest-caption close must leave an owned unsaved-change
# prompt visible and interactive. Cancel keeps the session alive; Discard exits.
start_presenter "$marker_close" close-prompt close-prompt
close_width=$(window_field "$window_id" WIDTH) || fail "could not read close-prompt width"
close_height=$(window_field "$window_id" HEIGHT) || fail "could not read close-prompt height"
click_at "$window_id" "$((close_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact_count "$marker_close" close-prompt-open 1
wait_marker_prefix_count "$marker_close" 'close-prompt-controls=' 1
wait_visual_sentinel close-prompt-cancel "$close_prompt_color" 0.01 \
    "$close_width" "$close_height"
close_prompt_text=$(guest_marker_text "$marker_close") \
    || fail "could not read close-prompt controls"
cancel_x=$(close_prompt_control_field "$close_prompt_text" cancel-x) \
    || fail "could not read close-prompt Cancel x-coordinate"
cancel_y=$(close_prompt_control_field "$close_prompt_text" cancel-y) \
    || fail "could not read close-prompt Cancel y-coordinate"
click_at "$window_id" "$cancel_x" "$cancel_y"
wait_marker_exact "$marker_close" close-prompt-cancel
wait_fixture_count "$marker_close" 1
click_percent "$window_id" 12 42
wait_marker_exact "$marker_close" left-click
pass native_close_prompt_cancel

dirty_process_before=$(fixture_process_id "$marker_close") \
    || fail "could not read dirty fixture PID before presenter crash"
dirty_hwnd_before=$(fixture_window_handle "$marker_close") \
    || fail "could not read dirty guest HWND before presenter crash"
kill -KILL "$presenter_pid" || fail "could not SIGKILL the dirty-window presenter"
run_windows_host 5 ReleaseAll 2>/dev/null || :
wait_presenter_exit
[ "$presenter_status" -ne 0 ] || fail "SIGKILLed dirty presenter unexpectedly returned success"
wait_no_fixture_window
wait_fixture_count "$marker_close" 1
dirty_survival_text=$(guest_marker_text "$marker_close") \
    || fail "could not read dirty-window survival markers"
if marker_has_exact "$dirty_survival_text" closed \
    || marker_has_exact "$dirty_survival_text" clipboard-restored
then
    fail "presenter crash discarded the simulated dirty guest HWND"
fi

start_presenter "$marker_close" close-prompt-reattach close-prompt
dirty_process_after=$(fixture_process_id "$marker_close") \
    || fail "could not read dirty fixture PID after reattach"
dirty_hwnd_after=$(fixture_window_handle "$marker_close") \
    || fail "could not read dirty guest HWND after reattach"
[ "$dirty_process_after" = "$dirty_process_before" ] \
    || fail "dirty recovery relaunched PID $dirty_process_after instead of $dirty_process_before"
[ "$dirty_hwnd_after" = "$dirty_hwnd_before" ] \
    || fail "dirty recovery replaced HWND $dirty_hwnd_before with $dirty_hwnd_after"
dirty_reattach_text=$(guest_marker_text "$marker_close") \
    || fail "could not read dirty reattach markers"
[ "$(marker_exact_count "$dirty_reattach_text" close-prompt-cancel)" -eq 1 ] \
    || fail "dirty recovery lost or duplicated the prior Cancel state"
[ "$(marker_prefix_count "$dirty_reattach_text" 'window-hwnd=')" -eq 1 ] \
    || fail "dirty recovery reran the fixture Shown handler"
pass dirty_window_exact_reattach

close_width=$(window_field "$window_id" WIDTH) || fail "could not reread close-prompt width"
close_height=$(window_field "$window_id" HEIGHT) || fail "could not reread close-prompt height"
click_at "$window_id" "$((close_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact_count "$marker_close" close-prompt-open 2
wait_marker_prefix_count "$marker_close" 'close-prompt-controls=' 2
wait_visual_sentinel close-prompt-discard "$close_prompt_color" 0.01 \
    "$close_width" "$close_height"
close_prompt_text=$(guest_marker_text "$marker_close") \
    || fail "could not reread close-prompt controls"
discard_x=$(close_prompt_control_field "$close_prompt_text" discard-x) \
    || fail "could not read close-prompt Discard x-coordinate"
discard_y=$(close_prompt_control_field "$close_prompt_text" discard-y) \
    || fail "could not read close-prompt Discard y-coordinate"
click_at "$window_id" "$discard_x" "$discard_y"
wait_marker_exact "$marker_close" close-prompt-discard
wait_marker_exact "$marker_close" clipboard-restored
wait_marker_exact "$marker_close" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "native close-prompt presenter returned status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_close" 0
pass native_close_prompt_discard
pass native_close_prompt_cleanup
pass guest_clipboard_restored

# Session 7: a Windows host close of the WSLg Wayland proxy must use the protocol
# close path, release forwarded input, and wait for guest acknowledgement.
start_presenter "$marker_host_close" host-close
move_percent "$window_id" 50 75
host_close_input_text=$(guest_marker_text "$marker_host_close") \
    || fail "could not read pre-close input markers"
host_close_ctrl_before=$(marker_exact_count "$host_close_input_text" ctrl-key-down)
host_close_left_before=$(marker_exact_count "$host_close_input_text" left-button-down)
host_close_ctrl_up_before=$(marker_exact_count "$host_close_input_text" ctrl-key-up)
host_close_left_up_before=$(marker_exact_count "$host_close_input_text" left-button-up)
run_fixture_host 8 CloseWithHeldInput -Hwnd "$window_id" \
    || fail "could not hold input and request the Windows host-window close"
wait_marker_exact_count "$marker_host_close" ctrl-key-down "$((host_close_ctrl_before + 1))"
wait_marker_exact_count "$marker_host_close" left-button-down "$((host_close_left_before + 1))"
wait_marker_exact_count "$marker_host_close" ctrl-key-up "$((host_close_ctrl_up_before + 1))"
wait_marker_exact_count "$marker_host_close" left-button-up "$((host_close_left_up_before + 1))"
wait_marker_exact "$marker_host_close" "closing-$released_input_state"
run_windows_host 5 ReleaseAll 2>/dev/null || :
wait_marker_exact "$marker_host_close" clipboard-restored
wait_marker_exact "$marker_host_close" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "Windows host-window close returned presenter status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_host_close" 0
pass host_window_manager_close
pass host_close_releases_input

# Session 8: an application that remembers a maximized guest state must make
# the new host window maximized, and its visible Restore button must idempotently
# restore both sides rather than blindly toggling an already divergent host.
start_presenter "$marker_initial_max" initial-maximized maximized
wait_marker_exact "$marker_initial_max" 'window-state=Maximized'
wait_window_state "$window_id" ZOOMED 1
pass initial_guest_maximize_sync
initial_restore_text=$(guest_marker_text "$marker_initial_max") \
    || fail "could not read initial-maximized markers"
initial_restore_before=$(marker_exact_count "$initial_restore_text" 'window-state=Normal')
initial_restore_frame_before=$(marker_prefix_count "$initial_restore_text" 'frame-size=')
initial_max_width=$(window_field "$window_id" WIDTH) || fail "could not read initial-maximized width"
initial_max_height=$(window_field "$window_id" HEIGHT) || fail "could not read initial-maximized height"
initial_max_guest_frame=$(latest_guest_frame_size "$marker_initial_max") \
    || fail "could not read the initial-maximized guest DWM frame"
initial_max_guest_width=${initial_max_guest_frame%% *}
initial_max_guest_height=${initial_max_guest_frame#* }
report_fitted_viewport INITIAL_MAXIMIZE_VIEWPORT \
    "$initial_max_width" "$initial_max_height" \
    "$initial_max_guest_width" "$initial_max_guest_height"
click_guest_point "$window_id" "$initial_max_guest_width" "$initial_max_guest_height" \
    "$((initial_max_guest_width - caption_maximize_offset))" "$caption_control_y"
wait_window_state "$window_id" ZOOMED 0
wait_marker_exact_count "$marker_initial_max" 'window-state=Normal' \
    "$((initial_restore_before + 1))"
wait_marker_prefix_count "$marker_initial_max" 'frame-size=' \
    "$((initial_restore_frame_before + 1))"
pass initial_guest_restore_sync
initial_restore_guest_frame=$(latest_guest_frame_size "$marker_initial_max") \
    || fail "could not read the initial-restored guest DWM frame"
initial_restore_guest_width=${initial_restore_guest_frame%% *}
initial_restore_guest_height=${initial_restore_guest_frame#* }
click_guest_point "$window_id" "$initial_restore_guest_width" "$initial_restore_guest_height" \
    "$((initial_restore_guest_width - caption_close_offset))" "$caption_control_y"
wait_marker_exact "$marker_initial_max" clipboard-restored
wait_marker_exact "$marker_initial_max" closed
wait_presenter_exit
if [ "$presenter_status" -ne 0 ]; then
    tail -n 100 "$presenter_log" >&2 || :
    fail "initial-maximized presenter returned status $presenter_status"
fi
wait_no_fixture_window
wait_fixture_count "$marker_initial_max" 0
pass explicit_maximize_restore_state

if ! cleanup_guest_root; then
    fail "guest fixture cleanup failed"
fi
pass guest_artifact_cleanup
matrix_complete=1
