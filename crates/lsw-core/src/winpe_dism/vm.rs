// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::io::{path_is_missing, require_regular_file};
use super::{
    require_real_directory, WinPeControlMediaPlan, WinPeDismBackend, WinPeDismVmPhase,
    WinPeDismVmPlan, WINPE_WORKSPACE_SIZE_GIB,
};
use crate::{
    CommandInvocation, HostCapabilities, InstanceManifest, PreparationPlan, PreparationStep,
    QemuBackend, Result,
};

impl WinPeDismBackend {
    /// Builds the network-disabled QEMU plan for one WinPE phase.
    pub fn plan_vm(
        capabilities: HostCapabilities,
        manifest: &InstanceManifest,
        instance_dir: &Path,
        phase: WinPeDismVmPhase,
    ) -> Result<WinPeDismVmPlan> {
        manifest.spec.validate_for_create()?;
        require_real_directory(instance_dir)?;
        require_real_directory(&instance_dir.join("run"))?;
        require_real_directory(&instance_dir.join(phase.seed_directory()))?;

        let backend = QemuBackend::select(&capabilities);
        let program = capabilities
            .qemu_system
            .clone()
            .unwrap_or_else(|| PathBuf::from("qemu-system-x86_64"));
        let firmware_code = capabilities
            .firmware_code(manifest.spec.profile)
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/path/to/OVMF_CODE.fd"));
        let firmware_vars_source = capabilities
            .firmware_vars(manifest.spec.profile)
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/path/to/OVMF_VARS.fd"));
        let phase_name = match phase {
            WinPeDismVmPhase::Prepare => "prepare",
            WinPeDismVmPhase::Apply => "apply",
        };
        let firmware_vars = instance_dir.join(format!("run/winpe-{phase_name}-OVMF_VARS.fd"));
        let workspace_disk = instance_dir.join("run/winpe-workspace.qcow2");
        let target_disk = instance_dir.join("disk.qcow2");
        let status_directory = instance_dir.join(format!("run/winpe-{phase_name}-status"));
        let status_log = status_directory.join("status.log");
        let dism_log = status_directory.join("dism.log");
        let serial_log = instance_dir.join(format!("run/winpe-{phase_name}-serial.log"));
        let qemu_log = instance_dir.join(format!("winpe-{phase_name}-qemu.log"));
        let qmp_socket = instance_dir.join(format!("run/winpe-{phase_name}-qmp.sock"));
        let control_media_root = instance_dir.join("run/winpe-control-root");
        let control_media_iso = instance_dir.join("run/winpe-control.iso");
        let mut steps = Vec::new();
        let mut missing_capabilities = Vec::new();

        plan_new_firmware_vars(
            &firmware_vars_source,
            &firmware_vars,
            &mut steps,
            &mut missing_capabilities,
            capabilities.firmware_vars(manifest.spec.profile).is_some(),
        )?;
        match phase {
            WinPeDismVmPhase::Prepare => {
                if path_is_missing(&workspace_disk, "WinPE workspace disk")? {
                    let qemu_img = capabilities
                        .qemu_img
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("qemu-img"));
                    steps.push(PreparationStep::CreateDisk {
                        program: qemu_img,
                        destination: workspace_disk.clone(),
                        size_gib: WINPE_WORKSPACE_SIZE_GIB,
                    });
                    if capabilities.qemu_img.is_none() {
                        missing_capabilities.push("qemu-img");
                    }
                } else {
                    require_regular_file(&workspace_disk, "WinPE workspace disk")?;
                }
            }
            WinPeDismVmPhase::Apply => {
                require_regular_file(&workspace_disk, "prepared WinPE workspace disk")?;
                require_regular_file(&target_disk, "target system disk")?;
            }
        }

        if capabilities.qemu_system.is_none() {
            missing_capabilities.push("qemu-system-x86_64");
        }
        if capabilities.firmware_code(manifest.spec.profile).is_none() {
            missing_capabilities.push("OVMF code firmware");
        }
        if capabilities.seven_zip.is_none() {
            missing_capabilities.push("7z (UDF-capable ISO extractor)");
        }
        if capabilities.wimlib_imagex.is_none() {
            missing_capabilities.push("wimlib-imagex");
        }
        if capabilities.xorriso.is_none() {
            missing_capabilities.push("xorriso");
        }
        if capabilities.qemu_img.is_none() {
            missing_capabilities.push("qemu-img");
        }
        missing_capabilities.sort_unstable();
        missing_capabilities.dedup();

        let mut arguments = Vec::new();
        push_pair(
            &mut arguments,
            "-name",
            format!("lsw-{}-winpe-{phase_name}", manifest.spec.name),
        );
        push_pair(&mut arguments, "-machine", "q35,usb=on");
        push_pair(&mut arguments, "-smp", manifest.spec.cpus.to_string());
        push_pair(
            &mut arguments,
            "-m",
            format!("{}M", manifest.spec.memory_mib),
        );
        arguments.extend(backend.acceleration_arguments().iter().map(OsString::from));
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "if=pflash,format=raw,readonly=on,file={}",
                qemu_path(&firmware_code)
            ),
        );
        push_pair(
            &mut arguments,
            "-drive",
            format!("if=pflash,format=raw,file={}", qemu_path(&firmware_vars)),
        );
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "file={},if=none,id=workspace,format=qcow2,discard=unmap",
                qemu_path(&workspace_disk)
            ),
        );
        push_pair(
            &mut arguments,
            "-device",
            "nvme,drive=workspace,serial=lsw-winpe-workspace,addr=0x4",
        );
        if phase == WinPeDismVmPhase::Apply {
            push_pair(
                &mut arguments,
                "-drive",
                format!(
                    "file={},if=none,id=target,format=qcow2,discard=unmap",
                    qemu_path(&target_disk)
                ),
            );
            push_pair(
                &mut arguments,
                "-device",
                "nvme,drive=target,serial=lsw-system,addr=0x5",
            );
        }
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "media=cdrom,readonly=on,file={}",
                qemu_path(&control_media_iso)
            ),
        );
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "media=cdrom,readonly=on,file={}",
                qemu_path(&manifest.spec.source_iso)
            ),
        );
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "file=fat:ro:{},format=raw,if=none,id=lsw-winpe-seed,snapshot=on",
                qemu_path(&instance_dir.join(phase.seed_directory()))
            ),
        );
        push_pair(&mut arguments, "-device", "qemu-xhci");
        push_pair(
            &mut arguments,
            "-device",
            "usb-storage,drive=lsw-winpe-seed,removable=on",
        );
        push_pair(
            &mut arguments,
            "-drive",
            format!(
                "file=fat:rw:{},format=raw,if=none,id=lsw-winpe-status",
                qemu_path(&status_directory)
            ),
        );
        push_pair(
            &mut arguments,
            "-device",
            "usb-storage,drive=lsw-winpe-status,removable=on",
        );
        push_pair(&mut arguments, "-device", "VGA");
        push_pair(&mut arguments, "-boot", "once=d,menu=off");
        push_pair(&mut arguments, "-nic", "none");
        push_pair(
            &mut arguments,
            "-serial",
            format!("file:{}", qemu_path(&serial_log)),
        );
        push_pair(
            &mut arguments,
            "-qmp",
            format!("unix:{},server=on,wait=off", qemu_path(&qmp_socket)),
        );
        arguments.push("-nodefaults".into());
        arguments.push("-no-reboot".into());
        push_pair(&mut arguments, "-monitor", "none");
        push_pair(&mut arguments, "-display", "none");

        let mut notes = vec![match phase {
            WinPeDismVmPhase::Prepare => {
                "preparation VM attaches only the private workspace as writable Disk 0; the target disk is not attached"
                    .to_owned()
            }
            WinPeDismVmPhase::Apply => {
                "apply VM attaches the prepared workspace as Disk 0 and the LSW target qcow2 as Disk 1"
                    .to_owned()
            }
        }];
        notes.push("preparation networking is disabled".to_owned());
        notes.push(
            "WinPE boots from an ephemeral control ISO derived from the supplied Microsoft media"
                .to_owned(),
        );
        notes.push(format!(
            "successful completion requires private status-volume marker {:?}",
            phase.completion_marker()
        ));
        if let Some(note) = backend.fallback_note() {
            notes.push(note);
        }

        Ok(WinPeDismVmPlan {
            phase,
            backend,
            invocation: CommandInvocation {
                program: program.into_os_string(),
                arguments,
            },
            host_preparation: PreparationPlan {
                steps,
                missing_capabilities: missing_capabilities.clone(),
            },
            status_log,
            dism_log,
            serial_log,
            qemu_log,
            qmp_socket,
            missing_capabilities,
            notes,
            target_disk: (phase == WinPeDismVmPhase::Apply).then_some(target_disk),
            qemu_img: capabilities
                .qemu_img
                .clone()
                .unwrap_or_else(|| PathBuf::from("qemu-img")),
            control_media: WinPeControlMediaPlan {
                source_iso: manifest.spec.source_iso.clone(),
                root: control_media_root,
                destination: control_media_iso,
                seven_zip: capabilities
                    .seven_zip
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("7z")),
                wimlib_imagex: capabilities
                    .wimlib_imagex
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("wimlib-imagex")),
                xorriso: capabilities
                    .xorriso
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("xorriso")),
            },
        })
    }
}
fn plan_new_firmware_vars(
    source: &Path,
    destination: &Path,
    steps: &mut Vec<PreparationStep>,
    missing_capabilities: &mut Vec<&'static str>,
    source_available: bool,
) -> Result<()> {
    if path_is_missing(destination, "WinPE firmware variable store")? {
        steps.push(PreparationStep::CopyFirmwareVariables {
            source: source.to_owned(),
            destination: destination.to_owned(),
        });
        if !source_available {
            missing_capabilities.push("OVMF variable template");
        }
    } else {
        require_regular_file(destination, "WinPE firmware variable store")?;
    }
    Ok(())
}

fn push_pair(
    arguments: &mut Vec<OsString>,
    option: impl Into<OsString>,
    value: impl Into<OsString>,
) {
    arguments.push(option.into());
    arguments.push(value.into());
}

fn qemu_path(path: &Path) -> String {
    path.to_string_lossy().replace(',', ",,")
}
