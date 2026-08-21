# Windows/KVM release gate

The real Windows gate validates the release candidate on an explicitly
dedicated Linux x86_64 KVM runner. It resolves Microsoft's current English x64
ISO metadata, requires the operator-supplied read-only media to match that exact
published SHA-256, runs the network-disabled WinPE DISM prepare/apply path, and
checks the boot-time agent before normal OOBE and first login. It then checks
the exact automatic `LSWAgent` Windows service configuration and virtual-account
process SID, PowerShell command execution, guest exit-code propagation,
graceful shutdown, an interactive ConPTY shell, QEMU/daemon/viewer cleanup,
socket cleanup, and host-port release. The release job exercises the beta.5
beginner `slim` profile. It then closes the installation viewer, cold-starts
the installed guest through a bare `lsw`, proves ConPTY and service-backed agent
execution return at the Windows sign-in screen, requires the same service SID,
and verifies that neither the ISO nor the seed is attached to the restarted
QEMU process.

The gate also requires both private-volume WinPE completion markers, proves that the
workspace and all token-bearing seeds were removed, queries WMI license status
without a key, and verifies that `LSWLicenseHelper` returns to a stopped,
demand-start LocalSystem state while `LSWAgent` remains the narrow virtual
account.

The workflow is deliberately manual. GitHub-hosted runners do not expose KVM,
and the beta.5 harness requires an operator to complete OOBE in the private LSW
viewer. A successful ordinary CI run is not a substitute for this gate.

## Security boundary

Use a dedicated or ephemeral runner that holds no unrelated credentials or
workloads. Do not attach the label below to a general-purpose self-hosted
runner. The workflow accepts only:

- a manual dispatch from `master` in `0x6Ason/lsw`;
- an exact, operator-entered 40-character candidate commit;
- the protected `windows-kvm-e2e` GitHub environment; and
- a runner with all four labels: `self-hosted`, `Linux`, `X64`, and
  `lsw-windows-kvm-e2e`.

The job has read-only repository permission, checkout does not persist its
credential, concurrent KVM runs are serialized, and the job has a 90-minute
timeout. The harness itself has a shorter whole-run timeout around its 45-minute
Setup/OOBE deadline and five-minute graceful-shutdown deadline, leaving time
for build, evidence upload, and job cleanup. Only maintainers should be allowed
to edit or dispatch this workflow.

Configure the `windows-kvm-e2e` repository environment with required reviewers
and a deployment-branch rule limited to `master`. Do not put credentials in
that environment. If organization runner groups are available, restrict this
runner to this repository and, preferably, this workflow.

Self-hosted workflows execute repository code. Review the candidate before
approval, never expose the runner to pull-request workflows, and isolate its
network and filesystem from production systems.

## Host preparation

Use a physical or nested-virtualization Linux x86_64 machine with at least 8
GiB of host RAM and roughly 80 GiB of free SSD space for a disposable Windows
11 installation. The runner account must have read/write access to `/dev/kvm`,
normally through the `kvm` group.

Install these dependencies before registering the runner:

- QEMU x86_64, `qemu-img`, OVMF, and swtpm;
- wimlib, xorriso, a UDF-capable `7z`, and virt-viewer (`remote-viewer`);
- Git, Python 3, GNU coreutils, and standard POSIX shell tools;
- rustup with Rust 1.76.0 and the `x86_64-pc-windows-gnu` target; and
- the MinGW-w64 x86_64 compiler.

On Debian or Ubuntu, the relevant packages include:

```sh
sudo apt-get install \
  coreutils gcc-mingw-w64-x86-64 git ovmf python3 qemu-system-x86 \
  7zip qemu-utils swtpm util-linux virt-viewer wimtools xorriso
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

The beta.5 gate is attended. Start the runner interactively from a logged-in
Linux desktop session where `DISPLAY` or `WAYLAND_DISPLAY`, the session bus,
and `remote-viewer` work. A background service without access to that desktop
session will fail the preflight instead of starting an unreachable installer.

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

## Provision Windows media

Obtain an unmodified, properly licensed Windows 11 x64 ISO outside GitHub.
Place it outside the Actions checkout and runner temporary directory. The
runner account must be able to read the file but must not be able to modify it;
a root-owned read-only mount or file is recommended.

Set these variables in the runner process environment before starting the
runner:

```sh
export LSW_WINDOWS_ISO=/srv/lsw-media/Windows11.iso
export LSW_WINDOWS_ISO_SHA256=replace-with-the-64-character-sha256
export LSW_E2E_ROOT_BASE=/e
./run.sh
```

Use a canonical absolute path, not a symlink. Calculate the digest once from
the provisioned file with `sha256sum`, and compare it with an expected digest
obtained independently from Microsoft before configuring the runner. The
workflow variable enforces that reviewed value consistently; it does not by
itself prove that an arbitrary ISO is official. The preflight rejects missing,
workspace-local, writable, symlinked, or digest-mismatched media.

The workflow never downloads the ISO payload, copies it into the repository,
caches it, or uploads it as an artifact. It does contact Microsoft's resolver
and download page to obtain the current signed-link metadata and published
SHA-256. VM disks, vTPM state, and agent credentials
also remain on the dedicated runner. The workflow uploads only the harness's
redacted `attestation.env`, `doctor.txt`, `bench.json`, and
`diagnose.tar.gz` evidence for 14 days. Cargo may still use the network for
normal Rust dependencies; pre-warm the dedicated runner and apply an outbound
firewall if a fully controlled network boundary is required.

## Run the gate

1. Review `master` and record its full commit with `git rev-parse origin/master`.
2. Open **Actions > Windows/KVM release gate > Run workflow**.
3. Select `master`, enter the exact 40-character commit, and enter
   `RUN-WINDOWS-KVM-E2E` as the confirmation.
4. Leave **keep_state** disabled for normal release validation.
5. Approve the protected environment. After WinPE prepare/apply and the
   boot-time agent checks pass, attend the LSW viewer and complete normal
   Windows OOBE and the first administrative login.

After first login, the harness owns the remaining sequence. It closes the
viewer before the cold-restart check; do not perform a second manual sign-in.
The gate fails unless a bare `lsw` restores an agent-backed ConPTY shell from
the installed disk without installation media or an interactive console user.

Use a disposable, password-protected local Windows test identity. Do not enable
automatic logon or use a blank password. While the operator is still signed in,
the harness captures the console identity from `Win32_ComputerSystem.UserName`,
requires an enabled local-account SID, checks `PasswordRequired`, rejects a
blank-password `LogonUserW` success, and rejects Winlogon automatic-logon state
or a stored `DefaultPassword`. These checks apply to the OOBE user, not to the
agent service account. Do not enter a personal Microsoft account or production
credential into a release-gate guest.

## Guest service contract

The `specialize` pass must register exactly one service named `LSWAgent` with
`StartMode=Auto`, `State=Running`, and
`StartName=NT SERVICE\LSWAgent`. All agent commands, including ConPTY sessions,
intentionally run in Windows Session 0 under that virtual service account; the
service does not impersonate the OOBE user or store that user's password.

It must also register `LSWLicenseHelper` as Manual/demand-start under
LocalSystem. The E2E status request proves that the agent SID can start it and
that the helper exits after one authenticated WMI query. No product key is used
by the release gate.

Before the first shutdown, the harness resolves both the service process owner
and the command process identity, requires both to equal the translated
`NT SERVICE\LSWAgent` SID, and requires the SID to begin with `S-1-5-80-`.
After a full guest shutdown and bare-`lsw` boot, the harness repeats the service
configuration and identity checks and requires the same SID. It also requires
`Win32_ComputerSystem.UserName` to be empty and rechecks that Winlogon has
neither automatic logon nor a stored default password. This prevents a hidden
desktop login from making the cold-start test pass.

Session 0 is sufficient for the beta.5 command and ConPTY contract, but it does
not provide visible desktop GUI applications. A future user-session companion
will own GUI, clipboard, audio, and per-window integration without changing the
boot-time service boundary.

This workflow and harness define the required release evidence; they do not
claim the service path has passed on real hardware. Tag beta.5 only after the
exact candidate completes this job successfully on the dedicated KVM runner.

Tag only the exact commit named in a successful run. If `master` changes, run
the gate again for the new commit. The release workflow rejects every new tag
after the existing beta.1–beta.4 tags unless it finds the successful real-KVM
job for that exact SHA. The workflow summary records the validated SHA so the
beta release decision is auditable.

By default, per-run build output and guest state are deleted after either
success or failure. Enabling **keep_state** retains failed-run guest state only
below `LSW_E2E_ROOT_BASE` for diagnosis; successful runs still prove removal
and leave no state behind. Retained state is never uploaded; remove it manually
when investigation is complete.
