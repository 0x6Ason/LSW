# SPDX-License-Identifier: GPL-3.0-or-later

param(
    [Parameter(Mandatory = $true)]
    [string]$MarkerPath,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$')]
    [string]$RunId,

    [Parameter(Mandatory = $true)]
    [string]$CleanupSignalPath,

    [switch]$Animate,

    [switch]$ClosePrompt,

    [switch]$StartMaximized
)

$ErrorActionPreference = "Stop"

$MarkerPath = [System.IO.Path]::GetFullPath($MarkerPath)
$CleanupSignalPath = [System.IO.Path]::GetFullPath($CleanupSignalPath)
$markerDirectory = [System.IO.Path]::GetDirectoryName($MarkerPath)
$cleanupDirectory = [System.IO.Path]::GetDirectoryName($CleanupSignalPath)
if ($cleanupDirectory -cne $markerDirectory -or
    [System.IO.Path]::GetFileName($CleanupSignalPath) -cne 'cleanup.signal') {
    throw 'CleanupSignalPath must be the cleanup.signal file beside MarkerPath'
}
if (-not [string]::IsNullOrEmpty($markerDirectory)) {
    [System.IO.Directory]::CreateDirectory($markerDirectory) | Out-Null
}
[System.IO.File]::WriteAllText($MarkerPath, "", [System.Text.Encoding]::UTF8)

function Write-Marker {
    param([Parameter(Mandatory = $true)][string]$Event)

    $line = "{0:o}`t{1}`r`n" -f [DateTime]::UtcNow, $Event
    [System.IO.File]::AppendAllText($MarkerPath, $line, [System.Text.Encoding]::UTF8)
}

trap {
    try {
        $detail = ($_ | Out-String)
        Write-Marker ("fatal-error-base64={0}" -f
            [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($detail)))
    }
    catch {}
    break
}

function Get-DwmFrameSize {
    $frame = New-Object LswSeamlessFixture.NativeRect
    $frameResult = [LswSeamlessFixture.NativeWindow]::DwmGetWindowAttribute(
        $form.Handle,
        9,
        [ref]$frame,
        [Runtime.InteropServices.Marshal]::SizeOf([type][LswSeamlessFixture.NativeRect]))
    if ($frameResult -ne 0 -or
        $frame.Right -le $frame.Left -or
        $frame.Bottom -le $frame.Top) {
        throw ("DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS) failed: {0}" -f $frameResult)
    }

    return New-Object System.Drawing.Size(
        ($frame.Right - $frame.Left),
        ($frame.Bottom - $frame.Top))
}

function Write-DwmFrameSize {
    $frameSize = Get-DwmFrameSize
    Write-Marker ("frame-size={0}x{1}" -f $frameSize.Width, $frameSize.Height)
    Write-Marker "frame-size-source=dwm"
    return $frameSize
}

Write-Marker "startup=begin"
Write-Marker ("process-id={0}" -f $PID)
Add-Type -AssemblyName System.Drawing
Write-Marker "startup=drawing-ready"
Add-Type -AssemblyName System.Windows.Forms
Write-Marker "startup=forms-ready"
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;

namespace LswSeamlessFixture {
    [StructLayout(LayoutKind.Sequential)]
    public struct NativeRect {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct NativePoint {
        public int X;
        public int Y;
    }

    public static class NativeWindow {
        [DllImport("dwmapi.dll")]
        public static extern int DwmGetWindowAttribute(
            IntPtr window, int attribute, out NativeRect value, int valueSize);

        [DllImport("user32.dll")]
        public static extern IntPtr GetAncestor(IntPtr window, uint flags);

        [DllImport("user32.dll")]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool GetCursorPos(out NativePoint point);

        [DllImport("user32.dll")]
        public static extern IntPtr GetForegroundWindow();

        [DllImport("user32.dll")]
        public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        public static extern int GetClassNameW(IntPtr window, StringBuilder value, int capacity);

        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        public static extern int GetWindowTextW(IntPtr window, StringBuilder value, int capacity);

        [DllImport("user32.dll")]
        public static extern IntPtr WindowFromPoint(NativePoint point);

        [DllImport("user32.dll")]
        public static extern IntPtr GetWindow(IntPtr window, uint command);

        [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
        public static extern IntPtr GetWindowLongPtr(IntPtr window, int index);

        [DllImport("user32.dll")]
        private static extern IntPtr SendMessageW(
            IntPtr window, uint message, IntPtr wParam, IntPtr lParam);

        public static int HitTest(IntPtr window, int x, int y) {
            int packed = unchecked((int)(((uint)(ushort)y << 16) | (ushort)x));
            return SendMessageW(window, 0x0084, IntPtr.Zero, new IntPtr(packed)).ToInt32();
        }
    }
}
"@
Write-Marker "startup=window-api-ready"
Write-Marker "startup=input-api-ready"

$script:clipboardSnapshotAvailable = $false
$script:clipboardSnapshot = $null
Write-Marker "startup=clipboard-snapshot-begin"
try {
    $script:clipboardSnapshot = [System.Windows.Forms.Clipboard]::GetDataObject()
    $script:clipboardSnapshotAvailable = $true
}
catch {
    Write-Error "Could not snapshot the guest clipboard for restoration"
}
Write-Marker "startup=clipboard-snapshot-complete"

function Get-InputStateEvent {
    $modifiers = [System.Windows.Forms.Control]::ModifierKeys
    $buttons = [System.Windows.Forms.Control]::MouseButtons
    $states = @(
        (($modifiers -band [System.Windows.Forms.Keys]::Control) -ne 0),
        (($modifiers -band [System.Windows.Forms.Keys]::Shift) -ne 0),
        (($modifiers -band [System.Windows.Forms.Keys]::Alt) -ne 0),
        (($buttons -band [System.Windows.Forms.MouseButtons]::Left) -ne 0),
        (($buttons -band [System.Windows.Forms.MouseButtons]::Middle) -ne 0),
        (($buttons -band [System.Windows.Forms.MouseButtons]::Right) -ne 0),
        (($buttons -band [System.Windows.Forms.MouseButtons]::XButton1) -ne 0),
        (($buttons -band [System.Windows.Forms.MouseButtons]::XButton2) -ne 0)
    ) | ForEach-Object { if ($_) { "1" } else { "0" } }

    return "input-state=ctrl:{0},shift:{1},alt:{2},left:{3},middle:{4},right:{5},x1:{6},x2:{7}" -f $states
}

$releasedInputState = "input-state=ctrl:0,shift:0,alt:0,left:0,middle:0,right:0,x1:0,x2:0"
Write-Marker (Get-InputStateEvent)

$form = New-Object System.Windows.Forms.Form
$form.Text = "LSW Seamless Fixture $RunId"
$form.KeyPreview = $true
$form.ClientSize = New-Object System.Drawing.Size(760, 480)
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
$form.Location = New-Object System.Drawing.Point(120, 100)
$form.BackColor = [System.Drawing.Color]::FromArgb(32, 37, 43)
if ($StartMaximized) {
    $form.WindowState = [System.Windows.Forms.FormWindowState]::Maximized
}

$menu = New-Object System.Windows.Forms.MenuStrip
$fileMenu = New-Object System.Windows.Forms.ToolStripMenuItem("File")
$fileAction = New-Object System.Windows.Forms.ToolStripMenuItem("Record file menu")
$fileAction.Name = "FileAction"
$fileAction.BackColor = [System.Drawing.Color]::FromArgb(236, 64, 122)
$fileAction.ForeColor = [System.Drawing.Color]::White
$fileAction.Add_Click({ Write-Marker "file-menu" })
[void]$fileMenu.DropDownItems.Add($fileAction)
$fileMenu.DropDown.BackColor = [System.Drawing.Color]::FromArgb(236, 64, 122)
[void]$menu.Items.Add($fileMenu)
$form.MainMenuStrip = $menu
$form.Controls.Add($menu)

$source = New-Object System.Windows.Forms.TextBox
$source.Name = "SourceText"
$source.Multiline = $true
$source.ShortcutsEnabled = $true
$source.Location = New-Object System.Drawing.Point(20, 55)
$source.Size = New-Object System.Drawing.Size(320, 80)
$source.Text = "alpha beta"
$source.Add_MouseUp({
    $source.SelectAll()
    Write-Marker "source-selected"
})
$form.Controls.Add($source)

$destination = New-Object System.Windows.Forms.TextBox
$destination.Name = "DestinationText"
$destination.Multiline = $true
$destination.ShortcutsEnabled = $true
$destination.Location = New-Object System.Drawing.Point(400, 55)
$destination.Size = New-Object System.Drawing.Size(320, 80)
$form.Controls.Add($destination)

$leftButton = New-Object System.Windows.Forms.Button
$leftButton.Name = "LeftButton"
$leftButton.Text = "Record left click"
$leftButton.Location = New-Object System.Drawing.Point(20, 170)
$leftButton.Size = New-Object System.Drawing.Size(150, 36)
$leftButton.Add_Click({ Write-Marker "left-click" })
$form.Controls.Add($leftButton)

$textButton = New-Object System.Windows.Forms.Button
$textButton.Name = "TextButton"
$textButton.Text = "Record text"
$textButton.Location = New-Object System.Drawing.Point(190, 170)
$textButton.Size = New-Object System.Drawing.Size(150, 36)
$textButton.Add_Click({
    $encoded = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($destination.Text))
    Write-Marker "text-base64=$encoded"
    $destination.Clear()
})
$form.Controls.Add($textButton)

$modalButton = New-Object System.Windows.Forms.Button
$modalButton.Name = "ModalButton"
$modalButton.Text = "Open modal"
$modalButton.Location = New-Object System.Drawing.Point(360, 170)
$modalButton.Size = New-Object System.Drawing.Size(150, 36)
$modalButton.Add_Click({
    Write-Marker "modal-open"
    $dialog = New-Object System.Windows.Forms.Form
    $dialog.Text = "LSW Modal Fixture $RunId"
    $dialog.ClientSize = New-Object System.Drawing.Size(360, 200)
    $dialog.StartPosition = [System.Windows.Forms.FormStartPosition]::CenterParent
    $dialog.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::FixedDialog
    $dialog.MinimizeBox = $false
    $dialog.MaximizeBox = $false
    $dialog.ShowInTaskbar = $false
    $dialog.BackColor = [System.Drawing.Color]::FromArgb(126, 34, 206)

    $dialogLabel = New-Object System.Windows.Forms.Label
    $dialogLabel.Text = "Secondary owned HWND capture"
    $dialogLabel.ForeColor = [System.Drawing.Color]::White
    $dialogLabel.BackColor = [System.Drawing.Color]::Transparent
    $dialogLabel.AutoSize = $true
    $dialogLabel.Location = New-Object System.Drawing.Point(66, 52)
    $dialog.Controls.Add($dialogLabel)

    $dialogButton = New-Object System.Windows.Forms.Button
    $dialogButton.Text = "OK"
    $dialogButton.DialogResult = [System.Windows.Forms.DialogResult]::OK
    $dialogButton.Location = New-Object System.Drawing.Point(135, 120)
    $dialogButton.Size = New-Object System.Drawing.Size(90, 34)
    $dialog.Controls.Add($dialogButton)
    $dialog.AcceptButton = $dialogButton
    $dialog.CancelButton = $dialogButton

    $dialog.Add_Shown({
        $dialogHandle = $dialog.Handle
        $ownerHandle = [LswSeamlessFixture.NativeWindow]::GetWindow($dialogHandle, 4)
        $rootHandle = [LswSeamlessFixture.NativeWindow]::GetAncestor($dialogHandle, 2)
        $rootOwnerHandle = [LswSeamlessFixture.NativeWindow]::GetAncestor($dialogHandle, 3)
        $style = [LswSeamlessFixture.NativeWindow]::GetWindowLongPtr($dialogHandle, -16).ToInt64()
        $extendedStyle = [LswSeamlessFixture.NativeWindow]::GetWindowLongPtr($dialogHandle, -20).ToInt64()
        Write-Marker (
            "modal-window=hwnd:{0},owner:{1},root:{2},root-owner:{3},style:0x{4:X16},exstyle:0x{5:X16}" -f
            $dialogHandle.ToInt64(),
            $ownerHandle.ToInt64(),
            $rootHandle.ToInt64(),
            $rootOwnerHandle.ToInt64(),
            $style,
            $extendedStyle)
        if ($ownerHandle -ne $form.Handle -or
            $rootHandle -ne $dialogHandle -or
            $rootOwnerHandle -ne $dialogHandle -or
            (($style -band [Int64]0x80000000) -ne 0) -or
            (($extendedStyle -band 0x80) -ne 0)) {
            throw 'The modal fixture is not an ordinary top-level window owned by the main form'
        }
        Write-Marker "modal-kind=ordinary-owned"
    })

    [void]$dialog.ShowDialog($form)
    $dialog.Dispose()
    Write-Marker "modal-close"
})
$form.Controls.Add($modalButton)

$focusButton = New-Object System.Windows.Forms.Button
$focusButton.Name = "FocusButton"
$focusButton.Text = "Record focus recovery"
$focusButton.Location = New-Object System.Drawing.Point(530, 170)
$focusButton.Size = New-Object System.Drawing.Size(170, 36)
$focusButton.Add_Click({ Write-Marker "focus-recovered" })
$form.Controls.Add($focusButton)

$context = New-Object System.Windows.Forms.ContextMenuStrip
$contextAction = New-Object System.Windows.Forms.ToolStripMenuItem("Record context menu")
$contextAction.Name = "ContextAction"
$contextAction.BackColor = [System.Drawing.Color]::FromArgb(22, 163, 74)
$contextAction.ForeColor = [System.Drawing.Color]::White
$contextAction.Add_Click({ Write-Marker "context-menu-item" })
[void]$context.Items.Add($contextAction)
$context.BackColor = [System.Drawing.Color]::FromArgb(22, 163, 74)
$context.Add_Opened({
    # WinForms shows a ContextMenuStrip only after the native right-button
    # down/up gesture has completed. Form.MouseDown/MouseUp are not reliable
    # observation points once a ContextMenuStrip owns that gesture, so record
    # the completed pair after it is visible for the exact edge-delivery gate.
    Write-Marker "right-button"
    Write-Marker "right-button-up"
})
$form.ContextMenuStrip = $context

$initialSentinel = New-Object System.Windows.Forms.Panel
$initialSentinel.Name = "InitialSentinel"
$initialSentinel.Location = New-Object System.Drawing.Point(560, 320)
$initialSentinel.Size = New-Object System.Drawing.Size(160, 90)
$initialSentinel.BackColor = [System.Drawing.Color]::FromArgb(12, 140, 220)
$initialSentinel.Anchor = [System.Windows.Forms.AnchorStyles]::Bottom -bor [System.Windows.Forms.AnchorStyles]::Right
$form.Controls.Add($initialSentinel)
$script:initialFrameWidth = 0
$script:initialFrameHeight = 0

$resizeSentinel = New-Object System.Windows.Forms.Panel
$resizeSentinel.Name = "ResizeSentinel"
$resizeSentinel.Location = New-Object System.Drawing.Point(20, 320)
$resizeSentinel.Size = New-Object System.Drawing.Size(150, 70)
$resizeSentinel.BackColor = [System.Drawing.Color]::FromArgb(249, 115, 22)
$resizeSentinel.Anchor = [System.Windows.Forms.AnchorStyles]::Bottom -bor [System.Windows.Forms.AnchorStyles]::Left
$resizeSentinel.Visible = $false
$form.Controls.Add($resizeSentinel)

$maximizeSentinel = New-Object System.Windows.Forms.Panel
$maximizeSentinel.Name = "MaximizeSentinel"
$maximizeSentinel.Location = New-Object System.Drawing.Point(200, 320)
$maximizeSentinel.Size = New-Object System.Drawing.Size(150, 70)
$maximizeSentinel.BackColor = [System.Drawing.Color]::FromArgb(220, 38, 38)
$maximizeSentinel.Anchor = [System.Windows.Forms.AnchorStyles]::Bottom -bor [System.Windows.Forms.AnchorStyles]::Left
$maximizeSentinel.Visible = $false
$form.Controls.Add($maximizeSentinel)

$form.Add_MouseDown({
    param($sender, $event)
    if ($event.Button -eq [System.Windows.Forms.MouseButtons]::Left) {
        Write-Marker "left-button-down"
    }
    elseif ($event.Button -eq [System.Windows.Forms.MouseButtons]::Middle) {
        Write-Marker "middle-button"
    }
})
$form.Add_MouseUp({
    param($sender, $event)
    if ($event.Button -eq [System.Windows.Forms.MouseButtons]::Left) {
        Write-Marker "left-button-up"
    }
})
$form.Add_KeyDown({
    param($sender, $event)
    if ($event.KeyCode -eq [System.Windows.Forms.Keys]::ControlKey) {
        Write-Marker "ctrl-key-down"
    }
    elseif ($event.Control -and $event.KeyCode -eq [System.Windows.Forms.Keys]::C) {
        Write-Marker "ctrl-c-key-down"
        [void]$form.BeginInvoke([System.Action]{
            $clipboardText = [System.Windows.Forms.Clipboard]::GetText()
            $encoded = [Convert]::ToBase64String(
                [System.Text.Encoding]::UTF8.GetBytes($clipboardText)
            )
            Write-Marker "clipboard-copy-base64=$encoded"
        })
    }
    elseif ($event.Control -and $event.KeyCode -eq [System.Windows.Forms.Keys]::V) {
        Write-Marker "ctrl-v-key-down"
        [void]$form.BeginInvoke([System.Action]{
            $encoded = [Convert]::ToBase64String(
                [System.Text.Encoding]::UTF8.GetBytes($destination.Text)
            )
            Write-Marker "clipboard-paste-base64=$encoded"
        })
    }
})
$form.Add_KeyUp({
    param($sender, $event)
    if ($event.KeyCode -eq [System.Windows.Forms.Keys]::ControlKey) {
        Write-Marker "ctrl-key-up"
    }
})
$form.Add_MouseWheel({
    param($sender, $event)
    Write-Marker ("wheel={0}" -f $event.Delta)
})
$script:postResizePaintTicks = 0
$script:postResizeReadyMarker = $null
$postResizePaintTimer = New-Object System.Windows.Forms.Timer
$postResizePaintTimer.Interval = 75
$postResizePaintTimer.Add_Tick({
    if ($script:postResizePaintTicks -le 0) {
        $postResizePaintTimer.Stop()
        return
    }
    $script:postResizePaintTicks--
    $form.Invalidate($true)
    $form.Update()
    if ($script:postResizePaintTicks -eq 0) {
        $postResizePaintTimer.Stop()
        if ($null -ne $script:postResizeReadyMarker) {
            Write-Marker $script:postResizeReadyMarker
            $script:postResizeReadyMarker = $null
        }
    }
})
$form.Add_Resize({
    $width = $form.Width
    $height = $form.Height
    if ($form.Visible -and $form.IsHandleCreated) {
        $frameSize = Write-DwmFrameSize
        if ($script:initialFrameWidth -gt 0 -and $script:initialFrameHeight -gt 0) {
            $initialSentinel.Visible = (
                $frameSize.Width -eq $script:initialFrameWidth -and
                $frameSize.Height -eq $script:initialFrameHeight)
        }
        $resizeSentinel.Visible = ($frameSize.Width -eq 900 -and $frameSize.Height -eq 650)
        $maximizeSentinel.Visible = ($frameSize.Width -gt 900 -or $frameSize.Height -gt 650)
        # Make the fixture's post-resize visual state deterministic before the
        # marker is published. The gate still proves that WGC, the protocol,
        # the presenter, and the Windows host all carry this freshly painted
        # frame; it no longer depends on a later idle-paint scheduling race.
        $form.Invalidate($true)
        $form.Update()
        if ($initialSentinel.Visible -or $resizeSentinel.Visible -or $maximizeSentinel.Visible) {
            # Windows Graphics Capture can publish the geometry frame before
            # a WinForms Resize handler's child-control paint. Repaint for a
            # bounded post-resize interval so the gate validates a stable
            # application frame instead of one transient DWM ordering.
            $script:postResizePaintTicks = 8
            $script:postResizeReadyMarker = $null
            $postResizePaintTimer.Start()
        }
    }
    else {
        $initialSentinel.Visible = $false
        $resizeSentinel.Visible = $false
        $maximizeSentinel.Visible = $false
    }
    Write-Marker ("resize={0}x{1}" -f $width, $height)
    Write-Marker ("window-state={0}" -f $form.WindowState)
    Write-Marker ("resize-sentinel={0}" -f $(if ($resizeSentinel.Visible) { "visible" } else { "hidden" }))
    if ($maximizeSentinel.Visible) {
        Write-Marker ("max-ready={0}x{1}" -f $width, $height)
    }
})
$form.Add_Activated({ Write-Marker "focus" })
$form.Add_Deactivate({ Write-Marker "blur" })
$form.Add_Shown({
    $graphics = $form.CreateGraphics()
    try {
        $initialFrameSize = Write-DwmFrameSize
        $script:initialFrameWidth = $initialFrameSize.Width
        $script:initialFrameHeight = $initialFrameSize.Height
        $initialSentinel.Visible = $true
        Write-Marker ("window-hwnd={0}" -f $form.Handle.ToInt64())
        $frame = New-Object LswSeamlessFixture.NativeRect
        $frameResult = [LswSeamlessFixture.NativeWindow]::DwmGetWindowAttribute(
            $form.Handle,
            9,
            [ref]$frame,
            [Runtime.InteropServices.Marshal]::SizeOf([type][LswSeamlessFixture.NativeRect]))
        if ($frameResult -ne 0) {
            throw ("could not query the fixture frame for hit-test diagnostics: {0}" -f $frameResult)
        }
        $probeCenter = New-Object System.Drawing.Point(
            [int]($leftButton.ClientSize.Width / 2),
            [int]($leftButton.ClientSize.Height / 2))
        $probeScreen = $leftButton.PointToScreen($probeCenter)
        Write-Marker ("pointer-probe=x:{0},y:{1}" -f
            ($probeScreen.X - $frame.Left),
            ($probeScreen.Y - $frame.Top))
        $northwestHit = [LswSeamlessFixture.NativeWindow]::HitTest(
            $form.Handle, $frame.Left + 2, $frame.Top + 2)
        $southeastHit = [LswSeamlessFixture.NativeWindow]::HitTest(
            $form.Handle, $frame.Right - 2, $frame.Bottom - 2)
        Write-Marker ("hit-test=northwest:{0},southeast:{1}" -f $northwestHit, $southeastHit)
        $cornerHitTests = for ($offset = 0; $offset -le 8; $offset++) {
            $left = $frame.Left + $offset
            $top = $frame.Top + $offset
            $right = $frame.Right - 1 - $offset
            $bottom = $frame.Bottom - 1 - $offset
            "{0}:{1}/{2}/{3}/{4}" -f
                $offset,
                [LswSeamlessFixture.NativeWindow]::HitTest($form.Handle, $left, $top),
                [LswSeamlessFixture.NativeWindow]::HitTest($form.Handle, $right, $top),
                [LswSeamlessFixture.NativeWindow]::HitTest($form.Handle, $right, $bottom),
                [LswSeamlessFixture.NativeWindow]::HitTest($form.Handle, $left, $bottom)
        }
        Write-Marker ("corner-hit-tests={0}" -f ($cornerHitTests -join ','))
        $topCenterX = $frame.Left + [int](($frame.Right - $frame.Left) / 2)
        $topHitTests = for ($offset = 0; $offset -le 16; $offset++) {
            "{0}:{1}" -f $offset, [LswSeamlessFixture.NativeWindow]::HitTest(
                $form.Handle, $topCenterX, $frame.Top + $offset)
        }
        Write-Marker ("top-hit-tests={0}" -f ($topHitTests -join ','))
        $captionYOffset = [Math]::Max(2, [int](($frame.Bottom - $frame.Top) / 40))
        $captionY = $frame.Top + $captionYOffset
        $minimizeOffsets = @()
        $maximizeOffsets = @()
        $closeOffsets = @()
        $captionScanLimit = [Math]::Min(256, ($frame.Right - $frame.Left) - 1)
        for ($offset = 1; $offset -le $captionScanLimit; $offset++) {
            $hit = [LswSeamlessFixture.NativeWindow]::HitTest(
                $form.Handle, $frame.Right - $offset, $captionY)
            switch ($hit) {
                8 { $minimizeOffsets += $offset }
                9 { $maximizeOffsets += $offset }
                20 { $closeOffsets += $offset }
            }
        }
        $minimizeOffset = if ($minimizeOffsets.Count -gt 0) {
            $minimizeOffsets[[int](($minimizeOffsets.Count - 1) / 2)]
        } else { 0 }
        $maximizeOffset = if ($maximizeOffsets.Count -gt 0) {
            $maximizeOffsets[[int](($maximizeOffsets.Count - 1) / 2)]
        } else { 0 }
        $closeOffset = if ($closeOffsets.Count -gt 0) {
            $closeOffsets[[int](($closeOffsets.Count - 1) / 2)]
        } else { 0 }
        Write-Marker ("caption-controls=y:{0},minimize:{1},maximize:{2},close:{3}" -f
            $captionYOffset, $minimizeOffset, $maximizeOffset, $closeOffset)
        Write-Marker ("dpi={0}x{1}" -f [int][Math]::Round($graphics.DpiX), [int][Math]::Round($graphics.DpiY))
        Write-Marker ("window-state={0}" -f $form.WindowState)
        $form.Invalidate($true)
        $form.Update()
        $script:postResizePaintTicks = 8
        $script:postResizeReadyMarker = "initial-frame-ready"
        $postResizePaintTimer.Start()
    }
    finally {
        $graphics.Dispose()
    }
})

$script:observedHeldInput = $false
$script:recordedReleasedInput = $false
$heldCtrlLeftState = "input-state=ctrl:1,shift:0,alt:0,left:1,middle:0,right:0,x1:0,x2:0"
$inputTimer = New-Object System.Windows.Forms.Timer
$inputTimer.Interval = 40
$inputTimer.Add_Tick({
    $state = Get-InputStateEvent
    if (-not $script:observedHeldInput -and
        $state -ceq $heldCtrlLeftState) {
        $script:observedHeldInput = $true
        Write-Marker "input-held=ctrl,left"
    }
    elseif ($script:observedHeldInput -and
        -not $script:recordedReleasedInput -and
        $state -ceq $releasedInputState) {
        $script:recordedReleasedInput = $true
        Write-Marker "input-released=all"
    }
})
$inputTimer.Start()

$animationTimer = $null
if ($Animate) {
    $script:animationFrame = 0
    $script:animationColors = @(
        [System.Drawing.Color]::FromArgb(31, 41, 55),
        [System.Drawing.Color]::FromArgb(55, 48, 163),
        [System.Drawing.Color]::FromArgb(3, 105, 161),
        [System.Drawing.Color]::FromArgb(8, 145, 178),
        [System.Drawing.Color]::FromArgb(5, 150, 105),
        [System.Drawing.Color]::FromArgb(77, 124, 15),
        [System.Drawing.Color]::FromArgb(161, 98, 7),
        [System.Drawing.Color]::FromArgb(194, 65, 12),
        [System.Drawing.Color]::FromArgb(190, 24, 93),
        [System.Drawing.Color]::FromArgb(126, 34, 206),
        [System.Drawing.Color]::FromArgb(67, 56, 202)
    )
    $animationTimer = New-Object System.Windows.Forms.Timer
    $animationTimer.Interval = 16
    $animationTimer.Add_Tick({
        $script:animationFrame++
        $index = $script:animationFrame % $script:animationColors.Count
        $form.BackColor = $script:animationColors[$index]
        $form.Invalidate()
    })
    $animationTimer.Start()
    Write-Marker "animation-ready"
}

$form.Add_FormClosing({
    Write-Marker ("closing-{0}" -f (Get-InputStateEvent))
})

$script:allowClose = -not $ClosePrompt
if ($ClosePrompt) {
    $form.Add_FormClosing({
        param($sender, $event)

        if ($script:allowClose) {
            return
        }

        $event.Cancel = $true
        Write-Marker "close-prompt-open"
        $dialog = New-Object System.Windows.Forms.Form
        $dialog.Text = "LSW Unsaved Changes Fixture"
        $dialog.ClientSize = New-Object System.Drawing.Size(420, 220)
        $dialog.StartPosition = [System.Windows.Forms.FormStartPosition]::CenterParent
        $dialog.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::FixedDialog
        $dialog.MinimizeBox = $false
        $dialog.MaximizeBox = $false
        $dialog.ShowInTaskbar = $false
        $dialog.BackColor = [System.Drawing.Color]::FromArgb(250, 204, 21)

        $dialogLabel = New-Object System.Windows.Forms.Label
        $dialogLabel.Text = "Discard the simulated unsaved changes?"
        $dialogLabel.AutoSize = $true
        $dialogLabel.Location = New-Object System.Drawing.Point(86, 58)
        $dialog.Controls.Add($dialogLabel)

        $cancelButton = New-Object System.Windows.Forms.Button
        $cancelButton.Text = "Cancel"
        $cancelButton.DialogResult = [System.Windows.Forms.DialogResult]::Cancel
        $cancelButton.Location = New-Object System.Drawing.Point(85, 145)
        $cancelButton.Size = New-Object System.Drawing.Size(110, 36)
        $dialog.Controls.Add($cancelButton)

        $discardButton = New-Object System.Windows.Forms.Button
        $discardButton.Text = "Discard"
        $discardButton.DialogResult = [System.Windows.Forms.DialogResult]::Yes
        $discardButton.Location = New-Object System.Drawing.Point(235, 145)
        $discardButton.Size = New-Object System.Drawing.Size(110, 36)
        $dialog.Controls.Add($discardButton)
        $dialog.CancelButton = $cancelButton
        $dialog.AcceptButton = $discardButton

        $dialog.Add_Shown({
            $mainFrame = New-Object LswSeamlessFixture.NativeRect
            $frameResult = [LswSeamlessFixture.NativeWindow]::DwmGetWindowAttribute(
                $form.Handle,
                9,
                [ref]$mainFrame,
                [Runtime.InteropServices.Marshal]::SizeOf(
                    [type][LswSeamlessFixture.NativeRect]))
            if ($frameResult -ne 0) {
                throw ("could not query the main frame for close-prompt controls: {0}" -f
                    $frameResult)
            }
            $cancelCenter = $cancelButton.PointToScreen((New-Object System.Drawing.Point(
                [int]($cancelButton.ClientSize.Width / 2),
                [int]($cancelButton.ClientSize.Height / 2))))
            $discardCenter = $discardButton.PointToScreen((New-Object System.Drawing.Point(
                [int]($discardButton.ClientSize.Width / 2),
                [int]($discardButton.ClientSize.Height / 2))))
            Write-Marker ("close-prompt-controls=cancel-x:{0},cancel-y:{1},discard-x:{2},discard-y:{3}" -f
                ($cancelCenter.X - $mainFrame.Left),
                ($cancelCenter.Y - $mainFrame.Top),
                ($discardCenter.X - $mainFrame.Left),
                ($discardCenter.Y - $mainFrame.Top))
        })

        $result = $dialog.ShowDialog($form)
        $dialog.Dispose()
        if ($result -eq [System.Windows.Forms.DialogResult]::Yes) {
            Write-Marker "close-prompt-discard"
            $script:allowClose = $true
            [void]$form.BeginInvoke([System.Action]{ $form.Close() })
        }
        else {
            Write-Marker "close-prompt-cancel"
        }
    })
}

$cleanupTimer = New-Object System.Windows.Forms.Timer
$cleanupTimer.Interval = 100
$cleanupTimer.Add_Tick({
    if (-not (Test-Path -LiteralPath $CleanupSignalPath -PathType Leaf)) {
        return
    }

    $cleanupTimer.Stop()
    Write-Marker "cleanup-signal"
    $script:allowClose = $true
    [void]$form.BeginInvoke([System.Action]{ $form.Close() })
})
$cleanupTimer.Start()

$form.Add_FormClosed({
    $inputTimer.Stop()
    $cleanupTimer.Stop()
    $postResizePaintTimer.Stop()
    if ($null -ne $animationTimer) {
        $animationTimer.Stop()
    }
    if ($script:clipboardSnapshotAvailable) {
        try {
            if ($null -eq $script:clipboardSnapshot) {
                [System.Windows.Forms.Clipboard]::Clear()
            }
            else {
                [System.Windows.Forms.Clipboard]::SetDataObject($script:clipboardSnapshot, $true)
            }
            Write-Marker "clipboard-restored"
        }
        catch {
            Write-Marker "clipboard-restore-failed"
        }
    }
    Write-Marker "closed"
})

        Write-Marker "ready"
[System.Windows.Forms.Application]::Run($form)
