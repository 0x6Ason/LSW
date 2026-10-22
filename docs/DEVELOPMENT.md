# Development and release checks

LSW's automated checks deliberately separate source and firmware-level
confidence from hardware and guest validation. A green pull request means that
the Rust code, shell tooling, Windows guest-agent PE build/native agent tests,
and headless QEMU/OVMF TCG gates passed. It does **not** mean that GitHub-hosted
runners used KVM or installed Windows.

## Local checks

`rust-toolchain.toml` pins Rust 1.76.0 with `rustfmt` and `clippy`; `rustup`
installs and selects it for every `cargo` invocation in the checkout, so CI,
release builds, and local checks all use the exact MSRV instead of the moving
`stable` toolchain. Keep its `channel` equal to `rust-version` in the workspace
manifest.

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --target x86_64-pc-windows-gnu -p lsw-agent --all-targets --locked -- -D warnings
cargo deny check
scripts/check-shell.sh
scripts/check-source-layout.sh
```

The Windows-target Clippy run type-checks the `cfg(windows)` half of the guest
agent, including its FFI, on a Linux host; it needs only
`rustup target add x86_64-pc-windows-gnu`, not a linker. `cargo deny`
(`cargo install cargo-deny --locked`) enforces `deny.toml`: a GPL-compatible
license allowlist, crates.io as the only source, no wildcard or yanked
dependencies, and RustSec advisories. Advisories that cannot be fixed within
the MSRV are listed there with the reason, never silently.

`scripts/check-shell.sh` requires Bash, Dash, and ShellCheck. It parses every
script with the shell named by its shebang before running ShellCheck, then
lints each sourced `scripts/e2e/*.sh` library directly and rejects a library
whose first line is not a `#` comment: a `//` line parses as a command and
breaks every driver that sources the file.
`scripts/check-source-layout.sh` rejects Rust and automation files above 1,000
lines, executable `main.rs` entry points above 300 lines, and any reintroduced
Slice 4.5 exception. The checked-in exception file is intentionally empty.

## Apple Silicon x86_64 validation

An Apple Silicon maintainer can exercise the Linux x86_64 release path in a
full-system Lima guest. Configure the instance with `arch: x86_64` and
`vmType: qemu`, mount the checkout writable, and install the same build
dependencies used by CI. Inside that guest, confirm `uname -m` reports
`x86_64`, install Rust 1.76.0 for `x86_64-unknown-linux-gnu`, and run the local
checks above. The complete packaging check is:

```sh
LSW_WINDOWS_LINKER=mingw scripts/check-release-reproducibility.sh
```

This is full machine emulation rather than a cross-compile: the Linux CLI,
daemon, tests, Clippy, Windows MinGW agent build, source vendoring, archive
verification, and deterministic rebuild all execute in an x86_64 Linux guest.
It does not provide KVM or replace the real Windows/KVM release gate.

## Headless QEMU smoke

On Debian/Ubuntu, install `qemu-system-x86`, `qemu-utils`, `ovmf`, `swtpm`, and
Python 3, then run:

```sh
scripts/check-qemu-smoke.sh
```

Every wait is bounded. The script creates and checks a disposable qcow2 disk,
boots OVMF under TCG with a software TPM, confirms vTPM traffic, negotiates QMP,
checks `stop`/`cont`/`quit`, and connects to loopback-only usernet forwarding.
It uses no Windows media and cannot substitute for the hardware/guest gates in
`BETA.md`. `LSW_QEMU_STAGE_ROOT` selects an extracted package root for a
non-system installation; the individual `LSW_QEMU_SYSTEM`, `LSW_QEMU_IMG`,
`LSW_SWTPM`, `LSW_QEMU_DATA_DIR`, `LSW_QEMU_NIC_ROM`, `LSW_OVMF_CODE`, and
`LSW_OVMF_VARS` variables override discovered paths.

The Codex VPS run staged official Ubuntu packages and passed this path with
QEMU 8.2.2, TCG, OVMF, swtpm/vTPM, TCP QMP, and two loopback host-forward
endpoints targeting guest ports 35040 and 8080. The endpoints were released
after quit and both QEMU and swtpm exited with status zero. `/dev/kvm` and a
Windows ISO were unavailable, so Windows Setup, agent login, and ConPTY were
not exercised.

CI also builds `lsw` and `lswd` and runs:

```sh
scripts/check-systemd-socket-activation.sh
LSW_QEMU_LIFECYCLE_REQUIRE=1 scripts/check-lsw-qemu-lifecycle.sh
```

This second gate does not replace Windows media validation. It uses placeholder
media to drive the real manifest, storage preparation, `QemuPlanner`, and daemon
lifecycle, asserting OVMF, NVMe, e1000e, vTPM, loopback agent/published ports,
install, start, status, suspend, resume, forced stop, and port release. Setting
`LSW_QEMU_LIFECYCLE_REQUIRE=1` makes unavailable pathname sockets a failure
rather than a skip. This VPS returns `EPERM` for pathname AF_UNIX `bind`, so only
the lower-level smoke was run here and this product gate remains CI-only in this
environment.

## Windows guest-agent cross-build

Install the Rust GNU target and either Zig or the MinGW-w64 compiler:

```sh
rustup target add x86_64-pc-windows-gnu
LSW_WINDOWS_LINKER=mingw scripts/build-windows-agent.sh
# or: LSW_WINDOWS_LINKER=zig scripts/build-windows-agent.sh
```

`LSW_WINDOWS_LINKER=auto` prefers Zig and then tries
`x86_64-w64-mingw32-gcc`. `LSW_ZIG` and `LSW_MINGW_CC` override the executable
used by each backend. CI cross-builds the agent and inspects its PE machine
type. A separate Windows runner builds the MSVC target, runs the native agent
tests serially (including Job Object descendant cleanup, ConPTY setup, and the
SCM rejection path outside a service process), and loads the resulting
executable with `--help`. Linux-side validation cannot execute the Windows
binary. Neither gate exercises a managed guest session or a service-backed
Windows ConPTY session.

The beta.7 pre-applied unattend installs the agent during `specialize` as the
automatic Windows service `LSWAgent` under the virtual account
`NT SERVICE\LSWAgent`; it must not restore the old per-user `HKCU` startup
entry. It also registers the narrow, demand-start `LSWLicenseHelper` and
`LSWUserHelper` and `LSWMaintenanceHelper` services as LocalSystem. Cross-build
and executable-load checks cannot prove SCM registration, Session 0 ConPTY,
boot-time startup, or those helpers' access boundaries. The real Windows/KVM
gate therefore queries all four services, proves a
license-status request returns the helper to `Stopped`, verifies that the agent
service process and command identities resolve to the same `S-1-5-80-...` SID,
and requires that SID to remain stable across a full shutdown and bare-`lsw`
boot. The exact-commit gate also exercises cwd/environment injection,
SIGTERM status, detached completion, recursive tree round-trip, and live
additive `sync --watch` against that service-backed guest.
beta.7 additionally requires native standard-user creation through a stopped
demand-start account helper, linked-clone
identity isolation, folder-share escape rejection, balloon/TRIM, fixed-helper
Windows hibernate/resume, offline compaction, and zero QEMU RSS after
stop/hibernate.
beta.8 Slice 2 additionally requires an exact-root, per-instance authenticated
QEMU SMB export with signing and encryption, an agent-session `Linux (L:)`
mapping owned without a resident privileged helper, immediate host/guest
visibility, inferred `lsw cp`, machine-readable file benchmarks, unshare
preservation, an authenticated native fallback when ACPI shutdown is ignored,
and complete teardown of the restarted VM and Samba helper. The exact gate uses
a bounded small-file sample; the product benchmark defaults remain unchanged.

## Release bundle

The release builder supports Linux x86_64 and requires GNU tar and Python 3.
It builds the Linux CLI/daemon and Windows x86_64 agent from the checked-out
source. Packaging never downloads or embeds an operating-system installer or
guest image; the installed `lsw` runtime can resolve official media directly
from Microsoft when a user explicitly starts an installation.

```sh
LSW_WINDOWS_LINKER=mingw scripts/build-release.sh
scripts/verify-release.sh dist/lsw-*-linux-x86_64.tar.gz
```

Useful controls are:

- `LSW_DIST_DIR`: write the unpacked bundle, archive, and checksum elsewhere.
- `CARGO_TARGET_DIR`: isolate Cargo artifacts; relative paths are resolved from
  the workspace root and are respected by both host and guest-agent builders.
- `LSW_EXPECT_VERSION`: require the binary version to match a tag such as
  `v1.0.0-beta.7`.
- `SOURCE_DATE_EPOCH`: set the reproducible build environment and all archive
  member timestamps. The default is a fixed epoch. Archive ownership and modes
  are normalized, and gzip timestamps are disabled.
- `scripts/check-release-reproducibility.sh`: perform two complete builds in
  separate, initially empty Cargo target directories and require
  byte-for-byte identical archives. This tests clean-build reproducibility for
  the selected compiler and linker; it does not claim that different
  toolchains produce identical binaries.

The verifier checks the SHA-256 sidecar before extraction, rejects unsafe or
multi-root archives and links/special files, validates executable formats and
metadata, requires the Windows agent's PE/COFF TimeDateStamp and optional-header
CheckSum to be zero,
smoke-tests the Linux binaries when possible, and rejects common OS media and
VM disk-image formats. On Linux x86_64 it also installs into an isolated
temporary prefix and compares the installed binaries with the bundle.
`SOURCE-MANIFEST.sha256` records every packaged build/source file and is checked
before the embedded corresponding source is accepted. `cargo vendor --locked
--versioned-dirs` places the exact Rust dependency sources and their upstream
license files under `source/vendor`; `source/.cargo/config.toml` makes that tree
the offline Cargo source. The manifest covers the vendored tree too.

## GitHub Actions boundary

`.github/workflows/ci.yml` runs on ordinary pushes and pull requests; a newer
push to the same branch or pull request cancels the superseded run, every job
has a timeout, and the Rust toolchain comes from `rust-toolchain.toml`. Its
jobs are:

- `lint`: formatting, shell syntax and ShellCheck, the source-layout ratchet,
  and the profile-comparison self-test, with no Rust build.
- `rust`: unit tests and Clippy from a shared dependency cache, plus Clippy on
  the `x86_64-pc-windows-gnu` target so the guest agent's Windows-only code is
  linted on every push. It publishes the debug `lsw`/`lswd` binaries as a
  short-lived artifact.
- `supply-chain`: `cargo deny check` against `deny.toml`.
- `stable`: an advisory-only `cargo check` on current stable Rust, so compiler
  breakage is visible before the MSRV moves; it never blocks a merge.
- `windows-agent-native`: MSVC Job/ConPTY setup tests, a dev-profile build,
  an executable load smoke, two SCM start/stop cycles against that binary, and
  a parse of every PowerShell script. It does not boot a managed guest or
  exercise a service-backed ConPTY shell end to end.
- `qemu-smoke`: installs distribution QEMU/OVMF/swtpm, reuses the `rust` job's
  binaries, and runs the systemd socket-activation check, the timeout-bounded
  TCG firmware smoke, and the non-skippable product-lifecycle gate described
  above. It uses no Windows ISO.
- `package`: on a pinned `ubuntu-24.04` image, builds, installs, and verifies
  a disposable release bundle without publishing it, then inspects the
  cross-built guest-agent PE. `BUILDINFO.txt` identifies all four shipped
  binaries by SHA-256, and the bundle also carries an `lswg` runtime sidecar.
  Verification checks its ELF target, shared version/protocol identity, and
  installed bytes.

`.github/dependabot.yml` proposes grouped weekly updates for the SHA-pinned
actions and the exact-pinned crates.

`.github/workflows/release.yml` runs only for version tags or a manual
dispatch. It repeats the source gates, checks package determinism, verifies the
finished bundle, and uploads a short-lived workflow artifact. A version tag
also creates a GitHub Release with `GITHUB_TOKEN`; no repository secret is
required. Tags containing a prerelease suffix such as `-beta.4` are published
as prereleases.

`.github/workflows/windows-kvm-e2e.yml` is the separate beta release gate. It
can run only by manual dispatch from an exact `master` commit. Its ordinary
headless mode uses the explicitly labeled `lsw-windows-kvm-e2e` self-hosted
runner. Its beta.8 native mode uses `lsw-windows-kvm-gui-e2e` and runs the same
headless install followed by the real-install linked-clone GUI matrix in one
job. The authorization job resolves Microsoft's current English x64 metadata,
transfers only a short-lived private request, and the KVM runner downloads the ISO
directly from Microsoft, verifies the published SHA-256, and removes it after
the run; the ISO payload is never uploaded. The job requires both WinPE
completion markers and removal
of token-bearing transient media. The gate requires unattended OOBE to remove
the one-shot local account, cached answer files, SetupComplete script, and
staging payload; it rejects a console login or automatic-logon credential. All
agent commands run as `NT SERVICE\LSWAgent`. It also verifies WMI license status through the stopped,
demand-start LocalSystem helper. After shutdown it requires a no-login cold
boot, the same service SID, true ConPTY, detached installation media, and
complete runtime cleanup. A second, independently labeled
`lsw-windows-seamless-e2e` runner currently inherits a signed-in WSLg session
for the opt-in Windows-hosted development adapter. Its job runs only when the
dispatch explicitly selects it and only after the headless job succeeds. It
consumes that run's accepted evidence and attests the candidate CLI, active
daemon, and installed guest agent by SHA-256 before the first-HWND matrix. It
neither weakens nor replaces the no-login contract. See
[`WINDOWS_KVM_E2E.md`](WINDOWS_KVM_E2E.md) for runner isolation, protected
environment, media provisioning, and operator instructions. Tagged releases
after the existing beta.1–beta.7 tags fail closed unless one successful
exact-SHA native-runner job contains the trusted-runner check, real Windows
install, real-install handoff, and native Linux Wayland GUI steps. The current
release checker intentionally does not accept the WSLg compatibility job. WSLg
is test infrastructure, not a runtime dependency. The native driver and handoff
are source-complete but remain runtime-unaccepted until that runner passes the
exact candidate.
Manual untagged bundle builds remain available without that hardware
attestation.

Before describing a build as runtime-validated, a maintainer must separately
run the hardware gates documented in `BETA.md` on a Linux x86_64 host with KVM,
QEMU, suitable OVMF firmware, and user-supplied licensed Windows media.
