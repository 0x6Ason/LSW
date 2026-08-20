# WinPE DISM backend

`lsw-core::WinPeDismBackend` implements the isolated pre-install
image-preparation and target-apply primitives. The beta.5 one-shot
`lsw install NAME` path invokes both phases before the normal guest boot.

## Implemented contract

Given a profile, a validated WIM index and an instance directory, the backend:

1. Produces a declarative list of preparation stages.
2. Generates an `Autounattend.xml` `windowsPE` synchronous command.
3. Generates a `cmd.exe` script that requires no PowerShell optional component.
4. Initializes the preparation VM's only writable disk, virtual Disk 0, as a
   private NTFS workspace.
5. Locates `sources/install.wim` or `sources/install.esd` on the official ISO.
6. Uses the ISO's Windows `dism.exe` to export and mount the selected edition.
7. Inventories provisioned AppX packages with `/English`, resolves the profile's
   display-name allowlist to full package names, and removes only matches.
8. Commits the result as `lsw-prepared.wim`, with integrity checks on export,
   mount and commit.
9. Emits phase markers to COM1 and discards a mounted image after an error.

The apply plan then boots a separate WinPE phase with the prepared workspace as
Disk 0 and the new instance qcow2 as Disk 1. It partitions only Disk 1, applies
the prepared WIM with integrity checking and optional CompactOS, stages a
private local agent/unattend payload, creates UEFI boot files with BCDBoot, and
requires a distinct `apply-complete` serial marker.

The generated seed contains no Microsoft binary, Windows image, product key,
activation data or agent token. Linux `wimlib` is not called for package, AppX
or feature servicing.

The `slim` plan requests CompactOS when the prepared image is eventually
applied. It does not run `ResetBase`, remove WinSxS, disable Windows Update or
Defender, remove Store/App Installer/WebView2, or disable WMI, hibernation or
recovery.

## Safety boundary

The prepare script contains a destructive `diskpart clean` for virtual Disk 0;
the apply script does the same only for virtual Disk 1. The QEMU planner keeps
these topologies distinct: prepare attaches only a newly created private sparse
workspace, while apply attaches that workspace first and the LSW target second.
Both phases disable networking, use per-phase OVMF variables and require a
bounded run plus an exact serial completion marker. No host block device is
accepted by the plan.

The workspace is expected to be a sparse 32 GiB qcow2. Its final artifact is
`W:\lsw-prepared.wim`. The backend owns both QEMU children until exit, enforces a
two-hour default timeout, retains bounded serial/QEMU logs, and waits for the
process after termination so preparation RAM is released before normal boot.

## Verification status

Unit tests cover stage construction, image-index validation, atomic/private seed
writing, absence of product keys, the conservative stock path, exact DISM
operations, separated disk topology, apply/CompactOS behavior, payload ACL
staging, and mandatory completion markers. The backend is enabled in the
beta.5 installer. A successful run on the dedicated Windows 11/KVM release
gate remains mandatory before tagging beta.5; source tests do not substitute
for execution of the official ISO's WinPE and DISM binaries.

The command sequence follows Microsoft's documentation for
[windowsPE RunSynchronous](https://learn.microsoft.com/en-us/windows-hardware/customize/desktop/unattend/microsoft-windows-setup-runsynchronous),
[offline DISM image modification](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/mount-and-modify-a-windows-image-using-dism?view=windows-11), and
[provisioned AppX servicing](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/dism-app-package--appx-or-appxbundle--servicing-command-line-options?view=windows-11).
