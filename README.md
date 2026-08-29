# LSW 1.0 beta

LSW is a local Windows development runtime for Linux. It provides a WSL-like
command-line experience while running a real Windows kernel inside one managed
QEMU/KVM virtual machine per instance.

The latest tagged release is `1.0.0-beta.7`. It adds sealed linked-clone images,
opt-in low-memory lifecycle policies, disk-backed Windows hibernation,
declarative folder synchronization, and WSL-style permanent-user registration
to the beta.6 terminal-first runtime on Linux x86_64. Development on `master`
now targets beta.8 seamless desktop integration.

LSW does not start a new VM for every application. Shells, commands, file
transfers, and GUI processes for an instance all use the same running Windows
guest.

## Product principles

- Running `lsw` starts the default Windows instance when necessary and enters
  PowerShell, with Windows PowerShell and CMD as fallbacks.
- Users do not need to operate QEMU, OVMF, swtpm, QMP, or VNC sockets directly.
- Windows may be installed once, sealed by exact content identity, and reused
  through small per-instance linked-clone overlays with private agent secrets.
- The default device model uses Windows inbox NVMe, e1000e, and VGA drivers.
  Signed VirtIO acceleration will remain optional.
- The beta.7 `vanilla` and `slim` profiles retain the driverless installation
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
- `lsw run --gui` additionally needs a native Wayland session. The beta.8
  release gate uses WSLg from an interactive Windows sign-in; terminal-only
  install, exec, and recovery remain headless. X11 is a later compatibility
  target, not a fallback in the Slice 4 correctness path.
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
lsw install win-dev --accept-windows-license --defer-user-setup
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
7. In an interactive terminal, asks for a permanent Windows desktop user, a
   masked password, and an account role. Personal development instances
   recommend administrator membership while retaining filtered normal tokens
   and UAC. Automation must explicitly defer this step and may later use
   `lsw user setup --username USER --password-stdin` with an optional
   `--administrator`.
8. Separately offers the recommended `~/LSW` integration, showing the exact
   read-write host root before consent. If accepted, it performs one normal
   guest restart and maps only that directory as agent-session `Linux (L:)`.

The terminal UI renders byte-accurate bars for native ISO transfer,
range assembly, SHA-256 verification, and DISM percentages. WinPE boot,
specialize, OOBE, agent installation, and cleanup instead display their real
named stage and elapsed time; LSW does not invent percentages for work that
Windows does not expose measurably. Redirected output emits bounded progress
updates suitable for CI logs.

Real Windows/KVM measurement found that fast WIM compression shortened export
but made total preparation slower because the larger intermediate increased
later mount and commit work. LSW therefore retains maximum compression and
uses the private NTFS scratch directory explicitly. An offline AppX marker
prevents first boot from repeating provisioned-package work. CompactOS runs in
the named Windows `applying-profile` stage after a reliable non-compact image
apply; this avoids making target-disk creation depend on DISM's non-deterministic
CompactOS-on-apply path. Exact media SHA-256 and `/CheckIntegrity` remain
mandatory.

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
lsw run --gui -- notepad.exe 'L:\notes.txt'
lsw app install --title Notepad -- notepad.exe
lsw push ./main.rs 'C:\src\main.rs'
lsw push --recursive ./project 'C:\src\project'
lsw sync --watch ./project 'C:\src\project'
lsw pull 'C:\src\build\app.exe' ./app.exe
lsw user promote win-dev       # UAC remains enabled
lsw user demote win-dev
lsw user add win-dev --username rescue-admin --administrator
lsw sudo status win-dev
lsw sudo enable win-dev        # native Windows sudo, new-window mode only
```

Interactive installation recommends making the confirmed desktop identity a
local administrator for a WSL-like personal development experience, but keeps
a standard-account choice. `lsw user setup` remains standard unless
`--administrator` is explicit, and an existing default user can be changed with
`lsw user promote` or `lsw user demote`. Normal applications still receive a
filtered token and Windows UAC remains the elevation boundary. The password
never enters argv, environment, the manifest, installation media, logs, or
diagnostic bundles. The unprivileged agent forwards one authenticated loopback
frame to the demand-start LocalSystem `LSWUserHelper`; that helper performs one
bounded native account operation and exits. AutoLogon remains disabled. Session
0 CLI commands continue to use the boot-time service identity. `run --gui`
uses an authenticated, on-demand companion in an already signed-in registered
user's Windows session; it never turns on AutoLogon or stores the user's
password. If no such session exists, LSW asks the user to open `lsw view` and
sign in once.
`lsw user add [NAME] --username USER --administrator` can create a separately
confirmed administrator without changing the default desktop identity.

On Windows 11 24H2 or later, an interactive administrator setup also offers to
enable the inbox `sudo.exe` in its safer new-window mode. `lsw sudo
status|enable|disable [NAME]` keeps the choice explicit and reversible. LSW
does not install a third-party sudo replacement, offer inline/input-closed
modes, change managed policy, or disable UAC; Windows still presents consent
when an elevated process is requested.

`lsw exec` and ordinary `lsw run` wait and return guest exit codes 0–255
unchanged. Windows has a 32-bit exit-code space; when a value cannot be
represented by a Unix shell, LSW prints the exact unsigned decimal and
hexadecimal Windows value and returns 255. For controlled noninteractive
sessions, host `SIGINT` and `SIGTERM` terminate the owned Windows Job and return
130 and 143 respectively. `run --detach` returns the guest PID after a
successful start handshake, disconnects from its standard streams, and lets the
agent retain lifecycle ownership until the process exits.

`lsw run --gui [NAME] [--cwd PATH] [-e KEY=VALUE] -- PROGRAM.exe [ARG ...]`
starts a GUI program with the registered Windows identity and presents its first
visible top-level HWND as an undecorated native Wayland window. The captured
Windows non-client area supplies the only title-bar controls, so there is no
second host title bar. The window carries damage-only frames and forwards focus,
keyboard, pointer, wheel, all resize borders, guest-caption move/minimize/
maximize/restore/close actions, and guest exit state. Maximized surfaces
aspect-fit the guest frame with non-interactive black letterboxing instead of
stretching it. `lsw app install`
discovers the EXE icon in Windows and writes a Linux `.desktop` entry;
`lsw app list` and `lsw app remove ID` manage those entries. Launchers accept
dropped files through `%F` and translate existing host paths below the approved
live-share root to `L:\...`. Slice 4 intentionally exposes only the first HWND;
multi-window ownership, DPI, clipboard, drag-and-drop, and UAC presentation are
later beta.8 slices.

Recursive transfer refuses host symlinks, guest reparse points, traversal, and
implicit overwrite. `sync` is intentionally host-to-guest and additive:
changed/new files are atomically replaced, while deletion on the host does not
delete the guest copy. `--watch` polls a bounded local snapshot every 750 ms and
retries failed changes.

The common file workflow uses the default instance and infers direction from
the absolute Windows path:

```bash
lsw share ~/LSW                    # live read-write Linux (L:)
lsw share                          # list configured shares
lsw cp ./build.zip 'C:\work\build.zip'
lsw cp 'C:\work\result.txt' .
lsw unshare linux                  # unmount; host files are preserved
```

The live folder is a real host-backed view over the VM's private QEMU
user-network SMB path; changes are visible in both directions without a sync
step. Fresh manifest-v8 instances authenticate with a domain-separated
credential derived from their private agent token, and the connection requires
SMB signing and encryption. Existing manifest-v7 instances retain their legacy
agent-token mapping until their guest agent is explicitly migrated, so a host
update does not break an already installed `L:` drive. It requires the host Samba
server executable but no Windows filesystem driver, public SMB listener, RDP,
WebDAV, anonymous guest login, or whole-home export. The first add and final
remove restart the guest normally because QEMU fixes the exported root when the
VM starts. The restricted agent maps `L:` inside its own Windows logon session
so `lsw` terminal commands and benchmarks use the live path without elevation.
The on-demand desktop companion creates the same ownership-checked mapping in
the registered user's independent logon session and restores it on the next GUI
or integration request after a session restart. LSW refuses to replace a
pre-existing unrelated `L:` mapping in either session.

The authenticated agent mirror remains available for offline hosts, staging,
recovery, explicit read-only ACLs, and automation:

```bash
lsw share add win-dev source ./project 'C:\Users\dev\source' --read-write
lsw share sync win-dev source
lsw share watch win-dev source              # additive host -> guest updates
lsw share sync win-dev source --from-guest  # explicit RW guest -> host merge
```

Read-only shares replace the guest root ACL with a protected allow-list:
SYSTEM, Administrators, and the LSW service retain inheritable FullControl,
while built-in Users receive only ReadAndExecute. Removing a share preserves
its guest files and ACL instead of silently loosening access. Both sides reject
symlinks/reparse points and parent traversal. Neither direction propagates
deletions.

`lsw bench files [NAME] --json` compares the live SMB and guest-local paths
using a 1 GiB sequential file, 4,096 small files, a metadata walk, optional
`git status`, and a deterministic hash-based build simulation. It also records
the equivalent agent-mirror dataset sync. `--size-mib` and `--small-files`
provide bounded CI dimensions; temporary host and guest data are removed even
after a failed run.

Create a pristine reusable base before registering a permanent desktop user:

```bash
lsw shutdown win-base
lsw image seal win-base
lsw image list
lsw image verify IMAGE_SHA256
lsw clone win-base win-dev
```

The image key covers the exact ISO, profile/preparation identity, agent,
firmware, and source disk. Sealing records the converted base's SHA-256;
`image verify` re-reads it explicitly while normal clone creation stays fast.
The sealed qcow2 is read-only; each clone receives a
linked overlay, fresh host token/control port, and a private boot identity
volume. The SCM agent rotates the embedded credential before listening or
atomically switches its live authenticator if Windows mounts the volume late.
The identity disk uses the Windows inbox IDE path, and the agent enumerates all
volume GUID paths as well as drive letters, so first-boot USB driver timing and
automount do not decide whether a clone can authenticate. Sealing performs one
preparation boot with the existing credential before rotating it, ensuring the
device is registered in the reusable Windows base. LSW also disables Windows
Fast Startup while preserving explicit hibernation, preventing a normal
shutdown from caching stale private-media contents into a linked clone.

Path conversion is explicit and syntactic; it does not make the independent
guest disk a host mount:

```bash
lsw path -w /mnt/c/Users/Jason/project
lsw path -u 'C:\Users\Jason\project'
```

Use `push`, `pull`, or `sync` when content must cross the VM boundary. Use the
approved live root when a GUI application must open a host file directly.

## Daily management

```bash
lsw config get win-dev
lsw config set win-dev memory.max=4GiB idle-timeout=10m
lsw logs win-dev
lsw logs win-dev --follow
lsw status win-dev
lsw suspend win-dev
lsw resume win-dev
lsw hibernate win-dev
lsw memory reclaim win-dev
lsw trim win-dev
lsw compact win-dev                 # stopped or hibernated instance
lsw shutdown --all
lsw diagnose win-dev --bundle
lsw remove win-dev
```

`lsw trim`, the fixed Windows hibernation transition, and the bounded native
shutdown fallback cross the privilege boundary through the authenticated,
demand-start `LSWMaintenanceHelper`. Each helper invocation remains one-shot;
the shutdown request is fixed, uses no `/f`, and is attempted only when Windows
has not exited 15 seconds after an ACPI powerdown. Live SMB setup does not cross
that boundary: the restricted `NT SERVICE\LSWAgent` process calls the fixed
Windows networking API, keeps the credential out of argv and the environment,
and owns its session's `L:` connection directly. The main agent token never
enters the desktop session; a domain-separated live-share credential is passed
only to the companion and removed from every GUI child's environment.

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

`memory.max` is applied to the next QEMU start. Manifest v5 also stores
`memory.min`, `idle-policy`, `idle-timeout`, and `hibernate-timeout`. Automatic
balloon, pause, and hibernate behavior is disabled by default; opt in with:

```bash
lsw config set win-dev memory.min=2GiB idle-policy=pause-hibernate \
  idle-timeout=10m hibernate-timeout=5m
```

The current policy measures inactivity at LSW's host control boundary, so it
should remain off for a VM operated primarily through the recovery viewer.

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

`slim` is the beta.7 default. Both profiles are embedded versioned declarative
manifests. They preserve WinSxS, Windows Update and the servicing stack,
MSI/MSIX, Defender, Terminal, PowerShell, ConPTY, Store, winget, WebView2, WMI,
hibernation, Recovery, and common development-tool dependencies. LSW does not
enable test signing or install a self-signed certificate by default. The
experimental `shell-light` profile remains beta.9 work; user-versioned custom
profiles are not part of the current release.

## Advanced commands

The following commands remain available for debugging, automation, and manual
recovery, but are not part of the beginner path:

```bash
lsw create NAME --iso PATH --accept-windows-license [OPTIONS]
lsw prepare NAME [--execute]
lsw seed NAME [OPTIONS] [--execute]
lsw plan NAME [--run]
lsw daemon <enable|disable|start|status|diagnose>
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

The beta.7 agent runs at boot as the automatic `LSWAgent` Windows service under
`NT SERVICE\LSWAgent`. Shell and `exec` processes therefore use that service
identity in Windows Session 0; they do not impersonate a desktop user. This
provides command access at the Windows sign-in screen without storing a daily
user credential.
The beta.8 companion is launched on demand through a fixed LocalSystem
WTS boundary and runs as the registered, already signed-in Windows user. It can
start GUI processes, own that user's `L:` mapping, and stream the first captured
HWND to a native Wayland window. Clipboard, audio, multi-HWND relationships,
DPI transitions, drag-and-drop, and UAC presentation are not enabled yet.

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
lsw daemon enable
```

The CLI first connects to the private socket, so a listening systemd socket
activates `lswd` without a separate login-time daemon. With no active VM, the
daemon exits after 30 idle seconds and only the socket remains. `lsw daemon
disable` reverses the opt-in; direct CLI autostart remains available.

On Unix, a child enters a separate process group before `exec`; ordinary
descendants remaining in that group are reclaimed on exit, cancellation,
disconnect, protocol failure, or lease expiry. A process can deliberately
escape with `setsid` or `setpgid`, so this is lifecycle ownership, not a guest
security sandbox. On Windows, children start suspended and must enter a
kill-on-close Job Object before they are resumed.

## Windows/KVM end-to-end gate

Tagged releases fail closed unless their exact commit has passed the dedicated
Windows 11/KVM hardware gate with Microsoft's published ISO SHA-256. The gate
covers real WinPE DISM, unattended OOBE and cleanup, NetAPI user creation,
SCM/licensing identity, ConPTY, clone-secret isolation, folder boundaries,
balloon/TRIM/hibernate/compaction, full shutdown, no-login cold restart, and
complete runtime cleanup. Starting with beta.8, the same exact-SHA workflow run
must also pass an independent signed-in WSLg job on a separately labeled
interactive runner. That job attests the candidate CLI, active daemon, and
installed guest agent by SHA-256 before exercising the first-HWND input,
guest-only chrome, resize, maximize/restore, close-prompt, crash, and exact-window
reattach matrix. The signed-in job does not weaken or replace the headless
no-console-user gate; both are release requirements.

See [the operator workflow and evidence contract](docs/WINDOWS_KVM_E2E.md) and
[the detailed acceptance boundary](docs/BETA.md). Ordinary CI also runs bounded
firmware and product-lifecycle tests with placeholder media, but those tests do
not claim that Windows booted.

## Roadmap

Experience work now comes before architecture expansion. beta.7 delivers the
low-resource optional background runtime, linked clones, hibernation, user
registration, and safe host-folder foundation. beta.8 targets Linux desktop-native Windows application
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

- Tagged releases still require the dedicated KVM-capable release host. The
  guarded workflow resolves and verifies current official media automatically.
  beta.8 also requires a separate interactive WSLg runner with a pre-provisioned
  exact-candidate guest; ordinary GitHub-hosted CI cannot reproduce either
  Windows execution boundary.
- The optional installation and recovery display uses private Unix-socket VNC
  internally; LSW opens it only when requested and does not expose TCP VNC or RDP.
- Ordinary `lsw run` remains a Session 0 process. `lsw run --gui` starts in the
  registered user's existing Windows session and renders its first visible HWND
  as a native Wayland window. A bounded set of ordinary same-process owned
  dialogs is composited into that surface; X11 and independent host windows for
  additional top-level or transient HWNDs remain later-slice work.
- Signed VirtIO drivers are not bundled or silently installed. The balloon
  device and governor are available, but useful guest reclaim depends on a
  compatible signed Windows driver; the inbox NVMe/e1000e path remains valid.
- One driverless live read-write root can be mapped as `Linux (L:)` in the
  agent and registered desktop sessions per instance. Additional declared
  shares remain synchronized agent mirrors;
  host-to-guest watch is additive and conflicts/deletions are never resolved
  destructively in the background. Signed Virtio-fs is not required or enabled.
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
