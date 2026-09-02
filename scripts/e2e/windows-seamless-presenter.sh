// SPDX-License-Identifier: GPL-3.0-or-later
# shellcheck shell=sh
# Presenter lifecycle and interactive fixture scenarios.
# shellcheck disable=SC2154

start_focus_sink() {
    focus_sink_title="LSW Seamless Focus Sink $run_id"
    register_host_artifact focus-sink.log
    setsid powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass \
        -File "$host_helper_windows" -Action FocusSink -TitleNeedle "$focus_sink_title" \
        >"$host_root/focus-sink.log" 2>&1 &
    focus_sink_pid=$!
    focus_sink_deadline=$(( $(date +%s) + 8 ))
    while [ "$(date +%s)" -lt "$focus_sink_deadline" ]; do
        if ! kill -0 "$focus_sink_pid" 2>/dev/null; then
            tail -n 30 "$host_root/focus-sink.log" >&2 || :
            fail "Windows focus sink exited before its window appeared"
        fi
        focus_sink_find=$(run_windows_host 8 Find -TitleNeedle "$focus_sink_title" \
            -ProcessName powershell -ExactTitle 2>/dev/null || :)
        focus_sink_window=$(printf '%s\n' "$focus_sink_find" | awk -F= \
            '$1 == "HWND" && $2 ~ /^[1-9][0-9]*$/ { print $2; exit }')
        if [ -n "$focus_sink_window" ]; then
            activate_window "$focus_sink_window"
            return 0
        fi
        sleep 0.2
    done
    fail "timed out waiting for the native Windows focus sink"
}

start_presenter() {
    start_marker=$1
    start_label=$2
    start_mode=${3:-normal}
    start_status=$(run_lsw 6 status "$instance") \
        || fail "could not recheck instance $instance before $start_label"
    if ! printf '%s\n' "$start_status" | grep -Fx 'STATE=running' >/dev/null; then
        fail "instance $instance stopped before $start_label; this driver will not start it"
    fi
    if ! printf '%s\n' "$start_status" | grep -Fx 'AGENT=ready' >/dev/null; then
        fail "guest agent stopped being ready before $start_label"
    fi
    start_existing_window=$(find_fixture_window) \
        || fail "could not inspect Windows for an existing fixture before $start_label"
    if [ -n "$start_existing_window" ]; then
        fail "refusing to start $start_label while another fixture window is visible"
    fi
    case "$start_mode" in
        normal|animate|close-prompt|maximized) ;;
        *) fail "unknown seamless fixture mode $start_mode" ;;
    esac
    register_fixture_marker "$start_marker"
    register_host_artifact "presenter-$start_label.log"
    presenter_log="$host_root/presenter-$start_label.log"
    case "$start_mode" in
        normal)
            setsid "$lsw" run "$instance" --gui -- \
                powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
                -File "$guest_fixture" -MarkerPath "$start_marker" -RunId "$run_id" \
                -CleanupSignalPath "$guest_cleanup_signal" \
                >"$presenter_log" 2>&1 &
            ;;
        animate)
            setsid "$lsw" run "$instance" --gui -- \
                powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
                -File "$guest_fixture" -MarkerPath "$start_marker" -RunId "$run_id" \
                -CleanupSignalPath "$guest_cleanup_signal" -Animate \
                >"$presenter_log" 2>&1 &
            ;;
        close-prompt)
            setsid "$lsw" run "$instance" --gui -- \
                powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
                -File "$guest_fixture" -MarkerPath "$start_marker" -RunId "$run_id" \
                -CleanupSignalPath "$guest_cleanup_signal" -ClosePrompt \
                >"$presenter_log" 2>&1 &
            ;;
        maximized)
            setsid "$lsw" run "$instance" --gui -- \
                powershell.exe -NoLogo -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass \
                -File "$guest_fixture" -MarkerPath "$start_marker" -RunId "$run_id" \
                -CleanupSignalPath "$guest_cleanup_signal" -StartMaximized \
                >"$presenter_log" 2>&1 &
            ;;
        *) fail "unknown seamless fixture mode $start_mode" ;;
    esac
    presenter_pid=$!
    wait_fixture_window
    wait_marker_exact "$start_marker" "$released_input_state"
    wait_marker_exact "$start_marker" ready
    wait_marker_exact "$start_marker" 'frame-size-source=dwm'
    wait_marker_prefix "$start_marker" 'frame-size='
    wait_marker_prefix "$start_marker" 'window-hwnd='
    wait_marker_prefix "$start_marker" 'pointer-probe='
    assert_marker_before "$start_marker" "$released_input_state" ready
    if [ "$start_mode" = animate ]; then
        wait_marker_exact "$start_marker" animation-ready
    fi
    activate_window "$window_id"
    wait_fixture_count "$start_marker" 1
    refresh_pointer_epoch "after starting the $start_label presenter"
    ensure_pointer_ready "$start_marker" "$start_label"
}

refresh_pointer_epoch() {
    refresh_context=$1
    if [ "$wslg_transport_probe" = copy ]; then
        # Copy-mode WSLg can defer a RAIL proxy's CursorLeft until after the
        # next synthetic input edge. Hand both foreground focus and the pointer
        # to a distinct identity-checked native HWND, destroy that HWND, then
        # return focus to the exact WSLg HWND. Guest WinForms activation is not
        # the proof: host focus loss releases injected input without changing
        # the guest desktop's foreground window.
        start_focus_sink
        run_windows_host 8 Pointer -Hwnd "$focus_sink_window" \
            -TitleNeedle "$focus_sink_title" -ProcessName powershell -ExactTitle \
            -X 20 -Y 20 \
            || fail "could not move the pointer into the native focus sink $refresh_context"
        sleep 0.4
        terminate_focus_sink
        activate_window "$window_id"
        sleep 0.4
        # A corner point can belong to the Windows RAIL proxy's invisible
        # resize border without being accepted as the Wayland surface's first
        # enter coordinate. Enter through an unobscured interior point first.
        move_percent "$window_id" 80 75
        sleep 0.4
    else
        run_fixture_host 8 PointerAway -Hwnd "$window_id" \
            || fail "could not leave the host window $refresh_context"
        sleep 0.25
    fi
}

ensure_pointer_ready() {
    ready_marker=$1
    ready_label=$2
    ready_text=$(guest_marker_text "$ready_marker") \
        || fail "could not read the $ready_label pointer-probe marker"
    ready_x=$(pointer_probe_field "$ready_text" x) \
        || fail "$ready_label did not report a valid pointer-probe X coordinate"
    ready_y=$(pointer_probe_field "$ready_text" y) \
        || fail "$ready_label did not report a valid pointer-probe Y coordinate"
    ready_guest_frame=$(latest_guest_frame_size "$ready_marker") \
        || fail "could not read the $ready_label guest frame for pointer mapping"
    ready_guest_width=${ready_guest_frame%% *}
    ready_guest_height=${ready_guest_frame#* }
    ready_before=$(marker_exact_count "$ready_text" left-click)
    ready_attempt=1
    while [ "$ready_attempt" -le 4 ]; do
        click_guest_point "$window_id" "$ready_guest_width" "$ready_guest_height" \
            "$ready_x" "$ready_y"
        ready_deadline=$(( $(date +%s) + 3 ))
        while [ "$(date +%s)" -lt "$ready_deadline" ]; do
            ready_latest=$(guest_marker_text "$ready_marker" || :)
            ready_observed=$(marker_exact_count "$ready_latest" left-click)
            if [ "$ready_observed" -gt "$ready_before" ]; then
                printf 'POINTER_HANDSHAKE SESSION=%s ATTEMPTS=%s\n' \
                    "$ready_label" "$ready_attempt"
                return 0
            fi
            sleep 0.2
        done
        ready_attempt=$((ready_attempt + 1))
        if [ "$ready_attempt" -le 4 ]; then
            refresh_pointer_epoch "while establishing $ready_label pointer input"
        fi
    done
    fail "$ready_label did not receive the bounded pointer-input handshake"
}

place_fixture_via_guest_caption() {
    placement_label=$1
    placement_marker=${2:-$marker_main}
    placement_query=$(run_fixture_host 8 Query -Hwnd "$window_id") \
        || fail "could not inspect the fixture placement $placement_label"
    placement_x=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "X" { print $2; exit }')
    placement_y=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "Y" { print $2; exit }')
    placement_width=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WIDTH" { print $2; exit }')
    placement_height=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "HEIGHT" { print $2; exit }')
    placement_work_left=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WORK_LEFT" { print $2; exit }')
    placement_work_top=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WORK_TOP" { print $2; exit }')
    placement_work_width=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WORK_WIDTH" { print $2; exit }')
    placement_work_height=$(printf '%s\n' "$placement_query" | awk -F= '$1 == "WORK_HEIGHT" { print $2; exit }')
    for placement_value in \
        "$placement_x" "$placement_y" "$placement_width" "$placement_height" \
        "$placement_work_left" "$placement_work_top" \
        "$placement_work_width" "$placement_work_height"
    do
        printf '%s\n' "$placement_value" | grep -Eq '^-?[0-9]+$' \
            || fail "the Windows work-area query was invalid $placement_label"
    done
    [ "$placement_width" -le "$placement_work_width" ] || fail "the seamless HWND cannot fit in its Windows monitor work area"
    [ "$placement_height" -le "$placement_work_height" ] || fail "the seamless HWND cannot fit in its Windows monitor work area"
    placement_target_x=$((placement_work_left + (placement_work_width - placement_width) / 2))
    placement_target_y=$((placement_work_top + (placement_work_height - placement_height) / 2))
    placement_delta_x=$((placement_target_x - placement_x))
    placement_delta_y=$((placement_target_y - placement_y))
    if [ "$placement_delta_x" -ge -2 ] && [ "$placement_delta_x" -le 2 ] \
        && [ "$placement_delta_y" -ge -2 ] && [ "$placement_delta_y" -le 2 ]
    then
        return 0
    fi

    # Move through the guest's real HTCAPTION and the presenter's compositor
    # drag. Direct SetWindowPos on a WSLg RAIL HWND can move the Windows proxy
    # without moving the Wayland surface origin, invalidating pointer mapping.
    ensure_pointer_ready "$placement_marker" "pre-placement-$placement_label"
    run_fixture_host 10 Drag -Hwnd "$window_id" \
        -X "$((placement_width / 2))" -Y "$caption_control_y" \
        -DeltaX "$placement_delta_x" -DeltaY "$placement_delta_y" \
        -DelayMilliseconds 300 \
        || fail "could not move the fixture through its guest caption $placement_label"
    placement_deadline=$(( $(date +%s) + 8 ))
    placement_observed_x=$placement_x
    placement_observed_y=$placement_y
    while [ "$(date +%s)" -lt "$placement_deadline" ]; do
        placement_observed_x=$(window_field "$window_id" X || printf '%s' "$placement_observed_x")
        placement_observed_y=$(window_field "$window_id" Y || printf '%s' "$placement_observed_y")
        if integer_near "$placement_observed_x" "$placement_target_x" 24 \
            && integer_near "$placement_observed_y" "$placement_target_y" 24
        then
            activate_window "$window_id"
            refresh_pointer_epoch "after guest-caption placement $placement_label"
            return 0
        fi
        sleep 0.2
    done
    fail "guest-caption placement settled at ${placement_observed_x},${placement_observed_y}; expected about ${placement_target_x},${placement_target_y} $placement_label"
}

test_guest_resize_direction() {
    resize_direction=$1
    place_fixture_via_guest_caption "before-$resize_direction-resize"
    ensure_pointer_ready "$marker_main" "pre-$resize_direction-resize"
    resize_marker_text=$(guest_marker_text "$marker_main") \
        || fail "could not read pre-$resize_direction-resize markers"
    resize_marker_before=$(marker_prefix_count "$resize_marker_text" 'resize=')
    resize_x_before=$(window_field "$window_id" X) \
        || fail "could not read pre-$resize_direction-resize X"
    resize_y_before=$(window_field "$window_id" Y) \
        || fail "could not read pre-$resize_direction-resize Y"
    resize_width_before=$(window_field "$window_id" WIDTH) \
        || fail "could not read pre-$resize_direction-resize width"
    resize_height_before=$(window_field "$window_id" HEIGHT) \
        || fail "could not read pre-$resize_direction-resize height"
    resize_right_before=$((resize_x_before + resize_width_before))
    resize_bottom_before=$((resize_y_before + resize_height_before))
    # Exercise the companion's guest border instead of its caption or client
    # area. The fixture proves the top corners at two pixels are native
    # HTTOPLEFT/HTTOPRIGHT. Eight pixels is used on the remaining edges, where
    # the companion's DPI-aware inner affordance avoids stealing caption
    # controls. refresh_pointer_epoch provides the separate outside-to-inside
    # transition required by copy-mode WSLg.
    resize_pointer_inset=8
    resize_left_inset=6
    resize_top_corner_inset=2

    case "$resize_direction" in
        north)
            resize_pointer_x=$((resize_width_before / 2)); resize_pointer_y=$resize_pointer_inset
            resize_delta_x=0; resize_delta_y=-35
            ;;
        northeast)
            resize_pointer_x=$((resize_width_before - resize_top_corner_inset)); resize_pointer_y=$resize_top_corner_inset
            resize_delta_x=45; resize_delta_y=-35
            ;;
        east)
            resize_pointer_x=$((resize_width_before - resize_pointer_inset)); resize_pointer_y=$((resize_height_before / 2))
            resize_delta_x=45; resize_delta_y=0
            ;;
        southeast)
            resize_pointer_x=$((resize_width_before - resize_pointer_inset)); resize_pointer_y=$((resize_height_before - resize_pointer_inset))
            resize_delta_x=45; resize_delta_y=35
            ;;
        south)
            resize_pointer_x=$((resize_width_before / 2)); resize_pointer_y=$((resize_height_before - resize_pointer_inset))
            resize_delta_x=0; resize_delta_y=35
            ;;
        southwest)
            resize_pointer_x=$resize_left_inset; resize_pointer_y=$((resize_height_before - resize_pointer_inset))
            resize_delta_x=-45; resize_delta_y=35
            ;;
        west)
            resize_pointer_x=$resize_left_inset; resize_pointer_y=$((resize_height_before / 2))
            resize_delta_x=-45; resize_delta_y=0
            ;;
        northwest)
            resize_pointer_x=$resize_top_corner_inset; resize_pointer_y=$resize_top_corner_inset
            resize_delta_x=-45; resize_delta_y=-35
            ;;
        *) fail "unknown guest resize direction $resize_direction" ;;
    esac

    run_fixture_host 8 Drag -Hwnd "$window_id" \
        -X "$resize_pointer_x" -Y "$resize_pointer_y" \
        -DeltaX "$resize_delta_x" -DeltaY "$resize_delta_y" \
        -DelayMilliseconds 300 \
        || fail "could not drag the guest $resize_direction resize border"

    resize_deadline=$(( $(date +%s) + 8 ))
    resize_changed=0
    while [ "$(date +%s)" -lt "$resize_deadline" ]; do
        resize_x_after=$(window_field "$window_id" X || printf '%s' "$resize_x_before")
        resize_y_after=$(window_field "$window_id" Y || printf '%s' "$resize_y_before")
        resize_width_after=$(window_field "$window_id" WIDTH || printf '%s' "$resize_width_before")
        resize_height_after=$(window_field "$window_id" HEIGHT || printf '%s' "$resize_height_before")
        resize_right_after=$((resize_x_after + resize_width_after))
        resize_bottom_after=$((resize_y_after + resize_height_after))
        resize_x_delta=$((resize_x_after - resize_x_before))
        resize_y_delta=$((resize_y_after - resize_y_before))
        resize_right_delta=$((resize_right_after - resize_right_before))
        resize_bottom_delta=$((resize_bottom_after - resize_bottom_before))
        if case "$resize_direction" in
            north)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" -35 18 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
            northeast)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 45 18 \
                    && integer_near "$resize_y_delta" -35 18 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
            east)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 45 18 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
            southeast)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 45 18 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 35 18 ;;
            south)
                integer_near "$resize_x_delta" 0 4 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 35 18 ;;
            southwest)
                integer_near "$resize_x_delta" -45 18 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 35 18 ;;
            west)
                integer_near "$resize_x_delta" -45 18 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" 0 4 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
            northwest)
                integer_near "$resize_x_delta" -45 18 \
                    && integer_near "$resize_right_delta" 0 4 \
                    && integer_near "$resize_y_delta" -35 18 \
                    && integer_near "$resize_bottom_delta" 0 4 ;;
        esac
        then
            resize_changed=1
            break
        fi
        sleep 0.2
    done
    [ "$resize_changed" -eq 1 ] \
        || fail "dragging the guest $resize_direction border changed host ${resize_x_before},${resize_y_before},${resize_width_before}x${resize_height_before} to ${resize_x_after},${resize_y_after},${resize_width_after}x${resize_height_after} (edge deltas ${resize_x_delta},${resize_y_delta},${resize_right_delta},${resize_bottom_delta})"
    wait_marker_prefix_count "$marker_main" 'resize=' "$((resize_marker_before + 1))"
    wait_visual_sentinel "edge-resize-$resize_direction" "$maximize_color" 0.003 \
        "$resize_width_after" "$resize_height_after"
    pass "guest_border_${resize_direction}_resize"
    refresh_pointer_epoch "after the guest $resize_direction resize"
    ensure_pointer_ready "$marker_main" "pre-$resize_direction-reset"

    # Each direction starts from the same guest-visible geometry. Cumulatively
    # growing all eight edges can place a normal Windows window beyond the
    # 1280x800 guest desktop, which tests an unreachable edge rather than the
    # direction mapping. Reverse the same proven guest edge to restore the
    # exact baseline; direct SetWindowPos on the WSLg proxy is not trusted.
    resize_reset_text=$(guest_marker_text "$marker_main") \
        || fail "could not read pre-$resize_direction-reset markers"
    resize_reset_before=$(marker_exact_count "$resize_reset_text" 'frame-size=900x650')
    case "$resize_direction" in
        north)
            resize_reset_x=$((resize_width_after / 2)); resize_reset_y=$resize_pointer_inset ;;
        northeast)
            resize_reset_x=$((resize_width_after - resize_top_corner_inset)); resize_reset_y=$resize_top_corner_inset ;;
        east)
            resize_reset_x=$((resize_width_after - resize_pointer_inset)); resize_reset_y=$((resize_height_after / 2)) ;;
        southeast)
            resize_reset_x=$((resize_width_after - resize_pointer_inset)); resize_reset_y=$((resize_height_after - resize_pointer_inset)) ;;
        south)
            resize_reset_x=$((resize_width_after / 2)); resize_reset_y=$((resize_height_after - resize_pointer_inset)) ;;
        southwest)
            resize_reset_x=$resize_left_inset; resize_reset_y=$((resize_height_after - resize_pointer_inset)) ;;
        west)
            resize_reset_x=$resize_left_inset; resize_reset_y=$((resize_height_after / 2)) ;;
        northwest)
            resize_reset_x=$resize_top_corner_inset; resize_reset_y=$resize_top_corner_inset ;;
        *) fail "unknown guest reset direction $resize_direction" ;;
    esac
    run_fixture_host 8 Drag -Hwnd "$window_id" \
        -X "$resize_reset_x" -Y "$resize_reset_y" \
        -DeltaX "$((-resize_delta_x))" -DeltaY "$((-resize_delta_y))" \
        -DelayMilliseconds 300 \
        || fail "could not reverse the guest $resize_direction resize border"
    resize_reset_deadline=$(( $(date +%s) + 8 ))
    resize_reset_complete=0
    while [ "$(date +%s)" -lt "$resize_reset_deadline" ]; do
        resize_reset_x_after=$(window_field "$window_id" X || printf '%s' "$resize_x_after")
        resize_reset_y_after=$(window_field "$window_id" Y || printf '%s' "$resize_y_after")
        resize_reset_width_after=$(window_field "$window_id" WIDTH || printf '%s' "$resize_width_after")
        resize_reset_height_after=$(window_field "$window_id" HEIGHT || printf '%s' "$resize_height_after")
        if integer_near "$resize_reset_x_after" "$resize_x_before" 4 \
            && integer_near "$resize_reset_y_after" "$resize_y_before" 4 \
            && integer_near "$resize_reset_width_after" "$resize_width_before" 4 \
            && integer_near "$resize_reset_height_after" "$resize_height_before" 4
        then
            resize_reset_complete=1
            break
        fi
        sleep 0.2
    done
    [ "$resize_reset_complete" -eq 1 ] \
        || fail "reversing the guest $resize_direction border restored ${resize_reset_x_after},${resize_reset_y_after},${resize_reset_width_after}x${resize_reset_height_after}; expected about ${resize_x_before},${resize_y_before},${resize_width_before}x${resize_height_before}"
    wait_marker_exact_count "$marker_main" 'frame-size=900x650' "$((resize_reset_before + 1))"
    # WinForms can publish the HWND size before WGC emits its matching Ready
    # frame. Prove that the reset visual crossed the complete guest-to-host
    # pipeline before using the next edge coordinate; the agent deliberately
    # rejects stale capture geometry instead of guessing a screen position.
    wait_visual_sentinel "edge-resize-$resize_direction-reset" "$resize_color" 0.005 \
        "$resize_reset_width_after" "$resize_reset_height_after"
    activate_window "$window_id"
    refresh_pointer_epoch "after resetting the guest $resize_direction resize"
}

wait_presenter_exit() {
    exit_deadline=$(( $(date +%s) + 12 ))
    while kill -0 "$presenter_pid" 2>/dev/null \
        && [ "$(date +%s)" -lt "$exit_deadline" ]
    do
        sleep 0.2
    done
    if kill -0 "$presenter_pid" 2>/dev/null; then
        fail "seamless presenter did not exit within twelve seconds"
    fi
    if wait "$presenter_pid"; then
        presenter_status=0
    else
        presenter_status=$?
    fi
    presenter_pid=
}
