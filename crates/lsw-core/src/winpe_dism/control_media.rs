// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;

use super::io::{
    require_regular_file, run_control_command, set_private_directory_permissions,
    set_private_file_permissions, write_private_new_file,
};
use super::{WinPeControlMediaPlan, WINPE_SHELL, WINPE_STARTNET};
use crate::{LswError, Result};

pub(super) fn prepare_control_media(plan: &WinPeControlMediaPlan) -> Result<()> {
    match fs::symlink_metadata(&plan.destination) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            return Ok(())
        }
        Ok(_) => {
            return Err(LswError::InvalidValue {
                field: "WinPE control ISO",
                reason: format!("{} is not a regular file", plan.destination.display()),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    require_regular_file(&plan.source_iso, "Windows source ISO")?;
    if fs::symlink_metadata(&plan.root).is_ok() {
        return Err(LswError::InvalidValue {
            field: "WinPE control media staging",
            reason: format!("{} already exists", plan.root.display()),
        });
    }
    fs::create_dir(&plan.root)?;
    set_private_directory_permissions(&plan.root)?;

    let temporary = plan
        .destination
        .with_extension(format!("iso.tmp-{}", std::process::id()));
    if fs::symlink_metadata(&temporary).is_ok() {
        let _ = fs::remove_dir_all(&plan.root);
        return Err(LswError::InvalidValue {
            field: "WinPE control ISO",
            reason: format!("temporary path {} already exists", temporary.display()),
        });
    }

    let result = (|| {
        run_control_command(
            &plan.seven_zip,
            &[
                "x".into(),
                "-y".into(),
                "-bd".into(),
                "-bso0".into(),
                "-bsp0".into(),
                format!("-o{}", plan.root.display()).into(),
                plan.source_iso.as_os_str().to_owned(),
                "boot/*".into(),
                "efi/*".into(),
                "sources/boot.wim".into(),
                "bootmgr".into(),
                "bootmgr.efi".into(),
            ],
            None,
        )?;
        let boot_wim = plan.root.join("sources/boot.wim");
        let bios_boot = plan.root.join("boot/etfsboot.com");
        let uefi_boot = plan.root.join("efi/microsoft/boot/efisys_noprompt.bin");
        for (path, field) in [
            (&boot_wim, "Windows PE boot.wim"),
            (&bios_boot, "Windows BIOS boot image"),
            (&uefi_boot, "Windows UEFI no-prompt boot image"),
        ] {
            require_regular_file(path, field)?;
        }

        let startnet = plan.root.join("lsw-startnet.cmd");
        let shell = plan.root.join("lsw-winpeshl.ini");
        write_private_new_file(&startnet, WINPE_STARTNET)?;
        write_private_new_file(&shell, WINPE_SHELL)?;
        for (source, destination) in [
            ("lsw-startnet.cmd", "/Windows/System32/startnet.cmd"),
            ("lsw-winpeshl.ini", "/Windows/System32/winpeshl.ini"),
        ] {
            run_control_command(
                &plan.wimlib_imagex,
                &[
                    "update".into(),
                    boot_wim.as_os_str().to_owned(),
                    "2".into(),
                    "--check".into(),
                    format!("--command=add {source} {destination}").into(),
                ],
                Some(&plan.root),
            )?;
        }
        fs::remove_file(startnet)?;
        fs::remove_file(shell)?;

        run_control_command(
            &plan.xorriso,
            &[
                "-as".into(),
                "mkisofs".into(),
                "-iso-level".into(),
                "3".into(),
                "-full-iso9660-filenames".into(),
                "-volid".into(),
                "LSW_WINPE".into(),
                "-eltorito-boot".into(),
                "boot/etfsboot.com".into(),
                "-no-emul-boot".into(),
                "-boot-load-size".into(),
                "8".into(),
                "-eltorito-alt-boot".into(),
                "-e".into(),
                "efi/microsoft/boot/efisys_noprompt.bin".into(),
                "-no-emul-boot".into(),
                "-output".into(),
                temporary.as_os_str().to_owned(),
                ".".into(),
            ],
            Some(&plan.root),
        )?;
        require_regular_file(&temporary, "temporary WinPE control ISO")?;
        set_private_file_permissions(&temporary)?;
        fs::rename(&temporary, &plan.destination)?;
        Ok(())
    })();

    let _ = fs::remove_dir_all(&plan.root);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
