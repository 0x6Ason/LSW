# LSW roadmap

LSW prioritizes a polished Linux desktop experience on the existing Linux
x86_64 and Windows 11 x64 foundation before adding more host and guest
architectures. Version labels describe intended ordering, not promised dates.

The roadmap follows five rules:

- User-visible latency, memory use, and integration come before a broad platform
  matrix.
- Background integration is optional, reversible, and measurable.
- Driverless and documented operating-system interfaces are the default path;
  signed acceleration remains optional.
- Host folders, clipboard data, drag-and-drop, and GUI input are explicit trust
  boundaries with narrow per-instance permissions.
- A release claim requires automated source gates and the exact-commit Windows/KVM
  gate on real hardware.

## Shipped: 1.0.0-beta.6

beta.6 completed the terminal-first runtime: one-command Windows installation,
official Microsoft ISO resolution and exact-SHA download, WinPE DISM preparation,
unattended OOBE, the boot-time SCM agent, complete remote process semantics,
recursive transfer and watch sync, dynamic ports, shell completion, and systemd
user socket activation.

## 1.0.0-beta.7: fast, quiet background runtime and folder sharing

This release makes an installed environment inexpensive to keep available and
gives desktop work a safe file-sharing foundation. Its tag is permitted only
after the exact commit passes the expanded Windows/KVM gate.

- Add prepared and sealed base images keyed by the exact ISO, profile, agent,
  firmware, and preparation identity; create instances as linked clones with
  per-instance secrets injected only after cloning.
- Turn the existing systemd user units into simple opt-in product UX with
  enable, disable, status, and diagnostics commands. Socket activation remains
  optional, and `lswd` must be able to exit when idle instead of consuming RAM
  merely because the user logged in.
- Enforce and publish idle resource gates: less than 30 MiB for a running idle
  daemon, no `lswd` process while only the socket unit is waiting, and zero QEMU
  RSS after a guest is stopped or hibernated.
- Add the memory balloon governor, memory-pressure response, the
  running -> pause -> hibernate policy, automatic resume, guest TRIM, qcow2
  discard, and offline compaction.
- Add opt-in per-instance host-folder sharing with explicit roots, read-only and
  read-write modes, reconnect/resume behavior, periodic change detection, and strict
  host-symlink and guest-reparse-point boundaries.
- Establish the permanent interactive-user identity and authenticated bulk
  transport boundary needed by the later companion, clipboard, and
  drag-and-drop work without weakening the Session 0 service boundary.
- After the agent becomes ready, make interactive `lsw install` enter a
  WSL-style registration flow before returning: create a permanent standard
  Windows desktop user and make it the instance's default interactive identity.
  Do not silently reuse the temporary `LSWSetup` account or derive a username
  from the Linux host without confirmation.
- Add `lsw user setup [NAME]` for deferred and recovery setup. Interactive users
  may explicitly defer; noninteractive automation must either use a dedicated
  secure credential-input path or pass `--defer-user-setup`. Username validation
  and masked password confirmation must be local. The agent must create the
  account through native Windows account APIs, and the password must never enter
  argv, environment, the manifest, installation seed, logs, or diagnostics.
  Administrator membership requires a separate explicit choice.
- Do not enable Windows AutoLogon. A desktop session may request the password at
  launch or use an explicitly enabled Linux Secret Service/keyring entry; the
  normal SCM agent must remain usable before any interactive user signs in.
- Keep signed VirtIO networking, filesystem, balloon, and vsock acceleration
  optional. Inbox-device installation and recovery must continue to work.

Acceptance requires clone identity tests, cross-instance secret isolation,
folder escape tests, daemon/QEMU RSS measurements, pressure and hibernation
soaks, and a cold-restart Windows/KVM gate.

## beta.8: Windows applications as Linux desktop applications

This release makes a Windows GUI application launched by LSW behave like a
native Linux desktop application rather than exposing a remote Windows desktop.

- Add a stable CLI and desktop-launcher path for `.exe` applications, including
  icon discovery, `.desktop` entries, file arguments, working directories, and
  environment values. The intended CLI shape is `lsw run --gui ...`; the final
  spelling will be fixed before the feature is declared stable.
- Map every eligible top-level Windows HWND to an independent Wayland window,
  with an X11 fallback, correct parent/modal relationships, focus, pointer,
  keyboard, resize, minimize/maximize, and per-monitor DPI behavior.
- Synchronize the text clipboard. When a GUI window has focus, Ctrl+C and
  Ctrl+V must reach that application and produce ordinary Linux clipboard
  behavior; terminal SIGINT semantics remain unchanged.
- Support native host-to-guest and guest-to-host file drag-and-drop through the
  folder-sharing/staging boundary, with visible progress, cancellation, resume,
  collision handling, and no implicit overwrite.
- Add audio, notifications, and single-window full-screen mode without requiring
  RDP or a public VNC listener.
- Start with documented Windows Graphics Capture and damage-aware transport.
  Shared-memory or GPU acceleration may improve performance later but cannot be
  required for correctness.

Acceptance requires common development applications, file dialogs, elevated
window boundaries, clipboard focus, large-file drag-and-drop, DPI transitions,
full-screen recovery, and desktop-session restart testing on Wayland and X11.

## beta.9: seamless desktop polish and shell-light mode

- Add multi-monitor placement, mixed-DPI movement, full-screen selection,
  window snapping, transient windows, taskbar/tray integration where practical,
  and session restore.
- Extend clipboard and drag-and-drop support to images, file lists, large trees,
  and interrupted transfers while retaining explicit trust prompts for paths
  outside configured shares.
- Add an experimental, opt-in `shell-light` profile for GUI-focused instances.
  It may use documented shell-selection and policy mechanisms to run the LSW
  user-session companion without a normal Explorer desktop when the installed
  Windows edition supports that configuration.
- Measure Explorer, Desktop Window Manager, shell services, startup tasks, idle
  CPU, committed memory, and working set before changing defaults. Keep a
  one-command fallback to the normal Windows shell.

LSW will not patch or replace the `explorer.exe` binary, remove the Windows
servicing stack, or make shell-light behavior part of `vanilla` or `slim` by
default. The optimization must be reversible, update-safe, and covered by the
real Windows/KVM gate.

## beta.10 and later: architecture and host expansion

Architecture work starts only after the desktop and low-resource experience
above has stable release gates.

1. Linux ARM64 host with Windows ARM64 guest, including a native Windows ARM64
   agent, firmware, Microsoft media selection, installer path, and CI hardware.
2. macOS Apple Silicon with HVF and Windows ARM64.
3. Windows host support with WHPX.

Windows x64 emulation on Apple Silicon is not an acceptable substitute for the
Windows ARM64 path.

## Not scheduled as default behavior

- Patching Microsoft binaries or redistributing modified Windows media.
- Requiring unsigned or test-signed guest drivers.
- Enabling host folders, clipboard, drag-and-drop, or background services
  without explicit user configuration.
- Claiming a platform based only on argument planning or emulation smoke tests.
