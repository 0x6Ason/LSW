# SPDX-License-Identifier: GPL-3.0-or-later

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ServiceName = 'LSWAgent'
$ServiceAccount = "NT SERVICE\$ServiceName"
$ListenAddress = '127.0.0.1'
$ListenPort = 47653
# The spaces are intentional: the smoke test must exercise SCM binPath quoting.
$TestRoot = Join-Path $env:RUNNER_TEMP 'lsw agent service smoke'
$AgentPath = Join-Path $TestRoot 'lsw-agent.exe'
$TokenPath = Join-Path $TestRoot 'agent.token'
$BuiltAgent = (Resolve-Path '.\target\x86_64-pc-windows-msvc\release\lsw-agent.exe').Path
$BinaryCommand = "`"$AgentPath`" --service --token-file `"$TokenPath`" --listen ${ListenAddress}:$ListenPort"
$OwnService = $false
$PrimaryFailure = $null
$CleanupFailures = New-Object System.Collections.Generic.List[string]
$CapturedProcesses = New-Object System.Collections.Generic.List[System.Diagnostics.Process]

function ConvertTo-ScBinaryPathArgument {
    param([Parameter(Mandatory = $true)][string] $Command)

    # Windows PowerShell 5.1 does not escape embedded quotes when serializing a
    # native argument with spaces. sc.exe needs them in the binPath value.
    if ($PSVersionTable.PSVersion.Major -le 5) {
        return $Command.Replace('"', '\"')
    }
    return $Command
}

function Invoke-CheckedNative {
    param(
        [Parameter(Mandatory = $true)][string] $FilePath,
        [Parameter(Mandatory = $true)][string[]] $ArgumentList
    )

    $Output = @(& $FilePath @ArgumentList 2>&1)
    $ExitCode = $LASTEXITCODE
    foreach ($Line in $Output) {
        Write-Host $Line
    }
    if ($ExitCode -ne 0) {
        throw "$FilePath failed with exit code $ExitCode"
    }
}

function Invoke-CleanupNative {
    param(
        [Parameter(Mandatory = $true)][string] $FilePath,
        [Parameter(Mandatory = $true)][string[]] $ArgumentList
    )

    $Output = @(& $FilePath @ArgumentList 2>&1)
    $ExitCode = $LASTEXITCODE
    foreach ($Line in $Output) {
        Write-Host $Line
    }
    return $ExitCode
}

function Get-LswService {
    $Services = @(Get-CimInstance -ClassName Win32_Service -Filter "Name='$ServiceName'")
    if ($Services.Count -eq 0) {
        return $null
    }
    if ($Services.Count -ne 1) {
        throw "Expected exactly one $ServiceName service, found $($Services.Count)"
    }
    return $Services[0]
}

function Wait-ServiceState {
    param(
        [Parameter(Mandatory = $true)][string] $ExpectedState,
        [int] $TimeoutSeconds = 30
    )

    $Deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $LastState = 'missing'
    do {
        $Service = Get-LswService
        if ($null -ne $Service) {
            $LastState = $Service.State
            if ($Service.State -ceq $ExpectedState) {
                return $Service
            }
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $Deadline)

    throw "$ServiceName did not reach $ExpectedState within ${TimeoutSeconds}s; last state: $LastState"
}

function Wait-ServiceAbsent {
    param([int] $TimeoutSeconds = 10)

    $Deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        if ($null -eq (Get-LswService)) {
            return
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $Deadline)

    throw "$ServiceName remained registered after sc.exe delete"
}

function Wait-ListenerOwned {
    param(
        [Parameter(Mandatory = $true)][uint32] $ProcessId,
        [int] $TimeoutSeconds = 10
    )

    $Deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $Listeners = @(
            Get-NetTCPConnection -State Listen -LocalPort $ListenPort -ErrorAction SilentlyContinue |
                Where-Object { $_.LocalAddress -ceq $ListenAddress }
        )
        if ($Listeners.Count -eq 1 -and [uint32] $Listeners[0].OwningProcess -eq $ProcessId) {
            return
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $Deadline)

    $Owners = @($Listeners | ForEach-Object { $_.OwningProcess }) -join ','
    throw "Listener ${ListenAddress}:$ListenPort was not owned solely by PID $ProcessId; owners: $Owners"
}

function Wait-RuntimeReleased {
    param(
        [Parameter(Mandatory = $true)][System.Diagnostics.Process] $Process,
        [int] $TimeoutSeconds = 30
    )

    if (-not $Process.WaitForExit($TimeoutSeconds * 1000)) {
        throw "$ServiceName process PID $($Process.Id) did not exit after SCM reported it stopped"
    }

    $Deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $Listeners = @(
            Get-NetTCPConnection -State Listen -LocalPort $ListenPort -ErrorAction SilentlyContinue
        )
        if ($Listeners.Count -eq 0) {
            return
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $Deadline)

    throw "TCP port $ListenPort was not released after stopping $ServiceName"
}

function Set-RestrictedAcl {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [Parameter(Mandatory = $true)][string] $ServiceRights,
        [Parameter(Mandatory = $true)][string] $RunnerSid,
        [Parameter(Mandatory = $true)][string] $ServiceSid
    )

    Invoke-CheckedNative 'icacls.exe' @($Path, '/inheritance:r')
    foreach ($Grant in @(
        "*${RunnerSid}:(F)",
        '*S-1-5-18:(F)',
        '*S-1-5-32-544:(F)',
        "*${ServiceSid}:($ServiceRights)"
    )) {
        Invoke-CheckedNative 'icacls.exe' @($Path, '/grant:r', $Grant)
    }
}

function Capture-ServiceProcess {
    param([Parameter(Mandatory = $true)] $Service)

    if ([uint32] $Service.ProcessId -eq 0) {
        throw "$ServiceName reported an invalid process ID"
    }
    $Process = [System.Diagnostics.Process]::GetProcessById([int] $Service.ProcessId)
    # Force .NET to retain an OS handle so PID reuse cannot redirect later waits or kills.
    [void] $Process.Handle
    $CapturedProcesses.Add($Process)
    return $Process
}

function Assert-ServiceRuntime {
    param(
        [Parameter(Mandatory = $true)] $Service,
        [Parameter(Mandatory = $true)][System.Diagnostics.Process] $Process,
        [Parameter(Mandatory = $true)][string] $ExpectedSid
    )

    if ($Service.State -cne 'Running') {
        throw "$ServiceName state was $($Service.State), expected Running"
    }
    if ($Service.StartMode -cne 'Auto') {
        throw "$ServiceName start mode was $($Service.StartMode), expected Auto"
    }
    if (-not [string]::Equals(
        $Service.StartName,
        $ServiceAccount,
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        throw "$ServiceName account was '$($Service.StartName)', expected '$ServiceAccount'"
    }
    if ($Service.PathName -cne $BinaryCommand) {
        throw "$ServiceName binary command did not match the configured command"
    }
    if ([uint32] $Service.ProcessId -ne [uint32] $Process.Id -or $Process.HasExited) {
        throw "$ServiceName did not retain its captured SCM process"
    }

    $CimProcess = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $($Process.Id)"
    if ($null -eq $CimProcess) {
        throw "Could not find the $ServiceName process with PID $($Process.Id)"
    }
    if ([uint32] $CimProcess.SessionId -ne 0) {
        throw "$ServiceName ran in session $($CimProcess.SessionId), expected service session 0"
    }
    $Owner = Invoke-CimMethod -InputObject $CimProcess -MethodName GetOwnerSid
    if ([uint32] $Owner.ReturnValue -ne 0) {
        throw "Win32_Process.GetOwnerSid failed with code $($Owner.ReturnValue)"
    }
    if ($Owner.Sid -cne $ExpectedSid) {
        throw "$ServiceName owner SID was '$($Owner.Sid)', expected '$ExpectedSid'"
    }

    Wait-ListenerOwned -ProcessId ([uint32] $Process.Id)
}

function Connect-And-Close {
    $Client = New-Object System.Net.Sockets.TcpClient
    try {
        $Connect = $Client.ConnectAsync($ListenAddress, $ListenPort)
        if (-not $Connect.Wait([TimeSpan]::FromSeconds(5))) {
            throw "Timed out connecting to ${ListenAddress}:$ListenPort"
        }
    } finally {
        $Client.Dispose()
    }
}

function Start-And-Verify {
    param([Parameter(Mandatory = $true)][string] $ExpectedSid)

    Invoke-CheckedNative 'sc.exe' @('start', $ServiceName)
    $Service = Wait-ServiceState -ExpectedState 'Running'
    $Process = Capture-ServiceProcess -Service $Service
    Assert-ServiceRuntime -Service $Service -Process $Process -ExpectedSid $ExpectedSid
    Connect-And-Close
    return $Process
}

function Stop-And-Verify {
    param([Parameter(Mandatory = $true)][System.Diagnostics.Process] $Process)

    Invoke-CheckedNative 'sc.exe' @('stop', $ServiceName)
    [void] (Wait-ServiceState -ExpectedState 'Stopped')
    Wait-RuntimeReleased -Process $Process
}

function Request-CleanupStop {
    param([int] $TimeoutSeconds = 20)

    $Deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $Service = Get-LswService
        if ($null -eq $Service -or $Service.State -ceq 'Stopped') {
            return $true
        }
        if ($Service.State -cne 'Start Pending' -and $Service.State -cne 'Stop Pending') {
            $ExitCode = Invoke-CleanupNative 'sc.exe' @('stop', $ServiceName)
            if ($ExitCode -ne 0 -and $ExitCode -ne 1061 -and $ExitCode -ne 1062) {
                throw "sc.exe stop failed during cleanup with exit code $ExitCode"
            }
        }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $Deadline)

    return $false
}

function Get-ExactCleanupProcess {
    param([Parameter(Mandatory = $true)] $Service)

    foreach ($Process in $CapturedProcesses) {
        if (-not $Process.HasExited -and (
            [uint32] $Service.ProcessId -eq 0 -or [uint32] $Process.Id -eq [uint32] $Service.ProcessId
        )) {
            return $Process
        }
    }

    if ([uint32] $Service.ProcessId -eq 0) {
        return $null
    }
    $CimProcess = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $($Service.ProcessId)"
    if ($null -eq $CimProcess -or $null -eq $CimProcess.ExecutablePath) {
        return $null
    }
    $ActualPath = [System.IO.Path]::GetFullPath($CimProcess.ExecutablePath)
    $ExpectedPath = [System.IO.Path]::GetFullPath($AgentPath)
    if (-not [string]::Equals(
        $ActualPath,
        $ExpectedPath,
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        throw "Refusing to terminate unexpected service PID $($Service.ProcessId) at '$ActualPath'"
    }

    $Process = [System.Diagnostics.Process]::GetProcessById([int] $Service.ProcessId)
    [void] $Process.Handle
    $ModulePath = [System.IO.Path]::GetFullPath($Process.MainModule.FileName)
    if (-not [string]::Equals(
        $ModulePath,
        $ExpectedPath,
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        $Process.Dispose()
        throw "Service PID $($Service.ProcessId) changed identity before cleanup capture"
    }
    $CapturedProcesses.Add($Process)
    return $Process
}

try {
    if ($null -ne (Get-LswService)) {
        throw "Refusing to replace a pre-existing $ServiceName service"
    }
    $PortUsers = @(
        Get-NetTCPConnection -State Listen -LocalPort $ListenPort -ErrorAction SilentlyContinue
    )
    if ($PortUsers.Count -ne 0) {
        throw "TCP port $ListenPort is already in use"
    }

    if (Test-Path -LiteralPath $TestRoot) {
        Remove-Item -LiteralPath $TestRoot -Recurse -Force
    }
    New-Item -ItemType Directory -Path $TestRoot | Out-Null
    Copy-Item -LiteralPath $BuiltAgent -Destination $AgentPath
    [System.IO.File]::WriteAllText(
        $TokenPath,
        ('a' * 64),
        (New-Object System.Text.UTF8Encoding($false))
    )
    $ScBinaryCommand = ConvertTo-ScBinaryPathArgument -Command $BinaryCommand

    Invoke-CheckedNative 'sc.exe' @(
        'create', $ServiceName,
        'binPath=', $ScBinaryCommand,
        'start=', 'auto'
    )
    $OwnService = $true
    Invoke-CheckedNative 'sc.exe' @('config', $ServiceName, 'obj=', $ServiceAccount)
    Invoke-CheckedNative 'sc.exe' @('sidtype', $ServiceName, 'unrestricted')

    $ServiceSid = (New-Object System.Security.Principal.NTAccount($ServiceAccount)).Translate(
        [System.Security.Principal.SecurityIdentifier]
    ).Value
    if (-not $ServiceSid.StartsWith('S-1-5-80-', [StringComparison]::Ordinal)) {
        throw "The translated virtual service account SID was unexpected: $ServiceSid"
    }
    $RunnerSid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    Set-RestrictedAcl -Path $TestRoot -ServiceRights 'RX' -RunnerSid $RunnerSid -ServiceSid $ServiceSid
    Set-RestrictedAcl -Path $AgentPath -ServiceRights 'RX' -RunnerSid $RunnerSid -ServiceSid $ServiceSid
    Set-RestrictedAcl -Path $TokenPath -ServiceRights 'R' -RunnerSid $RunnerSid -ServiceSid $ServiceSid

    $FirstProcess = Start-And-Verify -ExpectedSid $ServiceSid
    Stop-And-Verify -Process $FirstProcess

    $SecondProcess = Start-And-Verify -ExpectedSid $ServiceSid
    Stop-And-Verify -Process $SecondProcess
} catch {
    $PrimaryFailure = $_
} finally {
    if ($OwnService) {
        $Stopped = $false
        try {
            $Stopped = Request-CleanupStop
        } catch {
            $CleanupFailures.Add("service stop cleanup: $($_.Exception.Message)")
        }

        if (-not $Stopped) {
            try {
                $Service = Get-LswService
                if ($null -ne $Service -and $Service.State -cne 'Stopped') {
                    $Process = Get-ExactCleanupProcess -Service $Service
                    if ($null -eq $Process) {
                        throw "No exact $ServiceName test process was available for fallback termination"
                    }
                    if (-not $Process.HasExited) {
                        $Process.Kill()
                        if (-not $Process.WaitForExit(10000)) {
                            throw "$ServiceName process PID $($Process.Id) survived fallback termination"
                        }
                    }
                }
            } catch {
                $CleanupFailures.Add("service process cleanup: $($_.Exception.Message)")
            }

            try {
                [void] (Wait-ServiceState -ExpectedState 'Stopped' -TimeoutSeconds 15)
            } catch {
                $CleanupFailures.Add("service stopped-state cleanup: $($_.Exception.Message)")
            }
        }

        try {
            $DeleteExitCode = Invoke-CleanupNative 'sc.exe' @('delete', $ServiceName)
            if ($DeleteExitCode -ne 0 -and $DeleteExitCode -ne 1060) {
                throw "sc.exe delete failed during cleanup with exit code $DeleteExitCode"
            }
            if ($DeleteExitCode -eq 0) {
                Wait-ServiceAbsent
            }
        } catch {
            $CleanupFailures.Add("service deletion cleanup: $($_.Exception.Message)")
        }
    }

    try {
        if (Test-Path -LiteralPath $TestRoot) {
            Remove-Item -LiteralPath $TestRoot -Recurse -Force
        }
    } catch {
        $CleanupFailures.Add("temporary directory cleanup: $($_.Exception.Message)")
    }

    foreach ($Process in $CapturedProcesses) {
        try {
            $Process.Dispose()
        } catch {
            $CleanupFailures.Add("process-handle cleanup: $($_.Exception.Message)")
        }
    }
}

if ($null -ne $PrimaryFailure) {
    foreach ($CleanupFailure in $CleanupFailures) {
        Write-Warning $CleanupFailure
    }
    throw $PrimaryFailure
}
if ($CleanupFailures.Count -ne 0) {
    throw "SCM smoke-test cleanup failed: $($CleanupFailures -join '; ')"
}
