// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use lsw_core::{
    FolderShare, FolderShareMode, FolderShareTransport, HostCapabilities, InstanceState,
    LaunchPhase, StateStore,
};

use super::daemon_client::DaemonClient;
use super::{
    absolute_path, connect_agent, fix_host_dependencies, resolve_name, start_named_instance,
    transfer,
};

const LIVE_SHARE_NAME: &str = "linux";
const LIVE_SHARE_GUEST_PATH: &str = "L:\\";
const GUEST_READY_TIMEOUT: Duration = Duration::from_secs(180);

pub(super) fn command(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().and_then(|value| value.to_str()) {
        None => list(store, &[]),
        Some("add") => add(store, &arguments[1..]),
        Some("list") => list(store, &arguments[1..]),
        Some("remove") => remove(store, &arguments[1..]),
        Some("sync") => sync(store, &arguments[1..], false),
        Some("watch") => sync(store, &arguments[1..], true),
        Some(_) if arguments.len() == 1 => add_live(store, &arguments[0]),
        _ => Err("usage: lsw share [PATH] | lsw share <add|list|remove|sync|watch> ...".into()),
    }
}

pub(super) fn unshare(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    remove(store, arguments)
}

pub(super) fn offer_recommended_integration(
    store: &StateStore,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        println!("Run `lsw share ~/LSW` later to enable the recommended Linux folder.");
        return Ok(());
    }
    if store
        .load(name)?
        .folder_shares
        .iter()
        .any(|share| share.transport == FolderShareTransport::LiveSmb)
    {
        return Ok(());
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let root = PathBuf::from(home).join("LSW");
    println!("Recommended Linux folder integration:");
    println!("  Host root: {}", root.display());
    println!("  Windows drive: Linux (L:)");
    println!("  Access: read-write, live (files are not copied)");
    println!("  Scope: this directory only; the rest of the Linux home stays private");
    println!("Enabling it performs one normal Windows restart.");
    if prompt_choice("Enable this integration? [Y/n]: ")? {
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => return Err(format!("{} must be a real directory", root.display()).into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&root)?,
            Err(error) => return Err(error.into()),
        }
        add_live_path(store, name, &root)?;
    } else {
        println!("Linux folder integration was left disabled. Run `lsw share ~/LSW` later.");
    }
    Ok(())
}

fn add_live(store: &StateStore, path: &OsString) -> Result<(), Box<dyn std::error::Error>> {
    let name = resolve_name(store, None)?;
    add_live_path(store, &name, Path::new(path))
}

fn add_live_path(
    store: &StateStore,
    name: &str,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if HostCapabilities::detect().smbd.is_none() {
        println!(
            "The Samba user-mode server is required for live folders; installing host dependencies..."
        );
        fix_host_dependencies(false)?;
        if HostCapabilities::detect().smbd.is_none() {
            return Err(
                "live folders require smbd; install the Samba server package and retry".into(),
            );
        }
    }
    let host_path = canonical_real_directory(path)?;
    let share = FolderShare {
        name: LIVE_SHARE_NAME.to_owned(),
        host_path,
        guest_path: LIVE_SHARE_GUEST_PATH.to_owned(),
        mode: FolderShareMode::ReadWrite,
        transport: FolderShareTransport::LiveSmb,
    };
    share.validate()?;
    let mut manifest = store.load(name)?;
    if let Some(existing) = manifest
        .folder_shares
        .iter()
        .find(|existing| existing.transport == FolderShareTransport::LiveSmb)
    {
        if existing.host_path == share.host_path {
            ensure_live_mapping(store, name)?;
            println!(
                "Live folder {:?} is mounted as Linux (L:) for {name:?}.",
                existing.host_path.display()
            );
            return Ok(());
        }
        return Err(format!(
            "instance {name:?} already exposes live root {}; run `lsw unshare {}` first",
            existing.host_path.display(),
            existing.name
        )
        .into());
    }
    if manifest
        .folder_shares
        .iter()
        .any(|item| item.name == share.name)
    {
        return Err(format!(
            "folder share {:?} already exists; remove or rename it before enabling Linux (L:)",
            share.name
        )
        .into());
    }
    manifest.folder_shares.push(share.clone());
    store.update(&manifest)?;
    let result = restart_and_configure(store, name, true);
    if let Err(error) = result {
        let mut rollback = store.load(name)?;
        rollback.folder_shares.retain(|item| {
            !(item.name == share.name && item.transport == FolderShareTransport::LiveSmb)
        });
        store.update(&rollback)?;
        let _ = restart_instance(store, name);
        return Err(format!(
            "could not mount Linux (L:); the live export was rolled back: {error}"
        )
        .into());
    }
    println!(
        "Mounted live read-write folder {:?}: {} -> Linux (L:).",
        share.name,
        share.host_path.display()
    );
    Ok(())
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
        transport: FolderShareTransport::Mirror,
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
    println!("SHARE\tTYPE\tMODE\tHOST\tGUEST");
    for share in manifest.folder_shares {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            share.name,
            share.transport,
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
    if manifest
        .folder_shares
        .iter()
        .any(|share| share.name == share_name && share.transport == FolderShareTransport::LiveSmb)
    {
        ensure_live_mapping(store, &name)?;
        connect_agent(store, &name)?.configure_live_share(false)?;
        let status = connect_agent(store, &name)?.live_share_status()?;
        if status.mapped {
            return Err("Windows retained Linux (L:); the share was not removed".into());
        }
        manifest
            .folder_shares
            .retain(|share| share.name != share_name);
        store.update(&manifest)?;
        restart_instance(store, &name)?;
        println!(
            "Unmounted Linux (L:) and removed live folder share {share_name:?} from {name:?}; host files were preserved."
        );
        return Ok(());
    }
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

fn ensure_live_mapping(store: &StateStore, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let state = store.load(name)?.state;
    if !matches!(state, InstanceState::Running | InstanceState::Suspended) {
        start_named_instance(store, name, LaunchPhase::Run)?;
    }
    wait_for_agent(store, name)?;
    let status = connect_agent(store, name)?.live_share_status()?;
    if !status.mapped {
        connect_agent(store, name)?.configure_live_share(true)?;
    }
    if !connect_agent(store, name)?.live_share_status()?.mapped {
        return Err("Windows did not retain the Linux (L:) mapping".into());
    }
    Ok(())
}

fn restart_and_configure(
    store: &StateStore,
    name: &str,
    enable: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    restart_instance(store, name)?;
    wait_for_agent(store, name)?;
    connect_agent(store, name)?.configure_live_share(enable)?;
    let status = connect_agent(store, name)?.live_share_status()?;
    if status.mapped != enable {
        return Err("Windows did not retain the requested Linux (L:) state".into());
    }
    Ok(())
}

fn restart_instance(store: &StateStore, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let state = store.load(name)?.state;
    if state == InstanceState::Installing {
        return Err("live folders cannot change while Windows installation is active".into());
    }
    if state == InstanceState::Configured {
        return Err("install Windows before enabling a live folder".into());
    }
    let active = matches!(state, InstanceState::Running | InstanceState::Suspended);
    if active {
        let client = DaemonClient::new(store);
        for line in client.request_checked(&format!("STOP {name} graceful"))? {
            println!("{line}");
        }
        let deadline = Instant::now() + GUEST_READY_TIMEOUT;
        loop {
            let _ = client.request_checked(&format!("STATUS {name}"));
            if matches!(
                store.load(name)?.state,
                InstanceState::Stopped | InstanceState::Failed
            ) {
                break;
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for Windows to restart for live sharing".into());
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
    start_named_instance(store, name, LaunchPhase::Run)
}

fn wait_for_agent(store: &StateStore, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + GUEST_READY_TIMEOUT;
    loop {
        match connect_agent(store, name).and_then(|client| client.probe()) {
            Ok(()) => return Ok(()),
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(500)),
            Err(error) => return Err(format!("Windows agent did not become ready: {error}").into()),
        }
    }
}

fn prompt_choice(prompt: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let mut stderr = io::stderr().lock();
    loop {
        write!(stderr, "{prompt}")?;
        stderr.flush()?;
        let mut value = String::new();
        io::stdin().read_line(&mut value)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(stderr, "Enter y or n.")?,
        }
    }
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
    if share.transport == FolderShareTransport::LiveSmb {
        return Err(
            "live folders are already current in both directions and do not accept sync or watch"
                .into(),
        );
    }
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
