# Changelog

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
  pre-applied flow installs it during `specialize`, before interactive login.
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
  Setup, OOBE user credential policy, Windows build/edition identity, the
  automatic service's configuration and stable process SID, true ConPTY, guest
  exit codes, graceful shutdown, bare-`lsw` cold restart without install media
  or an interactive login, and exact daemon/viewer/QEMU/socket/port cleanup.
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
