# WinPE DISM backend

`lsw-core::WinPeDismBackend` implements the isolated pre-install
image-preparation and target-apply primitives. The beta.6 one-shot
`lsw install NAME` path invokes both phases before the normal guest boot.

## Implemented contract

Given a profile, a validated WIM index and an instance directory, the backend:

1. Produces a declarative list of preparation stages.
2. Extracts the official media's boot files into a private staging directory,
   adds a minimal `winpeshl.ini`/`startnet.cmd` launcher to boot WIM index 2,
   and atomically builds an ephemeral no-prompt control ISO.
3. Generates a `cmd.exe` script that requires no PowerShell optional component.
4. Initializes the preparation VM's only writable disk, virtual Disk 0, as a
   private NTFS workspace.
5. Locates `sources/install.wim` or `sources/install.esd` on the official ISO.
6. Uses the ISO's Windows `dism.exe` to export and mount the selected edition.
   Export uses maximum compression plus the private NTFS scratch directory;
   export, ordinary mount, and commit retain integrity checks. A real KVM
   comparison rejected fast compression because its larger intermediate made
   total preparation slower. Mount intentionally
   omits `/Optimize`: real Windows 11 25H2/KVM first-boot testing reproduced
   `PROCESS1_INITIALIZATION_FAILED (0x6B)` with that optional optimized path.
7. Inventories provisioned AppX packages with `/English`, resolves the profile's
   display-name allowlist to full package names, and removes only matches.
8. Commits the result as `lsw-prepared.wim`, with integrity checks on export,
   mount and commit. In the one-shot installer, it stages the private agent and
   answer file into the mounted WIM immediately before that commit. A bounded
   marker tells first boot not to repeat the already completed provisioned-AppX
   work.
9. Emits phase markers and bounded live DISM output to a private writable
   status volume, allowing the host to report real percentages, and discards a
   mounted image after an error.

The apply plan then boots a separate WinPE phase with the prepared workspace as
Disk 0 and the new instance qcow2 as Disk 1. It partitions only Disk 1, applies
the WIM without coupling target creation to CompactOS, creates UEFI boot files
with BCDBoot, requires a distinct `apply-complete` status-volume marker, and
rejects that marker if the resulting qcow2 is implausibly small or fails
`qemu-img check`.

The generated seed contains no Microsoft binary, Windows image, product key,
activation data or agent token. Linux `wimlib` is not called for package, AppX
or feature servicing.

The `slim` plan enables CompactOS during the named Windows
`applying-profile` setup stage after the target image has been applied safely.
The offline marker skips duplicate AppX work without skipping CompactOS. This
keeps a CompactOS failure recoverable and avoids the non-deterministic,
CPU-bound stall reproduced by the exact Windows/KVM gate when DISM combined
`/Apply-Image` with `/Compact:on`. The profile does not run `ResetBase`, remove
WinSxS, disable Windows Update or Defender, remove Store/App
Installer/WebView2, or disable WMI, hibernation or recovery.

## Safety boundary

The prepare script contains a destructive `diskpart clean` for virtual Disk 0;
the apply script does the same only for virtual Disk 1. The QEMU planner keeps
these topologies distinct: prepare attaches only a newly created private sparse
workspace, while apply attaches that workspace first and the LSW target second.
Both phases disable networking, use per-phase OVMF variables and require a
bounded run plus an exact completion marker. The host may read the dedicated
status and DISM logs while QEMU owns the volume, but no host process writes to
them until QEMU exits. No host block device is accepted by the plan.

The workspace is expected to be a sparse 32 GiB qcow2. Its final artifact is
`W:\lsw-prepared.wim`. The backend owns both QEMU children until exit, enforces a
two-hour default timeout, retains bounded serial/QEMU logs, and waits for the
process after termination so preparation RAM is released before normal boot.

## Verification status

Unit tests cover stage construction, image-index validation, atomic/private seed
writing, absence of product keys, the conservative stock path, exact DISM
operations, separated disk topology, deferred CompactOS behavior, payload ACL
staging, control-media topology, and mandatory completion markers. The backend
is enabled in the beta.6 installer. A real Windows 11 25H2/KVM run completed
both WinPE phases, specialize, and SCM agent startup, then exposed a race where
the first agent connection preceded the specialize-to-OOBE reboot. The headless
installer now waits for a post-OOBE cleanup marker instead. The protected
exact-commit release-gate job remains mandatory before tagging any release;
partial or local runs do not replace that publication control.

The command sequence follows Microsoft's documentation for
[WinPE startup scripts](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/wpeinit-and-startnetcmd-using-winpe-startup-scripts?view=windows-11),
[Winpeshl.ini](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/winpeshlini-reference-launching-an-app-when-winpe-starts?view=windows-11),
[offline DISM image modification](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/mount-and-modify-a-windows-image-using-dism?view=windows-11), and
[provisioned AppX servicing](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/dism-app-package--appx-or-appxbundle--servicing-command-line-options?view=windows-11).
