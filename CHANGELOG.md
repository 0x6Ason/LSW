# Changelog

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
