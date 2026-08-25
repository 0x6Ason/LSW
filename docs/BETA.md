# LSW 1.0 beta status

`1.0.0-beta.7` is a Linux x86_64 engineering beta, not a claim that every
target platform has passed general-availability hardware validation. It joins
Microsoft ISO resolution and download, WinPE DISM pre-application, the instance
lifecycle, the Windows agent, and a lawful activation boundary into one path.

## Completed and covered by source or CI gates

- Manifest v5, v1/v2/v3/v4 migration, default-instance selection, private state
  permissions, 256-bit tokens, and loopback TCP port validation.
- `lsw install NAME` resolves the Microsoft media session, accepts only
  allowlisted Microsoft HTTPS CDNs, downloads with aria2c or a native four-range
  resumable engine, refreshes expired signed URLs, and verifies Microsoft's
  exact SHA-256. Offline `--iso` mode remains available.
- New interactive installs require `[y/N]` Windows-license confirmation before
  download or instance creation. Redirected/noninteractive use fails closed
  unless `--accept-windows-license` is present; `--accept-license` remains a
  compatibility alias.
- The network-disabled `WinPeDismBackend` uses real DISM from the official ISO
  to prepare a profile WIM, then applies it to the instance qcow2 in a separate
  phase. The phases use distinct disk topologies, OVMF variables, and exact
  private status-volume completion markers. Workspace disks and token-bearing
  seeds are removed after success.
- Versioned declarative `vanilla` and default `slim` profiles. `slim` removes
  only an explicit AppX allowlist and preserves Windows Update, WinSxS,
  Defender, Store, winget, WebView2, Terminal, PowerShell, ConPTY, WMI,
  hibernation, and Recovery.
- The pre-applied unattend installs the Windows x64 PE agent during
  `specialize` and registers the automatic `LSWAgent` service as
  `NT SERVICE\LSWAgent`, rather than using an interactive user's `HKCU` startup
  entry.
- `lsw license status/activate/open`. Product keys travel only through masked
  input or stdin and the authenticated guest loopback. They never enter argv,
  the environment, seeds, base images, logs, or diagnostic bundles. The
  demand-start LocalSystem `LSWLicenseHelper` handles one WMI licensing request
  and then exits.
- QEMU backend selection, Linux KVM detection, and KVM/TCG/HVF/WHPX command
  planning.
- OVMF, vTPM, NVMe, e1000e, VGA, private VNC sockets, and NAT loopback host
  forwarding.
- The `lswd` protocol, QMP negotiation and status, in-memory suspend/resume,
  powerdown/quit, and child/helper supervision.
- Binary protocols for `lsw shell`, `exec`, `run`, `push`, and `pull`.
  Capability-gated `session-control-v1` adds explicit stdin EOF,
  cancel/disconnect cleanup, and legacy fallback. Capability-gated
  `session-lease-v1` provides 1-300 second leases, with a standard 120-second
  lease and 30-second heartbeat.
- Capability-gated process environment, detached-start acknowledgement, and
  interrupt/terminate frames. The client exposes `--cwd`, repeated `--env`, and
  `run --detach`; Windows process groups remain owned by a kill-on-close Job.
  Exact Windows 32-bit exit values stay intact on the wire, while the Unix CLI
  explains values that cannot fit its 0-255 process status.
- Recursive upload/download and additive `sync --watch`, with host-symlink,
  guest-reparse-point, traversal, overwrite, and bounded-output checks. New and
  changed files use guest-side atomic replacement; host deletions never imply a
  destructive guest deletion.
- Explicit drive-path conversion, dynamic host-loopback port allocation, and
  Bash/Zsh/Fish/PowerShell completion generation.
- Strict systemd-compatible user socket activation and packaged hardened unit
  files, covered with a real `systemd-socket-activate` process/socket smoke.
- Opt-in `lsw daemon enable|disable|status|diagnose`, 30-second idle daemon
  exit, reported RSS, and a source gate enforcing the below-30-MiB idle target.
- Content-addressed sealed qcow2 bases and linked clones with exact ISO,
  profile/preparation, agent, firmware, and disk identity; read-only bases;
  fresh clone tokens/ports; and boot-time credential rotation from a private
  identity volume. The running SCM agent reconciles removable identity media
  that Windows mounts after automatic services start.
- QMP balloon targets, host-pressure reclaim, opt-in pause/hibernate policy,
  Windows hibernate/resume, guest TRIM, qcow2 discard/detect-zeroes, and offline
  compaction. Guest TRIM and Windows hibernation cross a token-authenticated,
  fixed-operation, demand-start LocalSystem helper instead of elevating the
  normal agent.
  Stopped and hibernated instances retain no QEMU process.
- Declarative RO/RW folder synchronization, additive change watch, explicit RW
  guest merge, guest ACL enforcement, and host-symlink/guest-reparse boundary
  rejection. Shares and background watch are never enabled implicitly.
- Explicit driverless live-folder consent for one canonical host root, private
  QEMU user-network SMB, fixed agent-session `Linux (L:)` mapping, short
  `share`/`unshare`/`cp` commands, and machine-readable file benchmarks.
- Interactive post-install permanent-user registration and deferred
  `lsw user setup`. A demand-start authenticated LocalSystem helper calls native
  NetAPI once and exits; the normal agent remains an unprivileged virtual
  service account. Standard accounts are the default, administrator membership
  is explicit, AutoLogon remains disabled, and passwords never enter argv,
  environment, LSW manifests, seeds, logs, or diagnostics.
  `lsw user add` can create a separate confirmed administrator without changing
  the default desktop identity.
- Capability-gated Windows 11 native-sudo status and reversible
  disabled/new-window configuration. Managed policy and UAC remain untouched;
  no third-party replacement or less-safe console mode is offered.
- Unix children enter a dedicated process group before `exec`; remaining group
  members are cleaned up after normal leader exit, cancellation, disconnect,
  protocol failure, or lease expiry. Windows children must enter a
  kill-on-close Job Object before resume and fail closed if creation or
  assignment fails.
- ConPTY capability negotiation, console I/O bridging, TTY restoration, and
  resize protocol/unit tests.
- A bounded PE parser for `lsw inspect`, including imports, JSON output, and x64
  guest compatibility guidance.
- Real loopback agent E2E coverage for stdout/stderr, guest exit codes, Unicode
  filenames, and byte-exact binary transfer.
- Concurrent E2E coverage proving a second session completes while a long
  command is still running.
- Controlled-session loopback E2E for normal stdin EOF, authenticated cancel
  returning 130, disconnect/lease cleanup of owned processes, malformed-frame
  and authentication rejection, heartbeat liveness, and legacy half-close.
- On a Codex VPS without KVM or Windows media, the project passed QEMU 8.2.2
  TCG with OVMF, `qemu-img`, swtpm/vTPM traffic, TCP QMP
  `stop`/`cont`/`quit`, and two `127.0.0.1` usernet forwards to guest ports 35040
  and 8080. Both forwards were released after quit, and QEMU and swtpm exited
  successfully.
- CI has a non-skippable product-lifecycle gate using the real LSW manifest,
  preparation, `QemuPlanner`, and `lswd` paths. It verifies OVMF, NVMe, e1000e,
  vTPM, both loopback forwards, and install/start/status/suspend/resume/forced
  stop with placeholder media.
- Rust 1.76 Linux builds, Windows GNU PE32+ cross-builds, unit tests, rustfmt,
  Clippy, shell checks, and release-bundle verification. CI also has Windows
  MSVC native agent tests and executable-load checks, plus bounded QEMU
  firmware and product-lifecycle gates.

In addition to the source and ordinary CI gates above, the tagged beta.6 commit
`091089aca074b394e0c1934f3ec01f5fdfb7ef62` passed the dedicated
[Windows/KVM release gate](https://github.com/0x6Ason/lsw/actions/runs/32651006829)
before publication. That result is exact-commit evidence from one documented
Linux x86_64/KVM host, not a claim about untested hardware or host platforms.
Any beta.7 tag must point to an exact commit that passed the expanded gate.

## Real-host release gate and remaining matrix

Ordinary source and GitHub-hosted runners do not provide a real Windows 11/KVM
environment. Every new tagged release must pass the guarded workflow on a
dedicated, isolated Linux x86_64 self-hosted runner. The beta.6 run covered:

- Microsoft's current published English x64 SHA must exactly match the
  operator-provisioned read-only ISO.
- Real DISM execution for the network-disabled WinPE prepare/apply phases,
  completion markers, and transient workspace/seed cleanup.
- KVM cold boot, unattended Windows 11 OOBE, removal of the one-shot setup
  account and cached answer/staging files, absence of a console login or
  automatic-logon credential, and the automatic `LSWAgent` service's Name,
  StartMode, State, StartName, and `S-1-5-80-...` process SID. After a complete
  shutdown, bare `lsw` must restore an agent-backed ConPTY shell with the same
  service SID, no interactive console user, and no attached ISO or seed.
- WMI license status; the `LSWLicenseHelper` Manual/LocalSystem configuration,
  authenticated start permission, and return to Stopped after each request.
  The release gate never enters a product key.
- True ConPTY; working-directory and environment injection; host signal and
  exit-code propagation; detached execution; recursive round-trip transfer;
  additive watch sync; graceful shutdown; and complete runtime cleanup.

The broader hardware and soak matrix still includes:

- OVMF path differences across Linux distributions.
- The Windows firewall rule and QEMU slirp `10.0.2.2` source match.
- Data transfer from `--publish` to a real guest TCP service and sustained use
  across multiple instances. The lower-level loopback listener is already
  covered.
- Graceful/forced control of a real Windows workload and long-running vTPM
  behavior. QMP `stop`/`cont`/`quit` is covered against real QEMU, and the CI
  lifecycle gate checks `lswd` through a filesystem QMP socket.
- Extended ConPTY Unicode, resize, disconnect, and long interactive use.
- Optional private Unix-socket VNC viewer compatibility.
- Executables, paths, firmware, daemon IPC, and complete lifecycle behavior on
  macOS HVF and Windows WHPX hosts.

## Known beta limitations

- The deliverable host runtime supports Linux x86_64 only. The backend can
  select HVF/WHPX and generate accelerator arguments, but Windows and macOS host
  integration is not implemented or validated.
- ConPTY is capability-negotiated, with pipe fallback for older or unsupported
  agents. The beta.6 exact-commit gate covered a real console and signal path;
  extended Unicode, resize, disconnect, and long-session soaks remain.
- `session-control-v1` distinguishes stdin EOF, authenticated cancellation, and
  opted-in disconnect cleanup. `session-lease-v1` bounds recovery of half-open
  peers and cleanup covers more than the leader. Unix ownership can reclaim
  only ordinary descendants that remain in the process group; a guest process
  can escape with `setsid`/`setpgid`, and a very small numeric PGID reuse race
  remains after normal leader reaping. Windows Job Objects enforce ownership,
  but a host nested-job policy that rejects assignment makes the session fail
  closed. Neither mechanism is a guest security sandbox.
- `sync` and share watch are intentionally host-to-guest and additive, not a
  bidirectional conflict resolver. RW share import is an explicit
  `--from-guest` operation. Neither side's deletion is propagated, and tree
  transfer does not follow host symlinks or guest reparse points.
- `run --detach` discards process standard streams and keeps the process owned
  by the agent's Job. It does not turn a Session 0 process into a visible desktop
  application.
- There is no per-HWND Wayland/X11 compositor bridge, clipboard, audio, GPU
  acceleration, or shared-memory graphics driver. `lsw run` starts a guest
  process but does not create a Linux-native application window.
- Installation and recovery use private Unix-socket VNC. There is no RDP or TCP
  VNC listener.
- Agent commands and ConPTY sessions run in Windows Session 0 as
  `NT SERVICE\LSWAgent`; they do not impersonate a desktop user. This provides a
  CLI without login, but a service-launched GUI does not appear on the user's
  desktop. A user-session companion is not implemented.
- Suspend/resume applies QMP `stop`/`cont`; hibernate uses Windows' hiberfile
  and powers QEMU off. There is no cross-host live migration, kernel Virtio-fs
  mount, USB passthrough, or portable image export. The live folder uses
  driverless SMB and is limited to one approved read-write root.
- The balloon device is present, but useful Windows reclaim requires a
  compatible signed VirtIO driver. LSW does not bundle unsigned/test-signed
  drivers and retains the inbox NVMe/e1000e recovery path.
- Agent authentication is not encrypted and is limited to the designed local
  loopback/QEMU user-network path.
- `--publish` creates only a `127.0.0.1` TCP listener, but the guest service is
  still untrusted and must not be forwarded to a LAN or the Internet by another
  tool.
- QEMU does not yet have an LSW-specific seccomp, namespace, or service-account
  sandbox.
- LSW is `GPL-3.0-or-later`; binary bundles include the exact corresponding
  source snapshot.

## Recommended hardware acceptance order

1. On a dedicated Linux x86_64 KVM host, run `lsw doctor` and provision a
   read-only Windows 11 ISO that exactly matches Microsoft's current English
   x64 published SHA.
2. Run `lsw install NAME --iso PATH --edition pro --profile slim
   --accept-windows-license`; verify both WinPE completion markers and removal
   of the workspace and seed before normal boot.
3. Let the headless install complete without console input. Confirm the
   one-shot `LSWSetup` account, cached unattend, SetupComplete script, and setup
   payload are gone, with no console user or automatic login. Verify `LSWAgent`
   is Auto/Running with StartName `NT SERVICE\LSWAgent`. Under the service identity,
   test ConPTY Unicode, Ctrl, resize, stdin EOF, cancellation, disconnect and
   lease expiry; cwd/environment injection; signal and exit-code propagation;
   detached completion; descendant cleanup; recursive transfer; watch sync;
   1 GiB transfer; and concurrent commands. After full shutdown, use bare `lsw` to
   verify cold-boot recovery with no interactive console user, no ISO/seed, and
   an unchanged service SID.
4. Run `lsw license status`; verify the helper is Manual/LocalSystem and returns
   to Stopped. In a non-release disposable guest, test masked-key and stdin
   activation and confirm logs and diagnostics contain no key.
5. Publish a disposable guest TCP service, confirm the listener is restricted
   to `127.0.0.1`, and test port collisions.
6. Test suspend/resume, graceful stop, guest crash, daemon restart, and stale
   socket recovery.
7. Use disposable instances to test offline `--iso`, `vanilla`, and the
   advanced `--unattended-index` compatibility path.

Promote this beta to GA only after completing that hardware matrix.
