// SPDX-License-Identifier: GPL-3.0-or-later
# shellcheck shell=sh
# Candidate attestation, command wrappers, artifact ownership, and cleanup.
# shellcheck disable=SC2154

candidate_sha256() {
    candidate_value=$(sha256sum -- "$1" | awk '{ print $1; exit }') || {
        echo "error: could not hash the exact Linux $2 candidate" >&2
        return 1
    }
    printf '%s\n' "$candidate_value" | grep -Eq '^[0-9a-f]{64}$' || {
        echo "error: sha256sum returned an invalid Linux $2 candidate hash" >&2
        return 1
    }
    printf '%s\n' "$candidate_value"
}

run_lsw() {
    run_lsw_seconds=$1
    shift
    timeout --signal=TERM --kill-after=3s "${run_lsw_seconds}s" "$lsw" "$@"
}

run_host() {
    run_host_seconds=$1
    shift
    timeout --signal=TERM --kill-after=2s "${run_host_seconds}s" "$@"
}

run_windows_host() {
    windows_host_seconds=$1
    windows_host_action=$2
    shift 2
    run_host "$windows_host_seconds" powershell.exe -NoLogo -NoProfile -NonInteractive \
        -ExecutionPolicy Bypass -File "$host_helper_windows" \
        -Action "$windows_host_action" "$@"
}

run_fixture_host() {
    fixture_host_seconds=$1
    fixture_host_action=$2
    shift 2
    run_windows_host "$fixture_host_seconds" "$fixture_host_action" \
        -TitleNeedle "$window_title" -ProcessName msrdc "$@"
}

run_guest_powershell() {
    guest_powershell_seconds=$1
    guest_powershell_script=$2
    guest_powershell_encoded=$(printf '%s' "$guest_powershell_script" \
        | iconv -f UTF-8 -t UTF-16LE \
        | base64 \
        | tr -d '\n')
    run_lsw "$guest_powershell_seconds" exec "$instance" -- \
        powershell.exe -NoLogo -NoProfile -NonInteractive \
        -EncodedCommand "$guest_powershell_encoded" </dev/null
}

pass() {
    printf 'CHECK %s=pass\n' "$1"
}

fail() {
    echo "error: $*" >&2
    exit 1
}

register_host_artifact() {
    register_host_name=$1
    case "$register_host_name" in
        ''|*[!A-Za-z0-9._-]*) fail "refusing unexpected host artifact name $register_host_name" ;;
        *) ;;
    esac
    host_artifacts=${host_artifacts:+"$host_artifacts
"}$register_host_name
}

register_fixture_marker() {
    register_marker_name=${1##*\\}
    case "$register_marker_name" in
        main.tsv|kill.tsv|recovery.tsv|animate.tsv|presenter-error.tsv|close-prompt.tsv|host-close.tsv|initial-maximized.tsv) ;;
        *) fail "refusing to register unexpected fixture marker $register_marker_name" ;;
    esac
    case ",$cleanup_expected_markers," in
        *",$register_marker_name,"*) ;;
        *) cleanup_expected_markers=${cleanup_expected_markers:+$cleanup_expected_markers,}$register_marker_name ;;
    esac
}

terminate_presenter() {
    case "$presenter_pid" in
        ''|*[!0-9]*)
            presenter_pid=
            return 0
            ;;
    esac
    if kill -0 "$presenter_pid" 2>/dev/null; then
        kill -TERM "$presenter_pid" 2>/dev/null || :
        terminate_attempt=0
        while kill -0 "$presenter_pid" 2>/dev/null \
            && [ "$terminate_attempt" -lt 30 ]
        do
            terminate_attempt=$((terminate_attempt + 1))
            sleep 0.1
        done
    fi
    if kill -0 "$presenter_pid" 2>/dev/null; then
        kill -KILL "$presenter_pid" 2>/dev/null || :
    fi
    wait "$presenter_pid" 2>/dev/null || :
    presenter_pid=
}

terminate_focus_sink() {
    case "$focus_sink_pid" in
        ''|*[!0-9]*)
            focus_sink_pid=
            focus_sink_window=
            return 0
            ;;
    esac
    case "$focus_sink_window" in
        ''|*[!0-9]*) ;;
        *) run_windows_host 5 Close -Hwnd "$focus_sink_window" \
            -TitleNeedle "$focus_sink_title" -ProcessName powershell \
            -ExactTitle >/dev/null 2>&1 || : ;;
    esac
    if kill -0 "$focus_sink_pid" 2>/dev/null; then
        focus_stop_attempt=0
        while kill -0 "$focus_sink_pid" 2>/dev/null \
            && [ "$focus_stop_attempt" -lt 20 ]
        do
            focus_stop_attempt=$((focus_stop_attempt + 1))
            sleep 0.1
        done
    fi
    if kill -0 "$focus_sink_pid" 2>/dev/null; then
        kill -TERM "$focus_sink_pid" 2>/dev/null || :
        sleep 0.2
    fi
    if kill -0 "$focus_sink_pid" 2>/dev/null; then
        kill -KILL "$focus_sink_pid" 2>/dev/null || :
    fi
    wait "$focus_sink_pid" 2>/dev/null || :
    focus_sink_pid=
    focus_sink_window=
    focus_sink_title=
}

cleanup_guest_root() {
    if [ "$guest_root_created" -ne 1 ]; then
        return 0
    fi
    case "$guest_root" in
        'C:\Users\Public\Documents\LSW-Seamless-E2E-'*) ;;
        *)
            echo "error: refusing cleanup of unexpected guest path $guest_root" >&2
            return 1
            ;;
    esac
    cleanup_status=$(run_lsw 6 status "$instance" 2>/dev/null || :)
    if ! printf '%s\n' "$cleanup_status" | grep -Fx 'STATE=running' >/dev/null; then
        echo "warning: guest test directory remains because the existing instance is no longer running: $guest_root" >&2
        return 1
    fi
    cleanup_script="\$ErrorActionPreference='Stop'; \$Expected=[IO.Path]::GetFullPath('$guest_root'); \$Signal=[IO.Path]::GetFullPath('$guest_cleanup_signal'); \$ExpectedSignal=[IO.Path]::Combine(\$Expected,'cleanup.signal'); \$MarkerCsv='$cleanup_expected_markers'; if (\$Signal -cne \$ExpectedSignal) { throw 'guest cleanup signal path changed' }; if (Test-Path -LiteralPath \$Expected) { \$Directory=Get-Item -LiteralPath \$Expected -Force; if (\$Directory.FullName -cne \$Expected) { throw 'guest cleanup path changed' }; if ((\$Directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'guest cleanup directory must not be a reparse point' }; if (Test-Path -LiteralPath \$Signal) { \$SignalItem=Get-Item -LiteralPath \$Signal -Force; if (\$SignalItem.FullName -cne \$Signal -or \$SignalItem.PSIsContainer -or ((\$SignalItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'guest cleanup signal must be the exact regular file' } }; [IO.File]::WriteAllText(\$Signal,'cleanup',[Text.UTF8Encoding]::new(\$false)); \$MarkerNames=@(); if (-not [string]::IsNullOrEmpty(\$MarkerCsv)) { \$MarkerNames=@(\$MarkerCsv.Split(',')) }; \$Deadline=[DateTime]::UtcNow.AddSeconds(10); do { \$Pending=@(); foreach (\$MarkerName in \$MarkerNames) { if (\$MarkerName -notmatch '^[A-Za-z0-9-]+[.]tsv\$') { throw 'unexpected fixture marker name' }; \$MarkerPath=[IO.Path]::GetFullPath([IO.Path]::Combine(\$Expected,\$MarkerName)); if ([IO.Path]::GetDirectoryName(\$MarkerPath) -cne \$Expected -or -not (Test-Path -LiteralPath \$MarkerPath -PathType Leaf)) { \$Pending += \$MarkerName; continue }; \$Marker=Get-Item -LiteralPath \$MarkerPath -Force; if (\$Marker.FullName -cne \$MarkerPath -or ((\$Marker.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'fixture marker must be the exact regular file' }; \$Lines=@(Get-Content -LiteralPath \$MarkerPath -ErrorAction Stop); if (@(\$Lines | Where-Object { \$_ -match '\tclosed\$' }).Count -gt 0) { continue }; \$PidLine=@(\$Lines | Where-Object { \$_ -match '\tprocess-id=([1-9][0-9]*)\$' } | Select-Object -First 1); if (\$PidLine.Count -eq 0) { \$Pending += \$MarkerName; continue }; [void](\$PidLine[0] -match '\tprocess-id=([1-9][0-9]*)\$'); if (\$null -ne (Get-Process -Id ([int]\$Matches[1]) -ErrorAction SilentlyContinue)) { \$Pending += \$MarkerName } }; if (\$Pending.Count -eq 0) { break }; Start-Sleep -Milliseconds 100 } while ([DateTime]::UtcNow -lt \$Deadline); if (\$Pending.Count -ne 0) { throw ('fixture cleanup signal timed out: '+(\$Pending -join ',')) }; \$Stack=[Collections.Generic.Stack[string]]::new(); \$Stack.Push(\$Expected); while (\$Stack.Count -gt 0) { foreach (\$Entry in [IO.Directory]::EnumerateFileSystemEntries(\$Stack.Pop())) { \$Attributes=[IO.File]::GetAttributes(\$Entry); if ((\$Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'guest cleanup tree contains a reparse point' }; if ((\$Attributes -band [IO.FileAttributes]::Directory) -ne 0) { \$Stack.Push(\$Entry) } } }; for (\$Attempt=0; \$Attempt -lt 30; \$Attempt++) { try { Remove-Item -LiteralPath \$Expected -Recurse -Force -ErrorAction Stop; break } catch { if (\$Attempt -eq 29) { throw }; Start-Sleep -Milliseconds 200 } } }; if (Test-Path -LiteralPath \$Expected) { throw 'guest seamless E2E directory remained after cleanup' }"
    cleanup_script="\$ErrorActionPreference='Stop'; \$OwnershipRoot=[IO.Path]::GetFullPath('$guest_root'); \$Owner=[IO.Path]::GetFullPath('$guest_owner_token_path'); if (\$Owner -cne [IO.Path]::Combine(\$OwnershipRoot,'owner.token')) { throw 'guest owner-token path changed' }; if (Test-Path -LiteralPath \$OwnershipRoot) { if (-not (Test-Path -LiteralPath \$Owner -PathType Leaf)) { throw 'guest cleanup root has no ownership token' }; \$OwnerItem=Get-Item -LiteralPath \$Owner -Force; if (\$OwnerItem.FullName -cne \$Owner -or ((\$OwnerItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or [IO.File]::ReadAllText(\$Owner) -cne '$guest_owner_token') { throw 'guest cleanup root ownership token is invalid' } }; $cleanup_script"
    if run_guest_powershell 20 "$cleanup_script" >/dev/null 2>&1
    then
        guest_root_created=0
        return 0
    fi
    echo "error: could not remove guest seamless E2E directory $guest_root" >&2
    return 1
}

cleanup() {
    cleanup_result=$?
    trap - EXIT HUP INT TERM
    run_windows_host 5 ReleaseAll 2>/dev/null || :
    terminate_focus_sink
    terminate_presenter
    if [ "$cleanup_result" -ne 0 ] && [ -n "$presenter_log" ] && [ -f "$presenter_log" ]; then
        echo "--- seamless presenter log (tail) ---" >&2
        tail -n 100 "$presenter_log" >&2 || :
    fi
    if ! cleanup_guest_root && [ "$cleanup_result" -eq 0 ]; then
        cleanup_result=1
    fi
    host_cleanup_safe=0
    host_cleanup_fd_identity=$(stat -Lc '%d:%i:%u:%a' -- "$host_root_fd_path" 2>/dev/null || :)
    if [ "$host_cleanup_fd_identity" = "$host_root_identity" ]; then
        host_cleanup_safe=1
    fi
    if [ "$host_cleanup_safe" -eq 1 ]; then
        host_cleanup_old_ifs=$IFS
        IFS='
'
        for host_artifact in $host_artifacts
        do
            case "$host_artifact" in
                ''|*[!A-Za-z0-9._-]*)
                    echo "warning: refusing cleanup of unexpected host artifact $host_artifact" >&2
                    cleanup_result=1
                    ;;
                *)
                    if ! rm -f -- "$host_root_fd_path/$host_artifact"; then
                        echo "warning: could not remove exact host artifact $host_artifact" >&2
                        cleanup_result=1
                    fi
                    ;;
            esac
        done
        IFS=$host_cleanup_old_ifs
        host_cleanup_path_identity=
        case "$host_root" in
            "$temporary_base"/lsw-seamless-e2e.??????)
                if [ -d "$host_root" ] && [ ! -L "$host_root" ]; then
                    host_cleanup_path_identity=$(stat -Lc '%d:%i:%u:%a' -- "$host_root" 2>/dev/null || :)
                fi
                ;;
            *) ;;
        esac
        if [ "$host_cleanup_path_identity" != "$host_root_identity" ]; then
            echo "warning: refusing rmdir because the seamless E2E host path identity changed: $host_root" >&2
            cleanup_result=1
        elif ! rmdir -- "$host_root" 2>/dev/null; then
            echo "warning: seamless E2E host directory contains an unexpected artifact: $host_root" >&2
            cleanup_result=1
        fi
    else
        echo "warning: refusing cleanup because the seamless E2E host directory identity changed: $host_root" >&2
        cleanup_result=1
    fi
    exec 9<&-
    if [ "$cleanup_result" -eq 0 ] && [ "$matrix_complete" -eq 1 ]; then
        printf 'RESULT=pass instance=%s cli_sha256=%s guest_agent_sha256=%s\n' \
            "$instance" "$cli_sha" "$guest_agent_sha"
    fi
    exit "$cleanup_result"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

prepare_script="\$ErrorActionPreference='Stop'; \$Path=[IO.Path]::GetFullPath('$guest_root'); \$Owner=[IO.Path]::GetFullPath('$guest_owner_token_path'); if (\$Owner -cne [IO.Path]::Combine(\$Path,'owner.token')) { throw 'guest owner-token path changed' }; if (Test-Path -LiteralPath \$Path) { throw 'guest seamless E2E path already exists' }; \$Directory=New-Item -Path \$Path -ItemType Directory -ErrorAction Stop; if (\$Directory.FullName -cne \$Path -or ((\$Directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { throw 'guest seamless E2E directory identity changed' }; [IO.File]::WriteAllText(\$Owner,'$guest_owner_token',[Text.UTF8Encoding]::new(\$false))"
guest_root_created=1
run_guest_powershell 10 "$prepare_script" >/dev/null \
    || fail "could not create the bounded guest fixture directory"
run_lsw 15 cp "$fixture_source" "$guest_fixture" >/dev/null \
    || fail "lsw cp could not stage the seamless fixture in the default instance"
pass fixture_staged
