# Windows/KVM release gate

The real Windows gate validates the release candidate on an explicitly
dedicated Linux x86_64 KVM runner. It resolves Microsoft's current English x64
ISO metadata on a GitHub-hosted authorization job, transfers only the private
short-lived CDN request, downloads the payload directly from Microsoft on the
KVM runner, requires its exact published SHA-256, and runs the network-disabled
WinPE DISM prepare/apply path.
The normal boot then completes OOBE without a viewer, desktop session, or guest
input. The gate verifies that the one-shot setup account, cached answer files,
SetupComplete script, and staging payload are removed before accepting the
completion marker. It checks the exact automatic `LSWAgent` Windows service
configuration and virtual-account process SID, PowerShell command execution,
guest exit-code propagation, graceful shutdown, an interactive ConPTY shell,
QEMU/daemon cleanup, socket cleanup, and host-port release. The release job
exercises the `slim` profile, creates a permanent standard user through the
authenticated one-shot NetAPI helper, verifies explicit promotion, demotion,
and final administrator membership, seals and boots a linked clone with an
isolated secret, checks RO/RW mirror escape boundaries, mounts a driverless
live root as agent-session `Linux (L:)`, requires the private server's signing
and encryption policy, proves immediate bidirectional visibility without
retaining the privileged maintenance helper, inferred `lsw cp`, benchmark JSON,
and unshare preservation,
balloon/TRIM/hibernate/compaction, and cold-starts the installed guest
through a bare `lsw`, proves ConPTY and service-backed agent execution return at
the Windows sign-in screen, requires the same service SID, and verifies that
neither the ISO nor the seed is attached to the restarted QEMU process.

The gate also requires both private-volume WinPE completion markers, proves that the
workspace and all token-bearing seeds were removed, queries WMI license status
without a key, and verifies that `LSWLicenseHelper` returns to a stopped,
demand-start LocalSystem state while `LSWAgent` remains the narrow virtual
account.

The workflow dispatch and protected-environment approval are deliberately
manual because GitHub-hosted runners do not expose KVM. Guest setup itself is
fully headless. A successful ordinary CI run is not a substitute for this gate.

beta.8 adds a second, independent job to the same exact-SHA workflow run. The
headless KVM job keeps its no-console-user invariant. The separate signed-in
WSLg job exercises the seamless-window matrix against a pre-provisioned
candidate instance; neither job can substitute for the other. New release tags
require both jobs to complete successfully in the same workflow run.

## Security boundary

Use a dedicated or ephemeral runner that holds no unrelated credentials or
workloads. Do not attach the label below to a general-purpose self-hosted
runner. The headless job accepts only:

- a manual dispatch from `master` in `0x6Ason/LSW`;
- an exact, operator-entered 40-character candidate commit;
- the protected `windows-kvm-e2e` GitHub environment; and
- a runner with all four labels: `self-hosted`, `Linux`, `X64`, and
  `lsw-windows-kvm-e2e`.

The signed-in GUI job has a separate protected `windows-seamless-e2e`
environment and accepts only a repository-scoped runner carrying
`self-hosted`, `Linux`, `X64`, and `lsw-windows-seamless-e2e`. Never attach both
custom labels to one runner service: one contract requires no Windows desktop
user, while the other requires one interactive Explorer and WSLg session.

The job has read-only repository permission, checkout does not persist its
credential, concurrent KVM runs are serialized, and the job has a 360-minute
timeout. The harness reserves five hours for the complete official-media DISM,
Setup/OOBE, agent, ConPTY, and cold-restart validation, then adds a bounded
twenty-minute orchestration margin and a five-minute TERM-to-KILL cleanup
window. This leaves the job time for builds, evidence upload, and final cleanup.
Only maintainers should be allowed to edit or dispatch this workflow.

Configure the `windows-kvm-e2e` repository environment with required reviewers
and a deployment-branch rule limited to `master`. Do not put credentials in
that environment. If organization runner groups are available, restrict this
runner to this repository and, preferably, this workflow.

Apply the same reviewer, branch, repository, and workflow restrictions to the
`windows-seamless-e2e` environment and its separate runner. Neither environment
needs a repository secret.

Self-hosted workflows execute repository code. Review the candidate before
approval, never expose the runner to pull-request workflows, and isolate its
network and filesystem from production systems.

## Host preparation

Use a physical or nested-virtualization Linux x86_64 machine with at least 8
GiB of host RAM and at least 160 GiB of free SSD space for the release gate.
The preflight checks the filesystem containing `LSW_E2E_ROOT_BASE`; under WSL it
also checks the Windows volume mounted at `/mnt/c`, because that volume backs
the dynamically growing WSL virtual disk. The runner account must have
read/write access to `/dev/kvm`, normally through the `kvm` group.

Install these dependencies before registering the runner:

- QEMU x86_64, `qemu-img`, OVMF, and swtpm;
- wimlib, xorriso, and a UDF-capable `7z`;
- Git, Python 3, aria2, GNU coreutils, and standard POSIX shell tools;
- rustup with Rust 1.76.0 and the `x86_64-pc-windows-gnu` target; and
- the MinGW-w64 x86_64 compiler.

On Debian or Ubuntu, the relevant packages include:

```sh
sudo apt-get install \
  aria2 coreutils gcc-mingw-w64-x86-64 git ovmf python3 qemu-system-x86 \
  7zip qemu-utils swtpm util-linux wimtools xorriso
rustup toolchain install 1.76.0 --profile minimal
rustup target add --toolchain 1.76.0 x86_64-pc-windows-gnu
```

Register a repository-scoped Actions runner and add the custom label
`lsw-windows-kvm-e2e`. Keep the default `self-hosted`, `Linux`, and `X64`
labels. Run it as an unprivileged account; the workflow never invokes `sudo`.
All dependencies must therefore be installed before dispatch. If the
distribution does not use one of LSW's standard OVMF paths, set both
`LSW_OVMF_CODE` and `LSW_OVMF_VARS` to readable absolute paths in the runner
environment. The preflight fails before installation if no complete firmware
pair is available.

The beta.8 headless KVM job may run as a background service and does not need
`DISPLAY`, `WAYLAND_DISPLAY`, a session bus, or `remote-viewer`.

### Signed-in WSLg runner

Provision the GUI job separately on a Windows 11 host with WSLg. Sign in to the
Windows desktop user that owns the WSL session, open an interactive WSL shell,
and start the repository-scoped Actions runner from that shell so it inherits
`WSL_DISTRO_NAME`, `WAYLAND_DISPLAY`, and `XDG_RUNTIME_DIR`. Do not run this
runner as a background Windows or Linux service. Add only the custom label
`lsw-windows-seamless-e2e` in addition to the default Linux labels.

Install Rust 1.76.0 with the Windows GNU target, MinGW-w64, ImageMagick, GNU
coreutils, and the Windows interop tools used by WSL (`powershell.exe` and
`wslpath`). Before dispatch, provision one disposable default LSW instance with:

- the registered Windows desktop user already signed in;
- exactly one non-service Explorer session;
- the instance already running with an agent built from the candidate commit;
  and
- the active `lswd` built from that same candidate commit.

Build those binaries from the reviewed candidate before starting the runner.
The workflow rebuilds them independently, hashes its Linux CLI and daemon plus
the Windows agent, verifies `/proc/<pid>/exe` for the active daemon, and requires
the installed `C:\Program Files\LSW\lsw-agent.exe` to match the candidate agent
SHA-256 before it stages a fixture. A stale daemon or guest agent therefore
fails closed instead of producing mixed-revision GUI evidence. The GUI driver
never installs, starts, stops, creates, or removes a VM; the operator owns this
disposable prerequisite and must remove it after the workflow.

The GUI driver also writes and reads a short probe through WSLg's system-distro
shared-memory mount before it contacts the VM. A stale VAIL mount can still
create a RAIL proxy HWND while every submitted frame remains transparent, so a
mount-presence check is not sufficient. If this preflight fails, first stop the
disposable LSW instance and every other workload running in WSL, then run
`wsl --shutdown` from Windows and restart the signed-in runner session. The
driver deliberately never performs that host-wide restart itself because it
would also stop unrelated distributions, containers, and background services.

Provision a short, dedicated, encrypted, disk-backed state directory. Long
Actions paths can exceed Linux's 107-byte AF_UNIX socket pathname limit after
LSW adds its instance and socket names. `/e` is the recommended path:

```sh
sudo install -d -o lsw-runner -g lsw-runner -m 0700 /e
```

Replace `lsw-runner` with the Actions runner account. The workflow requires
this directory to be owned by that account, have mode `0700`, use a filesystem
other than tmpfs/ramfs, and have a canonical absolute path no longer than 16
bytes.

For a WSL runner whose large state must live on `D:`, do not point the gate at
the inherited `/mnt/d/e2e` mount. A default DrvFS mount can report every
directory as mode `0777`, so `chmod 0700` alone cannot satisfy the ownership
boundary. Mount only the dedicated Windows directory at the short Linux path
with DrvFS metadata enabled. From PowerShell, while the runner is stopped:

```powershell
New-Item -ItemType Directory -Force -Path 'D:\e2e' | Out-Null
$runnerUid = (wsl.exe -- id -u).Trim()
$runnerGid = (wsl.exe -- id -g).Trim()
wsl.exe -u root -- mkdir -p /e
wsl.exe -u root -- sh -lc 'mountpoint -q /e && umount /e || true'
wsl.exe -u root -- mount -t drvfs 'D:\e2e' /e -o "metadata,uid=$runnerUid,gid=$runnerGid,umask=077"
wsl.exe -- chmod 700 /e
wsl.exe -- stat -c '%u %a' /e
```

The final command must print the runner UID and `700`. Add the equivalent
`drvfs` entry to that distribution's `/etc/fstab` if the mount must survive a
WSL restart; keep the same UID, GID, `metadata`, and `umask=077` options. Set
`LSW_E2E_ROOT_BASE=/e`, not `/mnt/d/e2e`. This uses the real `D:\e2e` files and
does not create another ext4 image or consume the distribution VHD for VM
state.

## Windows media

Set only the short E2E state root in the runner process environment:

```sh
export LSW_E2E_ROOT_BASE=/e
./run.sh
```

The authorization job builds the exact candidate's resolver, creates a mode
`0600` request containing an allowlisted 24-hour Microsoft CDN URL and the
published English x64 SHA-256, and transfers that small file as a one-day
Actions artifact. The URL is never printed. The KVM runner downloads the ISO
with at most four aria2 connections, verifies the exact SHA-256, marks the ISO
read-only, and keeps it below the isolated per-run build root. The final
always-run cleanup deletes the request, URL input, ISO, build output, VM disks,
vTPM state, and agent credentials. It never uploads the ISO or caches it.

The workflow retains only the headless harness's redacted `attestation.env`,
`doctor.txt`, `bench.json`, and `diagnose.tar.gz` plus the GUI job's candidate
hash attestation and seamless-matrix log for 14 days. Cargo and the Microsoft
media path require outbound network access; pre-warm Rust dependencies and
restrict other destinations if a tighter boundary is needed.

## Run the gate

1. Review `master` and record its full commit with `git rev-parse origin/master`.
2. Open **Actions > Windows/KVM release gate > Run workflow**.
3. Select `master`, enter the exact 40-character commit, and enter
   `RUN-WINDOWS-KVM-E2E` as the confirmation.
4. Approve both protected environments after confirming that the two dedicated
   runners satisfy their opposite login contracts. No further input is required
   during either job. The headless job owns WinPE, unattended OOBE, agent
   verification, cold restart, and cleanup; the GUI job owns only its temporary
   fixture and presenter artifacts.

The gate fails unless `lsw install` waits through the specialize-to-OOBE reboot
for the exact setup-complete marker. It then requires `LSWSetup` to be absent,
`Win32_ComputerSystem.UserName` to be empty, the cached unattend and staging
paths to be gone, and Winlogon to contain neither automatic logon nor a stored
`DefaultPassword`. A bare `lsw` must restore an agent-backed ConPTY shell from
the installed disk without installation media or an interactive console user.
The harness keeps its one explicitly attested daemon active every five minutes
during direct WinPE preparation and apply, which can exceed the daemon's
one-hour configurable idle ceiling on slower disks. Autospawn stays disabled,
and the keepalive process group is removed before the independent bare-`lsw`
cold-start check.
Before that restart, `lsw user setup --password-stdin` must create an enabled
account in the well-known local Users group without administrator membership or
AutoLogon. The gate then
requires promote, demote, and final promote operations to update both the native
Administrators group and manifest v6 without exposing a password prefix in LSW
metadata, seeds, or logs. A separately authenticated administrator must be
created without entering or replacing the manifest's default identity. The gate
also requires `lsw run --gui` to fail closed with an actionable sign-in request
while no console user exists; that exercises the fixed WTS launch boundary
without weakening the headless cold-start invariant. The gate then enables
native Windows sudo in new-window
mode, verifies the exact local registry DWORD and `EnableLUA=1`, disables it to
prove reversibility, and enables the safe mode again. The disposable official
image must not contain a machine sudo policy.

## Guest service contract

The `specialize` pass must register exactly one service named `LSWAgent` with
`StartMode=Auto`, `State=Running`, and
`StartName=NT SERVICE\LSWAgent`. All agent commands, including ConPTY sessions,
intentionally run in Windows Session 0 under that virtual service account; the
service does not impersonate a desktop user or store a daily user password.

It must also register `LSWLicenseHelper` as Manual/demand-start under
LocalSystem. The E2E status request proves that the agent SID can start it and
that the helper exits after one authenticated WMI query. No product key is used
by the release gate.

`LSWUserHelper` must independently be Manual/demand-start under LocalSystem.
The permanent-user request proves the virtual-account agent can start it over
its narrow service ACL and that it exits after each authenticated native account
operation. User creation supplies the password only in the bounded binary
protocol; role changes contain no password and are separately capability-gated.
GUI launch uses a third request kind that accepts only the registered username;
the helper must reject it when no matching active WTS token exists. Executable
arguments are sent only to the unprivileged per-user companion.

`LSWMaintenanceHelper` must likewise be Manual/demand-start under LocalSystem.
The TRIM and hibernate checks prove that the restricted agent can request only
fixed operations without gaining an arbitrary privileged command channel. A
live-folder restart first uses ACPI and, if Windows has not exited after 15
seconds, exercises the same one-shot helper with a fixed normal shutdown request
that does not force open applications.

Before the first shutdown, the harness resolves both the service process owner
and the command process identity, requires both to equal the translated
`NT SERVICE\LSWAgent` SID, and requires the SID to begin with `S-1-5-80-`.
After a full guest shutdown and bare-`lsw` boot, the harness repeats the service
configuration and identity checks and requires the same SID. It also requires
`Win32_ComputerSystem.UserName` to be empty and rechecks that Winlogon has
neither automatic logon nor a stored default password. This prevents a hidden
desktop login from making the cold-start test pass.

Session 0 remains sufficient for the command and ConPTY contract. The on-demand
user-session companion handles GUI process launch, icon discovery, the user's
live `L:` mapping, and the Slice 4 first-HWND capture path without changing that
boot-time service boundary. This headless gate deliberately remains at the
sign-in screen, so it tests fail-closed WTS selection. The independent signed-in
job validates the Slice 4 native Wayland window, guest-only chrome, focus,
keyboard and pointer input, resize, explicit window state, close prompts,
presenter crash recovery, and exact-HWND reattach. Clipboard and audio remain
later beta.8 work.

The signed-in matrix has a 30-minute harness budget inside a 45-minute job.
Each guest, presenter, and Windows-host operation retains its shorter bounded
timeout; the outer budget only allows the complete sequential matrix and
cleanup to finish on slower interactive runners.

This workflow and both harnesses define the required release evidence; source
presence alone is not a runtime claim. Tag beta.8 only after the exact candidate
completes both the headless KVM job and the independent signed-in WSLg job in one
successful workflow run.

Tag only the exact commit named in a successful run. If `master` changes, run
the gate again for the new commit. The release workflow rejects every new tag
after the existing beta.1–beta.7 tags unless it finds both successful jobs for
that exact SHA. The workflow summary records the validated SHA so the beta
release decision is auditable.

Per-run build output and guest state are deleted after either success or
failure. The harness records the exact temporary root so the workflow can make
a second bounded cleanup attempt if the harness is interrupted. A new gate
refuses to start when an older `lsw-e2e.*` directory exists, preventing stale
VM state from silently consuming the runner's disk. Diagnostics use only the
redacted uploaded evidence; the release workflow has no state-retention mode.
