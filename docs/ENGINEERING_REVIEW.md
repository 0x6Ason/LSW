# Engineering review and improvement plan

This document records a whole-repository engineering review of the beta.8
development line: what was found, what was fixed on the review branch, and
what remains, in priority order. It is a working plan, not a release claim.
Line numbers refer to the tree at the time of the review and will drift.

## Summary

The code base is not sloppy in the usual machine-generated ways: there is no
`unwrap()` in production code, every `unsafe` block carries a `SAFETY:`
comment, dependencies are exact-pinned, and there are more than 330 tests.
The real problems are structural:

- a release-blocking defect in the end-to-end gate scripts that CI could not
  see;
- a god crate (`lsw-core`) that the Windows guest agent compiles most of;
- modules that are file splits sharing one namespace through `use super::*`
  rather than modules with interfaces;
- copy-pasted Windows service shells and helper functions;
- string-typed errors and a line-oriented daemon protocol that callers
  re-parse;
- CI that rebuilt everything from scratch in every job with no caching, no
  cancellation, and no timeouts;
- a reproducibility story that only holds on one machine.

## Fixed on master

| Area | Change |
| --- | --- |
| Release gate | `scripts/e2e/*.sh` began with `//`, so sourcing them failed with status 126 and both Windows gates were dead. Fixed; `check-shell.sh` now lints the libraries directly and rejects a non-comment header. |
| VM performance | KVM guests get Hyper-V enlightenments, the invariant TSC, a one-socket core topology, and RTC slewing (`QemuPlanner`, `GuestCpuTuning`, `LSW_GUEST_CPU_TUNING`). |
| Daemon correctness | QMP replies are parsed as JSON with `serde_json` instead of substring scans that could mistake an event for a response. |
| Duplication | One `windows_scm` module and one generic helper-service shell replace four copies; shared `fs_checks`, hex, unattend, QEMU argument, path-component, and state-root helpers replace their copies. |
| Dependencies | `rust-toolchain.toml`, `[workspace.dependencies]`, unused `ahash` removed, `deny.toml` added; rustls/rustls-webpki/webpki-roots upgraded to patched releases within the MSRV. |
| CI | Caching, cancellation, timeouts, one Rust build shared by tests and Clippy, Windows-target Clippy on Linux, binaries reused by the QEMU smoke, dev-profile-only Windows job, pinned `ubuntu-24.04` packaging, `cargo deny` job, advisory stable lane, Dependabot. `verify-release.sh` reads vendored versions from the lockfile. |
| Crate boundaries | The guest agent depends only on `lsw-protocol` (plus `windows` and `xml-rs`); the host-guest contract lives in `lsw_protocol::contract`; the 70-symbol re-export shim is gone and every crate imports the protocol directly. |
| Reproducibility | Releases are built from the staged, vendored source snapshot with the root remapped to `/lsw`; the check rebuilds from the shipped `source/` and the archives match byte for byte; `BUILDINFO.txt` records commit and toolchain. |
| Daemon protocol | Typed `DaemonRequest`/`DaemonResponse` over bounded JSON lines (protocol 3) with a typed `DaemonError` in `lsw-host`; the CLI text and the scripts that grep it are unchanged. |

## Handover: what still needs real hardware

Everything above was validated on Linux without KVM (unit tests, Clippy on
both targets, the TCG lifecycle gate, the release rebuild) and by GitHub CI.
Three things can only be proven on a machine with `/dev/kvm` and, for the
last, a real `systemd --user` session:

1. **Hyper-V enlightenments on real KVM.** Start any installed instance and
   confirm QEMU accepts the model:

   ```sh
   lsw plan win-dev run | grep -- '-cpu '          # shows host,migratable=off,hv-...
   lsw start win-dev && lsw status win-dev          # STATE=running QMP=running
   tail -n 20 ~/.local/share/lsw/instances/win-dev/qemu.log
   ```

   If `qemu.log` reports an unsupported `hv-*` property, run the daemon with
   `LSW_GUEST_CPU_TUNING=baseline`, note the kernel and QEMU versions, and
   open an issue so the default set can be narrowed. Inside the guest,
   `Get-ComputerInfo | Select HyperV*` and Task Manager's idle CPU are the
   quick checks; the `lsw bench` targets in the README are the real ones.

2. **The Windows/KVM gate after the `//` header fix.** Dispatch
   `windows-kvm-e2e.yml` from master with the exact SHA. Its first useful
   signal is simply that `check-windows-kvm-e2e.sh` gets past sourcing the
   `scripts/e2e` libraries; before the fix it exited 126 there. Run the
   native Wayland variant as well before the next tag.

3. **The systemd units.** On a desktop session, `lsw daemon enable`, then
   exercise `lsw push /tmp/some-file`, `lsw share add ... --read-write` for
   a directory outside `~/.local/share/lsw`, and `systemctl --user stop
   lswd` while a guest is running. `PrivateTmp`, `ProtectHome=read-only`,
   and the 15-second stop timeout in `contrib/systemd/lswd.service` are the
   directives under suspicion; relax whichever breaks and say so in the
   unit.

## Remaining work, by priority

### P1: structure

1. **Make the guest agent depend on `lsw-protocol`, not `lsw-core`.** The
   agent uses about thirty identifiers from `lsw-core`, all protocol types plus
   `derive_scoped_credential` and `validate_windows_user_name`, yet links
   `serde`, `sha2`, the PE parser, the DISM orchestrator, and the QEMU planner.
   Move `scoped_credential` and the user-name validator into `lsw-protocol`
   (or a tiny `lsw-guest-common`), delete the ~70-symbol compatibility
   re-export block at the bottom of `crates/lsw-core/src/lib.rs`, and have
   every crate import `lsw_protocol::` directly. Then split `lsw-core` into
   `lsw-manifest`, `lsw-qemu`, and `lsw-install`.
2. **Replace `use super::*` with explicit interfaces.** Thirty-six non-test
   glob imports (the agent's `main.rs` glob-imports six sibling modules; the
   CLI, daemon, `lswg`, and the ISO downloader do the same) mean no module has
   a stable boundary. Give each module an explicit `use` list and a
   `pub(crate)` surface; the daemon's twenty-two `pub` items and the CLI's ten
   should become `pub(crate)`.
3. **Typed errors.** Outside `lsw-core` every binary returns
   `Box<dyn Error>` from ~530 `Err("...".into())` sites, and eighteen
   functions return `Result<_, String>`. The daemon protocol is typed now
   (`lsw-core::daemon_protocol`, `lsw-host::DaemonError`); the agent, CLI,
   and `lswg` still need per-crate error enums (`thiserror` is acceptable;
   the hand-written `LswError` style also works).
4. **Break up the giant functions.** `install_seed/guest_setup.rs` (363
   lines), `lswg/presenter/mod.rs:53` (304), `winpe_dism/vm.rs:18` (263),
   `presenter/wslg.rs:67` (205), `lsw-agent/requests.rs:5` (171).
   `QemuPlanner::plan` is done: `LaunchArguments` adds one device group per
   method and `qemu/tests.rs` asserts on the argument vector; use it as the
   pattern for the others.
5. **Use the `windows` crate instead of hand-typed FFI.** The agent pulls
   `windows` 0.54 with 29 features and still declares its own
   `extern "system"` bindings and constants in twelve modules (registry, SCM,
   netapi, ConPTY, desktop). Enable the missing features and delete the extern
   blocks; `windows_scm` is the first step and shows the shape.

### P1: scripts and gates

6. **The e2e "libraries" are not libraries.** `windows-kvm-install.sh` and
   `windows-kvm-runtime.sh` are each one function whose body is top-level
   script reading ~40 driver globals, silenced by a blanket
   `# shellcheck disable=SC2154`. Turn the globals into parameters or an
   explicit `e2e_require_vars` check, split the bodies into functions, and
   drop the blanket disable.
7. **Move the seventeen embedded PowerShell programs (400 to 3,300
   characters each) into `.ps1` files** pushed to the guest, as
   `check-windows-slim-profile.ps1` already is. They interpolate shell
   variables into PowerShell source and cannot be read, diffed, or linted.
8. **Fix the native-gate time budget.** `windows-kvm-e2e.yml` allows 360
   minutes, but the KVM stage, GUI stage, and the unbounded
   `check-windows-profile-comparison.sh` add up to more than six hours before
   cleanup. Wrap the comparison in `timeout` like its siblings and size the
   job limit from the sum.
9. **Make reproducibility meaningful.** `check-release-reproducibility.sh`
   builds twice on one machine with the same `CARGO_HOME` and workspace path,
   so absolute paths baked into panic locations never differ. Add
   `--remap-path-prefix` for `CARGO_HOME` and the workspace in
   `build-release.sh` and `build-windows-agent.sh`, make the second pass build
   from the first pass's bundled `source/` tree, take the source snapshot from
   `git` rather than the working tree, fail on a dirty tree when
   `LSW_EXPECT_VERSION` is set, and record the commit, `rustc`, and MinGW
   versions in `BUILDINFO.txt`. Reusing the first pass as the shipped bundle
   also removes one of the three full release builds from `release.yml`.
10. **Test or relax the systemd unit hardening.** `lswd.service` sets
    `PrivateTmp=yes` and `ProtectHome=read-only`, which the socket-activation
    check never exercises. Under real activation the daemon and its QEMU
    children see a different `/tmp` from the CLI, and read-write shares
    outside the state directory are refused. Either test the unit under a
    real `systemd --user` session or drop those directives and say so.

### P2: hygiene

11. A shared `scripts/lib/common.sh` for the fourteen copies of the
    required-command preamble, thirteen copies of `workspace_root`, eight
    process-group terminators, five `timeout` re-exec blocks, and `fail`/`pass`.
12. Move daemon start/keepalive/stop, stopped-runtime assertions, attestation
    writing, and e2e root cleanup from shell into `lsw` subcommands; move GUI
    interaction polling into Python, as `qemu-smoke.py` already demonstrates.
13. Move the release gate check (`release.yml`, ~115 lines of inline bash with
    string-coupled job and step names) and the ~450 lines of inline bash in
    `windows-kvm-e2e.yml` into `scripts/ci/` so `check-shell.sh` covers them;
    add `actionlint`, `ruff`/`pyflakes`, and PSScriptAnalyzer.
14. Ratchet: exempt test files from the 1,000-line cap, add a per-`run:` block
    limit for workflows, and stop treating the cap as a design target (five
    Rust files sit between 911 and 983 lines).
15. `verify-release.sh` executes scripts and binaries from the archive under
    test; that is acceptable in CI but wrong for a verifier a user runs on a
    download. Use the repository copies when available.
16. `--help` for every shell script, one usage exit code, and documented exit
    codes for `check-wslg-transport.sh`.
17. Replace the hand-rolled JSON emitters in the CLI (`lifecycle.rs`,
    `file_bench.rs`, `reporting.rs`) with `serde_json`, which the workspace
    already compiles.
18. Tests: drop the `thread::sleep` timing fudges in the agent tests by
    injecting a clock or using channels; delete tests that assert a constant
    equals itself; the `inbox_capability_probe_and_registry_reads_are_non_mutating`
    test reads the host registry and passes or fails by runner image.
19. MSRV: `rust-version = "1.76"` holds today, but it already blocks the idna
    fix (`litemap` wants 1.81) and the winit 0.30 stack. Plan the bump with
    the next beta so the advisory ignores in `deny.toml` can be removed.

## Faster VM: what WSL2 does that LSW can and cannot copy

WSL2's speed comes from three things: Hyper-V enlightenments in the guest
kernel, a guest driver that reports free pages so the host reclaims memory,
and a saved-state resume instead of a cold boot.

- **Enlightenments** are now exposed under KVM (above). This is the part that
  needs no guest driver and gives the largest gain.
- **Memory reclaim** needs a guest balloon driver. The `virtio-balloon-pci`
  device is already attached, but Windows has no inbox virtio-balloon driver,
  so `BALLOON` requests are a no-op until signed VirtIO drivers are an
  opt-in. When they are, enable `free-page-reporting=on` on the device and
  let `lswd` set the target from host pressure rather than a fixed policy.
- **Resume** already exists as disk-backed hibernation. The remaining cold-boot
  cost is Windows itself; the slim profile is the lever there.
- **Disk I/O**: the system disk uses the default `aio=threads` and host page
  cache. `cache=none,aio=io_uring` (or `aio=native`) is faster on local
  filesystems but fails on filesystems without `O_DIRECT`; probe once at
  prepare time and record the choice in the manifest rather than guessing at
  launch.
- **Not worth copying**: `hv-passthrough` (exposes whatever the host has and
  breaks on kernel upgrades), `hv-evmcs` (Intel only), `-machine hpet=off`
  (needs QEMU 7.0 and gains little once `hv-stimer` is on).
