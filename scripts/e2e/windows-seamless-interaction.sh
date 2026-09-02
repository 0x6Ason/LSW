// SPDX-License-Identifier: GPL-3.0-or-later
# shellcheck shell=sh
# Marker, HWND, input, image, resize, and chrome assertions.
# shellcheck disable=SC2154

guest_marker_text() {
    marker_read_path=$1
    marker_read_attempt=1
    while [ "$marker_read_attempt" -le 3 ]; do
        if marker_read_raw=$(run_lsw 3 exec "$instance" -- \
            cmd.exe /d /c type "$marker_read_path" </dev/null 2>/dev/null)
        then
            printf '%s\n' "$marker_read_raw" | tr -d '\r'
            return 0
        fi
        marker_read_attempt=$((marker_read_attempt + 1))
        if [ "$marker_read_attempt" -le 3 ]; then
            sleep 0.2
        fi
    done
    return 1
}

marker_has_exact() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        '$2 == expected { found=1 } END { exit(found ? 0 : 1) }'
}

caption_control_field() {
    caption_marker_text=$1
    caption_field=$2
    printf '%s\n' "$caption_marker_text" | awk -F '\t' -v field="$caption_field" '
        $2 ~ /^caption-controls=/ {
            count = split(substr($2, length("caption-controls=") + 1), values, ",")
            for (entry = 1; entry <= count; entry++) {
                split(values[entry], pair, ":")
                if (pair[1] == field) {
                    observed = pair[2]
                }
            }
        }
        END {
            if (observed ~ /^[1-9][0-9]*$/) {
                print observed
                exit 0
            }
            exit 1
        }
    '
}

close_prompt_control_field() {
    close_prompt_marker_text=$1
    close_prompt_field=$2
    printf '%s\n' "$close_prompt_marker_text" | awk -F '\t' -v field="$close_prompt_field" '
        $2 ~ /^close-prompt-controls=/ {
            count = split(substr($2, length("close-prompt-controls=") + 1), values, ",")
            for (entry = 1; entry <= count; entry++) {
                split(values[entry], pair, ":")
                if (pair[1] == field) {
                    observed = pair[2]
                }
            }
        }
        END {
            if (observed ~ /^[0-9]+$/) {
                print observed
                exit 0
            }
            exit 1
        }
    '
}

pointer_probe_field() {
    probe_marker_text=$1
    probe_field=$2
    printf '%s\n' "$probe_marker_text" | awk -F '\t' -v field="$probe_field" '
        $2 ~ /^pointer-probe=/ {
            count = split(substr($2, length("pointer-probe=") + 1), values, ",")
            for (entry = 1; entry <= count; entry++) {
                split(values[entry], pair, ":")
                if (pair[1] == field) {
                    observed = pair[2]
                }
            }
        }
        END {
            if (observed ~ /^[0-9]+$/) {
                print observed
                exit 0
            }
            exit 1
        }
    '
}

marker_has_prefix() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        'index($2, expected) == 1 { found=1 } END { exit(found ? 0 : 1) }'
}

marker_exact_count() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        '$2 == expected { count++ } END { print count + 0 }'
}

marker_prefix_count() {
    printf '%s\n' "$1" | awk -F '\t' -v expected="$2" \
        'index($2, expected) == 1 { count++ } END { print count + 0 }'
}

latest_guest_frame_size() {
    latest_frame_path=$1
    latest_frame_text=$(guest_marker_text "$latest_frame_path") || return 1
    printf '%s\n' "$latest_frame_text" | awk -F '[=x\t]' '
        $2 == "frame-size" && $3 ~ /^[1-9][0-9]*$/ && $4 ~ /^[1-9][0-9]*$/ {
            size=$3 " " $4; found=1
        }
        END { if (found) print size; else exit 1 }
    '
}

assert_marker_before() {
    order_path=$1
    order_first=$2
    order_second=$3
    order_text=$(guest_marker_text "$order_path") \
        || fail "could not read guest marker ordering from $order_path"
    printf '%s\n' "$order_text" | awk -F '\t' -v first="$order_first" -v second="$order_second" '
        $2 == first && first_line == 0 { first_line=NR }
        $2 == second && second_line == 0 { second_line=NR }
        END { exit(!(first_line > 0 && second_line > first_line)) }
    ' || fail "guest marker $order_first did not precede $order_second"
}

wait_marker_exact() {
    wait_marker_path=$1
    wait_marker_expected=$2
    wait_marker_deadline=$(( $(date +%s) + 15 ))
    wait_marker_latest=
    while [ "$(date +%s)" -lt "$wait_marker_deadline" ]; do
        wait_marker_latest=$(guest_marker_text "$wait_marker_path" || :)
        if marker_has_exact "$wait_marker_latest" "$wait_marker_expected"; then
            return 0
        fi
        sleep 0.2
    done
    printf '%s\n' "$wait_marker_latest" >&2
    fail "guest marker did not appear: $wait_marker_expected"
}

wait_marker_prefix() {
    wait_prefix_path=$1
    wait_prefix_expected=$2
    wait_prefix_deadline=$(( $(date +%s) + 15 ))
    wait_prefix_latest=
    while [ "$(date +%s)" -lt "$wait_prefix_deadline" ]; do
        wait_prefix_latest=$(guest_marker_text "$wait_prefix_path" || :)
        if marker_has_prefix "$wait_prefix_latest" "$wait_prefix_expected"; then
            return 0
        fi
        sleep 0.2
    done
    printf '%s\n' "$wait_prefix_latest" >&2
    fail "guest marker prefix did not appear: $wait_prefix_expected"
}

wait_marker_exact_count() {
    wait_count_path=$1
    wait_count_expected=$2
    wait_count_minimum=$3
    wait_count_deadline=$(( $(date +%s) + 15 ))
    wait_count_latest=
    wait_count_observed=0
    while [ "$(date +%s)" -lt "$wait_count_deadline" ]; do
        wait_count_latest=$(guest_marker_text "$wait_count_path" || :)
        wait_count_observed=$(marker_exact_count "$wait_count_latest" "$wait_count_expected")
        if [ "$wait_count_observed" -ge "$wait_count_minimum" ]; then
            return 0
        fi
        sleep 0.2
    done
    printf '%s\n' "$wait_count_latest" >&2
    fail "guest marker $wait_count_expected appeared $wait_count_observed times; expected at least $wait_count_minimum"
}

wait_marker_prefix_count() {
    wait_prefix_count_path=$1
    wait_prefix_count_expected=$2
    wait_prefix_count_minimum=$3
    wait_prefix_count_deadline=$(( $(date +%s) + 15 ))
    wait_prefix_count_latest=
    wait_prefix_count_observed=0
    while [ "$(date +%s)" -lt "$wait_prefix_count_deadline" ]; do
        wait_prefix_count_latest=$(guest_marker_text "$wait_prefix_count_path" || :)
        wait_prefix_count_observed=$(marker_prefix_count \
            "$wait_prefix_count_latest" "$wait_prefix_count_expected")
        if [ "$wait_prefix_count_observed" -ge "$wait_prefix_count_minimum" ]; then
            return 0
        fi
        sleep 0.2
    done
    printf '%s\n' "$wait_prefix_count_latest" >&2
    fail "guest marker prefix $wait_prefix_count_expected appeared $wait_prefix_count_observed times; expected at least $wait_prefix_count_minimum"
}

fixture_process_count() {
    process_marker=$1
    process_id=$(fixture_process_id "$process_marker") || return 1
    process_output_raw=$(run_lsw 3 exec "$instance" -- tasklist.exe \
        /FI "PID eq $process_id" /FO CSV /NH </dev/null 2>/dev/null) || return 1
    process_output=$(printf '%s\n' "$process_output_raw" | tr -d '\r')
    printf '%s\n' "$process_output" | awk -F, -v expected="\"$process_id\"" '
        $1 == "\"powershell.exe\"" && $2 == expected { count++ }
        END { print count + 0 }
    '
}

fixture_process_id() {
    identity_marker=$1
    identity_marker_text=$(guest_marker_text "$identity_marker" || :)
    printf '%s\n' "$identity_marker_text" | awk -F '[=\t]' \
        '$2 == "process-id" && $3 ~ /^[1-9][0-9]*$/ { print $3; found=1; exit } END { if (!found) exit 1 }'
}

fixture_window_handle() {
    identity_marker=$1
    identity_marker_text=$(guest_marker_text "$identity_marker" || :)
    printf '%s\n' "$identity_marker_text" | awk -F '[=\t]' \
        '$2 == "window-hwnd" && $3 ~ /^[1-9][0-9]*$/ { print $3; found=1; exit } END { if (!found) exit 1 }'
}

wait_fixture_count() {
    wait_count_marker=$1
    wait_count_expected=$2
    wait_count_deadline=$(( $(date +%s) + 15 ))
    wait_count_observed=unknown
    while [ "$(date +%s)" -lt "$wait_count_deadline" ]; do
        wait_count_observed=$(fixture_process_count "$wait_count_marker" || printf unknown)
        if [ "$wait_count_observed" = "$wait_count_expected" ]; then
            return 0
        fi
        sleep 0.25
    done
    fail "fixture process count was $wait_count_observed; expected $wait_count_expected"
}

find_fixture_window() {
    # WSLg can dynamically wrap the Wayland title with host-controlled text
    # such as a copy-mode warning and the distro name. The complete production
    # title remains an ordinal substring containing this run-unique, non-secret ID;
    # combine it with the exact msrdc process and exact HWND on every action.
    find_output=$(run_windows_host 8 Find -TitleNeedle "$window_title" \
        -ProcessName msrdc 2>/dev/null) || return 1
    find_count=$(printf '%s\n' "$find_output" | awk -F= \
        '$1 == "HWND" && $2 ~ /^[1-9][0-9]*$/ { count++ } END { print count + 0 }')
    if [ "$find_count" -gt 1 ]; then
        echo "error: more than one visible WSLg fixture HWND matched the exact test title" >&2
        return 1
    fi
    printf '%s\n' "$find_output" | awk -F= \
        '$1 == "HWND" && $2 ~ /^[1-9][0-9]*$/ { print $2; exit }'
}

dump_window_start_diagnostics() {
    echo "--- seamless presenter process ---" >&2
    ps -o pid=,ppid=,state=,etimes=,wchan=,args= -p "$presenter_pid" >&2 || :
    echo "--- seamless presenter log (tail) ---" >&2
    tail -n 100 "$presenter_log" >&2 || :

    diagnostic_marker=$(guest_marker_text "$start_marker" || :)
    echo "--- seamless guest marker ---" >&2
    if [ -n "$diagnostic_marker" ]; then
        printf '%s\n' "$diagnostic_marker" >&2
    else
        echo "unavailable" >&2
    fi

    diagnostic_fixture_pid=$(fixture_process_id "$start_marker" 2>/dev/null || :)
    case "$diagnostic_fixture_pid" in
        [1-9][0-9]*)
            diagnostic_guest_script="\$ErrorActionPreference='Stop'; \$Process=Get-Process -Id $diagnostic_fixture_pid -ErrorAction Stop; [Console]::Out.WriteLine('FIXTURE_PID='+\$Process.Id); [Console]::Out.WriteLine('FIXTURE_SESSION='+\$Process.SessionId); [Console]::Out.WriteLine('FIXTURE_MAIN_HWND='+\$Process.MainWindowHandle); [Console]::Out.WriteLine('FIXTURE_TITLE_BASE64='+[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(\$Process.MainWindowTitle)))"
            echo "--- seamless guest process identity ---" >&2
            run_guest_powershell 8 "$diagnostic_guest_script" >&2 || :
            ;;
        *) ;;
    esac

    echo "--- WSLg host title-prefix HWNDs ---" >&2
    run_windows_host 8 Find -TitleNeedle "$window_title_prefix" \
        -ProcessName msrdc >&2 || :
    echo "--- Windows msrdc process identities ---" >&2
    # PowerShell expands the process properties; POSIX sh must pass them literally.
    # shellcheck disable=SC2016
    run_host 8 powershell.exe -NoLogo -NoProfile -NonInteractive \
        -ExecutionPolicy Bypass -Command \
        '$ErrorActionPreference="Stop"; @(Get-Process -Name msrdc -ErrorAction SilentlyContinue | Sort-Object Id) | ForEach-Object { [Console]::Out.WriteLine("MSRDC_PID="+$_.Id+",SESSION="+$_.SessionId+",MAIN_HWND="+$_.MainWindowHandle+",TITLE_BASE64="+[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($_.MainWindowTitle))) }' \
        >&2 || :
}

wait_fixture_window() {
    # This outer harness bound must exceed the agent's own 15-second HWND
    # discovery deadline plus transport, WTS launch, and cold PowerShell startup.
    # It does not relax the production selector or its fail-closed deadline.
    wait_window_deadline=$(( $(date +%s) + 40 ))
    while [ "$(date +%s)" -lt "$wait_window_deadline" ]; do
        if ! kill -0 "$presenter_pid" 2>/dev/null; then
            dump_window_start_diagnostics
            fail "seamless presenter exited before creating its WSLg Wayland host window"
        fi
        window_id=$(find_fixture_window || :)
        if [ -n "$window_id" ]; then
            return 0
        fi
        sleep 0.2
    done
    dump_window_start_diagnostics
    fail "timed out waiting for the LSW Seamless Fixture WSLg Wayland host window"
}

wait_no_fixture_window() {
    wait_gone_deadline=$(( $(date +%s) + 10 ))
    wait_gone_observation=unavailable
    while [ "$(date +%s)" -lt "$wait_gone_deadline" ]; do
        if wait_gone_observation=$(find_fixture_window); then
            if [ -z "$wait_gone_observation" ]; then
                return 0
            fi
        fi
        sleep 0.2
    done
    fail "could not prove the LSW Seamless Fixture host window was gone (last observation: $wait_gone_observation)"
}

window_field() {
    field_window=$1
    field_name=$2
    field_output=$(run_fixture_host 8 Query -Hwnd "$field_window") || return 1
    printf '%s\n' "$field_output" | awk -F= -v name="$field_name" \
        '$1 == name { print substr($0, length($1) + 2); found=1; exit } END { if (!found) exit 1 }'
}

activate_window() {
    activate_target=$1
    if [ -n "$focus_sink_window" ] && [ "$activate_target" = "$focus_sink_window" ]; then
        run_windows_host 8 Activate -Hwnd "$activate_target" \
            -TitleNeedle "$focus_sink_title" -ProcessName powershell -ExactTitle \
            || fail "Windows did not focus host HWND $activate_target"
        activate_query=$(run_windows_host 8 Query -Hwnd "$activate_target" \
            -TitleNeedle "$focus_sink_title" -ProcessName powershell -ExactTitle || :)
        activate_observed=$(printf '%s\n' "$activate_query" | awk -F= \
            '$1 == "FOREGROUND" { print $2; exit }')
    else
        run_fixture_host 8 Activate -Hwnd "$activate_target" \
            || fail "Windows did not focus host HWND $activate_target"
        activate_observed=$(window_field "$activate_target" FOREGROUND || :)
    fi
    [ "$activate_observed" = 1 ] \
        || fail "Windows foreground focus did not settle on host HWND $activate_target"
}

move_at() {
    move_window=$1
    move_x=$2
    move_y=$3
    move_attempt=0
    while [ "$move_attempt" -lt 3 ]; do
        if run_fixture_host 8 Pointer -Hwnd "$move_window" \
            -X "$move_x" -Y "$move_y" 2>/dev/null
        then
            return 0
        fi
        move_attempt=$((move_attempt + 1))
        # Non-activating Explorer popup surfaces can temporarily cover an
        # otherwise foreground WSLg proxy. Never forward through them: move
        # away, let the popup dismiss, reactivate the exact HWND, and retry the
        # same WindowFromPoint identity proof.
        run_fixture_host 8 PointerAway -Hwnd "$move_window" >/dev/null 2>&1 \
            || break
        sleep 0.4
        activate_window "$move_window"
    done
    run_fixture_host 8 Pointer -Hwnd "$move_window" -X "$move_x" -Y "$move_y" \
        || fail "could not move the Windows pointer into the unobscured seamless host HWND"
}

button_action() {
    button_name=$1
    run_fixture_host 8 Button -Hwnd "$window_id" -Button "$button_name" \
        || fail "could not send Windows host pointer action $button_name"
}

click_at() {
    click_window=$1
    click_x=$2
    click_y=$3
    # Pointer motion and button input reach winit as distinct asynchronous
    # Wayland events. Prove the Windows cursor landed in the exact proxy first,
    # then leave a bounded settling interval before sending explicit edges.
    # The later Burst gate still exercises rapid pairs without this interval.
    click_attempt=0
    while [ "$click_attempt" -lt 3 ]; do
        if run_fixture_host 8 PointerButton -Hwnd "$click_window" \
            -X "$click_x" -Y "$click_y" -Button Left -DelayMilliseconds 50 \
            2>/dev/null
        then
            return 0
        fi
        click_attempt=$((click_attempt + 1))
        # A non-activating Explorer popup can cover an otherwise foreground
        # WSLg proxy after a taskbar restore. The failed PointerButton sends no
        # edge until both identity checks pass, so dismiss and retry safely.
        run_fixture_host 8 PointerAway -Hwnd "$click_window" >/dev/null 2>&1 \
            || break
        sleep 0.4
        activate_window "$click_window"
    done
    run_fixture_host 8 PointerButton -Hwnd "$click_window" \
        -X "$click_x" -Y "$click_y" -Button Left -DelayMilliseconds 50 \
        || fail "could not click the identity-checked seamless host HWND"
}

click_percent() {
    percent_window=$1
    percent_x=$2
    percent_y=$3
    percent_width=$(window_field "$percent_window" WIDTH) || fail "could not read host window width"
    percent_height=$(window_field "$percent_window" HEIGHT) || fail "could not read host window height"
    click_at "$percent_window" "$((percent_width * percent_x / 100))" \
        "$((percent_height * percent_y / 100))"
}

fit_viewport_values() {
    fit_host_width=$1
    fit_host_height=$2
    fit_guest_width=$3
    fit_guest_height=$4
    if [ "$((fit_host_width * fit_guest_height))" -le \
        "$((fit_host_height * fit_guest_width))" ]
    then
        fit_width=$fit_host_width
        fit_height=$((fit_guest_height * fit_host_width / fit_guest_width))
    else
        fit_width=$((fit_guest_width * fit_host_height / fit_guest_height))
        fit_height=$fit_host_height
    fi
    [ "$fit_width" -gt 0 ] || fail "aspect-fit viewport collapsed to ${fit_width}x${fit_height}"
    [ "$fit_height" -gt 0 ] || fail "aspect-fit viewport collapsed to ${fit_width}x${fit_height}"
    fit_x=$(((fit_host_width - fit_width) / 2))
    fit_y=$(((fit_host_height - fit_height) / 2))
    printf '%s %s %s %s\n' "$fit_x" "$fit_y" "$fit_width" "$fit_height"
}

click_guest_point() {
    guest_point_window=$1
    guest_point_width=$2
    guest_point_height=$3
    guest_point_x=$4
    guest_point_y=$5
    guest_point_host_width=$(window_field "$guest_point_window" WIDTH) \
        || fail "could not read host width for a guest-space click"
    guest_point_host_height=$(window_field "$guest_point_window" HEIGHT) \
        || fail "could not read host height for a guest-space click"
    guest_point_fit=$(fit_viewport_values \
        "$guest_point_host_width" "$guest_point_host_height" \
        "$guest_point_width" "$guest_point_height")
    guest_point_view_x=${guest_point_fit%% *}
    guest_point_fit=${guest_point_fit#* }
    guest_point_view_y=${guest_point_fit%% *}
    guest_point_fit=${guest_point_fit#* }
    guest_point_view_width=${guest_point_fit%% *}
    guest_point_view_height=${guest_point_fit#* }
    guest_point_host_x=$((guest_point_view_x \
        + (2 * guest_point_x + 1) * guest_point_view_width / (2 * guest_point_width)))
    guest_point_host_y=$((guest_point_view_y \
        + (2 * guest_point_y + 1) * guest_point_view_height / (2 * guest_point_height)))
    click_at "$guest_point_window" "$guest_point_host_x" "$guest_point_host_y"
}

report_fitted_viewport() {
    fitted_label=$1
    fitted_host_width=$2
    fitted_host_height=$3
    fitted_guest_width=$4
    fitted_guest_height=$5
    fitted_values=$(fit_viewport_values \
        "$fitted_host_width" "$fitted_host_height" \
        "$fitted_guest_width" "$fitted_guest_height")
    fitted_x=${fitted_values%% *}
    fitted_values=${fitted_values#* }
    fitted_y=${fitted_values%% *}
    fitted_values=${fitted_values#* }
    fitted_width=${fitted_values%% *}
    fitted_height=${fitted_values#* }
    printf '%s HOST=%sx%s GUEST=%sx%s VIEWPORT=%s,%s,%sx%s\n' \
        "$fitted_label" "$fitted_host_width" "$fitted_host_height" \
        "$fitted_guest_width" "$fitted_guest_height" \
        "$fitted_x" "$fitted_y" "$fitted_width" "$fitted_height"
}

resize_via_guest_southeast() {
    resize_target_width=$1
    resize_target_height=$2
    resize_target_observed_width=unknown
    resize_target_observed_height=unknown
    for resize_edge_inset in 8 6; do
        resize_current_width=$(window_field "$window_id" WIDTH) \
            || fail "could not read the host width before guest resize"
        resize_current_height=$(window_field "$window_id" HEIGHT) \
            || fail "could not read the host height before guest resize"
        resize_target_delta_x=$((resize_target_width - resize_current_width))
        resize_target_delta_y=$((resize_target_height - resize_current_height))
        [ "$resize_target_delta_x" -ne 0 ] \
            || [ "$resize_target_delta_y" -ne 0 ] \
            || return 0
        if ! run_fixture_host 8 Drag -Hwnd "$window_id" \
            -X "$((resize_current_width - resize_edge_inset))" \
            -Y "$((resize_current_height - resize_edge_inset))" \
            -DeltaX "$resize_target_delta_x" -DeltaY "$resize_target_delta_y" \
            -DelayMilliseconds 300
        then
            continue
        fi
        resize_target_deadline=$(( $(date +%s) + 4 ))
        resize_target_observed_width=$resize_current_width
        resize_target_observed_height=$resize_current_height
        while [ "$(date +%s)" -lt "$resize_target_deadline" ]; do
            resize_target_observed_width=$(window_field "$window_id" WIDTH \
                || printf '%s' "$resize_target_observed_width")
            resize_target_observed_height=$(window_field "$window_id" HEIGHT \
                || printf '%s' "$resize_target_observed_height")
            if integer_near "$resize_target_observed_width" "$resize_target_width" 4 \
                && integer_near "$resize_target_observed_height" "$resize_target_height" 4
            then
                return 0
            fi
            sleep 0.2
        done
    done
    fail "guest southeast resize settled at ${resize_target_observed_width}x${resize_target_observed_height}; expected about ${resize_target_width}x${resize_target_height}"
}

move_percent() {
    move_window=$1
    move_x_percent=$2
    move_y_percent=$3
    move_width=$(window_field "$move_window" WIDTH) || fail "could not read host window width"
    move_height=$(window_field "$move_window" HEIGHT) || fail "could not read host window height"
    move_at "$move_window" "$((move_width * move_x_percent / 100))" \
        "$((move_height * move_y_percent / 100))"
}

capture_window() {
    capture_label=$1
    register_host_artifact "$capture_label.png"
    capture_path="$host_root/$capture_label.png"
    capture_windows=$(wslpath -w "$capture_path") \
        || fail "could not translate the $capture_label capture path"
    run_fixture_host 10 Screenshot -Hwnd "$window_id" -Output "$capture_windows" \
        || fail "the Windows host helper could not capture $capture_label"
    if [ ! -s "$capture_path" ]; then
        fail "ImageMagick produced an empty $capture_label capture"
    fi
}

image_has_color() {
    color_image=$1
    color_value=$2
    color_minimum=$3
    color_fraction=$(run_host 8 convert "$color_image" -alpha off -fuzz 6% \
        -fill black +opaque "$color_value" -fill white -opaque "$color_value" \
        -colorspace gray -format '%[fx:mean]' info: 2>/dev/null) || return 1
    awk -v observed="$color_fraction" -v minimum="$color_minimum" \
        'BEGIN { exit(!(observed + 0 >= minimum + 0)) }'
}

image_has_dimensions() {
    dimension_image=$1
    dimension_width=$2
    dimension_height=$3
    if [ "$dimension_width" -eq 0 ] && [ "$dimension_height" -eq 0 ]; then
        return 0
    fi
    dimension_observed=$(run_host 5 identify -format '%w %h' "$dimension_image" 2>/dev/null) \
        || return 1
    [ "$dimension_observed" = "$dimension_width $dimension_height" ]
}

wait_visual_sentinel() {
    visual_label=$1
    visual_color=$2
    visual_minimum=$3
    visual_width=$4
    visual_height=$5
    visual_deadline=$(( $(date +%s) + 12 ))
    visual_last_fraction=none
    visual_last_dimensions=none
    while [ "$(date +%s)" -lt "$visual_deadline" ]; do
        capture_window "$visual_label"
        visual_last_fraction=$(run_host 8 convert "$capture_path" -alpha off -fuzz 6% \
            -fill black +opaque "$visual_color" -fill white -opaque "$visual_color" \
            -colorspace gray -format '%[fx:mean]' info: 2>/dev/null || printf none)
        visual_last_dimensions=$(run_host 5 identify -format '%w %h' "$capture_path" \
            2>/dev/null || printf none)
        if image_has_color "$capture_path" "$visual_color" "$visual_minimum" \
            && image_has_dimensions "$capture_path" "$visual_width" "$visual_height"
        then
            return 0
        fi
        sleep 0.2
    done
    fail "$visual_label did not show sentinel $visual_color (fraction=$visual_last_fraction, dimensions=$visual_last_dimensions)"
}

wait_host_frame_change() {
    frame_label=$1
    capture_window "$frame_label-initial"
    frame_initial=$(run_host 5 identify -format '%#' "$capture_path" 2>/dev/null) \
        || fail "could not hash the initial $frame_label frame"
    frame_deadline=$(( $(date +%s) + 10 ))
    frame_previous=$frame_initial
    frame_transitions=0
    while [ "$(date +%s)" -lt "$frame_deadline" ]; do
        sleep 0.08
        capture_window "$frame_label-next"
        frame_observed=$(run_host 5 identify -format '%#' "$capture_path" 2>/dev/null) \
            || fail "could not hash a later $frame_label frame"
        if [ "$frame_observed" != "$frame_previous" ]; then
            frame_transitions=$((frame_transitions + 1))
            frame_previous=$frame_observed
            if [ "$frame_transitions" -ge 3 ]; then
                return 0
            fi
        fi
    done
    fail "$frame_label produced only $frame_transitions host-frame transitions"
}

wait_window_state() {
    state_window=$1
    state_name=$2
    state_expected=$3
    state_deadline=$(( $(date +%s) + 10 ))
    state_observed=unavailable
    while [ "$(date +%s)" -lt "$state_deadline" ]; do
        state_observed=$(window_field "$state_window" "$state_name" || printf unavailable)
        if [ "$state_observed" = "$state_expected" ]; then
            return 0
        fi
        sleep 0.2
    done
    fail "Windows host HWND state $state_name was $state_observed; expected $state_expected"
}

integer_near() {
    near_observed=$1
    near_expected=$2
    near_tolerance=$3
    [ "$near_observed" -ge "$((near_expected - near_tolerance))" ] \
        && [ "$near_observed" -le "$((near_expected + near_tolerance))" ]
}

assert_guest_only_chrome() {
    chrome_window=$1
    chrome_query=$(run_fixture_host 8 Query -Hwnd "$chrome_window") \
        || fail "could not inspect the WSLg host HWND"
    for chrome_required in \
        PROCESS=msrdc VISIBLE=1 CLOAKED=0 OWNER=0 HAS_CAPTION=0 \
        HAS_DLG_MODAL_FRAME=0 HAS_CLIENT_EDGE=0
    do
        if ! printf '%s\n' "$chrome_query" | grep -Fx "$chrome_required" >/dev/null; then
            printf '%s\n' "$chrome_query" >&2
            fail "WSLg host HWND violated the guest-only chrome invariant: $chrome_required"
        fi
    done

    chrome_marker=$(guest_marker_text "$marker_main") \
        || fail "could not read the guest frame-size marker"
    marker_has_exact "$chrome_marker" 'frame-size-source=dwm' \
        || fail "guest frame-size marker did not come from DWM visible bounds"
    chrome_size=$(printf '%s\n' "$chrome_marker" | awk -F '[=x\t]' \
        '$2 == "frame-size" && $3 ~ /^[1-9][0-9]*$/ && $4 ~ /^[1-9][0-9]*$/ { size=$3 " " $4 } END { print size }')
    [ -n "$chrome_size" ] || fail "guest fixture did not report its exact WGC frame size"
    chrome_expected_width=${chrome_size% *}
    chrome_expected_height=${chrome_size#* }
    chrome_width=$(window_field "$chrome_window" WIDTH) \
        || fail "could not read WSLg host client width"
    chrome_height=$(window_field "$chrome_window" HEIGHT) \
        || fail "could not read WSLg host client height"
    if [ "$chrome_width" -ne "$chrome_expected_width" ] \
        || [ "$chrome_height" -ne "$chrome_expected_height" ]
    then
        fail "WSLg host client ${chrome_width}x${chrome_height} added outer chrome around guest frame ${chrome_expected_width}x${chrome_expected_height}"
    fi

    chrome_window_left=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "WINDOW_LEFT" && $2 ~ /^-?[0-9]+$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg GetWindowRect left edge"
    chrome_window_top=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "WINDOW_TOP" && $2 ~ /^-?[0-9]+$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg GetWindowRect top edge"
    chrome_window_width=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "WINDOW_WIDTH" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg GetWindowRect width"
    chrome_window_height=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "WINDOW_HEIGHT" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg GetWindowRect height"
    chrome_dwm_left=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "DWM_LEFT" && $2 ~ /^-?[0-9]+$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg DWM left edge"
    chrome_dwm_top=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "DWM_TOP" && $2 ~ /^-?[0-9]+$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg DWM top edge"
    chrome_dwm_width=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "DWM_WIDTH" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg DWM width"
    chrome_dwm_height=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "DWM_HEIGHT" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read WSLg DWM height"
    chrome_style=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "STYLE" && $2 ~ /^0x[0-9A-F]{16}$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read the exact WSLg host style"
    chrome_exstyle=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "EXSTYLE" && $2 ~ /^0x[0-9A-F]{16}$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read the exact WSLg host extended style"
    chrome_window_edge=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "HAS_WINDOW_EDGE" && $2 ~ /^[01]$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "could not read the WSLg WS_EX_WINDOWEDGE state"
    chrome_session=$(printf '%s\n' "$chrome_query" | awk -F= \
        '$1 == "SESSION" && $2 ~ /^[1-9][0-9]*$/ { print $2; found=1; exit } END { if (!found) exit 1 }') \
        || fail "WSLg proxy was not owned by an interactive Windows session"

    # DWM extended bounds describe visible pixels. Equality with GetClientRect,
    # plus absence of caption/edge styles above, proves the host did not add a
    # second visible title bar around the already captured guest DWM frame.
    if [ "$chrome_dwm_width" -ne "$chrome_width" ] \
        || [ "$chrome_dwm_height" -ne "$chrome_height" ]
    then
        printf '%s\n' "$chrome_query" >&2
        fail "WSLg DWM frame ${chrome_dwm_width}x${chrome_dwm_height} differs from client ${chrome_width}x${chrome_height}; a visible outer frame remains"
    fi
    chrome_inset_left=$((chrome_dwm_left - chrome_window_left))
    chrome_inset_top=$((chrome_dwm_top - chrome_window_top))
    chrome_inset_right=$((chrome_window_left + chrome_window_width - chrome_dwm_left - chrome_dwm_width))
    chrome_inset_bottom=$((chrome_window_top + chrome_window_height - chrome_dwm_top - chrome_dwm_height))
    for chrome_inset in \
        "$chrome_inset_left" "$chrome_inset_top" "$chrome_inset_right" "$chrome_inset_bottom"
    do
        if [ "$chrome_inset" -lt 0 ] || [ "$chrome_inset" -gt 64 ]; then
            printf '%s\n' "$chrome_query" >&2
            fail "WSLg outer/DWM inset $chrome_inset exceeded the bounded invisible resize border"
        fi
    done
    # WSLg RAIL currently sets WS_EX_WINDOWEDGE on its proxy even when it owns
    # zero DWM-visible pixels. Record that bit, but decide visual chrome from
    # the exact DWM/client geometry rather than the nominal style alone.
    printf 'CHROME SESSION=%s STYLE=%s EXSTYLE=%s WINDOW_EDGE=%s WINDOW=%sx%s DWM=%sx%s CLIENT=%sx%s INSETS=%s,%s,%s,%s\n' \
        "$chrome_session" "$chrome_style" "$chrome_exstyle" "$chrome_window_edge" \
        "$chrome_window_width" "$chrome_window_height" \
        "$chrome_dwm_width" "$chrome_dwm_height" "$chrome_width" "$chrome_height" \
        "$chrome_inset_left" "$chrome_inset_top" "$chrome_inset_right" "$chrome_inset_bottom"
    capture_window guest-only-chrome
    chrome_capture_dimensions=$(run_host 5 identify -format '%w %h' "$capture_path" 2>/dev/null) \
        || fail "could not inspect the guest-only chrome capture dimensions"
    [ "$chrome_capture_dimensions" = "$chrome_size" ] \
        || fail "host capture dimensions $chrome_capture_dimensions differ from guest frame $chrome_size"
}
