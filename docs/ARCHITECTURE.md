# LSW architecture

## Product model

LSW exposes a container-like local developer workflow for an operating system
whose kernel cannot be shared with the host. Windows-on-Linux therefore uses a
QEMU/KVM microVM as its isolation boundary. No component claims that Windows is
running as a Linux namespace container.

The delivered beta runtime supports a Linux x86_64 host and a Windows 11 x64
guest. Host platform and accelerator selection are separated from guest
protocols. Linux KVM capability detection is implemented; HVF and WHPX selection
and QEMU argument generation exist as planner-level work only. There are no
macOS or Windows host-side binaries, daemon IPC integrations, or runtime
validation claims on the beta.8 development line.

## Components

| Component | Beta state | Responsibility |
| --- | --- | --- |
| `lsw` | Implemented | One-shot media resolution/download, instance installation, default shell, process/file and activation commands |
| `lsw-host` | Host-client extraction implemented | Reusable Unix clients for private `lswd` IPC and authenticated agent control, process, GUI, and file sessions; contains no command parser or Wayland presenter |
| `lswd` | Implemented | User-private Unix API, QEMU/swtpm supervision, QMP reconciliation and shutdown |
| QEMU backend | Linux runtime implemented; firmware smoke passed | UEFI/vTPM microVM, KVM/TCG, private storage, loopback forwarding and recovery display |
| Backend selector | Planner implemented | KVM/HVF/WHPX/TCG selection and acceleration argv; only Linux KVM detection is wired to a delivered host |
| `lsw-agent.exe` | Implemented | Token authentication, concurrent pipe/ConPTY sessions, capability-gated control/leases, process ownership and file transfer |
| `lsw-protocol` | Agent wire extraction implemented | Dependency-free bounded frame, session, user, transfer, and GUI wire types shared by host and guest; `lsw-core` retains compatibility re-exports |
| ConPTY transport | Implemented; beta.6 exact KVM gate passed | Capability negotiation, console I/O, signals and terminal resize |
| PE inspector | Implemented | Bounded PE/COFF metadata, imports, JSON and conservative beta compatibility assessment |
| WinPE DISM backend | Integrated; beta.6 exact KVM gate passed | Runs two network-disabled WinPE microVM phases using the official media's `dism.exe`: prepare a profile WIM, then partition/apply it to the private target qcow2 |
| Image manager | Implemented | Content-addressed sealed qcow2 bases, linked overlays, and post-clone credential rotation |
| Resource governor | Implemented; opt-in | QMP balloon targets, host-pressure response, pause, Windows hibernate, resume, TRIM, and compaction |
| Folder-share mirror | Implemented; opt-in | Manifest-bound RO/RW roots over authenticated bulk transfer with additive change watch |
| Live Linux folder | Implemented; opt-in | One canonical host root exported by private QEMU user-network SMB and mapped as agent-session and registered-user-session Windows `L:` without a guest filesystem driver |
| User-session companion | Slice 3 implemented; Slice 4 source-gated | On-demand authenticated broker launched through a SID-checked WTS token; GUI ownership, first-HWND capture, icon discovery, desktop `L:`, and idle exit |
| Host compositor bridge | Slice 4 source-gated | First visible HWND mapped to an undecorated native Wayland window; focus, keyboard, pointer, resize, explicit window actions, and exact-HWND crash reattach |
| `lswg` | Slice 4.5 extraction implemented | Version-locked native Linux Wayland integration runtime, exact-hash checked, bundled with every standard release, and demand-started for GUI work |
| Fast graphics transport | CPU correctness path implemented | Bounded 128-pixel damage tiles over the authenticated channel; shared-memory or GPU acceleration remains optional future work |

## Source ownership

The current product boundaries are correct at the process level, but several
implementation files combine entry-point routing, protocol encoding, runtime
state, platform integration, and tests. Slice 4.5 makes the source layout match
the process model before the desktop matrix expands. It follows the applicable
WSL management pattern of separate client, service, host, guest, shared, and GUI
components while retaining LSW's own QEMU/KVM direction and non-RDP seamless
protocol.

The first Slice 4.5 migration is complete: the former 2,442-line
`lsw-core/src/agent_protocol.rs` now has focused modules in the dependency-free
`lsw-protocol` crate. Encoded bytes and protocol version remain unchanged, and
`lsw-core` keeps its existing public names while host consumers are migrated.
The second migration moved the former CLI-private daemon and agent clients into
`lsw-host`; process, GUI, transfer, and daemon transports are now separately
owned modules, while command parsing and user notices remain in `lsw`. The
third migration moved the former 2,001-line native presenter into the bundled
`lswg` process. Rendering, input, WSLg compatibility, orchestration, and tests
now have focused modules below 1,000 lines; `lsw` retains only the bounded
launch handoff and exact helper-integrity checks.

The target tree, migration order, bundled `lswg` contract, line-count limits,
and default-image acceptance measurements are specified in
[`SLICE_4_5_OPTIMIZATION.md`](SLICE_4_5_OPTIMIZATION.md). CI immediately applies
a no-growth ratchet to legacy files above 1,000 lines and rejects new oversized
Rust or automation files. The completed slice removes all production
exceptions; moving code without a single-responsibility boundary does not count
as completion.

## Lifecycle

1. `lsw install NAME` provides the beta.7 beginner path; `lsw create` remains an
   advanced primitive. Before a new instance is created or media is downloaded,
   an interactive terminal requires `[y/N]` Windows-license confirmation and
   noninteractive use requires `--accept-windows-license`. The installer then
   validates the requested shape and stores manifest v8 plus a random 256-bit
   per-instance agent token. Without `--iso`, it resolves the current official
   Windows 11 x64 media from Microsoft, downloads from an allowlisted HTTPS CDN
   with at most four connections, and verifies Microsoft's published SHA-256.
   `--iso` retains a local offline path. Version 1 and 2 manifests migrate with
   no published ports; version 3 manifests receive the default idle-timeout;
   version 4 migrates with all beta.7 policies and shares disabled; version 5
   adds users and mirrors; version 6 adds explicit account roles; version 7
   keeps every older share on the mirror transport; version 8 opts fresh
   installs into a scoped live-share credential while migrated guests retain
   their compatible legacy scheme.
2. The installer selects Windows 11 Pro by WIM metadata unless the user chooses
   another edition. A network-disabled WinPE microVM uses the official media's
   DISM to export and service a profile-specific WIM in a private workspace,
   stages the agent/unattend payload while that WIM is mounted, and commits it.
   A second network-disabled WinPE phase partitions only the instance disk,
   applies that WIM, creates UEFI boot files, and validates the resulting qcow2.
   After exact markers on private writable status volumes, LSW removes
   the workspace, ephemeral control ISO, and every token-bearing seed before
   marking the instance installed and starting its disk normally.
   Profile schema v2 makes every slim mutation typed. WinPE inventories exact
   provisioned AppX and optional-feature targets, removes matches, verifies the
   mounted image, and persists offline evidence. First boot then removes exact
   installed identities, applies the OneDrive/service/policy contract, verifies
   readback, and writes `C:\ProgramData\LSW\profile\report.json`. The profile
   revision is part of the sealed-image preparation identity.
3. `lswd` waits for the swtpm and QMP Unix sockets before reporting success. It
   owns child handles while running and reconciles a surviving VM through QMP
   after daemon restart.
4. A normal guest shutdown leaves the base disk intact. Old preview manifests
   using the retired `ephemeral` selector retain their disposable-overlay
   behavior, but new beta.7 installs expose only `vanilla` and `slim`.
5. During the first boot's `specialize` pass, the pre-applied unattend installs
   the agent as the automatic `LSWAgent` Windows service under the virtual account
   `NT SERVICE\LSWAgent`. It removes the legacy per-user startup entry and
   restricts the guest token to SYSTEM, Administrators, and that service
   identity. Supported OOBE settings create a random one-shot `LSWSetup` user
   without automatic logon. `SetupComplete.cmd` removes that user, cached answer
   files, its own script, and the staging payload before writing the completion
   marker that the host waits for. Commands and ConPTY sessions therefore run
   in Windows Session 0 without storing a daily user's password or requiring login.
   After the service is ready, an interactive install registers a confirmed
   permanent desktop account and recommends local administrator membership for
   a personal development VM while retaining an explicit standard-account
   choice. The normal agent forwards one authenticated loopback request to the
   demand-start LocalSystem `LSWUserHelper`, which calls native Windows NetAPI
   and exits. Capability-gated promote and demote operations use the same
   one-request boundary, while `lsw user add` creates an additional confirmed
   account without changing the manifest default; old manifests migrate as
   standard without changing the guest. Automation must defer or invoke the
   stdin-only recovery path. Normal
   applications remain filtered, UAC remains enabled, and AutoLogon is never
   enabled. GUI requests ask the same fixed helper to launch the installed
   desktop companion with the existing registered user's active WTS token. The
   helper verifies the token SID and requires an unelevated or linked limited
   UAC token, accepts no executable arguments, and fails closed if the user has
   not signed in. The companion uses scoped credentials, launches GUI children
   as that user, captures only the selected HWND through documented Windows
   Graphics Capture, and exits when idle. `gui-window-v3` sends bounded BGRA
   damage tiles to an undecorated native Wayland presenter and accepts only
   identity-checked focus, key, pointer, resize, drag-hint, and explicit window
   actions for that owned HWND. Abnormal presenter loss releases injected input
   and retains the exact HWND in a bounded registry for reattach.
   A separate, demand-start LocalSystem helper accepts one authenticated guest-
   loopback request, performs only bounded WMI licensing operations, and exits.
   Storage retrim, Windows hibernation, the fixed non-forcing shutdown fallback,
   and native Windows-sudo inspection use the same boundary through
   `LSWMaintenanceHelper`. Sudo configuration accepts only the protocol's
   boolean disabled/new-window request, refuses managed policy, verifies native
   registry readback, and preserves UAC. The helper exits after one LocalSystem
   operation.
6. Bare `lsw` resolves the default instance and requests `pwsh.exe`, `pwsh`,
   Windows PowerShell, then `cmd.exe`/`cmd` in order. When both ends advertise
   ConPTY and host stdin is a terminal, the host enters raw mode and forwards
   resize events; otherwise the established pipe session remains available.
   When both ends advertise `session-control-v1`, the host prefixes the start
   request with per-session options and uses distinct stdin-close and cancel
   frames. When both ends also advertise `session-lease-v1`, the host then sends
   a bounded lease before the start request and heartbeats while the process is
   live. Older peers retain the version-one half-close behavior.
7. `lsw suspend` sends QMP `stop`; `lsw resume` sends `cont` for a live paused
   process. `lsw hibernate` briefly resumes a paused guest when necessary,
   authenticates a Windows hibernate request, forwards only that fixed request
   to the demand-start helper, and records `hibernated` only after QEMU exits.
   Resume then performs a normal boot from the Windows hiberfile. The opt-in
   idle policy balloons, pauses, and later hibernates.
   Live-folder reconfiguration first asks QMP for an ACPI powerdown. If Windows
   is still running after 15 seconds, the authenticated agent asks the
   maintenance helper for the fixed native shutdown operation; the original
   bounded deadline remains authoritative and open applications are not forced.
8. A pristine stopped/hibernated instance can be sealed. A clone uses a qcow2
   backing overlay and read-only FAT identity volume; the SCM agent rotates the
   embedded token before binding its listener or atomically updates its live
   authenticator if Windows mounts the volume after SCM startup. Removable
   volume GUID enumeration covers media that has no drive letter, and bounded
   slow polling covers delayed Plug and Play arrival without delaying ordinary
   boots. Permanent users, shares, and published ports are never copied into a
   clone manifest.

Instance state is stored under `$LSW_STATE_DIR` or, by default,
`$HOME/.local/share/lsw`:

```text
instances/NAME/
  instance.lsw          non-secret manifest
  agent.token           private host copy
  disk.qcow2            persistent base disk
  OVMF_VARS.fd          private guest firmware variables
  seed/                 transient install payload; removed before normal boot
  winpe-seed/           transient prepare control seed
  winpe-apply-seed/     transient apply control seed
  swtpm-state/          vTPM state
  run/                  QMP/VNC/swtpm sockets, retained WinPE logs/status,
                        and transient control media during installation
images/SHA256/
  image.lsw             exact non-secret identity and source/base SHA-256 values
  base.qcow2            mode-0400 sealed base
```

## Control and guest protocols

The daemon protocol is newline-delimited, versioned by `PING`, bounded, and
available only on a mode-0600 socket in a mode-0700 directory. `lswd` can bind
that socket directly or accept exactly one PID-scoped descriptor through the
systemd activation environment; an inherited descriptor must resolve to the
same private pathname before use. Mutations are
strict commands (`START`, `SUSPEND`, `RESUME`, `HIBERNATE`, `ACTIVITY`,
`BALLOON`, `STOP`) rather than shell strings.
QEMU state is read and changed through negotiated QMP commands, never by
trusting a PID file.

The agent protocol uses a five-byte binary frame header, an 8 MiB frame limit,
explicit UTF-8 string lengths, a protocol version, and constant-time comparison
of the per-instance token. Capability strings negotiate ConPTY, terminal resize,
`session-control-v1`, `session-lease-v1`, `process-environment-v1`,
`detached-run-v1`, `session-signal-v1`, `power-hibernate-v1`, and
`user-account-v1` without breaking older pipe-only
agents. Host forwarding binds to `127.0.0.1`; commands and file payloads are
binary-safe. Upload and download destinations are never overwritten implicitly.

For an opted-in controlled session, `STDIN_CLOSE` drops only the child's input
handle so it can observe EOF and exit normally. Authenticated `SESSION_CANCEL`
terminates the owned process group/Job and reports exit code 130; the negotiated
cancel-on-disconnect option also asks the agent to clean up that set when the
connection disappears. With a legacy peer, TCP write-half-close continues to
mean stdin EOF. These additions are append-only frames under protocol version
one and are advertised by capability.

`session-lease-v1` is valid only after controlled-session options and before the
start frame. The encoded timeout is strictly bounded to 1–300 seconds. LSW's
standard client requests 120 seconds and sends a heartbeat every 30 seconds;
expiry closes both directions of the socket before output bridges are joined
and reclaims the owned process group/Job. Capability negotiation leaves older
or inconsistently advertising peers on the legacy path.

Process environment and detach frames are valid only in the controlled-session
preamble. Environment payloads are bounded, reject NUL and `=`, and treat names
case-insensitively. Detached mode is limited to `RUN`, nulls standard streams,
and returns a bounded process ID only after the Windows child has entered its
Job. Signal frames are authenticated session-control messages; interrupt and
terminate map to exact status 130 and 143 after the owned process tree stops.

Recursive transfer is composed from bounded single-file protocol operations.
Remote directory discovery runs a fixed PowerShell program with paths supplied
only through the validated environment frame; paths are emitted as hexadecimal
UTF-8 fields so whitespace cannot change framing. Watch sync uploads a unique
temporary peer and renames it into place, which keeps each observed file update
atomic without defining destructive mirror semantics.
Manifest-bound folder shares reuse this channel. RO and RW are explicit; RO
roots receive a protected allow-list ACL. SYSTEM, Administrators, and the SCM
identity retain inheritable FullControl while built-in Users receive only
ReadAndExecute. Guest-to-host RW import is explicit, and neither direction
propagates deletions. Removing a share preserves its guest ACL. Every existing
host component rejects symlinks and every existing guest component rejects
Windows reparse points.

Manifest version 7 distinguishes the existing `mirror` transport from one
`live-smb` root. Older manifests migrate every share to `mirror`. On a normal
run, the planner re-canonicalizes the live root, rejects symlinks, and adds it
only to that instance's QEMU user-network backend. The restricted Windows agent
uses `WNetAddConnection2W` with the fixed `L:` and private SMB path, then clears
its mutable credential buffer. The resulting mapping belongs to the agent's
Windows logon session and is inherited by agent-launched terminal tools. The
Slice 3 companion performs the same operation in the registered user's active
session, so Explorer and file dialogs receive an independent mapping. Fresh
manifest-v8 guests use a live-share credential domain-separated from the main
agent token; manifest-v7 and older guests retain the legacy credential expected
by their installed agent. Authenticated unmount ownership-checks and removes
only LSW's mapping.
Adding or removing the root performs a graceful restart because the QEMU SMB
root is immutable after launch.

On Unix the child enters a new process group in the pre-`exec` path. LSW signals
that group after a normal leader exit and on cancellation, disconnect, protocol
failure, or lease expiry, preventing ordinary background descendants that
inherit output pipes from retaining a session. A process can deliberately use
`setsid` or `setpgid` to escape, so the group is lifecycle containment, not a
security boundary. On Windows, both pipe and ConPTY leaders are created
suspended, assigned to a kill-on-close Job Object, and resumed only after
successful assignment. A restrictive nested-job policy therefore fails the
session closed rather than starting an unowned child. The Windows GNU gate
cross-builds the executable; Windows-native CI separately covers Job descendant
cleanup and ConPTY setup.

## Headless runtime smoke boundary

On the Codex VPS, Ubuntu's QEMU 8.2.2, OVMF and swtpm packages were staged in a
temporary root because they were not preinstalled. With TCG, the test created
and checked a qcow2 disk, entered OVMF, observed vTPM command traffic, drove TCP
QMP through `stop`, `cont` and `quit`, and connected to two loopback-only usernet
host-forward endpoints targeting guest ports 35040 and 8080. Both host ports were
released after QMP quit, and QEMU and swtpm exited with status zero. CI now
repeats a timeout-bounded version of this firmware-level smoke with distribution
packages. A second, non-skippable CI gate uses the actual `lsw`
manifest/preparation/planner path and `lswd` with placeholder media. It checks
the planned OVMF, NVMe, e1000e, vTPM and exact loopback hostfwd topology, then
executes install, start, status, suspend, resume and forced stop through product
interfaces while checking QMP and port release.

That environment has no `/dev/kvm`, licensed Windows ISO, graphical desktop, or
pathname Unix sockets (AF_UNIX `bind` returns `EPERM`). The firmware smoke
therefore validates the QEMU building blocks locally, while the product daemon's
Unix-socket lifecycle is a CI-only gate in this environment. Neither gate
validates KVM acceleration, WinPE DISM execution, unattended Windows OOBE, the installed
service agent, the licensing helper, or ConPTY end to end. Those are required
by the dedicated real Windows/KVM release gate.

## Network publishing

QEMU user networking always forwards the private per-instance agent port to
host loopback. A manifest may additionally contain repeatable TCP mappings from
`--publish HOST:GUEST`; these are rendered as loopback-only QEMU `hostfwd`
entries. Validation rejects persisted port zero, duplicate host ports, the instance's
reserved agent port, collisions with another instance or an already-bound local
listener at creation time, and publishing in `offline` mode. This feature does
not expose UDP, bind a LAN address, or provide transport authentication for the
guest application.

At the CLI boundary, `auto:GUEST` and `0:GUEST` ask the kernel for an available
loopback port before manifest validation; the selected nonzero value is then
persisted and subjected to the same collision checks as an explicit port.

## PE inspection

`lsw inspect` is a host-side parser and does not execute the inspected file. It
caps input at 512 MiB and bounds PE headers, section-backed RVAs, import tables,
and strings before reporting architecture, subsystem, CLR and certificate-table
presence, sections, and imports. Its assessment is advisory: certificate-table
presence is not Authenticode verification, and compatibility still depends on
the installed guest and application dependencies.

## Display design

The beta explicitly adds standard VGA because `-nodefaults` removes QEMU's
implicit adapter. A VNC server is bound to a private Unix socket for Windows
Setup and recovery only; there is no TCP VNC listener and no RDP dependency.

The seamless path remains driverless first:

1. The Slice 4 user-session companion discovers the first visible top-level
   HWND and captures it through documented Windows Graphics Capture.
2. The authenticated `gui-window-v3` channel carries window metadata, bounded
   changed tiles, position-bound non-client drag hints, and explicit window
   actions. A normal close uses an acknowledgement handshake; an abnormal
   disconnect releases input and retains only the exact selected HWND for
   bounded reattach.
3. The Linux client creates an undecorated native Wayland surface and explicitly
   maps focus, keyboard, pointer, move, all resize borders, minimize, maximize,
   restore, and close. When host and guest aspect ratios differ, it aspect-fits
   the guest frame into a centered viewport and does not forward input through
   the black letterbox. Slice 5 expands this to all eligible HWNDs and DPI
   behavior; Slice 6 adds host clipboard integration and files. X11 remains a
   later compatibility matrix.
4. UIPI, UAC and elevated-window boundaries are reported, not weakened.

An optional guest-only accelerator may later reduce copies. It cannot be a
requirement for the driverless path and would need an independently reviewed
signing/enrollment workflow.

## Performance priorities

- KVM with host CPU passthrough; TCG is a diagnostic fallback.
- Windows-inbox NVMe, e1000e and VGA for installability before optional drivers.
- qcow2 overlays for disposable runs without modifying the base disk.
- Separate control, process/file, and future graphics data paths.
- Damage-based window transport rather than full-desktop polling.
