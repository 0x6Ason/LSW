// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use lsw_core::{FolderShare, FolderShareMode, StateStore};

use super::{absolute_path, resolve_name, transfer};

pub(super) fn command(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().and_then(|value| value.to_str()) {
        Some("add") => add(store, &arguments[1..]),
        Some("list") => list(store, &arguments[1..]),
        Some("remove") => remove(store, &arguments[1..]),
        Some("sync") => sync(store, &arguments[1..], false),
        Some("watch") => sync(store, &arguments[1..], true),
        _ => Err("usage: lsw share <add|list|remove|sync|watch> ...".into()),
    }
}

fn add(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let mut mode = None;
    let mut positional = Vec::new();
    for argument in arguments {
        match argument
            .to_str()
            .ok_or("share arguments must be valid UTF-8")?
        {
            "--read-only" => set_mode(&mut mode, FolderShareMode::ReadOnly)?,
            "--read-write" => set_mode(&mut mode, FolderShareMode::ReadWrite)?,
            value if value.starts_with('-') => {
                return Err(format!("unknown share option {value:?}").into())
            }
            value => positional.push(value.to_owned()),
        }
    }
    let (requested, share_name, host_path, guest_path) = match positional.as_slice() {
        [share, host, guest] => (None, share, host, guest),
        [name, share, host, guest] => (Some(name.as_str()), share, host, guest),
        _ => return Err(add_usage().into()),
    };
    let name = resolve_name(store, requested)?;
    let host_path = canonical_real_directory(Path::new(host_path))?;
    let share = FolderShare {
        name: share_name.clone(),
        host_path,
        guest_path: guest_path.clone(),
        mode: mode.ok_or("choose exactly one of --read-only or --read-write")?,
    };
    share.validate()?;
    let mut manifest = store.load(&name)?;
    if manifest
        .folder_shares
        .iter()
        .any(|existing| existing.name == share.name)
    {
        return Err(format!(
            "folder share {:?} already exists for {name:?}; remove it before replacing its trust boundary",
            share.name
        )
        .into());
    }
    manifest.folder_shares.push(share.clone());
    store.update(&manifest)?;
    println!(
        "Added {} folder share {:?}: {} -> {}.",
        share.mode,
        share.name,
        share.host_path.display(),
        share.guest_path
    );
    println!("Run `lsw share sync {name} {}` to populate it.", share.name);
    Ok(())
}

fn list(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let requested = match arguments {
        [] => None,
        [name] => Some(name.to_str().ok_or("instance name must be valid UTF-8")?),
        _ => return Err("usage: lsw share list [NAME]".into()),
    };
    let name = resolve_name(store, requested)?;
    let manifest = store.load(&name)?;
    if manifest.folder_shares.is_empty() {
        println!("No folder shares configured for {name:?}.");
        return Ok(());
    }
    println!("SHARE\tMODE\tHOST\tGUEST");
    for share in manifest.folder_shares {
        println!(
            "{}\t{}\t{}\t{}",
            share.name,
            share.mode,
            share.host_path.display(),
            share.guest_path
        );
    }
    Ok(())
}

fn remove(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let (requested, share_name) = match arguments {
        [share] => (
            None,
            share.to_str().ok_or("share name must be valid UTF-8")?,
        ),
        [name, share] => (
            Some(name.to_str().ok_or("instance name must be valid UTF-8")?),
            share.to_str().ok_or("share name must be valid UTF-8")?,
        ),
        _ => return Err("usage: lsw share remove [NAME] SHARE".into()),
    };
    let name = resolve_name(store, requested)?;
    let mut manifest = store.load(&name)?;
    let previous = manifest.folder_shares.len();
    manifest
        .folder_shares
        .retain(|share| share.name != share_name);
    if manifest.folder_shares.len() == previous {
        return Err(format!("folder share {share_name:?} does not exist for {name:?}").into());
    }
    store.update(&manifest)?;
    println!(
        "Removed folder share {share_name:?} from {name:?}; existing files and guest ACLs were preserved."
    );
    Ok(())
}

fn sync(
    store: &StateStore,
    arguments: &[OsString],
    watch: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut from_guest = false;
    let mut positional = Vec::new();
    for argument in arguments {
        if argument == OsStr::new("--from-guest") && !watch {
            if from_guest {
                return Err("--from-guest was supplied more than once".into());
            }
            from_guest = true;
        } else if argument.to_string_lossy().starts_with('-') {
            return Err(if watch {
                "usage: lsw share watch [NAME] SHARE"
            } else {
                "usage: lsw share sync [NAME] SHARE [--from-guest]"
            }
            .into());
        } else {
            positional.push(
                argument
                    .to_str()
                    .ok_or("share arguments must be valid UTF-8")?
                    .to_owned(),
            );
        }
    }
    let (requested, share_name) = match positional.as_slice() {
        [share] => (None, share.as_str()),
        [name, share] => (Some(name.as_str()), share.as_str()),
        _ => {
            return Err(if watch {
                "usage: lsw share watch [NAME] SHARE"
            } else {
                "usage: lsw share sync [NAME] SHARE [--from-guest]"
            }
            .into())
        }
    };
    let name = resolve_name(store, requested)?;
    let manifest = store.load(&name)?;
    let share = manifest
        .folder_shares
        .into_iter()
        .find(|share| share.name == share_name)
        .ok_or_else(|| format!("folder share {share_name:?} does not exist for {name:?}"))?;
    canonical_real_directory(&share.host_path)?;
    if from_guest {
        if share.mode != FolderShareMode::ReadWrite {
            return Err("read-only shares cannot synchronize changes from the guest".into());
        }
        transfer::sync_guest_to_host(store, &name, &share.guest_path, &share.host_path)?;
        return Ok(());
    }
    transfer::sync_host_to_guest(store, &name, &share.host_path, &share.guest_path, false)?;
    if share.mode == FolderShareMode::ReadOnly {
        transfer::set_guest_share_read_only(store, &name, &share.guest_path)?;
    }
    if watch {
        println!(
            "Periodic change detection is active; agent reconnects are retried and deletions are preserved."
        );
        transfer::sync_host_to_guest(store, &name, &share.host_path, &share.guest_path, true)?;
    }
    Ok(())
}

fn set_mode(
    mode: &mut Option<FolderShareMode>,
    value: FolderShareMode,
) -> Result<(), Box<dyn std::error::Error>> {
    if mode.replace(value).is_some() {
        return Err("choose exactly one of --read-only or --read-write".into());
    }
    Ok(())
}

fn canonical_real_directory(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = absolute_path(path)?;
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "folder share root crosses a symbolic link: {}",
                    ancestor.display()
                )
                .into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("{} must be a real directory", path.display()).into());
    }
    let canonical = fs::canonicalize(&path)?;
    let canonical_metadata = fs::symlink_metadata(&canonical)?;
    if !canonical_metadata.file_type().is_dir() || canonical_metadata.file_type().is_symlink() {
        return Err(format!("{} must resolve to a real directory", path.display()).into());
    }
    Ok(canonical)
}

fn add_usage() -> &'static str {
    "usage: lsw share add [NAME] SHARE HOST_PATH GUEST_PATH (--read-only|--read-write)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_is_explicit_and_unique() {
        let mut mode = None;
        set_mode(&mut mode, FolderShareMode::ReadOnly).unwrap();
        assert_eq!(mode, Some(FolderShareMode::ReadOnly));
        assert!(set_mode(&mut mode, FolderShareMode::ReadWrite).is_err());
    }
}
