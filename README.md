# LSW 1.0 beta

LSW is a local Windows development runtime for Linux. It provides a WSL-like
command-line experience while running a real Windows kernel inside one managed
QEMU/KVM virtual machine per instance.

The current release is `1.0.0-beta.5`. Its focus is daily usability on Linux
x86_64: one-command installation, automatic host dependency repair, Windows
edition selection by name, an automatically opened installation viewer,
configuration and log commands, redacted diagnostic bundles, safe instance
removal, all-instance shutdown, and a machine-readable performance baseline.

LSW does not start a new VM for every application. Shells, commands, file
transfers, and GUI processes for an instance all use the same running Windows
guest.

## Product principles

- Running `lsw` starts the default Windows instance when necessary and enters
  PowerShell, with Windows PowerShell and CMD as fallbacks.
- Users do not need to operate QEMU, OVMF, swtpm, QMP, or VNC sockets directly.
- Windows is installed once per instance. Linked-clone image workflows are
  planned for beta.7.
- The default device model uses Windows inbox NVMe, e1000e, and VGA drivers.
  Signed VirtIO acceleration will remain optional.
- The `secure` profile always retains a driverless path.
- LSW manages a complete Windows kernel, so it cannot match the absolute memory
  density of a Linux namespace container. The goal is WSL-like lifecycle and
  memory behavior, not misleading container terminology.

## Supported scope

- Host runtime: Linux x86_64. KVM is strongly recommended; TCG is diagnostic
  only.
- Guest: a user-supplied and properly licensed Windows 11 x64 installation ISO.
- Runtime dependencies: QEMU, `qemu-img`, OVMF, swtpm, wimlib, xorriso, and
  remote-viewer.
- LSW never downloads or distributes Windows media, product keys, activation
  data, preactivated disks, or modified Windows images.

The backend layer can plan HVF and WHPX arguments, but macOS and Windows hosts
are not supported deliverables in this beta.

## Quick start

Install the release bundle and repair host dependencies:

```bash
./install.sh
lsw doctor --fix
```

Create and start a Windows development environment in one command:

```bash
lsw install win-dev \
  --iso ~/Downloads/Windows11.iso \
  --edition pro \
  --profile slim
```

The one-shot installer performs the following steps:

1. Checks the host and installs missing packages through the distribution's
   package manager.
2. Reads edition names from `sources/install.wim` or `install.esd` in the ISO.
3. Creates the instance manifest, sparse qcow2 disk, private OVMF variables,
   runtime directories, and vTPM state.
4. Creates a read-only installation seed containing the answer file and the
   matching LSW guest agent.
5. Starts Windows Setup and opens the integrated installation viewer.

`--edition pro` is matched against the ISO metadata and is written to the
answer file as `/IMAGE/NAME`; users no longer need to guess a WIM index. The
selected edition flow wipes only that instance's dedicated virtual Disk 0.
It never touches a host disk.

Edition inspection temporarily extracts the ISO's install WIM/ESD into LSW's
private state directory and removes it immediately after reading its metadata.
Ensure the state filesystem has enough free space for that temporary file.

The command records that the user is responsible for the license terms of the
media they supplied. It does not add a product key, bypass activation, skip
OOBE, or create a prebuilt account.

On a headless host, pass `--no-viewer`; later, reopen the display from a Linux
graphical session without handling a socket path:

```bash
lsw view win-dev
```

After completing OOBE and the first administrative login:

```bash
lsw use win-dev
lsw                         # enter PowerShell in the default instance
lsw exec -- cmd.exe /c ver
lsw run -- notepad.exe
lsw push ./main.rs 'C:\Users\you\src\main.rs'
lsw pull 'C:\build\app.exe' ./app.exe
```

`lsw exec` waits and returns the real guest exit code. `lsw run` currently uses
the same session transport and waits; detached GUI launch becomes the default
in beta.6 when the lifecycle contract for detached processes lands.

## Daily management

```bash
lsw config get win-dev
lsw config set win-dev memory.max=4GiB idle-timeout=10m
lsw logs win-dev
lsw logs win-dev --follow
lsw status win-dev
lsw suspend win-dev
lsw resume win-dev
lsw shutdown --all
lsw diagnose win-dev --bundle
lsw remove win-dev
```

`memory.max` is applied to the next QEMU start. `idle-timeout` is stored in
manifest v4 so the beta.7 memory and hibernation governor can enforce one
stable configuration contract; beta.5 does not yet hibernate automatically.

`lsw diagnose --bundle` creates a support archive containing a redacted
manifest, host capability report, daemon status, a redacted QEMU plan, and
bounded log tails. It excludes the agent token, installation seed, Windows
media, virtual disks, and absolute ISO/state paths.

`lsw remove` refuses to remove an active instance. Shut it down first; removal
then deletes that instance's manifest, firmware variables, local virtual disk,
seed, TPM state, and logs.

## Performance baseline

Record a non-mutating local baseline:

```bash
lsw bench --json
lsw bench win-dev --json
```

The JSON includes capability-scan, daemon round-trip, manifest-load, and live
agent-probe timings when those measurements are available. Cold boot, resume,
RSS, and lifecycle-soak measurements require the real Windows/KVM harness.

The project targets are:

| Metric | Target |
| --- | ---: |
| Shell in an already running guest | p95 below 300 ms |
| KVM cold boot to agent ready | p95 below 15 s |
| Disk-backed resume | below 3 s |
| Stopped or hibernated QEMU RSS | 0 |
| Idle `lswd` RSS | below 30 MiB |
| Running Windows idle CPU | below 1% |
| 4 GiB VM idle host RSS with ballooning | below 2.5 GiB |
| Linked clone creation | below 5 s and 256 MiB initial growth |
| Twenty install/start/stop cycles | no orphan process, socket, or port |

Targets are not release claims until the corresponding benchmark has passed on
documented hardware.

## Profiles

| Profile | Behavior | Guest Secure Boot |
| --- | --- | --- |
| `standard` | Stock Windows; servicing remains intact | Off |
| `slim` | Removes only an explicit optional AppX allowlist and enables CompactOS | Off |
| `ephemeral` | Slim behavior with a disposable qcow2 overlay per run | Off |
| `secure` | Disallows test-signed custom drivers and requires key-enrolled OVMF variables | On |

Every beta profile preserves WinSxS, Windows Update and the servicing stack,
MSI/MSIX support, Defender, and common development-tool dependencies. LSW does
not enable test signing or install a self-signed certificate by default.

## Advanced commands

The following commands remain available for debugging, automation, and manual
recovery, but are not part of the beginner path:

```bash
lsw create NAME --iso PATH --accept-license [OPTIONS]
lsw prepare NAME [--execute]
lsw seed NAME [OPTIONS] [--execute]
lsw plan NAME [--run]
lsw daemon [start|status]
```

For legacy guided installation of an already-created instance:

```bash
lsw install NAME
```

The destructive numeric selector remains available only as an advanced
compatibility option:

```bash
lsw install NAME --unattended-index 6
```

Prefer `--edition NAME`, which validates and displays the names actually present
in the ISO.

TCP services can be published on host loopback during creation or one-shot
installation:

```bash
lsw install win-web \
  --iso ~/Downloads/Windows11.iso \
  --edition pro \
  --publish 8080:80 \
  --publish 8443:443
```

Published ports bind only to `127.0.0.1`. LSW rejects duplicates, ports already
owned by another instance or local process, agent-port collisions, and port
publishing with the `offline` network mode.

## Terminal and agent behavior

Interactive host terminals negotiate Windows ConPTY when both peers advertise
support. LSW forwards input/output and terminal resize events; older agents
fall back to pipe sessions.

Controlled sessions distinguish stdin EOF, authenticated cancellation, and
disconnect cleanup. The capability-gated session lease is bounded to 1–300
seconds; the default client requests 120 seconds and sends a heartbeat every
30 seconds. Lease expiry closes the transport and reclaims owned processes.

On Unix, a child enters a separate process group before `exec`; ordinary
descendants remaining in that group are reclaimed on exit, cancellation,
disconnect, protocol failure, or lease expiry. A process can deliberately
escape with `setsid` or `setpgid`, so this is lifecycle ownership, not a guest
security sandbox. On Windows, children start suspended and must enter a
kill-on-close Job Object before they are resumed.

## Windows/KVM end-to-end gate

The firmware and product-lifecycle CI tests exercise the real planner and
daemon with QEMU, OVMF, NVMe, e1000e, vTPM, QMP, loopback forwarding, suspend,
resume, and forced stop. Placeholder media is used, so those tests do not claim
that Windows booted.

Before beta.5 is promoted, run the real hardware gate on a Linux x86_64 machine
with KVM and a licensed Windows 11 ISO:

```bash
LSW_WINDOWS_ISO=/absolute/path/Windows11.iso \
LSW_WINDOWS_EDITION=pro \
LSW_WINDOWS_AGENT=/absolute/path/lsw-agent.exe \
scripts/check-windows-kvm-e2e.sh
```

The operator completes normal OOBE in the viewer. The harness then verifies
agent readiness, ConPTY/command execution, exit-code propagation, graceful
shutdown, and cleanup of processes, sockets, and loopback ports.

## Roadmap

- beta.6: working directory and environment injection, signal and Ctrl-C
  propagation, recursive/resumable transfer, workspace watch sync, Linux ↔
  Windows path translation, dynamic ports, completion, systemd user socket
  activation, detached `run`, and fully specified `exec` semantics.
- beta.7: sealed base images, linked clones, optional signed VirtIO networking,
  ballooning and vsock, memory-pressure governor, Windows hibernation, automatic
  resume, guest TRIM, discard, and compaction.
- beta.8: driverless per-HWND Windows Graphics Capture transported to independent
  Wayland windows, with X11 fallback, input, resize, DPI, clipboard, audio, and
  notifications. Shared-memory/GPU acceleration remains optional.
- beta.9 and later: Linux ARM64 with Windows ARM64, then macOS Apple Silicon with
  HVF and Windows ARM64, then a Windows host with WHPX.

Apple Silicon cannot efficiently accelerate the current Windows x64 guest. It
requires a separate Windows ARM64 agent, firmware, installer path, and CI.

## Build from source

The host binaries have no third-party Rust crate dependencies. The MSRV is Rust
1.76, which is also pinned in CI:

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Build the Windows GNU guest agent with MinGW:

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu --bin lsw-agent
```

If MinGW is unavailable, configure a Zig executable:

```bash
LSW_ZIG=/path/to/zig scripts/build-windows-agent.sh
LSW_ZIG=/path/to/zig scripts/build-release.sh
```

The release builder produces a Linux x86_64 archive, SHA-256 sidecar, Windows
PE agent, exact corresponding-source snapshot, and reproducibility metadata.
It does not download Rust targets, Zig, or operating-system media.

## Current limitations

- A real Windows Setup → OOBE → agent → ConPTY → shutdown run still requires a
  KVM-capable test host and user-provided ISO.
- The installation and recovery display uses private Unix-socket VNC internally;
  LSW opens the viewer and does not expose TCP VNC or RDP.
- `lsw run` can start a GUI process, but per-window Wayland/X11 integration is
  not implemented yet.
- Suspend/resume currently uses QMP stop/continue and retains guest RAM. It is
  not hibernation or disk-backed resume.
- Automatic memory reclaim, balloon control, and idle hibernation are beta.7
  work. The beta.5 idle timeout is configuration only.
- Agent authentication is not encrypted and is limited to LSW's local
  loopback/QEMU user-network path.
- QEMU does not yet run inside an LSW-specific seccomp/namespace/service-account
  sandbox.

See [the beta acceptance boundary](docs/BETA.md) for detailed validation status.

## License

LSW-owned code is licensed under
[GNU GPL 3.0 or any later version](LICENSE), identified as
`GPL-3.0-or-later`. This license does not cover Windows, QEMU, OVMF, swtpm,
macOS, or user-supplied media; those remain subject to their respective owners'
terms.

## Documentation

- [Beta acceptance scope and known limitations](docs/BETA.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Security model](docs/SECURITY.md)
- [Development and release gates](docs/DEVELOPMENT.md)
- [License and distribution boundaries](docs/LEGAL_BOUNDARIES.md)
- [Design references](docs/REFERENCES.md)
