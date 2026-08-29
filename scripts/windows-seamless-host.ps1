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

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class LswSeamlessHost {
    public const int GWL_STYLE = -16;
    public const int GWL_EXSTYLE = -20;
    public const uint GW_OWNER = 4;
    public const uint GA_ROOT = 2;
    public const uint DWMWA_EXTENDED_FRAME_BOUNDS = 9;
    public const uint DWMWA_CLOAKED = 14;
    public const int SW_MAXIMIZE = 3;
    public const uint INPUT_MOUSE = 0;
    public const uint INPUT_KEYBOARD = 1;
    public const uint KEYEVENTF_KEYUP = 0x0002;
    public const uint KEYEVENTF_UNICODE = 0x0004;
    public const uint MOUSEEVENTF_MOVE = 0x0001;
    public const uint MOUSEEVENTF_MOVE_NOCOALESCE = 0x2000;
    public const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    public const uint MOUSEEVENTF_LEFTUP = 0x0004;
    public const uint MOUSEEVENTF_RIGHTDOWN = 0x0008;
    public const uint MOUSEEVENTF_RIGHTUP = 0x0010;
    public const uint MOUSEEVENTF_MIDDLEDOWN = 0x0020;
    public const uint MOUSEEVENTF_MIDDLEUP = 0x0040;
    public const uint MOUSEEVENTF_WHEEL = 0x0800;
    public const uint MOUSEEVENTF_VIRTUALDESK = 0x4000;
    public const uint MOUSEEVENTF_ABSOLUTE = 0x8000;
    public const int SM_XVIRTUALSCREEN = 76;
    public const int SM_YVIRTUALSCREEN = 77;
    public const int SM_CXVIRTUALSCREEN = 78;
    public const int SM_CYVIRTUALSCREEN = 79;
    public const uint WM_CLOSE = 0x0010;
    public const uint WM_SYSCOMMAND = 0x0112;
    public const int SC_MINIMIZE = 0xF020;
    public const int SC_RESTORE = 0xF120;

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct POINT {
        public int X;
        public int Y;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct INPUT {
        public uint Type;
        public INPUTUNION Union;
    }

    [StructLayout(LayoutKind.Explicit)]
    public struct INPUTUNION {
        [FieldOffset(0)] public MOUSEINPUT Mouse;
        [FieldOffset(0)] public KEYBDINPUT Keyboard;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct MOUSEINPUT {
        public int Dx;
        public int Dy;
        public uint MouseData;
        public uint Flags;
        public uint Time;
        public UIntPtr ExtraInfo;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct KEYBDINPUT {
        public ushort VirtualKey;
        public ushort ScanCode;
        public uint Flags;
        public uint Time;
        public UIntPtr ExtraInfo;
    }

    public delegate bool EnumWindowsCallback(IntPtr hwnd, IntPtr parameter);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool EnumWindows(EnumWindowsCallback callback, IntPtr parameter);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowTextW(IntPtr hwnd, StringBuilder text, int maximum);
    [DllImport("user32.dll")]
    private static extern int GetWindowTextLengthW(IntPtr hwnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetClassNameW(IntPtr hwnd, StringBuilder text, int maximum);
    [DllImport("user32.dll")]
    public static extern bool IsWindow(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern bool IsZoomed(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern IntPtr GetWindow(IntPtr hwnd, uint command);
    [DllImport("user32.dll")]
    public static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hwnd, out RECT rectangle);
    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr hwnd, out RECT rectangle);
    [DllImport("user32.dll")]
    public static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
    public static extern IntPtr GetWindowLongPtr(IntPtr hwnd, int index);
    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr hwnd, int command);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern bool BringWindowToTop(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern IntPtr SetActiveWindow(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern IntPtr SetFocus(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();
    [DllImport("kernel32.dll")]
    public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")]
    public static extern bool AttachThreadInput(uint first, uint second, bool attach);
    [DllImport("user32.dll")]
    public static extern bool PostMessageW(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")]
    public static extern uint SendInput(uint count, INPUT[] inputs, int inputSize);
    [DllImport("user32.dll")]
    public static extern int GetSystemMetrics(int index);
    [DllImport("user32.dll")]
    public static extern bool GetCursorPos(out POINT point);
    [DllImport("user32.dll")]
    public static extern IntPtr WindowFromPoint(POINT point);
    [DllImport("user32.dll")]
    private static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("dwmapi.dll")]
    private static extern int DwmGetWindowAttribute(IntPtr hwnd, uint attribute,
                                                     out RECT value, int size);
    [DllImport("dwmapi.dll")]
    private static extern int DwmGetWindowAttribute(IntPtr hwnd, uint attribute,
                                                     out uint value, int size);

    public static string Title(IntPtr hwnd) {
        int length = GetWindowTextLengthW(hwnd);
        StringBuilder text = new StringBuilder(Math.Max(length + 1, 2));
        GetWindowTextW(hwnd, text, text.Capacity);
        return text.ToString();
    }

    public static string ClassName(IntPtr hwnd) {
        StringBuilder text = new StringBuilder(512);
        GetClassNameW(hwnd, text, text.Capacity);
        return text.ToString();
    }

    public static IntPtr[] Find(string titleNeedle, string processName, bool exactTitle) {
        List<IntPtr> matches = new List<IntPtr>();
        bool enumerated = EnumWindows(delegate(IntPtr hwnd, IntPtr parameter) {
            if (!IsWindowVisible(hwnd) || GetAncestor(hwnd, GA_ROOT) != hwnd) {
                return true;
            }
            string title = Title(hwnd);
            if (exactTitle
                    ? !String.Equals(title, titleNeedle, StringComparison.Ordinal)
                    : title.IndexOf(titleNeedle, StringComparison.Ordinal) < 0) {
                return true;
            }
            uint pid;
            GetWindowThreadProcessId(hwnd, out pid);
            try {
                using (System.Diagnostics.Process process = System.Diagnostics.Process.GetProcessById((int)pid)) {
                    if (!String.IsNullOrEmpty(processName) &&
                        !String.Equals(process.ProcessName, processName, StringComparison.OrdinalIgnoreCase)) {
                        return true;
                    }
                }
            } catch {
                return true;
            }
            matches.Add(hwnd);
            return true;
        }, IntPtr.Zero);
        if (!enumerated) {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "EnumWindows failed");
        }
        matches.Sort(delegate(IntPtr left, IntPtr right) {
            return left.ToInt64().CompareTo(right.ToInt64());
        });
        return matches.ToArray();
    }

    public static RECT DwmFrame(IntPtr hwnd) {
        RECT value;
        int result = DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, out value,
                                           Marshal.SizeOf(typeof(RECT)));
        if (result != 0) {
            throw new COMException(
                "DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS) failed", result
            );
        }
        return value;
    }

    public static uint Cloaked(IntPtr hwnd) {
        uint value;
        int result = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, out value, sizeof(uint));
        if (result != 0) {
            throw new COMException("DwmGetWindowAttribute(DWMWA_CLOAKED) failed", result);
        }
        return value;
    }

    public static void RequireWindow(IntPtr hwnd) {
        if (hwnd == IntPtr.Zero || !IsWindow(hwnd)) {
            throw new InvalidOperationException("the requested host HWND does not exist");
        }
    }

    public static void EnablePerMonitorDpiAwareness() {
        // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2. Failure is harmless when
        // the process inherited an equal or stronger context from its host.
        SetProcessDpiAwarenessContext(new IntPtr(-4));
    }

    private static void Submit(INPUT[] inputs) {
        uint sent = SendInput((uint)inputs.Length, inputs, Marshal.SizeOf(typeof(INPUT)));
        if (sent != inputs.Length) {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "SendInput was incomplete");
        }
    }

    private static INPUT KeyboardInput(ushort virtualKey, bool down) {
        INPUT input = new INPUT();
        input.Type = INPUT_KEYBOARD;
        input.Union.Keyboard.VirtualKey = virtualKey;
        input.Union.Keyboard.Flags = down ? 0u : KEYEVENTF_KEYUP;
        return input;
    }

    public static void Key(ushort virtualKey, bool down) {
        Submit(new INPUT[] { KeyboardInput(virtualKey, down) });
    }

    public static void Chord(ushort[] virtualKeys, int holdMilliseconds) {
        if (virtualKeys == null || virtualKeys.Length == 0) {
            throw new ArgumentException("a chord requires at least one virtual key", "virtualKeys");
        }
        if (holdMilliseconds < 10 || holdMilliseconds > 1000) {
            throw new ArgumentOutOfRangeException(
                "holdMilliseconds", "a chord hold must be between 10 and 1000 milliseconds"
            );
        }
        List<INPUT> inputs = new List<INPUT>(virtualKeys.Length);
        foreach (ushort virtualKey in virtualKeys) {
            inputs.Add(KeyboardInput(virtualKey, true));
        }
        // Submit each half as one serial batch, but keep the keys physically
        // held long enough for a remote UI thread to process the command.
        Submit(inputs.ToArray());
        Thread.Sleep(holdMilliseconds);
        inputs.Clear();
        for (int index = virtualKeys.Length - 1; index >= 0; index--) {
            inputs.Add(KeyboardInput(virtualKeys[index], false));
        }
        Submit(inputs.ToArray());
    }

    public static void UnicodeText(string text) {
        List<INPUT> inputs = new List<INPUT>();
        foreach (char value in text) {
            INPUT down = new INPUT();
            down.Type = INPUT_KEYBOARD;
            down.Union.Keyboard.ScanCode = value;
            down.Union.Keyboard.Flags = KEYEVENTF_UNICODE;
            inputs.Add(down);
            INPUT up = down;
            up.Union.Keyboard.Flags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
            inputs.Add(up);
        }
        if (inputs.Count > 0) {
            Submit(inputs.ToArray());
        }
    }

    public static void KeyboardText(string text) {
        foreach (char value in text) {
            ushort virtualKey = 0;
            bool shift = false;
            if (value >= 'a' && value <= 'z') {
                virtualKey = (ushort)Char.ToUpperInvariant(value);
            } else if (value >= 'A' && value <= 'Z') {
                virtualKey = (ushort)value;
                shift = true;
            } else if (value >= '0' && value <= '9') {
                virtualKey = (ushort)value;
            } else if (value == ' ') {
                virtualKey = 0x20;
            }
            if (virtualKey == 0) {
                UnicodeText(value.ToString());
                continue;
            }
            if (shift) {
                Key(0x10, true);
            }
            Key(virtualKey, true);
            Key(virtualKey, false);
            if (shift) {
                Key(0x10, false);
            }
        }
    }

    public static void Mouse(uint flags, uint data, int dx, int dy) {
        INPUT input = new INPUT();
        input.Type = INPUT_MOUSE;
        input.Union.Mouse.Dx = dx;
        input.Union.Mouse.Dy = dy;
        input.Union.Mouse.MouseData = data;
        input.Union.Mouse.Flags = flags;
        Submit(new INPUT[] { input });
    }

    public static void MovePointer(int screenX, int screenY) {
        int left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        int top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        int width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        int height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if (width <= 1 || height <= 1) {
            throw new InvalidOperationException("the Windows virtual desktop has invalid dimensions");
        }
        POINT start;
        if (!GetCursorPos(out start)) {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        const int steps = 12;
        for (int step = 1; step <= steps; step++) {
            int nextX = start.X + (int)Math.Round((screenX - start.X) * step / (double)steps);
            int nextY = start.Y + (int)Math.Round((screenY - start.Y) * step / (double)steps);
            int absoluteX = (int)Math.Round((nextX - left) * 65535.0 / (width - 1));
            int absoluteY = (int)Math.Round((nextY - top) * 65535.0 / (height - 1));
            absoluteX = Math.Max(0, Math.Min(65535, absoluteX));
            absoluteY = Math.Max(0, Math.Min(65535, absoluteY));
            Mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE |
                  MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                  0, absoluteX, absoluteY);
            if (step != steps) {
                System.Threading.Thread.Sleep(8);
            }
        }
    }

    public static POINT ClientOrigin(IntPtr hwnd) {
        RECT client;
        if (!GetClientRect(hwnd, out client)) {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        POINT origin = new POINT();
        if (!ClientToScreen(hwnd, ref origin)) {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        return origin;
    }

    public static void Activate(IntPtr hwnd) {
        RequireWindow(hwnd);
        if (IsIconic(hwnd)) {
            if (!PostMessageW(hwnd, WM_SYSCOMMAND, new IntPtr(SC_RESTORE), IntPtr.Zero)) {
                throw new Win32Exception(Marshal.GetLastWin32Error(),
                                         "PostMessage(SC_RESTORE) failed");
            }
            for (int restoreAttempt = 0; restoreAttempt < 100 && IsIconic(hwnd);
                 restoreAttempt++) {
                Thread.Sleep(20);
            }
            if (IsIconic(hwnd)) {
                throw new InvalidOperationException(
                    "the requested host HWND did not leave its iconic state");
            }
        }
        for (int attempt = 0; attempt < 20; attempt++) {
            IntPtr foreground = GetForegroundWindow();
            uint foregroundThread = 0;
            uint ignored;
            if (foreground != IntPtr.Zero) {
                foregroundThread = GetWindowThreadProcessId(foreground, out ignored);
            }
            uint currentThread = GetCurrentThreadId();
            bool attached = foregroundThread != 0 && foregroundThread != currentThread &&
                            AttachThreadInput(currentThread, foregroundThread, true);
            try {
                BringWindowToTop(hwnd);
                SetActiveWindow(hwnd);
                SetFocus(hwnd);
                SetForegroundWindow(hwnd);
            } finally {
                if (attached) {
                    AttachThreadInput(currentThread, foregroundThread, false);
                }
            }
            if (GetForegroundWindow() == hwnd) {
                return;
            }
            // A synthetic Alt press permits SetForegroundWindow without leaving a key held.
            Key(0x12, true);
            Key(0x12, false);
            Thread.Sleep(50);
        }
        throw new InvalidOperationException("the requested host HWND did not become foreground");
    }
}
'@

[LswSeamlessHost]::EnablePerMonitorDpiAwareness()

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
