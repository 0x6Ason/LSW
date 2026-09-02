# SPDX-License-Identifier: GPL-3.0-or-later

param(
    [Parameter(Mandatory = $true)]
    [ValidateSet(
        "Find", "Query", "Activate", "Pointer", "PointerAway", "PointerButton", "MoveRelative", "Drag", "Button", "Burst",
        "KeyDown", "KeyUp", "Chord", "Type", "Minimize", "Maximize", "Restore", "Close",
        "CloseWithHeldInput",
        "Screenshot", "ReleaseAll", "FocusSink"
    )]
    [string]$Action,

    [Int64]$Hwnd = 0,
    [string]$TitleNeedle = "",
    [string]$ProcessName = "",
    [int]$X = 0,
    [int]$Y = 0,
    [int]$DeltaX = 0,
    [int]$DeltaY = 0,
    [ValidateSet(
        "Left", "LeftDown", "LeftUp", "Middle", "MiddleDown", "MiddleUp",
        "Right", "RightDown", "RightUp", "WheelUp", "WheelDown"
    )]
    [string]$Button = "Left",
    [string]$Key = "",
    [string]$Text = "",
    [string]$Output = "",
    [int]$Repeat = 0,
    [int]$DelayMilliseconds = 75,
    [switch]$ExactTitle
)

$ErrorActionPreference = "Stop"
[Console]::Out.NewLine = "`n"

$NativeHelperPath = Join-Path -Path $PSScriptRoot -ChildPath "windows-seamless-host-native.ps1"
$NativeHelper = Get-Item -LiteralPath $NativeHelperPath -Force
if (-not ($NativeHelper -is [System.IO.FileInfo]) -or
    (($NativeHelper.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0)) {
    throw "the seamless native helper must be a regular non-reparse file"
}
. $NativeHelper.FullName

function Convert-ToHwnd {
    if ($Hwnd -le 0) {
        throw "-Hwnd must be a positive integer"
    }
    if ([string]::IsNullOrEmpty($TitleNeedle) -or [string]::IsNullOrEmpty($ProcessName)) {
        throw "HWND actions require -TitleNeedle and -ProcessName identity constraints"
    }
    $value = [IntPtr]::new($Hwnd)
    [LswSeamlessHost]::RequireWindow($value)
    if (-not [LswSeamlessHost]::IsWindowVisible($value) -or
        [LswSeamlessHost]::GetAncestor($value, [LswSeamlessHost]::GA_ROOT) -ne $value) {
        throw "the requested HWND is no longer a visible root window"
    }
    $observedTitle = [LswSeamlessHost]::Title($value)
    $titleMatches = if ($ExactTitle.IsPresent) {
        [string]::Equals($observedTitle, $TitleNeedle, [StringComparison]::Ordinal)
    }
    else {
        $observedTitle.IndexOf($TitleNeedle, [StringComparison]::Ordinal) -ge 0
    }
    if (-not $titleMatches) {
        throw "the requested HWND title identity changed"
    }
    [UInt32]$processId = 0
    [void][LswSeamlessHost]::GetWindowThreadProcessId($value, [ref]$processId)
    $observedProcess = Get-Process -Id $processId -ErrorAction Stop
    if (-not [string]::Equals(
        $observedProcess.ProcessName, $ProcessName, [StringComparison]::OrdinalIgnoreCase
    )) {
        throw "the requested HWND process identity changed"
    }
    return $value
}
function Assert-InputTarget {
    param([Parameter(Mandatory = $true)][IntPtr]$Window)

    if ([LswSeamlessHost]::GetForegroundWindow() -ne $Window) {
        throw "the identity-checked HWND is not the Windows foreground input target"
    }
}

function Assert-PointerTarget {
    param([Parameter(Mandatory = $true)][IntPtr]$Window)

    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        $point = New-Object LswSeamlessHost+POINT
        if (-not [LswSeamlessHost]::GetCursorPos([ref]$point)) {
            throw "GetCursorPos failed"
        }
        $target = [LswSeamlessHost]::WindowFromPoint($point)
        $root = [LswSeamlessHost]::GetAncestor($target, [LswSeamlessHost]::GA_ROOT)
        if ($target -ne [IntPtr]::Zero -and $root -eq $Window) {
            [Console]::Out.WriteLine("POINTER_X={0}" -f $point.X)
            [Console]::Out.WriteLine("POINTER_Y={0}" -f $point.Y)
            [Console]::Out.WriteLine("POINTER_TARGET_HWND={0}" -f $target.ToInt64())
            [Console]::Out.WriteLine("POINTER_ROOT_HWND={0}" -f $root.ToInt64())
            return
        }
        Start-Sleep -Milliseconds 25
    }
    throw ("the Windows pointer did not land inside the identity-checked HWND " +
        "(point={0},{1}, target={2}, root={3}, expected={4})" -f
        $point.X, $point.Y, $target.ToInt64(), $root.ToInt64(), $Window.ToInt64())
}

function Assert-PointerPosition {
    param(
        [Parameter(Mandatory = $true)][int]$ExpectedX,
        [Parameter(Mandatory = $true)][int]$ExpectedY
    )

    $point = New-Object LswSeamlessHost+POINT
    if (-not [LswSeamlessHost]::GetCursorPos([ref]$point)) {
        throw "GetCursorPos failed"
    }
    if ([Math]::Abs($point.X - $ExpectedX) -gt 1 -or
        [Math]::Abs($point.Y - $ExpectedY) -gt 1) {
        throw "the Windows pointer did not reach the requested screen coordinate"
    }
    [Console]::Out.WriteLine("POINTER_X={0}" -f $point.X)
    [Console]::Out.WriteLine("POINTER_Y={0}" -f $point.Y)
}

function Get-VirtualKey {
    param([Parameter(Mandatory = $true)][string]$Name)

    switch ($Name.ToUpperInvariant()) {
        "CTRL" { return [UInt16]0x11 }
        "CONTROL" { return [UInt16]0x11 }
        "SHIFT" { return [UInt16]0x10 }
        "ALT" { return [UInt16]0x12 }
        "WIN" { return [UInt16]0x5B }
        "ENTER" { return [UInt16]0x0D }
        "RETURN" { return [UInt16]0x0D }
        "ESC" { return [UInt16]0x1B }
        "ESCAPE" { return [UInt16]0x1B }
        "TAB" { return [UInt16]0x09 }
        "BACKSPACE" { return [UInt16]0x08 }
        "DELETE" { return [UInt16]0x2E }
        "END" { return [UInt16]0x23 }
        "HOME" { return [UInt16]0x24 }
        "SPACE" { return [UInt16]0x20 }
        default {
            if ($Name.Length -eq 1) {
                $character = [char]::ToUpperInvariant($Name[0])
                if (($character -ge 'A' -and $character -le 'Z') -or
                    ($character -ge '0' -and $character -le '9')) {
                    return [UInt16][int]$character
                }
            }
            throw "unsupported virtual key: $Name"
        }
    }
}

function Write-Query {
    param([Parameter(Mandatory = $true)][IntPtr]$Window)

    $windowRect = New-Object LswSeamlessHost+RECT
    if (-not [LswSeamlessHost]::GetWindowRect($Window, [ref]$windowRect)) {
        throw "GetWindowRect failed"
    }
    $clientRect = New-Object LswSeamlessHost+RECT
    if (-not [LswSeamlessHost]::GetClientRect($Window, [ref]$clientRect)) {
        throw "GetClientRect failed"
    }
    $clientOrigin = [LswSeamlessHost]::ClientOrigin($Window)
    $dwmRect = [LswSeamlessHost]::DwmFrame($Window)
    [UInt32]$processId = 0
    [void][LswSeamlessHost]::GetWindowThreadProcessId($Window, [ref]$processId)
    $process = Get-Process -Id $processId -ErrorAction Stop
    Add-Type -AssemblyName System.Windows.Forms
    $workArea = [Windows.Forms.Screen]::FromHandle($Window).WorkingArea
    [UInt64]$style = [UInt64][Int64][LswSeamlessHost]::GetWindowLongPtr(
        $Window, [LswSeamlessHost]::GWL_STYLE
    )
    [UInt64]$exStyle = [UInt64][Int64][LswSeamlessHost]::GetWindowLongPtr(
        $Window, [LswSeamlessHost]::GWL_EXSTYLE
    )
    $titleBytes = [Text.Encoding]::UTF8.GetBytes([LswSeamlessHost]::Title($Window))
    $values = [ordered]@{
        HWND = $Window.ToInt64()
        TITLE_BASE64 = [Convert]::ToBase64String($titleBytes)
        CLASS_BASE64 = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes([LswSeamlessHost]::ClassName($Window))
        )
        PID = $processId
        PROCESS = $process.ProcessName
        SESSION = $process.SessionId
        VISIBLE = [int][LswSeamlessHost]::IsWindowVisible($Window)
        ICONIC = [int][LswSeamlessHost]::IsIconic($Window)
        ZOOMED = [int][LswSeamlessHost]::IsZoomed($Window)
        CLOAKED = [LswSeamlessHost]::Cloaked($Window)
        OWNER = [LswSeamlessHost]::GetWindow($Window, [LswSeamlessHost]::GW_OWNER).ToInt64()
        FOREGROUND = [int]([LswSeamlessHost]::GetForegroundWindow() -eq $Window)
        STYLE = ("0x{0:X16}" -f $style)
        EXSTYLE = ("0x{0:X16}" -f $exStyle)
        HAS_CAPTION = [int](($style -band [UInt64]0x00C00000) -ne 0)
        HAS_DLG_MODAL_FRAME = [int](($exStyle -band [UInt64]0x00000001) -ne 0)
        HAS_WINDOW_EDGE = [int](($exStyle -band [UInt64]0x00000100) -ne 0)
        HAS_CLIENT_EDGE = [int](($exStyle -band [UInt64]0x00000200) -ne 0)
        X = $clientOrigin.X
        Y = $clientOrigin.Y
        LEFT = $clientOrigin.X
        TOP = $clientOrigin.Y
        WIDTH = $clientRect.Right - $clientRect.Left
        HEIGHT = $clientRect.Bottom - $clientRect.Top
        WINDOW_LEFT = $windowRect.Left
        WINDOW_TOP = $windowRect.Top
        WINDOW_WIDTH = $windowRect.Right - $windowRect.Left
        WINDOW_HEIGHT = $windowRect.Bottom - $windowRect.Top
        DWM_LEFT = $dwmRect.Left
        DWM_TOP = $dwmRect.Top
        DWM_WIDTH = $dwmRect.Right - $dwmRect.Left
        DWM_HEIGHT = $dwmRect.Bottom - $dwmRect.Top
        WORK_LEFT = $workArea.Left
        WORK_TOP = $workArea.Top
        WORK_WIDTH = $workArea.Width
        WORK_HEIGHT = $workArea.Height
    }
    foreach ($entry in $values.GetEnumerator()) {
        [Console]::Out.WriteLine(
            ([string]$entry.Key + "=" + [Convert]::ToString($entry.Value, [Globalization.CultureInfo]::InvariantCulture))
        )
    }
}

switch ($Action) {
    "Find" {
        if ([string]::IsNullOrEmpty($TitleNeedle)) {
            throw "Find requires -TitleNeedle"
        }
        foreach ($window in [LswSeamlessHost]::Find(
            $TitleNeedle, $ProcessName, $ExactTitle.IsPresent
        )) {
            [Console]::Out.WriteLine("HWND={0}" -f $window.ToInt64())
        }
    }
    "Query" {
        Write-Query (Convert-ToHwnd)
    }
    "Activate" {
        [LswSeamlessHost]::Activate((Convert-ToHwnd))
    }
    "Pointer" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        $client = New-Object LswSeamlessHost+RECT
        if (-not [LswSeamlessHost]::GetClientRect($window, [ref]$client)) {
            throw "GetClientRect failed"
        }
        $clientWidth = $client.Right - $client.Left
        $clientHeight = $client.Bottom - $client.Top
        if ($X -lt 0 -or $Y -lt 0 -or $X -ge $clientWidth -or $Y -ge $clientHeight) {
            throw "pointer coordinates are outside the host client area"
        }
        $origin = [LswSeamlessHost]::ClientOrigin($window)
        [LswSeamlessHost]::MovePointer($origin.X + $X, $origin.Y + $Y)
        Assert-PointerTarget $window
    }
    "PointerAway" {
        $window = Convert-ToHwnd
        $rectangle = New-Object LswSeamlessHost+RECT
        if (-not [LswSeamlessHost]::GetWindowRect($window, [ref]$rectangle)) {
            throw "GetWindowRect failed"
        }
        $virtualLeft = [LswSeamlessHost]::GetSystemMetrics(
            [LswSeamlessHost]::SM_XVIRTUALSCREEN
        )
        $virtualTop = [LswSeamlessHost]::GetSystemMetrics(
            [LswSeamlessHost]::SM_YVIRTUALSCREEN
        )
        $virtualRight = $virtualLeft + [LswSeamlessHost]::GetSystemMetrics(
            [LswSeamlessHost]::SM_CXVIRTUALSCREEN
        )
        $virtualBottom = $virtualTop + [LswSeamlessHost]::GetSystemMetrics(
            [LswSeamlessHost]::SM_CYVIRTUALSCREEN
        )
        if (0 -lt $rectangle.Left -or 0 -ge $rectangle.Right -or
            0 -lt $rectangle.Top -or 0 -ge $rectangle.Bottom) {
            $awayX = 0
            $awayY = 0
        }
        elseif ($virtualLeft -lt $rectangle.Left -or
                $virtualTop -lt $rectangle.Top) {
            $awayX = $virtualLeft
            $awayY = $virtualTop
        }
        elseif (($virtualRight - 1) -ge $rectangle.Right -or
                ($virtualBottom - 1) -ge $rectangle.Bottom) {
            $awayX = $virtualRight - 1
            $awayY = $virtualBottom - 1
        }
        else {
            throw "the target HWND covers the Windows virtual desktop"
        }
        [LswSeamlessHost]::MovePointer($awayX, $awayY)
        Assert-PointerPosition -ExpectedX $awayX -ExpectedY $awayY
        $point = New-Object LswSeamlessHost+POINT
        if (-not [LswSeamlessHost]::GetCursorPos([ref]$point)) {
            throw "GetCursorPos failed"
        }
        $target = [LswSeamlessHost]::WindowFromPoint($point)
        $root = [LswSeamlessHost]::GetAncestor($target, [LswSeamlessHost]::GA_ROOT)
        if ($root -eq $window) {
            throw "the Windows pointer remained inside the target HWND"
        }
    }
    "PointerButton" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        if ($DelayMilliseconds -lt 50 -or $DelayMilliseconds -gt 1000) {
            throw "PointerButton requires -DelayMilliseconds between 50 and 1000"
        }
        $client = New-Object LswSeamlessHost+RECT
        if (-not [LswSeamlessHost]::GetClientRect($window, [ref]$client)) {
            throw "GetClientRect failed"
        }
        $clientWidth = $client.Right - $client.Left
        $clientHeight = $client.Bottom - $client.Top
        if ($X -lt 0 -or $Y -lt 0 -or $X -ge $clientWidth -or $Y -ge $clientHeight) {
            throw "pointer coordinates are outside the host client area"
        }
        $origin = [LswSeamlessHost]::ClientOrigin($window)
        [LswSeamlessHost]::MovePointer($origin.X + $X, $origin.Y + $Y)
        Assert-PointerTarget $window
        Start-Sleep -Milliseconds $DelayMilliseconds
        Assert-InputTarget $window
        Assert-PointerTarget $window
        switch ($Button) {
            "Left" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0)
                Start-Sleep -Milliseconds 50
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTUP, 0, 0, 0)
            }
            "LeftDown" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0)
            }
            "LeftUp" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTUP, 0, 0, 0)
            }
            default { throw "PointerButton supports only Left, LeftDown, or LeftUp" }
        }
    }
    "MoveRelative" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        $point = New-Object LswSeamlessHost+POINT
        if (-not [LswSeamlessHost]::GetCursorPos([ref]$point)) {
            throw "GetCursorPos failed"
        }
        [LswSeamlessHost]::MovePointer($point.X + $DeltaX, $point.Y + $DeltaY)
        Assert-PointerTarget $window
    }
    "Drag" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        if ($DelayMilliseconds -lt 50 -or $DelayMilliseconds -gt 1000) {
            throw "Drag requires -DelayMilliseconds between 50 and 1000"
        }
        $client = New-Object LswSeamlessHost+RECT
        if (-not [LswSeamlessHost]::GetClientRect($window, [ref]$client)) {
            throw "GetClientRect failed"
        }
        $clientWidth = $client.Right - $client.Left
        $clientHeight = $client.Bottom - $client.Top
        if ($X -lt 0 -or $Y -lt 0 -or $X -ge $clientWidth -or $Y -ge $clientHeight) {
            throw "drag start coordinates are outside the host client area"
        }
        $origin = [LswSeamlessHost]::ClientOrigin($window)
        $startX = $origin.X + $X
        $startY = $origin.Y + $Y
        $endX = $startX + $DeltaX
        $endY = $startY + $DeltaY
        [LswSeamlessHost]::MovePointer($startX, $startY)
        Assert-PointerTarget $window
        Start-Sleep -Milliseconds $DelayMilliseconds
        $pressed = $false
        try {
            [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0)
            $pressed = $true
            Start-Sleep -Milliseconds $DelayMilliseconds
            [LswSeamlessHost]::MovePointer($endX, $endY)
            # During an interactive compositor grab the pointer may be outside
            # the old proxy rectangle until asynchronous resize/move catches up.
            # Prove the requested physical coordinate without requiring the
            # transient WindowFromPoint root to remain the pre-drag HWND.
            Assert-PointerPosition -ExpectedX $endX -ExpectedY $endY
            Start-Sleep -Milliseconds $DelayMilliseconds
        }
        finally {
            if ($pressed) {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTUP, 0, 0, 0)
            }
        }
    }
    "Button" {
        # A down edge can synchronously minimize or close the target before
        # the matching release arrives. Releases are global fail-safe edges:
        # they can only clear input state, so send them even when the HWND
        # validated for the preceding down edge has already disappeared.
        switch ($Button) {
            "LeftUp" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTUP, 0, 0, 0)
                return
            }
            "MiddleUp" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_MIDDLEUP, 0, 0, 0)
                return
            }
            "RightUp" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_RIGHTUP, 0, 0, 0)
                return
            }
        }
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        Assert-PointerTarget $window
        switch ($Button) {
            "Left" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0)
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTUP, 0, 0, 0)
            }
            "LeftDown" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0)
            }
            "Middle" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_MIDDLEDOWN, 0, 0, 0)
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_MIDDLEUP, 0, 0, 0)
            }
            "MiddleDown" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_MIDDLEDOWN, 0, 0, 0)
            }
            "Right" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_RIGHTDOWN, 0, 0, 0)
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_RIGHTUP, 0, 0, 0)
            }
            "RightDown" {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_RIGHTDOWN, 0, 0, 0)
            }
            "WheelUp" {
                [LswSeamlessHost]::Mouse(
                    [LswSeamlessHost]::MOUSEEVENTF_WHEEL, [UInt32]120, 0, 0
                )
            }
            "WheelDown" {
                [LswSeamlessHost]::Mouse(
                    [LswSeamlessHost]::MOUSEEVENTF_WHEEL, ([UInt32]::MaxValue - 119), 0, 0
                )
            }
        }
    }
    "Burst" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        Assert-PointerTarget $window
        if ($Repeat -lt 1 -or $Repeat -gt 100) {
            throw "Burst requires -Repeat between 1 and 100"
        }
        if ($DelayMilliseconds -lt 1 -or $DelayMilliseconds -gt 100) {
            throw "Burst requires -DelayMilliseconds between 1 and 100"
        }
        if ($Button -ne "Left" -and $Button -ne "Right") {
            throw "Burst supports only Left or Right button pairs"
        }
        for ($index = 0; $index -lt $Repeat; $index++) {
            if ($Button -eq "Left") {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0)
                Start-Sleep -Milliseconds $DelayMilliseconds
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTUP, 0, 0, 0)
            }
            else {
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_RIGHTDOWN, 0, 0, 0)
                Start-Sleep -Milliseconds $DelayMilliseconds
                [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_RIGHTUP, 0, 0, 0)
                Start-Sleep -Milliseconds $DelayMilliseconds
                [LswSeamlessHost]::Key([UInt16]0x1B, $true)
                [LswSeamlessHost]::Key([UInt16]0x1B, $false)
            }
            Start-Sleep -Milliseconds $DelayMilliseconds
        }
    }
    "KeyDown" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        [LswSeamlessHost]::Key((Get-VirtualKey $Key), $true)
    }
    "KeyUp" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        [LswSeamlessHost]::Key((Get-VirtualKey $Key), $false)
    }
    "Chord" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        $keys = @($Key.Split('+') | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        if ($keys.Count -eq 0) {
            throw "Chord requires -Key"
        }
        [UInt16[]]$virtualKeys = @($keys | ForEach-Object { Get-VirtualKey $_ })
        [LswSeamlessHost]::Chord($virtualKeys, $DelayMilliseconds)
    }
    "Type" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        [LswSeamlessHost]::KeyboardText($Text)
    }
    "Minimize" {
        $window = Convert-ToHwnd
        if (-not [LswSeamlessHost]::PostMessageW(
            $window, [LswSeamlessHost]::WM_SYSCOMMAND,
            [IntPtr]::new([LswSeamlessHost]::SC_MINIMIZE), [IntPtr]::Zero
        )) {
            throw "PostMessage(SC_MINIMIZE) failed"
        }
        $deadline = [DateTime]::UtcNow.AddSeconds(5)
        do {
            [void](Convert-ToHwnd)
            if ([LswSeamlessHost]::IsIconic($window)) {
                break
            }
            Start-Sleep -Milliseconds 50
        } while ([DateTime]::UtcNow -lt $deadline)
        if (-not [LswSeamlessHost]::IsIconic($window)) {
            throw "the identity-checked host HWND did not minimize"
        }
    }
    "Maximize" {
        $window = Convert-ToHwnd
        [LswSeamlessHost]::Activate($window)
        [void][LswSeamlessHost]::ShowWindow($window, [LswSeamlessHost]::SW_MAXIMIZE)
        $deadline = [DateTime]::UtcNow.AddSeconds(5)
        do {
            [void](Convert-ToHwnd)
            if ([LswSeamlessHost]::IsZoomed($window)) {
                break
            }
            Start-Sleep -Milliseconds 50
        } while ([DateTime]::UtcNow -lt $deadline)
        if (-not [LswSeamlessHost]::IsZoomed($window)) {
            throw "the identity-checked host HWND did not maximize"
        }
    }
    "Restore" {
        $window = Convert-ToHwnd
        if (-not [LswSeamlessHost]::PostMessageW(
            $window, [LswSeamlessHost]::WM_SYSCOMMAND,
            [IntPtr]::new([LswSeamlessHost]::SC_RESTORE), [IntPtr]::Zero
        )) {
            throw "PostMessage(SC_RESTORE) failed"
        }
        $deadline = [DateTime]::UtcNow.AddSeconds(5)
        do {
            [void](Convert-ToHwnd)
            if (-not [LswSeamlessHost]::IsZoomed($window) -and
                -not [LswSeamlessHost]::IsIconic($window)) {
                break
            }
            Start-Sleep -Milliseconds 50
        } while ([DateTime]::UtcNow -lt $deadline)
        if ([LswSeamlessHost]::IsZoomed($window) -or
            [LswSeamlessHost]::IsIconic($window)) {
            throw "the identity-checked host HWND did not restore"
        }
    }
    "Close" {
        if (-not [LswSeamlessHost]::PostMessageW(
            (Convert-ToHwnd), [LswSeamlessHost]::WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero
        )) {
            throw "PostMessage(WM_CLOSE) failed"
        }
    }
    "CloseWithHeldInput" {
        $window = Convert-ToHwnd
        Assert-InputTarget $window
        Assert-PointerTarget $window
        $closePosted = $false
        try {
            [LswSeamlessHost]::Key([UInt16]0x11, $true)
            Start-Sleep -Milliseconds 100
            [LswSeamlessHost]::Mouse([LswSeamlessHost]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0)
            # Keep both edges observable by the guest's 40 ms state sampler,
            # then request host close without paying another PowerShell startup
            # cost that could outlive the presenter's input safety lease.
            Start-Sleep -Milliseconds 1000
            if (-not [LswSeamlessHost]::PostMessageW(
                $window, [LswSeamlessHost]::WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero
            )) {
                throw "PostMessage(WM_CLOSE) failed"
            }
            $closePosted = $true
        }
        finally {
            if (-not $closePosted) {
                [LswSeamlessHost]::Mouse(
                    [LswSeamlessHost]::MOUSEEVENTF_LEFTUP, 0, 0, 0)
                [LswSeamlessHost]::Key([UInt16]0x11, $false)
            }
        }
    }
    "Screenshot" {
        if ([string]::IsNullOrEmpty($Output)) {
            throw "Screenshot requires -Output"
        }
        $window = Convert-ToHwnd
        $client = New-Object LswSeamlessHost+RECT
        if (-not [LswSeamlessHost]::GetClientRect($window, [ref]$client)) {
            throw "GetClientRect failed"
        }
        $origin = [LswSeamlessHost]::ClientOrigin($window)
        $captureWidth = $client.Right - $client.Left
        $captureHeight = $client.Bottom - $client.Top
        if ($captureWidth -le 0 -or $captureHeight -le 0) {
            throw "host client area is empty"
        }
        Add-Type -AssemblyName System.Drawing
        $bitmap = New-Object System.Drawing.Bitmap(
            $captureWidth, $captureHeight, [Drawing.Imaging.PixelFormat]::Format32bppArgb
        )
        $graphics = [Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.CopyFromScreen(
                $origin.X, $origin.Y, 0, 0,
                (New-Object Drawing.Size($captureWidth, $captureHeight)),
                [Drawing.CopyPixelOperation]::SourceCopy
            )
            $bitmap.Save([IO.Path]::GetFullPath($Output), [Drawing.Imaging.ImageFormat]::Png)
        }
        finally {
            $graphics.Dispose()
            $bitmap.Dispose()
        }
    }
    "ReleaseAll" {
        foreach ($virtualKey in @([UInt16]0x11, [UInt16]0x10, [UInt16]0x12, [UInt16]0x5B, [UInt16]0x5C)) {
            [LswSeamlessHost]::Key($virtualKey, $false)
        }
        foreach ($flags in @(
            [LswSeamlessHost]::MOUSEEVENTF_LEFTUP,
            [LswSeamlessHost]::MOUSEEVENTF_MIDDLEUP,
            [LswSeamlessHost]::MOUSEEVENTF_RIGHTUP
        )) {
            [LswSeamlessHost]::Mouse($flags, 0, 0, 0)
        }
    }
    "FocusSink" {
        if ([string]::IsNullOrEmpty($TitleNeedle)) {
            throw "FocusSink requires -TitleNeedle"
        }
        Add-Type -AssemblyName System.Drawing
        Add-Type -AssemblyName System.Windows.Forms
        $form = New-Object Windows.Forms.Form
        $form.Text = $TitleNeedle
        $form.ClientSize = New-Object Drawing.Size(360, 120)
        # Keep the sink's pointer target outside the centered WSLg proxy. If
        # the native sink overlaps the proxy, destroying it does not create a
        # real outside-to-inside pointer transition in WSLg copy mode.
        $form.StartPosition = [Windows.Forms.FormStartPosition]::Manual
        $form.Location = New-Object Drawing.Point(16, 16)
        $form.ShowInTaskbar = $false
        $form.TopMost = $true
        $label = New-Object Windows.Forms.Label
        $label.Text = "Temporary focus target for the LSW seamless E2E test."
        $label.AutoSize = $true
        $label.Location = New-Object Drawing.Point(20, 45)
        $form.Controls.Add($label)
        $form.Add_Shown({
            $form.Activate()
            $form.Focus()
            [Console]::Out.WriteLine("READY")
        })
        [Windows.Forms.Application]::Run($form)
    }
}
