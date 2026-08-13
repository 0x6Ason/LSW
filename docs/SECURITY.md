# Security model

## Trust boundaries

The Linux user account, QEMU process, guest OS, installation media, Windows
agent, and future display bridge are separate trust domains. A local LSW user is
trusted to control their own instances. Windows media and guest workloads are
untrusted inputs and receive no host filesystem share or display-server socket.

LSW beta does not yet launch QEMU under a separate service account, seccomp
profile, or namespace sandbox. Use current distribution security updates and do
not treat the VM as a hardened boundary against a hostile guest until that work
is complete.

## Host control plane

- State and instance directories are mode 0700; tokens, manifests, logs and
  firmware stores are mode 0600 on Unix.
- `lswd` listens on a mode-0600 Unix socket below a mode-0700 directory. It
  refuses to replace a non-socket path.
- The same Unix user can control all of that user's instances. There is no
  multi-user authorization server in this beta.
- Requests and responses are size-limited and escaped. Commands are parsed into
  fixed argument forms and are not passed through a shell.
- QEMU lifecycle uses QMP. A PID file is diagnostic state, not authority for
  sending signals to an arbitrary process.
- In-memory suspend uses QMP `stop`; resume requires the same reachable QEMU
  process in `paused` state. LSW does not serialize guest RAM or claim that a
  suspended instance survives QEMU or host termination.
- Storage preparation and Setup-seed creation reject symlink destinations and
  do not overwrite existing disks, firmware stores, seeds, or transferred files.

## Guest agent

Each instance receives a random 256-bit token from `/dev/urandom`. The host copy
never appears in `lsw show` or the manifest. Setup copies the guest copy to
`ProgramData\\LSW` with an ACL for the installing user, Administrators, and
SYSTEM.

The QEMU host forward binds only to `127.0.0.1`. The Windows firewall rule allows
guest port 5040 only from the QEMU user-network host address. Authentication is
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
fails closed. The Windows behavior has a native CI gate, but was only
cross-built—not executed—on this Linux VPS.

Agent powers are intentionally broad after authentication: it can execute as the
logged-in Windows user and read or create files that user can access. Protect the
host token and state backups accordingly. Transfers reject symlinks and existing
destinations and verify declared byte counts before committing a temporary file.

## Network policy

`nat` is the create default: QEMU user networking permits guest egress. The
agent port is always forwarded to host loopback; an instance may additionally
request repeatable TCP mappings with `--publish HOST:GUEST`. Every published
listener is explicitly bound to `127.0.0.1`. Validation rejects zero or
duplicate host ports, collision with the reserved agent port, collision with
another instance, ports already bound by another local process at creation
time, and all publishing in `offline` mode.

`offline` sets QEMU `restrict=on`, blocking normal guest egress while retaining
the host-only agent forward. Neither mode exposes a host directory. Loopback is
an exposure boundary, not application authentication: treat a published guest
service as untrusted, and do not re-export it to a LAN or the Internet without a
separately reviewed security design.

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
| `standard`, `slim`, `ephemeral` | Off | Permitted by profile, but not enabled or installed by beta |
| `secure` | On | Forbidden; driverless integrations only |

The secure profile requires a key-enrolled OVMF variable template and SMM-backed
flash protection. An empty OVMF store does not become secure merely because a
QEMU flag is present, so `LSW_OVMF_SECURE_CODE` and `LSW_OVMF_SECURE_VARS` must
point to the distribution's correct files.

LSW beta does not generate a certificate, enable Windows test signing, install a
root certificate, or ship a custom kernel driver. Any later developer-driver
workflow must require an explicit guest-only enrollment, keep private keys out
of shared images, and retain a driverless secure profile.

## Remaining security work

- Run QEMU/swtpm with a reduced host privilege and platform sandbox policy.
- Add encrypted transport before supporting any non-local agent connection.
- Harden Unix containment against deliberate process-group/session escape, and
  exercise both Unix groups and Windows Job Objects with longer-running native
  stress tests.
- Threat-model clipboard, host folders, USB and per-HWND input before enabling
  those integrations.
- Fuzz PE parsing and the ConPTY/resize/session-control protocol in addition to
  malformed agent and QMP traffic.
- Add signed release provenance and a documented vulnerability-reporting route.
- Exercise agent, QMP, suspend/resume, port forwarding, and ConPTY with
  long-running soak tests on a real Windows guest.
