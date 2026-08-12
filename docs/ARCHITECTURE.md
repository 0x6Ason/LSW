# LSW architecture

## Product model

LSW exposes a container-like local developer workflow for an operating system
whose kernel cannot be shared with the host. Windows-on-Linux therefore uses a
QEMU/KVM microVM as its isolation boundary. No component claims that Windows is
running as a Linux namespace container.

The beta supports a Linux x86_64 host and a Windows 11 x64 guest. Host backends
remain separated from guest protocols so WHPX, Hypervisor.framework, or another
OS can be added later without treating Windows boot and licensing rules as
universal policy.

## Components

| Component | Beta state | Responsibility |
| --- | --- | --- |
| `lsw` | Implemented | Instance management, installer seed, default shell, process and file commands |
| `lswd` | Implemented | User-private Unix API, QEMU/swtpm supervision, QMP reconciliation and shutdown |
| QEMU/KVM backend | Implemented | UEFI/vTPM microVM with private storage, loopback forwarding and recovery display |
| `lsw-agent.exe` | Implemented | Token authentication, concurrent pipe process sessions and file transfer |
| ConPTY transport | Not in beta | Terminal resize, console modes and full interactive TTY behavior |
| Host compositor bridge | Not in beta | One guest top-level HWND per Wayland/X11 host window |
| Fast graphics transport | Not in beta | Damage-aware frames, input, DPI and clipboard; optional shared-memory accelerator |

## Lifecycle

1. `lsw create` validates the requested shape and stores manifest v2 plus a
   random 256-bit per-instance agent token. It does not copy the ISO.
2. `lsw install` creates a read-only Setup seed if one does not exist, prepares
   qcow2 storage and a private OVMF variable store, starts swtpm, then starts
   QEMU in install mode. Guided installation is the default.
3. `lswd` waits for the swtpm and QMP Unix sockets before reporting success. It
   owns child handles while running and reconciles a surviving VM through QMP
   after daemon restart.
4. A normal guest shutdown leaves the base disk intact. An ephemeral instance
   uses a fresh qcow2 backing overlay for each run and removes that overlay only
   after QEMU has stopped.
5. At first administrative logon, the seed installs the agent in the interactive
   user's session and registers a per-user startup entry. This avoids placing
   GUI work in Windows Session 0.
6. Bare `lsw` resolves the default instance and requests `pwsh.exe`, `pwsh`,
   Windows PowerShell, then `cmd.exe`/`cmd` in order.

Instance state is stored under `$LSW_STATE_DIR` or, by default,
`$HOME/.local/share/lsw`:

```text
instances/NAME/
  instance.lsw          non-secret manifest
  agent.token           private host copy
  disk.qcow2            persistent base disk
  OVMF_VARS.fd          private guest firmware variables
  seed/                 read-only Setup files attached through QEMU vvfat
  swtpm-state/          vTPM state
  run/                  QMP, VNC, swtpm sockets and ephemeral overlay
```

## Control and guest protocols

The daemon protocol is newline-delimited, versioned by `PING`, bounded, and
available only on a mode-0600 socket in a mode-0700 directory. Mutations are
strict commands (`START`, `STOP`) rather than shell strings. QEMU state is read
and changed through negotiated QMP commands, never by trusting a PID file.

The agent protocol uses a five-byte binary frame header, an 8 MiB frame limit,
explicit UTF-8 string lengths, a protocol version, and constant-time comparison
of the per-instance token. Host forwarding binds to `127.0.0.1`; commands and
file payloads are binary-safe. Upload and download destinations are never
overwritten implicitly.

## Display design

The beta explicitly adds standard VGA because `-nodefaults` removes QEMU's
implicit adapter. A VNC server is bound to a private Unix socket for Windows
Setup and recovery only; there is no TCP VNC listener and no RDP dependency.

The intended seamless path remains driverless first:

1. A user-session agent discovers eligible top-level HWNDs and captures damaged
   regions through documented Windows graphics APIs.
2. An authenticated bulk channel carries per-window metadata and frames.
3. The Linux client creates native Wayland surfaces, with X11 fallback, and maps
   focus, input, resize, DPI and clipboard events explicitly.
4. UIPI, UAC and elevated-window boundaries are reported, not weakened.

An optional guest-only accelerator may later reduce copies. It cannot be a
requirement for the `secure` profile and would need an independently reviewed
signing/enrollment workflow.

## Performance priorities

- KVM with host CPU passthrough; TCG is a diagnostic fallback.
- Windows-inbox NVMe, e1000e and VGA for installability before optional drivers.
- qcow2 overlays for disposable runs without modifying the base disk.
- Separate control, process/file, and future graphics data paths.
- Damage-based window transport rather than full-desktop polling.
