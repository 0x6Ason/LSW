# Changelog

## 1.0.0-beta.5

- Added the one-shot `lsw install NAME --iso PATH --edition NAME` path. It
  checks host dependencies, reads the actual Windows edition names from the
  ISO's install WIM/ESD, creates and prepares the instance, selects the image by
  `/IMAGE/NAME`, creates the agent seed, starts Setup, and opens an integrated
  private-socket installation viewer.
- Added `lsw doctor --fix`, `config get/set`, `logs`, `view`, redacted
  `diagnose --bundle`, safe `remove`, `shutdown --all`, and `bench --json`.
  Manifest v4 persists `idle-timeout`; `memory.max` takes effect on the next VM
  start. Automatic idle hibernation remains beta.7 work.
- Added Windows media metadata inspection through xorriso and wimlib, friendly
  edition aliases such as `pro`, UTF-8/UTF-16 WIM XML handling, and answer-file
  XML escaping. Numeric image indexes remain an advanced compatibility option.
- Added cleanup of QEMU pid/viewer artifacts and stale runtime sockets after a
  stopped guest, plus a guarded real Windows/KVM operator workflow covering
  Setup, OOBE, Windows build/edition identity, agent readiness, true ConPTY,
  guest exit codes, graceful shutdown, bare-`lsw` cold restart without install
  media, and exact daemon/viewer/QEMU/socket/port cleanup. New tagged releases
  fail closed unless that job passed for the exact commit; beta.1–beta.4 remain
  grandfathered and untouched.
- Removed the Windows-native process-tree test's PowerShell startup timing race;
  it now observes descendant readiness through kernel Job Object membership.
- Rewrote the README completely in English and documented beta.5 commands,
  measurable performance targets, roadmap order, legal boundaries, and honest
  hardware-validation limits.

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
