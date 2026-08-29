# Security model

## Trust boundaries

The Linux user account, QEMU process, guest OS, installation media, Windows
agent, and future display bridge are separate trust domains. A local LSW user is
trusted to control their own instances. Windows media and guest workloads are
untrusted inputs and receive no host filesystem access unless the user approves
one exact live root; they receive no display-server socket.

LSW beta does not yet launch QEMU under a separate service account, seccomp
profile, or namespace sandbox. Use current distribution security updates and do
not treat the VM as a hardened boundary against a hostile guest until that work
is complete.

## Host control plane

- State and instance directories are mode 0700; tokens, manifests, logs and
  firmware stores are mode 0600 on Unix.
- `lswd` listens on a mode-0600 Unix socket below a mode-0700 directory. It
  refuses to replace a non-socket path. Socket activation accepts exactly one
  PID-scoped descriptor only when its pathname and permissions match that
  configured private socket.
- The same Unix user can control all of that user's instances. There is no
  multi-user authorization server in this beta.
- Requests and responses are size-limited and escaped. Commands are parsed into
  fixed argument forms and are not passed through a shell.
- QEMU lifecycle uses QMP. A PID file is diagnostic state, not authority for
  sending signals to an arbitrary process.
- In-memory suspend uses QMP `stop`; resume requires the same reachable QEMU
  process in `paused` state. Windows hibernation is a distinct authenticated
  request: the guest writes its hiberfile and powers off, after which QEMU has
  zero RSS. LSW does not implement cross-host migration.
- Storage preparation and install-seed creation reject symlink destinations and
  do not overwrite existing disks, firmware stores, seeds, or transferred files.
- `lsw media resolve --request-file` writes a create-only mode-0600 file and
  never prints its signed Microsoft CDN URL. The release workflow transfers
  that short-lived file as a one-day private artifact, validates its bounded
  allowlisted form, keeps the URL off process argv and logs, and deletes the
  request and verified ISO in its always-run cleanup.

## Guest agent

Each instance receives a random 256-bit token from the operating system. The
host copy never appears in `lsw show` or the manifest. Installation staging
copies the guest copy to
`ProgramData\LSW` with an ACL for SYSTEM, Administrators, and the
`NT SERVICE\LSWAgent` virtual service identity. No ACE is added for the
one-shot OOBE user; administrators retain access through the Administrators
group.

The pre-applied `specialize` pass registers `LSWAgent` as an automatic Windows
service. It runs in Session 0 under `NT SERVICE\LSWAgent`, without an automatic
desktop logon. OOBE receives a per-install random password that is independent
from the agent token and obfuscated with Windows unattend's `PlainText=false`
encoding, which is not encryption. SetupComplete removes the one-shot account,
cached answer files, its script, and the staging payload before the host accepts
the setup marker.

After cleanup, interactive installation creates a separately confirmed
permanent local account. `lsw user setup` validates the name locally and sends
the password through a dedicated authenticated frame. The normal virtual-account
agent starts `LSWUserHelper`, forwards the request over authenticated guest
loopback, and the demand-start LocalSystem helper calls `NetUserAdd`. Interactive
personal-development installation recommends administrator membership while
retaining a standard-account choice; every created account joins the well-known
local Users group, while deferred `lsw user setup` remains non-administrator
unless `--administrator` is explicit. Administrator membership uses the
well-known local Administrators SID and `NetLocalGroupAddMembers`.
Capability-gated `lsw user promote` and `lsw user demote` use the same one-shot
helper and `NetLocalGroupAddMembers` or `NetLocalGroupDelMembers`; reading an old
manifest never changes Windows membership. The helper exits after one request.
`lsw user add` uses the same credential and role boundary for an additional
account but never changes or replaces the manifest's default desktop identity.
Password bytes never enter argv, environment, LSW manifests, seeds, logs, or
diagnostics, and mutable protocol/UTF-16 buffers are cleared after use. Windows
retains its normal account verifier, normal administrator processes receive a
filtered token, UAC remains enabled, and Windows AutoLogon is never configured.

GUI launch uses a second fixed request on `LSWUserHelper`. LocalSystem accepts
only a validated local username, enumerates active WTS sessions, compares the
session token SID with that exact local account SID, and starts the fixed
installed `lsw-agent.exe --desktop-companion` command on `winsta0\default`.
It does not accept an executable or GUI arguments. If the registered user is
not already signed in, the request fails closed and asks for one interactive
sign-in; LSW does not cache a password or enable AutoLogon. The helper inspects
the token elevation type and elevation flag. If WTS supplies an elevated split
token, the helper selects and revalidates only its linked limited token; it
refuses any token that remains elevated, preserving UAC for administrator
accounts.

The companion binds only guest loopback port 35044 and authenticates with a
domain-separated credential derived from the per-instance agent token. The
main token is never readable by the desktop user. On fresh manifest-v8 guests,
a separate scoped credential authenticates the one approved SMB root. Both
scoped values arrive only in the companion environment and are explicitly
removed from every GUI and icon-helper
child. The companion also case-insensitively binds every request to its expected
username. It exits after 30 idle seconds when it owns no GUI child or live
mapping; an active mapping keeps the low-resource broker available.

The append-only `gui-window-v3` request selects only the first visible
top-level HWND owned by the launched process. The companion uses documented
Windows Graphics Capture without a picker, caps complete frames at 128 MiB,
splits output into bounded 128-pixel tiles, and validates every dimension and
payload length at the companion, service relay, and host client. The relay
accepts only focus, key, pointer, resize, position-bound drag-hint, and explicit
window-action frames for that session. Before each input action, the guest
revalidates the exact process, HWND identity, and foreground relationship; the
controlled `SendInput` path is needed for real non-client controls, menus, and
context menus. The presenter ignores black letterbox input and consumes each
recent drag hint only at its exact guest coordinate. Focus loss and disconnect
release every injected key and button. An abnormal transport loss does not
silently kill a dirty application: it retains at most 16 exact window identities
for authenticated reattach, while an acknowledged explicit close uses the
normal Windows close path. No public VNC, RDP, or LAN listener is enabled. Slice
4 does not cross UIPI or the UAC secure desktop; later slices must retain the
private-viewer fallback instead of weakening those boundaries.

Native Windows sudo is a separate capability-gated fixed operation. The normal
agent asks `LSWMaintenanceHelper` to read the inbox binary and the documented
local and policy registry values. LSW can set only disabled or new-window mode,
refuses every write when a machine policy is present, verifies the readback,
and exits the helper after one request. It never exposes the input-closed or
inline modes and never changes UAC. `sudo.exe` still requests Windows consent;
it is not an elevation bypass or a substitute for the beta.8 UAC UI boundary.

The QEMU host forward binds only to `127.0.0.1`. The Windows firewall rule allows
guest port 35040 only from the QEMU user-network host address. Authentication is
required before process or file requests, and the agent caps concurrent sessions
at 32 by default.

The beta channel is authenticated but not encrypted. Its intended path is the
local loopback-to-QEMU user network, not a LAN or Internet connection. Do not
forward the agent port externally. A future non-local transport must add channel
encryption and peer identity rather than relying on this token protocol alone.

After authentication, peers advertising `session-control-v1` can opt a process
session into explicit control semantics. `STDIN_CLOSE` delivers EOF without
termination; `SESSION_CANCEL` terminates the session-owned process group/Job and
returns exit code 130; `cancel-on-disconnect` releases that owned set when a
disconnect is observed. Before a process starts, an unprefixed control request, an invalid
options payload, or any request sent before authentication is rejected. Once a
legacy process has started, it keeps the version-one behavior rather than
gaining the new control semantics, including TCP half-close as stdin EOF.

Peers that additionally advertise `session-lease-v1` may send exactly one lease
between session options and the start frame. Timeouts are bounded to 1–300
seconds; the standard client requests 120 seconds and sends a heartbeat every
30 seconds. A heartbeat cannot revive an expired lease. Expiry shuts down the
socket in both directions before waiting for I/O bridges, then reclaims the
session-owned process group/Job. Peers without both capabilities retain legacy
behavior.

The process-environment capability rejects NUL, `=`, oversized payloads, and
case-insensitive duplicate names before spawn. Values are not logged or stored
by LSW, but they are visible to the guest process and to principals with normal
Windows process-inspection authority; environment injection is not a secret
store. Authenticated interrupt and terminate frames stop the owned Job and
return 130/143. Detached mode nulls standard streams but retains Job ownership,
so an agent or VM exit still terminates the detached process tree.

On Unix, LSW places the child in a fresh process group before `exec` and cleans
every process still in that group after normal leader exit, cancellation,
disconnect, protocol error, or lease expiry. A guest process can deliberately
escape with `setsid` or `setpgid`; process-group ownership is therefore not a
security sandbox or an absolute descendant boundary. This beta uses portable
`std::process` waits rather than Linux-specific pidfds/cgroups; after a normally
exited leader is reaped, group cleanup also has a very small numeric-PGID reuse
race. Do not treat Unix group cleanup as an adversarial containment boundary.
On Windows, pipe and
ConPTY children start suspended and must enter a kill-on-close Job Object before
they resume. Assignment failure, including an incompatible nested-job policy,
fails closed. The Windows behavior has both cross-build and native CI gates;
service-backed execution remains part of the dedicated Windows/KVM release
gate.

Agent powers are intentionally broad after authentication: it can execute
arbitrary processes as the `NT SERVICE\LSWAgent` virtual account and read or
create files that service identity can access. It does not impersonate a desktop
user. Protect the host token and state backups accordingly. Single-file
transfers reject existing destinations and verify declared byte counts before
committing a temporary file. Recursive traversal rejects host symlinks, guest
reparse points, and unsafe relative components. Explicit `sync` updates use a
unique guest temporary file plus atomic rename; the operation is additive and
never turns a host deletion into a guest deletion.

Advanced folder shares remain synchronized mirrors. Each mirror names one
canonical host directory, one absolute guest drive path, and an explicit RO/RW
mode. Existing components on both sides are checked during every walk: host
symbolic links and Windows reparse points fail the operation. RO roots receive a
protected allow-list ACL: SYSTEM, Administrators, and the exact SCM service SID
have inheritable FullControl, while well-known BUILTIN Users have only
ReadAndExecute. RW guest imports are explicit, no background mirror operation
propagates deletion, and removing a mirror does not silently loosen its guest
ACL.

One explicitly approved live root may instead use QEMU's user-network SMB
helper. The CLI canonicalizes it and rejects every symlink ancestor; QEMU
repeats that check at every launch so replacing the root cannot silently widen
access. The generated Samba endpoint is reachable only inside that VM's private
user network, not through a host TCP listener, and separate instances have
separate network stacks. LSW never substitutes the Linux home or filesystem
root. The share is deliberately read-write: the Windows guest is therefore
trusted to modify or delete content below the approved root.

A capability-gated request makes the restricted agent add, query, or remove the
fixed `L:` to `\\10.0.2.4\qemu` mapping in its own Windows logon session through
`WNetAddConnection2W` and `WNetCancelConnection2W`. It refuses an unrelated
existing `L:` mapping. No LocalSystem helper is involved, and no username,
password, path, drive letter, or command text is accepted from the client. For
a fresh manifest-v8 guest, the domain-separated private credential is passed
only in memory to the networking API and its mutable UTF-16 copy is cleared
immediately. An older manifest retains the credential scheme expected by its
installed agent rather than losing an existing mapping after a host upgrade.
The Samba endpoint requires signing and encryption. Removing the share unmaps
the drive and restarts QEMU without the host export; host files are preserved.
The authenticated interactive
companion performs the same fixed ownership check in the registered user's
logon session. Its per-instance scoped credential and VM-private network keep
unrelated instances from authenticating to that export.

Sealed bases are mode 0400 and content-addressed. Sealing rejects an instance
that already contains a registered permanent user. Clones do not inherit host
tokens, control ports, published ports, shares, or default-user metadata; a
private read-only boot volume rotates the copied in-guest credential before the
agent opens its listener.

## Network policy

`nat` is the create default: QEMU user networking permits guest egress. The
agent port is always forwarded to host loopback; an instance may additionally
request repeatable TCP mappings with `--publish HOST:GUEST`. Every published
listener is explicitly bound to `127.0.0.1`. `auto:GUEST` and `0:GUEST` are
resolved to a nonzero loopback port before persistence. Manifest validation rejects zero or
duplicate host ports, collision with the reserved agent port, collision with
another instance, ports already bound by another local process at creation
time, and all publishing in `offline` mode.

`offline` sets QEMU `restrict=on`, blocking normal guest egress while retaining
the host-only agent forward. Neither mode exposes a host directory by itself;
only an explicitly configured live root adds the private SMB endpoint. Loopback
is an exposure boundary, not application authentication: treat a published
guest service as untrusted, and do not re-export it to a LAN or the Internet
without a separately reviewed security design.

## Untrusted PE inspection

`lsw inspect` parses input without executing or loading it. It limits files to
512 MiB, caps aggregate inspected imports at 65,536 symbols and 16 MiB of names,
and checks PE header, section, RVA, import-table, CLR-header, and string bounds.
These checks reduce parser risk but are not a sandbox. Run current LSW builds
when inspecting adversarial binaries. A reported certificate table means only
that the PE data directory is present and in bounds; it does not verify an
Authenticode chain, signature, timestamp, or publisher identity.

## Secure Boot and drivers

Host Secure Boot is never modified. Guest firmware settings apply only to the
instance.

| Profile | Guest Secure Boot | Test-signed custom driver policy |
| --- | --- | --- |
| `vanilla`, `slim` | Off | No unsigned or test-signed custom driver is enabled or installed by beta.7 |

Old preview manifests named `standard` migrate to `vanilla`. Existing
`ephemeral` and `secure` manifests retain their old runtime and firmware
semantics when loaded, but those names cannot be selected for new beta.7
instances. The old secure mode still requires a key-enrolled OVMF variable
template and SMM-backed flash protection.

LSW beta does not generate a certificate, enable Windows test signing, install a
root certificate, or ship a custom kernel driver. The VirtIO balloon device is
optional and harmless without a compatible signed guest driver; the default
NVMe/e1000e install and recovery path never depends on it. Any later developer-driver
workflow must require an explicit guest-only enrollment, keep private keys out
of shared images, and retain a driverless path.

## Windows activation helper

The network-facing agent remains the virtual account `NT SERVICE\LSWAgent`.
Activation does not broaden that service to LocalSystem. The `specialize` pass
creates a second `LSWLicenseHelper` service with `start=demand`; its service ACL
grants the agent SID only query/start rights. The helper binds guest loopback
port 35041, requires the existing per-instance agent token, accepts one bounded
request, invokes the Windows WMI `InstallProductKey`/`Activate` methods, and
exits.

Product keys arrive through masked host input or `--key-stdin`, use stdin on the
authenticated agent session, and are zero-filled in mutable buffers after use.
They are never argv, environment variables, seed/base-image content, logs, or
diagnostic material. PowerShell runs the fixed, installed
`license-helper.ps1`; only the bounded action is passed as an argument and a
product key is supplied to that process through stdin. Helper stderr is
discarded so a failed WMI operation cannot echo a key.

## Windows account helper

Permanent-account creation follows the same least-privilege shape without
sharing the activation protocol. `LSWUserHelper` is Manual/demand-start under
LocalSystem, binds guest loopback port 35042, authenticates the per-instance
token, accepts exactly one bounded `USER_CREATE` frame, calls NetAPI, and exits.
Its service ACL gives `NT SERVICE\LSWAgent` query/start rights only. Password
buffers are cleared at every mutable protocol/native boundary; no child process
or shell receives the password.

## Windows maintenance helper

Guest TRIM, Windows hibernation, and the native shutdown fallback do not grant
administrative rights to the network-facing agent. `LSWMaintenanceHelper` is
Manual/demand-start under LocalSystem, binds guest loopback port 35043,
authenticates the per-instance token, and accepts exactly one empty
`MAINTENANCE_TRIM`, `MAINTENANCE_HIBERNATE`, or `MAINTENANCE_SHUTDOWN` frame. It
runs fixed `Optimize-Volume C -ReTrim`, `powercfg /hibernate on` plus `shutdown
/h`, or a normal `shutdown /s /t 0` request without `/f`, then exits. Its
service ACL gives `NT SERVICE\LSWAgent` query/start rights only; callers cannot
supply a command, path, drive, option, or PowerShell fragment.

## Remaining security work

- Run QEMU/swtpm with a reduced host privilege and platform sandbox policy.
- Add encrypted transport before supporting any non-local agent connection.
- Harden Unix containment against deliberate process-group/session escape, and
  exercise both Unix groups and Windows Job Objects with longer-running native
  stress tests.
- Threat-model clipboard, drag-and-drop, file dialogs, USB, multi-HWND
  relationships, and secure-desktop presentation before enabling them.
- Fuzz PE parsing and the ConPTY/resize/session-control protocol in addition to
  malformed agent and QMP traffic.
- Add signed release provenance and a documented vulnerability-reporting route.
- Exercise agent, QMP, suspend/resume, port forwarding, and ConPTY with
  long-running soak tests on a real Windows guest.
