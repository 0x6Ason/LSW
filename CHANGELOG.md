# Changelog

## Unreleased

- Began beta.8 with explicit Windows desktop account roles and native Windows
  sudo integration. Interactive personal-development setup recommends an
  administrator while keeping standard accounts available. `lsw user add`
  creates a separately confirmed account without changing the desktop default,
  `lsw user promote|demote` reconciles native membership, and `lsw sudo
  status|enable|disable` detects the Windows 11 24H2 inbox feature and exposes
  only the reversible new-window configuration. The one-shot LocalSystem
  helper refuses managed policy, UAC remains enabled, and no third-party sudo
  package is installed.

## 1.0.0-beta.7

- Kept automatic guest-start and resume progress on stderr so scripted
  `lsw exec` stdout contains only the Windows process output.
- Made the first ConPTY session after a Windows cold boot tolerate the bounded
  `ERROR_PATH_NOT_FOUND` window while the console driver initializes. Other
  pseudoconsole errors still fail immediately.
- Added content-addressed sealed images and linked clones. Image identity covers
  the ISO, declarative profile/preparation contract, agent, firmware, and
  source disk. Sealed bases are read-only; clone overlays receive distinct
  tokens, ports, and a private boot-time identity volume. The SCM agent now
  reconciles identity media that Windows mounts after automatic services start,
  avoiding a boot-time token race without delaying ordinary boots. Discovery
  covers every volume GUID path even when Windows assigns no drive letter. The
  private identity disk uses the inbox IDE path instead of first-boot USB mass
  storage, remains discoverable across delayed mounting, and backs a bounded
  ten-minute host wait with live agent-start progress. Sealing now pre-registers
  that disk with the current credential before rotating it, covering Windows'
  first-boot device-registration delay without weakening token isolation. New
  installs attach the identity disk from their first Windows boot and disable
  Fast Startup while preserving explicit hibernation, so changed private media
  cannot be hidden by a cached hybrid-shutdown kernel.
- Added manifest v5 and the opt-in resource governor: minimum memory, QMP
  balloon control, host-pressure reclaim, running-to-pause-to-Windows-hibernate
  policy, automatic resume, guest TRIM, qcow2 discard/detect-zeroes, and safe
  stopped/hibernated compaction. `lswd` exits after 30 idle seconds when no VM
  is active, while optional systemd socket activation remains reversible via
  `lsw daemon enable|disable|status|diagnose`. Guest TRIM and the Windows
  hibernation transition use an authenticated, demand-start LocalSystem helper
  that accepts only empty fixed-operation request kinds; the network-facing
  agent remains a restricted virtual service account.
- Added declarative per-instance synchronized folders with explicit RO/RW
  roots, additive host watch, explicit RW guest-to-host merge, protected
  allow-list ACLs for RO views, reconnect retries, and host-symlink plus Windows
  reparse-point escape rejection. SYSTEM, Administrators, and the LSW service
  retain update access while built-in Users receive ReadAndExecute. No share is
  enabled implicitly, and deletions plus guest ACLs are preserved on removal.
- Added WSL-style permanent Windows-user registration after interactive
  installation and `lsw user setup` for deferred/recovery use. Passwords use
  masked input or stdin, travel only through the authenticated protocol, are
  redacted/zero-filled in mutable buffers, and reach Windows NetAPI only inside
  a demand-start authenticated LocalSystem helper; they never enter argv,
  environment, manifests, seeds, logs, or diagnostics.
  Accounts are standard by default, administrator membership is explicit, and
  AutoLogon remains disabled.
- Expanded the exact Windows/KVM release gate with native account creation,
  linked-clone secret isolation, share boundary escapes, balloon/TRIM,
  hibernate/resume, offline compaction, and the existing no-login cold restart.
  The release workflow now always deletes per-run VM state, records the exact
  temporary root for a second bounded cleanup pass, rejects stale gate roots,
  and requires 160 GiB free on both the E2E filesystem and the WSL host volume.
- Decoupled reliable WinPE target apply from CompactOS after the exact gate
  reproduced a CPU-bound DISM stall at 63 percent. WinPE still performs bounded
  offline AppX servicing and an integrity-checked image apply; the `slim`
  profile now enables CompactOS during the named Windows `applying-profile`
  stage, where setup can report progress and recover without leaving a partial
  target image.
- Added explicit Windows-license acceptance to new one-shot installations.
  Interactive terminals require `[y/N]` confirmation before media download or
  instance creation; noninteractive use requires `--accept-windows-license`.
  The older `--accept-license` spelling remains a compatibility alias. LSW's
  GPL-3.0-or-later notice stays separate and is not presented as another EULA.
- Added a dedicated experience-first roadmap covering optional low-memory
  systemd operation, desktop-user setup, host-folder sharing, Linux-native
  Windows GUI applications, clipboard and file drag-and-drop, full-screen and
  multi-monitor behavior, reversible shell-light optimization, and ARM64 only
  after those paths stabilize.

## 1.0.0-beta.6

- Added complete remote process semantics. `exec` and `run` accept one guest
  working directory and repeated, case-insensitively unique environment values;
  authenticated `SIGINT`/`SIGTERM` forwarding returns exact 130/143 status;
  ordinary Windows exit codes are preserved on the protocol, with explicit
  decimal/hex reporting when the Unix 0-255 process status cannot represent
  them. `run --detach` uses a capability-gated start acknowledgement and returns
  the guest PID without making a Session 0 GUI visible.
- Added recursive `push`/`pull` and additive host-to-guest `sync --watch`.
  Recursive traversal rejects host symlinks, guest reparse points, unsafe paths,
  and implicit overwrite. Watch sync atomically replaces changed files, creates
  new directories, retries failed updates, and deliberately preserves remote
  files after local deletion.
- Added explicit `lsw path -w/-u` drive-path conversion, repeatable dynamic
  `--publish auto:GUEST`/`0:GUEST` loopback allocation, and dependency-free
  completion generation for Bash, Zsh, Fish, and PowerShell.
- Added strict systemd user socket activation. `lswd` accepts exactly one
  PID-scoped, optionally named descriptor only when its private Unix path and
  permissions match the configured daemon socket. Release bundles include
  hardened service/socket units and an end-to-end activation smoke test, while
  direct CLI daemon autostart remains compatible.
- Added a single terminal progress model for official ISO transfer and SHA-256
  verification, WinPE preparation and application, and unattended Windows
  first boot. Interactive terminals receive an in-place progress bar for
  measurable work; redirected logs receive bounded ten-percent updates, while
  specialize and OOBE report named stages without invented percentages.
- Extended the private, network-disabled WinPE status volume with live DISM
  output and bounded percentage parsing. A real Windows/KVM benchmark rejected
  fast compression because the larger intermediate made total preparation
  slower, so maximum compression remains the default. Export now uses the
  private NTFS scratch directory explicitly; CompactOS and integrity checks are
  unchanged.
- Added an atomic guest setup-stage marker covering service configuration,
  profile application, agent startup, OOBE, cleanup, and completion. The host
  reads it only through the authenticated loopback agent channel. Pre-applied
  images also carry a bounded marker so first boot skips the duplicate online
  profile and CompactOS pass.
- Added safe `lsw install NAME` recovery when WinPE deployment completed but
  first boot or verification was interrupted. A stopped instance with no seed
  resumes normal Windows boot and authenticated setup verification without
  reattaching installation media.
- Expanded the real Windows/KVM exact-commit gate to cover cwd/environment,
  host signal status, detached completion, recursive round-trip transfer, and
  live additive workspace sync in addition to OOBE, SCM, ConPTY, activation,
  shutdown, and cold restart.

## 1.0.0-beta.5

- Added the one-shot `lsw install NAME` path. It resolves the current official
  Windows 11 ISO from Microsoft, prefers aria2c with a four-connection limit,
  falls back to a native four-range resumable downloader, refreshes expired
  signed URLs, verifies Microsoft's exact SHA-256, selects Pro, and retains
  `--iso` for offline media.
- Added the network-disabled `WinPeDismBackend`: it boots the official ISO's
  WinPE through an ephemeral no-prompt control ISO, uses real Windows DISM to
  export/mount/service a prepared WIM, stages the boot-time agent and answer
  file inside that WIM, then applies it to the instance qcow2 with UEFI boot
  files. Private status volumes retain bounded completion evidence; temporary
  control media, WIM workspace, and token-bearing seeds are removed after
  success.
- Kept DISM mount/export/commit integrity checks but deliberately avoided
  `/Mount-Image /Optimize`: repeated Windows 11 25H2 KVM installs reached
  `PROCESS1_INITIALIZATION_FAILED (0x6B)` on the optimized path, while the
  ordinary mount completed specialize and brought the SCM agent online. The
  pre-applied payload now has a tested six-file allowlist that includes the
  static activation helper, and the installer preserves the inherited Program
  Files ACL instead of replacing it during specialize.
- Added `lsw doctor --fix`, `config get/set`, `logs`, `view`, redacted
  `diagnose --bundle`, safe `remove`, `shutdown --all`, and `bench --json`.
  Manifest v4 persists `idle-timeout`; `memory.max` takes effect on the next VM
  start. Automatic idle hibernation remains beta.7 work.
- Added Windows media metadata inspection through xorriso, a UDF-capable `7z`
  fallback, and wimlib; friendly edition aliases such as `pro`; UTF-8/UTF-16
  WIM XML handling; and answer-file XML escaping. Numeric image indexes remain
  an advanced compatibility option.
- Replaced the old public profile names with versioned declarative `vanilla`
  and default `slim` manifests. The schema permits only bounded AppX selectors
  and CompactOS, and enforces preservation of servicing, Defender, Store,
  winget, WebView2, Terminal/PowerShell/ConPTY, WMI, hibernation and Recovery.
  Old `standard` manifests migrate to `vanilla`.
- Replaced the interactive user's `HKCU` agent startup entry with the automatic
  `LSWAgent` Windows service, running under the virtual account
  `NT SERVICE\LSWAgent`. Agent commands intentionally execute in that service
  identity and do not require a stored user password or automatic logon. The
  pre-applied flow installs it during `specialize`. Supported unattended OOBE
  settings use a per-install random one-shot account without AutoLogon;
  SetupComplete removes that account, cached answer files, its script, and the
  staging payload before `lsw install` reports completion. Installation is
  headless by default, with `--viewer` as an explicit diagnostic option.
- Moved the guest agent and activation-helper listeners to ports 35040/35041
  after clean Windows 11 testing found TCP 5040 occupied by the Connected
  Devices Platform service. SCM sessions now restore blocking mode on accepted
  sockets, completed commands close both duplicated socket directions without
  waiting for the 30-second heartbeat, and bounded service errors are retained
  in the ACL-protected `C:\ProgramData\LSW\agent.log`.
- Added `lsw license status/activate/open`. Product keys use masked input or
  stdin and never argv/environment/seed/base/log/diagnostic storage. A separate
  authenticated, guest-loopback, demand-start LocalSystem helper performs only
  WMI `InstallProductKey`/`Activate`; the main agent remains narrow. The helper
  runs a fixed, size-bounded script installed with the agent; the requested
  action is its only argument and a product key remains stdin-only.
- Detached daemon startup now enters a new session with `setsid`, so a one-shot
  `wsl.exe lsw start` cannot hang up `lswd` and its VM when that invocation
  exits. Graceful shutdown records intent before QEMU/helper cleanup, ensuring
  the supervisor classifies the resulting exit as `stopped` and removes only
  live runtime PID/socket/viewer markers.
- Added cleanup of QEMU pid/viewer artifacts and stale runtime sockets after a
  stopped guest, plus a guarded real Windows/KVM operator workflow covering
  headless Setup/OOBE cleanup, Windows build/edition identity, the
  automatic service's configuration and stable process SID, true ConPTY, guest
  exit codes, graceful shutdown, bare-`lsw` cold restart without install media
  or an interactive login, and exact daemon/QEMU/socket/port cleanup.
  The gate also requires Microsoft's current published ISO hash, both WinPE
  completion markers, transient cleanup, WMI license status and the activation
  helper boundary. New tagged releases fail closed unless that job passed for
  the exact commit; beta.1–beta.4 remain grandfathered and untouched.
- Removed the Windows-native process-tree test's PowerShell startup timing race;
  it now observes descendant readiness through kernel Job Object membership.
- Rewrote the README completely in English and documented beta.5 commands,
  measurable performance targets, roadmap order, legal boundaries, and honest
  hardware-validation limits.
- Release bundles now include the complete locked Rust dependency source and
  license files under `source/vendor`, use that tree for offline Cargo builds,
  and cover it with the corresponding-source SHA-256 manifest.
- Split the CLI and Windows agent entry points into focused argument,
  installation, licensing, process-tree, ConPTY, SCM, and test modules. Public
  ISO/WinPE APIs deny missing documentation, while platform FFI modules deny
  undocumented unsafe blocks so lifecycle and safety invariants stay explicit.

## 1.0.0-beta.4

- Added per-session process ownership. Unix children enter a new process group
  before `exec`, and LSW cleans up every process that remains in that group on
  normal leader exit, cancellation, disconnect, protocol failure, or lease
  expiry. Windows children start suspended and fail closed unless they can be
  assigned to a kill-on-close Job Object before resuming. The Windows-native CI
  gate exercises Job descendant cleanup and ConPTY setup; this Linux VPS ran
  only the Windows GNU cross-build. A Unix child can deliberately escape its
  group with `setsid`/`setpgid`, so this is lifecycle ownership rather than a
  security sandbox.
- Added the capability-gated `session-lease-v1` extension for controlled
  sessions. Leases are strictly bounded to 1–300 seconds; the standard client
  requests 120 seconds and sends a heartbeat every 30 seconds. Expiry closes
  the transport and reclaims the session's owned processes, while peers that do
  not advertise both control and lease capabilities retain the beta.3 behavior.
- Added a non-skippable CI product-lifecycle QEMU gate that uses the real `lsw`
  manifest/preparation/planner path and `lswd` to exercise OVMF, NVMe, e1000e,
  vTPM, loopback agent/published ports, install, start, status, suspend, resume,
  and forced stop. The lower-level TCG/OVMF smoke again passed on the Codex VPS;
  its sandbox rejects pathname Unix sockets, so the product gate runs only in
  CI. No Windows ISO was available, and Windows Setup, logged-in agent, and
  ConPTY guest E2E remain unverified.
- Strengthened the release reproducibility gate to compare two complete builds
  from separate clean Cargo target directories. Windows agent builds now
  validate the MZ/PE headers and normalize the COFF TimeDateStamp to zero; the
  bundle verifier enforces that invariant.

## 1.0.0-beta.3

- Added the capability-gated `session-control-v1` process-session extension
  without changing the version-one agent handshake. Explicit stdin close now
  delivers EOF without cancellation; authenticated cancellation terminates the
  direct child and reports exit code 130; opted-in disconnect cleanup releases
  the direct-child session. Older agents retain their half-close behavior.
- Added loopback protocol coverage for controlled EOF, cancellation,
  disconnect cleanup, malformed control frames, authentication, and legacy
  compatibility. Process-tree cleanup and bounded half-open-peer detection
  remain follow-up work.
- Added a timeout-bounded headless QEMU CI smoke gate covering `qemu-img`, TCG,
  OVMF, swtpm/vTPM traffic, QMP `stop`/`cont`/`quit`, and loopback-only usernet
  forwarding. The same firmware-level path was run with Ubuntu's QEMU 8.2.2
  packages on the Codex VPS; KVM and Windows guest E2E remain separate gates.

## 1.0.0-beta.2

- Added manifest v3 and repeatable `lsw create --publish HOST:GUEST` mappings.
  Published TCP ports are restricted to host loopback, require NAT, and are
  checked against duplicates, the per-instance agent port, other instances,
  and ports already bound by another local process.
- Added capability-negotiated Windows ConPTY shell sessions, raw host-terminal
  bridging, and terminal-resize frames while retaining pipe sessions as the
  compatibility fallback. Real installed-Windows ConPTY E2E remains a beta gate.
- Added in-memory `lsw suspend` and `lsw resume` through QMP `stop`/`cont`, with
  manifest/QMP state reconciliation. This is not save-to-disk hibernation.
- Added `lsw inspect` for bounded PE/COFF parsing, import inspection, JSON output,
  and a conservative Windows 11 x64 beta compatibility assessment.
- Separated host platform and QEMU accelerator selection. Linux KVM detection is
  implemented; HVF and WHPX selection/argument generation are covered by planner
  tests but are not yet supported host runtimes.
- Expanded CI and release gates around the Rust 1.76 MSRV, Linux source checks,
  Windows GNU cross-build/PE inspection, Windows MSVC load smoke testing, shell
  checks, reproducible packaging, and release-bundle verification.
- Hardened beta boundaries by reserving loopback ports during instance creation,
  refusing unprovable external force-stops, budgeting aggregate PE imports, and
  verifying a complete corresponding-source manifest in every release bundle.

## 1.0.0-beta.1

- Added the `lsw` Unix-host CLI with default-instance shell, `exec`, `run`,
  status, stop, push, and pull commands.
- Added `lswd`, a private Unix-socket daemon that supervises QEMU and swtpm,
  reconciles state through QMP, and handles graceful or forced shutdown.
- Added a dependency-free, authenticated Windows x64 guest agent with concurrent
  process sessions and binary-safe file transfer.
- Added guided and explicitly destructive unattended Windows Setup seeds. The
  generated answer file contains no product key, activation bypass, OOBE bypass,
  or preactivated image.
- Added standard, slim, ephemeral-overlay, and Secure Boot profiles. All beta
  profiles preserve Windows servicing components.
- Added KVM/TCG planning, private OVMF variable stores, vTPM, inbox Windows NVMe,
  e1000e and VGA devices, loopback-only agent forwarding, and Unix-socket VNC for
  installation or recovery.
- Added a reproducible Zig-backed Windows agent cross-build and Linux x86_64
  release-bundle scripts.
- Licensed LSW under `GPL-3.0-or-later`; binary bundles include the exact
  corresponding source and build scripts.

Known limitations are tracked in `docs/BETA.md`.
