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

Agent powers are intentionally broad after authentication: it can execute as the
logged-in Windows user and read or create files that user can access. Protect the
host token and state backups accordingly. Transfers reject symlinks and existing
destinations and verify declared byte counts before committing a temporary file.

## Network policy

`nat` is the create default: QEMU user networking permits guest egress, while
only the agent port is forwarded to host loopback. `offline` sets QEMU
`restrict=on`, blocking normal guest egress while retaining the host-only agent
forward. Neither mode exposes a host directory.

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
- Threat-model clipboard, host folders, USB and per-HWND input before enabling
  those integrations.
- Add signed release provenance and a documented vulnerability-reporting route.
- Exercise malformed agent/QMP traffic with fuzzing and long-running soak tests.
