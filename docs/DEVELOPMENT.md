# Development and release checks

LSW's automated checks deliberately separate source and firmware-level
confidence from hardware and guest validation. A green pull request means that
the Rust code, shell tooling, Windows guest-agent PE build/native agent tests,
and headless QEMU/OVMF TCG gates passed. It does **not** mean that GitHub-hosted
runners used KVM or installed Windows.

## Local checks

Use the Rust version declared by `rust-version` in the workspace manifest. The
beta.5 workflows pin Rust 1.76.0 instead of treating the moving `stable`
toolchain as the MSRV gate:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
scripts/check-shell.sh
```

`scripts/check-shell.sh` requires Bash, Dash, and ShellCheck. It parses every
script with the shell named by its shebang before running ShellCheck.

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
endpoints targeting guest ports 5040 and 8080. The endpoints were released
after quit and both QEMU and swtpm exited with status zero. `/dev/kvm` and a
Windows ISO were unavailable, so Windows Setup, agent login, and ConPTY were
not exercised.

CI also builds `lsw` and `lswd` and runs:

```sh
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
tests serially (including Job Object descendant cleanup and ConPTY setup), and
loads the resulting executable with `--help`. The Linux VPS performed only the
GNU cross-build. Neither gate exercises a managed guest session or a logged-in
Windows ConPTY session.

## Release bundle

The release builder supports Linux x86_64 and requires GNU tar and Python 3.
It builds the Linux CLI/daemon and Windows x86_64 agent from the checked-out
source; it never downloads an operating-system installer or guest image.

```sh
LSW_WINDOWS_LINKER=mingw scripts/build-release.sh
scripts/verify-release.sh dist/lsw-*-linux-x86_64.tar.gz
```

Useful controls are:

- `LSW_DIST_DIR`: write the unpacked bundle, archive, and checksum elsewhere.
- `CARGO_TARGET_DIR`: isolate Cargo artifacts; relative paths are resolved from
  the workspace root and are respected by both host and guest-agent builders.
- `LSW_EXPECT_VERSION`: require the binary version to match a tag such as
  `v1.0.0-beta.5`.
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
metadata, requires the Windows agent's PE/COFF TimeDateStamp to be zero,
smoke-tests the Linux binaries when possible, and rejects common OS media and
VM disk-image formats. On Linux x86_64 it also installs into an isolated
temporary prefix and compares the installed binaries with the bundle.
`SOURCE-MANIFEST.sha256` records every packaged build/source file and is checked
before the embedded corresponding source is accepted.

## GitHub Actions boundary

`.github/workflows/ci.yml` runs formatting, unit tests, Clippy, shell checks,
the MinGW agent cross-build plus PE-format inspection, and Windows-native MSVC
Job/ConPTY setup tests plus an executable load smoke on ordinary pushes and pull
requests. It also installs distribution QEMU/OVMF/swtpm packages and runs both
the timeout-bounded TCG firmware smoke and the non-skippable product-lifecycle
gate described above. All Rust jobs use the declared 1.76.0 MSRV. The native
Windows job does not boot a managed guest or exercise a logged-in ConPTY shell;
the QEMU jobs use no Windows ISO. CI also builds, installs, and verifies a
disposable release bundle without publishing it.

`.github/workflows/release.yml` runs only for version tags or a manual
dispatch. It repeats the source gates, checks package determinism, verifies the
finished bundle, and uploads a short-lived workflow artifact. A version tag
also creates a GitHub Release with `GITHUB_TOKEN`; no repository secret is
required. Tags containing a prerelease suffix such as `-beta.4` are published
as prereleases.

`.github/workflows/windows-kvm-e2e.yml` is the separate, attended beta release
gate. It can run only by manual dispatch from an exact `master` commit on the
explicitly labeled `lsw-windows-kvm-e2e` self-hosted runner. Windows media is
pre-provisioned read-only on that runner and is verified by SHA-256; the
workflow never downloads or uploads it. See
[`WINDOWS_KVM_E2E.md`](WINDOWS_KVM_E2E.md) for runner isolation, protected
environment, media provisioning, and operator instructions. Tagged releases
after the existing beta.1–beta.4 tags fail closed unless this workflow has a
successful real-KVM job for the exact tag commit; manual untagged bundle builds
remain available without that hardware attestation.

Before describing a build as runtime-validated, a maintainer must separately
run the hardware gates documented in `BETA.md` on a Linux x86_64 host with KVM,
QEMU, suitable OVMF firmware, and user-supplied licensed Windows media.
