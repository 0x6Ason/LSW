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

## Shipped: 1.0.0-beta.7

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

beta.8 is delivered as eight bounded slices. The slices are internal acceptance
boundaries, not additional version tags; each must keep existing terminal and
headless workflows working.

### Slice 1: desktop identity and consent foundation

- Make the Windows account role part of desktop setup. For a personal
  development VM, recommend adding the confirmed desktop user to the local
  Administrators group so normal processes still use a filtered token and UAC
  remains the elevation boundary. Keep a standard-user choice, support explicit
  promotion, demotion, and creation of a separate administrator, and never
  enable the built-in Administrator or create a hidden recovery account.
  Existing instances must not be promoted silently during an upgrade.
- Detect the native Windows 11 `sudo.exe` capability and offer explicit opt-in
  enablement using its safer new-window mode. Do not bundle a third-party sudo
  replacement, disable UAC, or treat sudo as a way to bypass Windows consent.
- Slice 1 is implemented on the beta.8 development line: manifest role and
  native membership reconciliation, the install prompt, explicit
  promote/demote and separate-account creation commands, capability-gated
  native-sudo status, and reversible safe-mode configuration all pass local
  source gates. The exact Windows/KVM commit gate remains the release acceptance
  boundary.

### Slice 2: zero-configuration UX and live Linux folders

- Make the common path short: `lsw install` may choose a safe default instance
  name, bare `lsw share` lists shares, `lsw share PATH` immediately adds and
  mounts a persistent read-write share for the default instance, and
  `lsw unshare SHARE` removes it. Add `lsw cp SOURCE DESTINATION` with direction
  inferred from the Windows path. Preserve `share add/sync/watch`, `push`, and
  `pull` as stable advanced and automation interfaces.
- After desktop-user setup, offer one recommended-integration dialog with an
  explicit `[Y/n]` choice. Show the exact host root and access mode before
  enabling it. Recommend a dedicated `~/LSW` read-write directory, map it in
  Windows as `Linux (L:)`, and never expose the whole Linux home directory
  implicitly. License acceptance and administrator selection remain separate
  consent steps.
- Prototype a driverless live share over the VM's private QEMU user-network SMB
  path so Explorer, file dialogs, and Windows applications see current host
  contents without copying. Keep the authenticated agent mirror for offline
  synchronization, recovery, staging, and hosts without the SMB helper. WebDAV
  is not the primary filesystem transport.
- Before changing the default, benchmark sequential 1 GiB I/O, large trees of
  small files, metadata walks, `git status`, and representative builds against
  the existing agent mirror and guest-local storage. Add a machine-readable
  `lsw bench files` result and publish the tested boundary.
- Treat signed Windows Virtio-fs plus WinFsp as a later opt-in accelerator after
  its signing, update, reconnect, locking, Unicode, and deletion behavior passes
  the same tests. It cannot be required for correct default sharing.
- Slice 2 is implemented on the beta.8 development line: nameless first install,
  short share/unshare/copy commands, separate recommended-integration consent,
  a single fail-closed driverless QEMU SMB root mounted globally as `Linux
  (L:)`, manifest migration, the retained agent-mirror interfaces, and
  machine-readable guest-local/live/mirror file benchmarks are covered by the
  exact Windows/KVM gate. The default benchmark uses 1 GiB and 4,096 small
  files; CI may select smaller bounded dimensions while retaining the schema.

### Slice 3: user-session companion and GUI launch

- Add an authenticated companion in the registered Windows user's interactive
  session without enabling AutoLogon. Start it on demand and let it exit when no
  GUI application, live share, clipboard, or integration client needs it.
- Map the approved live share in that user session and recover it after guest or
  desktop-session restart without exposing it to unrelated instances.
- Add a stable CLI and desktop-launcher path for `.exe` applications, including
  icon discovery, `.desktop` entries, file arguments, working directories, and
  environment values. The intended CLI shape is `lsw run --gui ...`; the final
  spelling will be fixed before the feature is declared stable.

### Slice 4: first seamless application window

- Launch one ordinary application in the desktop user's session, capture it
  through documented Windows Graphics Capture, and present a damage-aware
  Wayland window with an X11 fallback.
- Implement lifecycle ownership, focus, keyboard, pointer, resize, close, and
  crash recovery for that first window without requiring RDP or public VNC.
  Shared-memory or GPU acceleration may improve performance later but cannot be
  required for correctness.

### Slice 5: complete HWND and display behavior

- Map every eligible top-level Windows HWND to an independent Linux window with
  correct owner, parent, modal, transient, minimize/maximize, and task-switching
  relationships.
- Add per-monitor DPI, mixed-scale movement, resize negotiation, single-window
  full screen, input-method behavior, and desktop-session restart recovery.

### Slice 6: clipboard, file dialogs, and drag-and-drop

- Synchronize the text clipboard. When a GUI window has focus, Ctrl+C and Ctrl+V
  must reach that application and produce ordinary Linux clipboard behavior;
  terminal SIGINT semantics remain unchanged.
- Translate files already inside an approved live share directly to their
  `L:` paths. For paths outside configured roots, use an authenticated staging
  transfer with a visible progress bar, cancellation, resume, collision
  handling, and no implicit overwrite. Apply the same boundary to guest-to-host
  drag-and-drop and file-dialog results.

### Slice 7: UAC, elevation, and native sudo

- Preserve the Windows secure desktop for UAC. When an elevation prompt is
  active, freeze ordinary seamless input and open a trusted Linux modal that
  displays the real guest secure-desktop framebuffer through the private QEMU
  display channel. Forward input only while that modal has focus, clearly mark
  the instance and secure-desktop state, and return to per-window integration
  after consent. Never synthesize an approval dialog or approve elevation from
  a Linux notification; use the private full viewer as the recovery fallback.
- Handle an approved elevated application through a narrowly scoped elevated
  capture broker when available. If Windows integrity boundaries prevent safe
  capture or input, report the boundary and retain the trusted viewer fallback
  instead of weakening UIPI or secure-desktop policy.

### Slice 8: media integration and release hardening

- Add audio, notifications, launcher refresh, reconnect UX, diagnostics, and
  recovery from host sleep, guest restart, and Linux desktop-session restart.
- Measure idle companion memory, frame latency, input latency, live-share
  throughput, drag-and-drop resume, and audio stability. Finish the Wayland and
  X11 application matrix and the exact-commit Windows/KVM release gate.

Acceptance requires common development applications, administrator and standard
account flows, native-sudo detection, real UAC consent and credential prompts,
secure-desktop spoof boundaries, file dialogs, elevated window boundaries,
clipboard focus, large-file drag-and-drop, DPI transitions, full-screen recovery,
desktop-session restart testing on Wayland and X11, driverless live-share escape
tests, simple-command compatibility tests, and published file-performance
results. The existing synchronized mirror remains supported even after live
sharing becomes the recommended interactive path.

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
  without explicit user configuration. An interactive recommended-integration
  answer counts as consent only for the exact roots and modes displayed in that
  dialog.
- Claiming a platform based only on argument planning or emulation smoke tests.
