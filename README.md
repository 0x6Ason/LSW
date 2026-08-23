# LSW 1.0 beta

LSW is a local Windows development runtime for Linux. It provides a WSL-like
command-line experience while running a real Windows kernel inside one managed
QEMU/KVM virtual machine per instance.

The current release is `1.0.0-beta.6`. It provides a one-command Windows
environment with truthful installation progress, unattended first boot,
complete remote process semantics, and improved host integration on Linux
x86_64.

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
- The beta.6 `vanilla` and `slim` profiles retain the driverless installation
  and recovery path.
- LSW manages a complete Windows kernel, so it cannot match the absolute memory
  density of a Linux namespace container. The goal is WSL-like lifecycle and
  memory behavior, not misleading container terminology.

## Supported scope

- Host runtime: Linux x86_64. KVM is strongly recommended; TCG is diagnostic
  only.
- Guest: the current official Windows 11 x64 ISO downloaded from Microsoft, or
  a user-supplied authorized ISO selected with `--iso`.
- Runtime dependencies: QEMU, `qemu-img`, OVMF, swtpm, wimlib, xorriso, and a
  UDF-capable `7z`. `remote-viewer` is optional and used only for `--viewer` or
  `lsw view`.
- LSW downloads official media directly from allowlisted Microsoft HTTPS CDNs
  and verifies Microsoft's published SHA-256. It never redistributes Windows,
  product keys, activation data, preactivated disks, or modified images.

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
lsw install win-dev
lsw
```

A new interactive installation displays the Windows licensing boundary and
requires `[y/N]` confirmation before any download or instance creation.
Noninteractive automation must be explicit:

```bash
lsw install win-dev --accept-windows-license
```

`--accept-license` remains a compatibility alias. The confirmation applies only
to the Microsoft Software License Terms for the selected Windows media. LSW is
licensed under GPL-3.0-or-later and does not add a second click-through EULA.
Review the [Microsoft licensing documents](https://aka.ms/licensingdocs) and the
terms supplied with the media or applicable retail/volume agreement.

The one-shot installer performs the following steps:

1. Checks the host and repairs missing packages through the distribution's
   package manager.
2. Resolves the current ISO through Microsoft's session flow, downloads it with
   aria2c or LSW's four-range resumable downloader, and verifies exact SHA-256.
3. Selects Windows 11 Pro by inspected WIM metadata.
4. Builds a private, ephemeral control ISO from the official ISO's WinPE boot
   files, then boots it in a network-disabled microVM and uses its real DISM to
   prepare the selected `slim` image with the boot-time agent and answer file.
5. Applies the prepared WIM to the instance qcow2, validates the resulting
   qcow2, creates UEFI boot files, and deletes temporary seeds/workspace media.
6. Boots Windows headlessly, completes OOBE, removes the one-shot setup account
   and cached answer file, then verifies the boot-time agent.

The beta.6 terminal UI renders byte-accurate bars for native ISO transfer,
range assembly, SHA-256 verification, and DISM percentages. WinPE boot,
specialize, OOBE, agent installation, and cleanup instead display their real
named stage and elapsed time; LSW does not invent percentages for work that
Windows does not expose measurably. Redirected output emits bounded progress
updates suitable for CI logs.

Real Windows/KVM measurement found that fast WIM compression shortened export
but made total preparation slower because the larger intermediate increased
later mount and commit work. LSW therefore retains maximum compression and
uses the private NTFS scratch directory explicitly. A pre-applied profile marker
prevents first boot from repeating offline AppX and CompactOS work. Exact media
SHA-256, `/CheckIntegrity`, and CompactOS-on-apply behavior remain unchanged.

`--edition pro` remains available to override the default and is matched against
ISO metadata; users never need to guess a WIM index. The WinPE jobs attach only
LSW-owned qcow2 files, private seed/status volumes, and read-only ISO media;
they never attach a host block device.

Edition inspection temporarily extracts the ISO's install WIM/ESD into LSW's
private state directory and removes it immediately after reading its metadata.
Ensure the state filesystem has enough free space for that temporary file.

The command records explicit Windows-license acceptance before setting Windows
Setup `AcceptEula=true`. It does not add a product key, grant an entitlement,
bypass activation, use the deprecated `SkipMachineOOBE` setting, or disable UAC.
It automates the supported OOBE settings with a random one-shot local account,
then removes that account before installation is reported complete.

The pre-applied unattend registers the guest agent during `specialize` as the
automatic Windows service `LSWAgent`, running as the virtual account
`NT SERVICE\LSWAgent`. It does not add a per-user `HKCU` startup entry or require
automatic logon for a cold start. The temporary password is independent from
the agent token; its cached answer files are deleted after OOBE.

Offline media is still supported:

```bash
lsw install win-dev --iso ~/Downloads/Windows11.iso
```

Installation is headless by default. Pass `--viewer` when a graphical session
is available, or reopen the display later without handling a socket path:

```bash
lsw view win-dev
```

After `lsw install` returns:

```bash
lsw use win-dev
lsw                         # enter PowerShell in the default instance
lsw exec -- cmd.exe /c ver
lsw exec --cwd 'C:\src' --env BUILD_MODE=release -- cmd.exe /d /c build.cmd
lsw run --detach -- powershell.exe -NoProfile -File 'C:\src\worker.ps1'
lsw push ./main.rs 'C:\src\main.rs'
lsw push --recursive ./project 'C:\src\project'
lsw sync --watch ./project 'C:\src\project'
lsw pull 'C:\src\build\app.exe' ./app.exe
```

`lsw exec` and ordinary `lsw run` wait and return guest exit codes 0–255
unchanged. Windows has a 32-bit exit-code space; when a value cannot be
represented by a Unix shell, LSW prints the exact unsigned decimal and
hexadecimal Windows value and returns 255. For controlled noninteractive
sessions, host `SIGINT` and `SIGTERM` terminate the owned Windows Job and return
130 and 143 respectively. `run --detach` returns the guest PID after a
successful start handshake, disconnects from its standard streams, and lets the
agent retain lifecycle ownership until the process exits.

Recursive transfer refuses host symlinks, guest reparse points, traversal, and
implicit overwrite. `sync` is intentionally host-to-guest and additive:
changed/new files are atomically replaced, while deletion on the host does not
delete the guest copy. `--watch` polls a bounded local snapshot every 750 ms and
retries failed changes.

Path conversion is explicit and syntactic; it does not make the independent
guest disk a host mount:

```bash
lsw path -w /mnt/c/Users/Jason/project
lsw path -u 'C:\Users\Jason\project'
```

Use `push`, `pull`, or `sync` when content must cross the VM boundary. Visible
GUI launch still requires the future user-session companion described in the
roadmap.

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

## Windows activation

Installation does not add a product key or bypass activation. After the first
successful environment verification, an unactivated guest receives one
non-blocking notice. Manage activation without placing a key in shell history:

```bash
lsw license status win-dev
lsw license activate win-dev            # masked terminal input
lsw license activate win-dev --key-stdin
lsw license activate win-dev --online
lsw license open win-dev
```

The key travels through the authenticated agent channel as stdin and then over
guest loopback to `LSWLicenseHelper`, a LocalSystem service that starts only for
one authenticated operation. The regular agent remains `NT SERVICE\LSWAgent`.
The helper calls Windows WMI `InstallProductKey` and `Activate`; it does not put
the key in argv, environment, seed media, the base image, logs, or diagnostics.

`memory.max` is applied to the next QEMU start. `idle-timeout` is stored in
manifest v4 so the beta.7 memory and hibernation governor can enforce one
stable configuration contract; beta.6 does not yet hibernate automatically.

`lsw diagnose --bundle` creates a support archive containing a redacted
manifest, host capability report, daemon status, a redacted QEMU plan, and
bounded runtime and WinPE log tails. It excludes the agent token, installation
seed, Windows media, virtual disks, and absolute ISO/state paths.

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
| `vanilla` | Stock Windows plus the LSW agent | Off |
| `slim` | Removes only an explicit optional AppX allowlist and enables CompactOS | Off |

`slim` is the beta.6 default. Both profiles are embedded versioned declarative
manifests. They preserve WinSxS, Windows Update and the servicing stack,
MSI/MSIX, Defender, Terminal, PowerShell, ConPTY, Store, winget, WebView2, WMI,
hibernation, Recovery, and common development-tool dependencies. LSW does not
enable test signing or install a self-signed certificate by default. The
experimental `minimal` and user-versioned `custom` profiles remain beta.7 work.

## Advanced commands

The following commands remain available for debugging, automation, and manual
recovery, but are not part of the beginner path:

```bash
lsw create NAME --iso PATH --accept-windows-license [OPTIONS]
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

Let LSW select an available host port when a fixed port is unnecessary:

```bash
lsw install win-web --publish auto:8080
```

Published ports bind only to `127.0.0.1`. LSW rejects duplicates, ports already
owned by another instance or local process, agent-port collisions, and port
publishing with the `offline` network mode.

## Terminal and agent behavior

Interactive host terminals negotiate Windows ConPTY when both peers advertise
support. LSW forwards input/output and terminal resize events; older agents
fall back to pipe sessions.

The beta.6 agent runs at boot as the automatic `LSWAgent` Windows service under
`NT SERVICE\LSWAgent`. Shell and `exec` processes therefore use that service
identity in Windows Session 0; they do not impersonate a desktop user. This
provides command access at the Windows sign-in screen without storing a daily
user credential.
A later user-session companion will be required for visible desktop GUI
processes, clipboard, audio, and per-window integration.

Controlled sessions distinguish stdin EOF, authenticated cancellation,
interrupt/terminate signals, detached start acknowledgement, and disconnect
cleanup. Working-directory and environment frames are capability-gated and
validated before process creation. Environment keys are case-insensitively
unique to match Windows semantics. The capability-gated session lease is
bounded to 1–300 seconds; the default client requests 120 seconds and sends a
heartbeat every 30 seconds. Lease expiry closes the transport and reclaims
owned processes.

Generate completion without installing a shell framework:

```bash
lsw completion bash >~/.local/share/bash-completion/completions/lsw
lsw completion zsh >~/.local/share/zsh/site-functions/_lsw
lsw completion fish >~/.config/fish/completions/lsw.fish
lsw completion powershell >~/.config/powershell/lsw-completion.ps1
```

The release bundle also ships hardened `lswd.service` and `lswd.socket` user
units. A default-prefix install places them in the user systemd data directory;
enable on-demand startup with:

```bash
systemctl --user enable --now lswd.socket
```

The CLI first connects to the private socket, so a listening systemd socket
activates `lswd` without a separate login-time daemon. Direct CLI autostart
remains available when the socket unit is not enabled.

On Unix, a child enters a separate process group before `exec`; ordinary
descendants remaining in that group are reclaimed on exit, cancellation,
disconnect, protocol failure, or lease expiry. A process can deliberately
escape with `setsid` or `setpgid`, so this is lifecycle ownership, not a guest
security sandbox. On Windows, children start suspended and must enter a
kill-on-close Job Object before they are resumed.

## Windows/KVM end-to-end gate

Tagged releases fail closed unless their exact commit has passed the dedicated
Windows 11/KVM hardware gate with Microsoft's published ISO SHA-256. The gate
covers real WinPE DISM, unattended OOBE and cleanup, SCM and licensing-helper
identity, ConPTY and beta.6 process/file behavior, full shutdown, no-login cold
restart, and complete runtime cleanup. `v1.0.0-beta.6` passed that gate before
publication.

See [the operator workflow and evidence contract](docs/WINDOWS_KVM_E2E.md) and
[the detailed acceptance boundary](docs/BETA.md). Ordinary CI also runs bounded
firmware and product-lifecycle tests with placeholder media, but those tests do
not claim that Windows booted.

## Roadmap

Experience work now comes before architecture expansion. beta.7 targets a
low-resource optional background runtime, linked clones, hibernation, and safe
host-folder sharing. beta.8 targets Linux desktop-native Windows application
windows, clipboard, file drag-and-drop, audio, notifications, and full screen.
beta.9 adds multi-monitor polish and an experimental reversible shell-light
profile. Linux/Windows ARM64 and additional hosts follow only after those paths
have stable release gates.

See the [full roadmap](ROADMAP.md) for ordering, acceptance criteria, and the
Explorer/shell-light safety boundary.

## Build from source

The host resolver uses a small pinned Rust HTTP/TLS, URL, JSON and SHA-256
dependency set; the Windows agent does not link that host-only stack. The MSRV
is Rust 1.76, which is also pinned in CI:

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
PE agent, exact corresponding-source snapshot, vendored locked Rust dependency
sources/licenses, and reproducibility metadata. It does not download Rust
targets, Zig, or operating-system media.

## Current limitations

- Tagged releases still require the dedicated KVM-capable release host and a
  pre-provisioned current official ISO. beta.6 passed that exact-commit gate;
  ordinary GitHub-hosted CI cannot reproduce it without KVM and Windows media.
- The optional installation and recovery display uses private Unix-socket VNC
  internally; LSW opens it only when requested and does not expose TCP VNC or RDP.
- `lsw run` can start a Session 0 process, but a service-launched GUI is not a
  visible desktop application. A user-session companion and per-window
  Wayland/X11 integration are not implemented yet.
- Suspend/resume currently uses QMP stop/continue and retains guest RAM. It is
  not hibernation or disk-backed resume.
- Automatic memory reclaim, balloon control, and idle hibernation are beta.7
  work. The beta.6 idle timeout is configuration only.
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

The Rust Microsoft ISO session resolver adapts the MIT-licensed request flow
from [windows-iso-downloader/MSDL](https://github.com/starkSV/windows-iso-downloader).
Thanks to its authors for publishing that work. See
[third-party notices](THIRD_PARTY_NOTICES.md) for the retained attribution and
license text. LSW neither bundles MSDL nor uses its backend, telemetry, or
crowdsourced cache.

LSW's name and its WSL-like lifecycle and terminal UX are inspired by
[Microsoft's MIT-licensed WSL project](https://github.com/microsoft/WSL).
Thanks to Microsoft and the WSL contributors for that naming and UX reference.
LSW is an independent project, is not affiliated with or endorsed by Microsoft,
and does not incorporate WSL source code merely by following those interaction
conventions.

## Documentation

- [Roadmap](ROADMAP.md)
- [Beta acceptance scope and known limitations](docs/BETA.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Security model](docs/SECURITY.md)
- [Development and release gates](docs/DEVELOPMENT.md)
- [Real Windows/KVM release gate](docs/WINDOWS_KVM_E2E.md)
- [WinPE DISM backend](docs/WINPE_DISM_EXPERIMENT.md)
- [License and distribution boundaries](docs/LEGAL_BOUNDARIES.md)
- [Design references](docs/REFERENCES.md)
