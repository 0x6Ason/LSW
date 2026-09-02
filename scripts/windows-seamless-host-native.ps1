# SPDX-License-Identifier: GPL-3.0-or-later

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
