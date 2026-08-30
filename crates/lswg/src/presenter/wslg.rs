// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use winit::window::Window;

use super::{trace_gui, WSLG_HOST_CONTROL_TIMEOUT};

pub(super) fn display_title(title: &str) -> String {
    if title.trim().is_empty() {
        "Windows application - LSW".to_owned()
    } else {
        format!("{title} - LSW")
    }
}

pub(super) fn minimize_host_window(window: &Window, host_window_title: &str) -> Result<(), String> {
    if !running_under_wslg() {
        trace_gui("requesting native Wayland minimization");
        window.set_minimized(true);
        return Ok(());
    }

    // xdg_toplevel.set_minimized is explicitly advisory and current WSLg
    // releases ignore it. Ask Windows to minimize only the unique exact-title
    // RAIL proxy owned by msrdc. Identity values travel as hex over stdin
    // rather than becoming PowerShell source, so a guest-controlled title
    // cannot inject host commands.
    trace_gui("requesting identity-checked WSLg host minimization");
    let result = run_wslg_host_control("minimize", host_window_title);
    trace_gui(if result.is_ok() {
        "WSLg host minimization completed"
    } else {
        "WSLg host minimization failed"
    });
    result
}

pub(super) fn running_under_wslg() -> bool {
    if env::var_os("WSL_INTEROP").is_none() || env::var_os("WSL_DISTRO_NAME").is_none() {
        return false;
    }
    let Some(runtime_directory) = env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    let Some(display) = env::var_os("WAYLAND_DISPLAY") else {
        return false;
    };
    let display_path = Path::new(&display);
    let socket_path = if display_path.is_absolute() {
        display_path.to_path_buf()
    } else {
        Path::new(&runtime_directory).join(display_path)
    };
    fs::canonicalize(socket_path)
        .or_else(|_| fs::canonicalize(&runtime_directory))
        .map(|path| path.starts_with("/mnt/wslg"))
        .unwrap_or(false)
}

pub(super) fn run_wslg_host_control(
    operation: &str,
    host_window_title: &str,
) -> Result<(), String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
function ConvertFrom-LswHex {
    param([Parameter(Mandatory = $true)][string]$Value)
    if (($Value.Length % 2) -ne 0 -or $Value -notmatch '^[0-9a-f]*$') {
        throw 'the WSLg host-window identity was not canonical hex'
    }
    $bytes = [byte[]]::new($Value.Length / 2)
    for ($index = 0; $index -lt $bytes.Length; $index++) {
        $bytes[$index] = [Convert]::ToByte($Value.Substring($index * 2, 2), 16)
    }
    return [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
}
$identityLines = @([Console]::In.ReadToEnd().Split("`n"))
if ($identityLines.Count -lt 3) {
    throw 'the WSLg host-window identity was incomplete'
}
$operation = $identityLines[0].TrimEnd("`r")
$expectedTitle = ConvertFrom-LswHex $identityLines[1].TrimEnd("`r")
$distroName = ConvertFrom-LswHex $identityLines[2].TrimEnd("`r")
if ([string]::IsNullOrEmpty($expectedTitle) -or [string]::IsNullOrEmpty($distroName)) {
    throw 'the expected WSLg title or distro identity was empty'
}
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;

public static class LswWslgHostWindow {
    public const uint GA_ROOT = 2;
    public delegate bool EnumWindowsCallback(IntPtr hwnd, IntPtr parameter);

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool EnumWindows(EnumWindowsCallback callback, IntPtr parameter);
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")]
    public static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);
    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")]
    public static extern int GetWindowTextLengthW(IntPtr hwnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowTextW(IntPtr hwnd, StringBuilder text, int maximum);
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll")]
    public static extern bool PostMessageW(IntPtr hwnd, uint message, IntPtr wParam,
                                           IntPtr lParam);
    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hwnd);
    public static string Title(IntPtr hwnd) {
        int length = GetWindowTextLengthW(hwnd);
        StringBuilder text = new StringBuilder(Math.Max(length + 1, 2));
        GetWindowTextW(hwnd, text, text.Capacity);
        return text.ToString();
    }

    private static bool HasExpectedTitle(IntPtr hwnd, string[] expectedTitles) {
        string observed = Title(hwnd);
        foreach (string expected in expectedTitles) {
            if (String.Equals(observed, expected, StringComparison.Ordinal)) {
                return true;
            }
        }
        return false;
    }

    private static bool IsRailOwner(IntPtr hwnd) {
        uint processId;
        GetWindowThreadProcessId(hwnd, out processId);
        try {
            using (Process owner = Process.GetProcessById((int)processId)) {
                return String.Equals(owner.ProcessName, "msrdc",
                                     StringComparison.OrdinalIgnoreCase);
            }
        } catch {
            return false;
        }
    }

    private static bool IsExactRailWindow(IntPtr hwnd, string[] expectedTitles) {
        return hwnd != IntPtr.Zero && IsWindowVisible(hwnd) &&
               GetAncestor(hwnd, GA_ROOT) == hwnd &&
               HasExpectedTitle(hwnd, expectedTitles) && IsRailOwner(hwnd);
    }

    public static IntPtr Resolve(string[] expectedTitles) {
        IntPtr foreground = GetForegroundWindow();
        if (IsExactRailWindow(foreground, expectedTitles)) {
            return foreground;
        }
        List<IntPtr> matches = new List<IntPtr>();
        if (!EnumWindows(delegate(IntPtr hwnd, IntPtr parameter) {
            if (IsExactRailWindow(hwnd, expectedTitles)) {
                matches.Add(hwnd);
            }
            return true;
        }, IntPtr.Zero)) {
            throw new System.ComponentModel.Win32Exception(
                Marshal.GetLastWin32Error(), "EnumWindows failed");
        }
        if (matches.Count != 1) {
            throw new InvalidOperationException(
                "the exact WSLg RAIL proxy identity was absent or ambiguous");
        }
        return matches[0];
    }
}
'@
[string[]]$allowedTitles = @(
    "$expectedTitle ($distroName)",
    "[WARN:COPY MODE] $expectedTitle ($distroName)"
)
$window = [LswWslgHostWindow]::Resolve($allowedTitles)
switch ($operation) {
    'minimize' {
        if (-not [LswWslgHostWindow]::PostMessageW(
            $window, 0x0112, [IntPtr]::new(0xF020), [IntPtr]::Zero
        )) {
            throw 'PostMessage(SC_MINIMIZE) failed for the identity-checked WSLg proxy'
        }
        $deadline = [DateTime]::UtcNow.AddSeconds(2)
        do {
            if ([LswWslgHostWindow]::IsIconic($window)) {
                return
            }
            [Threading.Thread]::Sleep(20)
        } while ([DateTime]::UtcNow -lt $deadline)
        throw 'the identity-checked WSLg host window did not minimize'
    }
    default { throw 'unsupported WSLg host-window control operation' }
}
"#;

    let distro_name = env::var("WSL_DISTRO_NAME")
        .map_err(|_| "WSL_DISTRO_NAME is required for WSLg host-window control".to_owned())?;
    if distro_name.is_empty() {
        return Err("WSL_DISTRO_NAME is empty during WSLg host-window control".to_owned());
    }
    let identity = format!(
        "{operation}\n{}\n{}",
        hex_encode(host_window_title),
        hex_encode(&distro_name)
    );

    let mut child = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start WSLg {operation} control: {error}"))?;

    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| "could not open WSLg host-window control input".to_owned())
        .and_then(|mut stdin| {
            stdin
                .write_all(identity.as_bytes())
                .map_err(|error| format!("could not send the WSLg host-window identity: {error}"))
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let deadline = Instant::now() + WSLG_HOST_CONTROL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("WSLg {operation} control timed out"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "could not wait for WSLg {operation} control: {error}"
                ));
            }
        }
    };
    let mut stderr = Vec::new();
    if let Some(stream) = child.stderr.take() {
        let _ = stream.take(8 * 1024).read_to_end(&mut stderr);
    }
    let detail = String::from_utf8_lossy(&stderr);
    let detail = detail.trim_matches(['\0', '\r', '\n', ' ']);
    if status.success() {
        if !detail.is_empty() {
            trace_gui(format_args!("WSLg {operation} control: {detail}"));
        }
        return Ok(());
    }
    if detail.is_empty() {
        Err(format!("WSLg {operation} control failed with {status}"))
    } else {
        Err(format!(
            "WSLg {operation} control failed with {status}: {detail}"
        ))
    }
}

pub(super) fn hex_encode(value: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len().saturating_mul(2));
    for byte in value.bytes() {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}
