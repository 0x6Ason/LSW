// SPDX-License-Identifier: GPL-3.0-or-later
# shellcheck shell=sh
# Shared lifecycle, cleanup, attestation, and GUI-handoff helpers.
# shellcheck disable=SC2154

run_conpty_probe() {
    python3 "$workspace_root/scripts/run-windows-conpty-probe.py" "$@"
}

audit_slim_profile() {
    phase=$1
    [ "$profile" = slim ] || return 0
    report_destination=
    if [ "$phase" = boot-1 ]; then
        report_destination=$profile_report_destination
    fi
    sh "$workspace_root/scripts/run-windows-slim-profile-audit.sh" \
        "$lsw" "$instance" "$phase" "$profile_audit_dir/$phase.json" 30 \
        "$report_destination"
    profile_audit_boots=$((profile_audit_boots + 1))
    profile_revision=slim-v2
}

collect_e2e_artifacts() {
    result=$1
    if [ -z "$artifact_dir" ]; then
        return 0
    fi

    metadata="$artifact_dir/attestation.env"
    metadata_tmp="$metadata.tmp-$$"
    {
        printf 'result=%s\n' "$result"
        printf 'commit=%s\n' "${GITHUB_SHA:-unknown}"
        printf 'edition_requested=%s\n' "$edition"
        printf 'edition_installed=%s\n' "$guest_edition"
        printf 'profile_requested=%s\n' "$profile"
        printf 'windows_build=%s\n' "$guest_build"
        printf 'setup_account_removed=%s\n' "$setup_account_removed"
        printf 'agent_service_name=%s\n' "$agent_service_name"
        printf 'agent_service_start_mode=%s\n' "$agent_service_start_mode"
        printf 'agent_service_start_name=%s\n' "$agent_service_start_name"
        printf 'agent_service_state=%s\n' "$agent_service_state"
        printf 'agent_service_sid_initial=%s\n' "$agent_service_sid"
        printf 'agent_service_sid_cold=%s\n' "$cold_agent_service_sid"
        printf 'agent_service_pid_initial=%s\n' "$agent_service_pid"
        printf 'agent_service_pid_cold=%s\n' "$cold_agent_service_pid"
        printf 'cold_interactive_user=%s\n' "$cold_interactive_user"
        printf 'initial_interactive_user=%s\n' "$initial_interactive_user"
        printf 'cached_unattend_removed=%s\n' "$cached_unattend_removed"
        printf 'setup_payload_removed=%s\n' "$setup_payload_removed"
        printf 'automatic_logon=%s\n' "$automatic_logon"
        printf 'desktop_user_registered=%s\n' "$desktop_user_registered"
        printf 'desktop_user_role=%s\n' "$desktop_user_role"
        printf 'separate_administrator_created=%s\n' "$separate_administrator_created"
        printf 'windows_sudo_force_new_window=%s\n' "$windows_sudo_force_new_window"
        printf 'windows_sudo_policy=%s\n' "$windows_sudo_policy"
        printf 'windows_uac_enabled=%s\n' "$windows_uac_enabled"
        printf 'clone_identity_isolated=%s\n' "$clone_identity_isolated"
        printf 'folder_share_boundaries=%s\n' "$folder_share_boundaries"
        printf 'live_folder_verified=%s\n' "$live_folder_verified"
        printf 'resource_governor_verified=%s\n' "$resource_governor_verified"
        printf 'exec_context_verified=%s\n' "$exec_context_verified"
        printf 'signal_status=%s\n' "$signal_status"
        printf 'detached_run_verified=%s\n' "$detached_run_verified"
        printf 'recursive_transfer_verified=%s\n' "$recursive_transfer_verified"
        printf 'watch_sync_verified=%s\n' "$watch_sync_verified"
        printf 'gui_handoff_requested=%s\n' "$gui_handoff"
        printf 'gui_handoff_ready=%s\n' "$gui_handoff_complete"
        printf 'gui_handoff_instance=%s\n' "$gui_instance"
        printf 'iso_sha256=%s\n' "$iso_sha256"
        printf 'official_iso_sha256=%s\n' "$official_iso_sha256"
        printf 'license_status=%s\n' "$license_status"
        printf 'license_helper_start_mode=%s\n' "$license_helper_start_mode"
        printf 'license_helper_start_name=%s\n' "$license_helper_start_name"
        printf 'user_helper_start_mode=%s\n' "$user_helper_start_mode"
        printf 'user_helper_start_name=%s\n' "$user_helper_start_name"
        printf 'maintenance_helper_start_mode=%s\n' "$maintenance_helper_start_mode"
        printf 'maintenance_helper_start_name=%s\n' "$maintenance_helper_start_name"
        printf 'profile_revision=%s\n' "$profile_revision"
        printf 'profile_audit_boots=%s\n' "$profile_audit_boots"
        printf 'profile_host_allocated_bytes_after_trim=%s\n' "$profile_host_allocated_bytes"
        printf 'lsw_sha256=%s\n' "$(sha256sum "$lsw" | awk '{ print $1 }')"
        printf 'lswd_sha256=%s\n' "$(sha256sum "$lswd" | awk '{ print $1 }')"
        printf 'lswg_sha256=%s\n' "$(sha256sum "$lswg" | awk '{ print $1 }')"
        printf 'agent_sha256=%s\n' "$(sha256sum "$agent" | awk '{ print $1 }')"
        printf 'ovmf_code_sha256=%s\n' "$ovmf_code_sha256"
        printf 'ovmf_vars_sha256=%s\n' "$ovmf_vars_sha256"
        printf 'kernel=%s\n' "$(uname -srmo)"
        printf 'qemu=%s\n' "$(qemu-system-x86_64 --version 2>/dev/null | awk 'NR == 1 { print; exit }')"
        printf 'swtpm=%s\n' "$(swtpm --version 2>/dev/null | awk 'NR == 1 { print; exit }')"
    } >"$metadata_tmp"
    chmod 600 "$metadata_tmp"
    mv "$metadata_tmp" "$metadata"

    if [ -f "$LSW_STATE_DIR/instances/$instance/instance.lsw" ]; then
        if [ ! -e "$artifact_dir/bench.json" ]; then
            timeout 30s "$lsw" bench "$instance" --json \
                >"$artifact_dir/bench.json" 2>/dev/null || :
            if [ -e "$artifact_dir/bench.json" ]; then
                chmod 600 "$artifact_dir/bench.json"
            fi
        fi
        if [ ! -e "$artifact_dir/diagnose.tar.gz" ]; then
            timeout 30s "$lsw" diagnose "$instance" --bundle \
                --output "$artifact_dir/diagnose.tar.gz" >/dev/null 2>&1 || :
        fi
    fi
    if [ -f "$e2e_root/lswd.log" ]; then
        tail -c 131072 "$e2e_root/lswd.log" >"$artifact_dir/lswd.log"
        chmod 600 "$artifact_dir/lswd.log"
    fi
    if [ -f "$LSW_STATE_DIR/lswd.log" ]; then
        tail -c 131072 "$LSW_STATE_DIR/lswd.log" >"$artifact_dir/lswd-autospawn.log"
        chmod 600 "$artifact_dir/lswd-autospawn.log"
    fi
    if [ -f "$e2e_root/lswd-keepalive.log" ]; then
        tail -c 131072 "$e2e_root/lswd-keepalive.log" >"$artifact_dir/lswd-keepalive.log"
        chmod 600 "$artifact_dir/lswd-keepalive.log"
    fi
    artifacts_collected=1
}

terminate_viewer() {
    case "$viewer_pid" in
        ''|*[!0-9]*) ;;
        *)
            kill -TERM "-$viewer_pid" 2>/dev/null || :
            viewer_attempt=0
            while kill -0 "-$viewer_pid" 2>/dev/null \
                && [ "$viewer_attempt" -lt 50 ]
            do
                viewer_attempt=$((viewer_attempt + 1))
                sleep 0.1
            done
            if kill -0 "-$viewer_pid" 2>/dev/null; then
                kill -KILL "-$viewer_pid" 2>/dev/null || :
                viewer_attempt=0
                while kill -0 "-$viewer_pid" 2>/dev/null \
                    && [ "$viewer_attempt" -lt 20 ]
                do
                    viewer_attempt=$((viewer_attempt + 1))
                    sleep 0.1
                done
            fi
            if kill -0 "-$viewer_pid" 2>/dev/null; then
                echo "error: installation viewer process group $viewer_pid survived cleanup" >&2
                return 1
            fi
            ;;
    esac
    viewer_pid=
}

terminate_sync() {
    case "$sync_pid" in
        ''|*[!0-9]*) ;;
        *)
            kill -TERM "-$sync_pid" 2>/dev/null \
                || kill -TERM "$sync_pid" 2>/dev/null \
                || :
            wait "$sync_pid" 2>/dev/null || :
            ;;
    esac
    sync_pid=
}

adopt_cold_daemon_if_present() {
    if [ -n "$daemon_pid" ] || [ ! -f "$cold_daemon_pid_file" ]; then
        return 0
    fi
    candidate_daemon_pid=$(awk 'NR == 1 { print $1 }' "$cold_daemon_pid_file")
    case "$candidate_daemon_pid" in
        ''|*[!0-9]*)
            echo "error: bare lsw recorded an invalid daemon PID" >&2
            return 1
            ;;
    esac
    daemon_pid=$candidate_daemon_pid
    if ! printf '%s\n' "$daemon_pid" >"$daemon_pid_file" \
        || ! chmod 600 "$daemon_pid_file"
    then
        echo "error: could not track the cold-start daemon PID" >&2
        return 1
    fi
    rm -f -- "$cold_daemon_pid_file"
}

assert_daemon_alive() {
    case "$daemon_pid" in
        ''|*[!0-9]*)
            echo "error: the tracked lswd PID is unavailable" >&2
            return 1
            ;;
    esac
    if ! kill -0 "$daemon_pid" 2>/dev/null; then
        echo "error: tracked lswd process $daemon_pid exited during the gate" >&2
        return 1
    fi
}

assert_daemon_keepalive_alive() {
    case "$daemon_keepalive_pid" in
        ''|*[!0-9]*)
            echo "error: the tracked lswd keepalive PID is unavailable" >&2
            return 1
            ;;
    esac
    if ! kill -0 "-$daemon_keepalive_pid" 2>/dev/null; then
        echo "error: tracked lswd keepalive process group $daemon_keepalive_pid exited during the gate" >&2
        return 1
    fi
}

terminate_daemon_keepalive() {
    case "$daemon_keepalive_pid" in
        ''|*[!0-9]*)
            daemon_keepalive_pid=
            return 0
            ;;
    esac

    kill -TERM "-$daemon_keepalive_pid" 2>/dev/null \
        || kill -TERM "$daemon_keepalive_pid" 2>/dev/null \
        || :
    keepalive_attempt=0
    while kill -0 "-$daemon_keepalive_pid" 2>/dev/null \
        && [ "$keepalive_attempt" -lt 50 ]
    do
        keepalive_attempt=$((keepalive_attempt + 1))
        sleep 0.1
    done
    if kill -0 "-$daemon_keepalive_pid" 2>/dev/null; then
        kill -KILL "-$daemon_keepalive_pid" 2>/dev/null || :
    fi
    wait "$daemon_keepalive_pid" 2>/dev/null || :
    daemon_keepalive_pid=
}

terminate_daemon() {
    terminate_daemon_keepalive
    case "$daemon_pid" in
        ''|*[!0-9]*)
            daemon_pid=
            rm -f -- "$daemon_pid_file"
            return 0
            ;;
    esac

    if ! kill -TERM "-$daemon_pid" 2>/dev/null; then
        kill -TERM "$daemon_pid" 2>/dev/null || :
    fi
    daemon_attempt=0
    while kill -0 "-$daemon_pid" 2>/dev/null \
        && [ "$daemon_attempt" -lt 100 ]
    do
        if [ -r "/proc/$daemon_pid/stat" ] \
            && [ "$(awk '{ print $3 }' "/proc/$daemon_pid/stat")" = Z ]
        then
            wait "$daemon_pid" 2>/dev/null || :
        fi
        daemon_attempt=$((daemon_attempt + 1))
        sleep 0.1
    done
    if kill -0 "-$daemon_pid" 2>/dev/null; then
        kill -KILL "-$daemon_pid" 2>/dev/null || :
        daemon_attempt=0
        while kill -0 "-$daemon_pid" 2>/dev/null \
            && [ "$daemon_attempt" -lt 20 ]
        do
            if [ -r "/proc/$daemon_pid/stat" ] \
                && [ "$(awk '{ print $3 }' "/proc/$daemon_pid/stat")" = Z ]
            then
                wait "$daemon_pid" 2>/dev/null || :
            fi
            daemon_attempt=$((daemon_attempt + 1))
            sleep 0.1
        done
    fi
    if kill -0 "-$daemon_pid" 2>/dev/null; then
        echo "error: lswd process group $daemon_pid survived cleanup" >&2
        return 1
    fi
    wait "$daemon_pid" 2>/dev/null || :
    daemon_pid=
    rm -f -- "$daemon_pid_file"
}

assert_stopped_runtime_released() {
    stopped_qemu_pid=$1
    stopped_agent_port=$2
    stopped_instance=${3:-$instance}

    deadline=$(( $(date +%s) + 300 ))
    stopped=0
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if "$lsw" status "$stopped_instance" 2>/dev/null | grep -Fx 'STATE=stopped' >/dev/null; then
            stopped=1
            break
        fi
        sleep 2
    done
    if [ "$stopped" -ne 1 ]; then
        echo "error: Windows did not complete graceful shutdown within five minutes" >&2
        exit 1
    fi
    case "$stopped_qemu_pid" in
        ''|*[!0-9]*) ;;
        *)
            if kill -0 "$stopped_qemu_pid" 2>/dev/null; then
                echo "error: QEMU process $stopped_qemu_pid survived guest shutdown" >&2
                exit 1
            fi
            ;;
    esac

    for stale in qmp.sock swtpm.sock recovery-vnc.sock qemu.pid; do
        if [ -e "$LSW_STATE_DIR/instances/$stopped_instance/run/$stale" ]; then
            echo "error: stale runtime artifact remained after shutdown: $stale" >&2
            exit 1
        fi
    done

    python3 - "$stopped_agent_port" <<'PY'
import socket
import sys

port = int(sys.argv[1])
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    # A completed agent connection can leave the old host-forward endpoint in
    # TIME_WAIT after QEMU exits. Match QEMU's reusable-listener semantics while
    # still rejecting a port held by a live runtime.
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", port))
PY
}

query_agent_service() {
    # Every command in this gate is expected to inherit the auto-start
    # service's virtual-account token. Query the registered service as well as
    # its actual process owner so a same-name process cannot satisfy the gate.
    # PowerShell expands its own variables in the guest.
    # shellcheck disable=SC2016
    "$lsw" exec "$instance" -- powershell.exe -NoLogo -NoProfile -Command \
        '$Service=Get-CimInstance -ClassName Win32_Service | Where-Object { $_.Name -ceq "LSWAgent" }; if ($null -eq $Service -or @($Service).Count -ne 1) { exit 51 }; if ($Service.Name -cne "LSWAgent") { exit 52 }; if ($Service.StartMode -ne "Auto") { exit 53 }; if ($Service.State -ne "Running") { exit 54 }; if ($Service.StartName -ine "NT SERVICE\LSWAgent") { exit 55 }; if ([uint32]$Service.ProcessId -eq 0) { exit 56 }; $Process=Get-CimInstance -ClassName Win32_Process | Where-Object { $_.ProcessId -eq [uint32]$Service.ProcessId }; if ($null -eq $Process -or @($Process).Count -ne 1) { exit 57 }; $Owner=Invoke-CimMethod -InputObject $Process -MethodName GetOwnerSid; if ($Owner.ReturnValue -ne 0 -or [string]::IsNullOrWhiteSpace([string]$Owner.Sid)) { exit 58 }; $Expected=([System.Security.Principal.NTAccount]::new("NT SERVICE","LSWAgent")).Translate([System.Security.Principal.SecurityIdentifier]).Value; $Current=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value; if ($Owner.Sid -ne $Expected -or $Current -ne $Expected) { exit 59 }; $CurrentProcess=[System.Diagnostics.Process]::GetCurrentProcess(); if ([uint32]$Process.SessionId -ne 0 -or [uint32]$CurrentProcess.SessionId -ne 0) { exit 60 }; [Console]::Out.Write("LSW_WINDOWS_KVM_SERVICE_OK|$Expected|$($Service.ProcessId)")'
}

read_agent_service_identity() {
    service_output=$(query_agent_service)
    service_marker=$(printf '%s\n' "$service_output" | awk -F'|' '{ print $1 }')
    service_sid=$(printf '%s\n' "$service_output" | awk -F'|' '{ print $2 }')
    service_pid=$(printf '%s\n' "$service_output" | awk -F'|' '{ print $3 }')
    service_field_count=$(printf '%s\n' "$service_output" | awk -F'|' '{ print NF }')
    if [ "$service_marker" != LSW_WINDOWS_KVM_SERVICE_OK ] \
        || [ "$service_field_count" -ne 3 ] \
        || ! printf '%s\n' "$service_sid" | grep -E '^S-1-5-80-[0-9]+(-[0-9]+)+$' >/dev/null
    then
        echo "error: LSWAgent did not report a valid virtual-account process SID" >&2
        return 1
    fi
    case "$service_pid" in
        ''|*[!0-9]*)
            echo "error: LSWAgent did not report a numeric service process ID" >&2
            return 1
            ;;
    esac
    if [ "$service_pid" -eq 0 ]; then
        echo "error: LSWAgent reported service process ID zero" >&2
        return 1
    fi
}

prepare_gui_handoff() {
    gui_instance="${instance}-gui"
    gui_instance_directory="$LSW_STATE_DIR/instances/$gui_instance"
    if [ -e "$gui_instance_directory" ] || [ -L "$gui_instance_directory" ]; then
        echo "error: refusing to replace an existing GUI handoff instance" >&2
        return 1
    fi

    "$lsw" clone "$instance" "$gui_instance"
    gui_login_secret="$e2e_root/gui-login.secret"
    gui_handoff_metadata="$e2e_root/gui-handoff.env"
    if [ -e "$gui_login_secret" ] || [ -L "$gui_login_secret" ] \
        || [ -e "$gui_handoff_metadata" ] || [ -L "$gui_handoff_metadata" ]
    then
        echo "error: refusing to replace GUI handoff metadata" >&2
        return 1
    fi

    gui_password="Lsw!$(tr -d '-' </proc/sys/kernel/random/uuid)9a"
    (umask 077; printf '%s\n' "$gui_password" >"$gui_login_secret")
    if ! printf '%s\n' "$gui_password" \
        | timeout "${agent_boot_timeout_seconds}s" "$lsw" user setup "$gui_instance" \
            --username lsw-e2e-gui --password-stdin --administrator
    then
        gui_password=
        unset gui_password
        echo "error: could not register the GUI handoff desktop user" >&2
        return 1
    fi
    gui_password=
    unset gui_password

    # PowerShell expands its own account and Winlogon variables in the guest.
    # shellcheck disable=SC2016
    gui_user_output=$(timeout 60s "$lsw" exec "$gui_instance" -- \
        powershell.exe -NoLogo -NoProfile -NonInteractive -Command \
        '$ErrorActionPreference="Stop"; $User=Get-LocalUser -Name "lsw-e2e-gui"; if (-not $User.Enabled) { exit 91 }; $Administrators=Get-LocalGroup -SID "S-1-5-32-544"; if (-not (Get-LocalGroupMember -Group $Administrators | Where-Object { $_.SID -eq $User.SID })) { exit 92 }; $Interactive=[string](Get-CimInstance -ClassName Win32_ComputerSystem).UserName; if (-not [string]::IsNullOrWhiteSpace($Interactive)) { exit 93 }; $Winlogon=Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon"; if ([string]$Winlogon.AutoAdminLogon -eq "1") { exit 94 }; $StoredPassword=$Winlogon.PSObject.Properties["DefaultPassword"]; if ($null -ne $StoredPassword -and -not [string]::IsNullOrEmpty([string]$StoredPassword.Value)) { exit 95 }; [Console]::Out.Write("LSW_GUI_HANDOFF_USER_READY")')
    if [ "$gui_user_output" != LSW_GUI_HANDOFF_USER_READY ]; then
        echo "error: GUI handoff user is missing, non-administrative, signed in, or configured for AutoLogon" >&2
        return 1
    fi

    gui_qemu_pid=$(awk 'NR == 1 { print $1 }' \
        "$gui_instance_directory/run/qemu.pid")
    gui_agent_port=$(
        "$lsw" show "$gui_instance" |
            awk -v prefix='agent host port:      ' \
                'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }'
    )
    case "$gui_qemu_pid:$gui_agent_port" in
        *[!0-9:]*|:*|*:)
            echo "error: GUI handoff runtime identity is invalid" >&2
            return 1
            ;;
    esac
    if [ "$gui_qemu_pid" -eq 0 ] || [ "$gui_agent_port" -eq 0 ] \
        || [ "$gui_agent_port" -gt 65535 ]
    then
        echo "error: GUI handoff runtime identity is out of range" >&2
        return 1
    fi
    "$lsw" shutdown "$gui_instance"
    assert_stopped_runtime_released "$gui_qemu_pid" "$gui_agent_port" "$gui_instance"
    "$lsw" use "$gui_instance"
    "$lsw" remove "$instance"
    if [ -e "$LSW_STATE_DIR/instances/$instance" ]; then
        echo "error: real-install source remained after GUI clone handoff" >&2
        return 1
    fi

    gui_handoff_tmp="$gui_handoff_metadata.tmp-$$"
    {
        printf 'version=1\n'
        printf 'candidate_sha=%s\n' "$candidate_sha"
        printf 'source=real-install-linked-clone\n'
        printf 'state_dir=%s\n' "$LSW_STATE_DIR"
        printf 'instance=%s\n' "$gui_instance"
        printf 'login_secret=%s\n' "$gui_login_secret"
        printf 'lsw_sha256=%s\n' "$(sha256sum "$lsw" | awk '{ print $1 }')"
        printf 'lswd_sha256=%s\n' "$(sha256sum "$lswd" | awk '{ print $1 }')"
        printf 'lswg_sha256=%s\n' "$(sha256sum "$lswg" | awk '{ print $1 }')"
        printf 'agent_sha256=%s\n' "$(sha256sum "$agent" | awk '{ print $1 }')"
    } >"$gui_handoff_tmp"
    chmod 600 "$gui_handoff_tmp"
    mv "$gui_handoff_tmp" "$gui_handoff_metadata"
    gui_handoff_complete=1
}

cleanup_e2e() {
    status=$?
    trap - EXIT HUP INT TERM
    cleanup_failed=0

    if ! adopt_cold_daemon_if_present; then
        cleanup_failed=1
    fi

    if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
        if [ "$gui_instance" != none ] \
            && [ -f "$LSW_STATE_DIR/instances/$gui_instance/instance.lsw" ]
        then
            timeout 30s "$lsw" stop "$gui_instance" --force >/dev/null 2>&1 || :
        fi
        timeout 30s "$lsw" stop "$instance" --force >/dev/null 2>&1 || :
    fi

    if ! terminate_viewer; then
        cleanup_failed=1
    fi
    terminate_sync
    if ! terminate_daemon; then
        cleanup_failed=1
    fi

    if [ "$artifacts_collected" -ne 1 ]; then
        collect_e2e_artifacts "$status" || :
    fi

    if [ "$cleanup_failed" -ne 0 ] && [ "$status" -eq 0 ]; then
        status=1
    fi
    if [ "$gui_handoff" = 1 ] && [ "$status" -eq 0 ] \
        && [ "$gui_handoff_complete" -eq 1 ]
    then
        echo "LSW Windows/KVM E2E handed its stopped GUI clone to $e2e_root"
    elif [ "$keep_state" = 1 ] && [ "$status" -ne 0 ]; then
        echo "LSW Windows/KVM E2E state retained by explicit request at $e2e_root" >&2
    else
        rm -rf -- "$e2e_root"
        if [ -n "$active_root_file" ]; then
            rm -f -- "$active_root_file"
        fi
    fi
    exit "$status"
}
