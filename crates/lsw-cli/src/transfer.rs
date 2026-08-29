// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lsw_core::{ProcessEnvironment, SessionKind, StartRequest, StateStore};

use super::{connect_agent, resolve_name};

const REMOTE_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const WATCH_INTERVAL: Duration = Duration::from_millis(750);
const READ_ONLY_SHARE_ACL_SCRIPT: &str = r#"
$ErrorActionPreference='Stop'
$Full=[IO.Path]::GetFullPath($env:LSW_PATH)
$Drive=[IO.Path]::GetPathRoot($Full)
$Current=$Drive
foreach ($Part in $Full.Substring($Drive.Length).Split([char[]]'\/',[StringSplitOptions]::RemoveEmptyEntries)) {
    $Current=[IO.Path]::Combine($Current,$Part)
    $Item=Get-Item -LiteralPath $Current -Force
    if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw ('share path crosses a reparse point: '+$Item.FullName)
    }
}
$Root=Get-Item -LiteralPath $Full -Force
if (-not $Root.PSIsContainer) { throw 'share root is not a directory' }
$Agent=[Security.Principal.WindowsIdentity]::GetCurrent().User
$Inheritance=[Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
$Propagation=[Security.AccessControl.PropagationFlags]::None
$Allow=[Security.AccessControl.AccessControlType]::Allow
$FullControl=[Security.AccessControl.FileSystemRights]::FullControl
$ReadOnly=[Security.AccessControl.FileSystemRights]::ReadAndExecute
$Existing=Get-Acl -LiteralPath $Root.FullName
$ExistingRules=@($Existing.Access)
$FullSids=@('S-1-5-18','S-1-5-32-544',$Agent.Value)
$ExistingFull=@($ExistingRules | Where-Object {
    $FullSids -contains $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -and
    $_.AccessControlType -eq $Allow -and -not $_.IsInherited -and
    $_.InheritanceFlags -eq $Inheritance -and $_.PropagationFlags -eq $Propagation -and
    [int]($_.FileSystemRights) -eq [int]$FullControl
})
$ExistingUsers=@($ExistingRules | Where-Object {
    $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -eq 'S-1-5-32-545' -and
    $_.AccessControlType -eq $Allow -and -not $_.IsInherited -and
    $_.InheritanceFlags -eq $Inheritance -and $_.PropagationFlags -eq $Propagation -and
    [int]($_.FileSystemRights) -eq [int]([Security.AccessControl.FileSystemRights]'ReadAndExecute, Synchronize')
})
if ($Existing.AreAccessRulesProtected -and $ExistingRules.Count -eq 4 -and $ExistingFull.Count -eq 3 -and $ExistingUsers.Count -eq 1) {
    return
}
$Acl=New-Object Security.AccessControl.DirectorySecurity
$Acl.SetAccessRuleProtection($true,$false)
foreach ($Entry in @(
    @([Security.Principal.SecurityIdentifier]'S-1-5-18',$FullControl),
    @([Security.Principal.SecurityIdentifier]'S-1-5-32-544',$FullControl),
    @($Agent,$FullControl),
    @([Security.Principal.SecurityIdentifier]'S-1-5-32-545',$ReadOnly)
)) {
    $Rule=New-Object Security.AccessControl.FileSystemAccessRule($Entry[0],$Entry[1],$Inheritance,$Propagation,$Allow)
    $Acl.AddAccessRule($Rule) | Out-Null
}
Set-Acl -LiteralPath $Root.FullName -AclObject $Acl
"#;

pub(super) fn push(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = TransferArguments::parse(arguments, "push", true)?;
    let name = resolve_name(store, parsed.requested.as_deref())?;
    let source = PathBuf::from(&parsed.source);
    let metadata = fs::symlink_metadata(&source)?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{} must not be a symbolic link", source.display()).into());
    }
    if metadata.is_dir() {
        if !parsed.recursive {
            return Err("pushing a directory requires --recursive".into());
        }
        push_tree(store, &name, &source, &parsed.destination, false, true)?;
    } else if metadata.is_file() {
        let bytes = connect_agent(store, &name)?.put_file(&source, &parsed.destination)?;
        println!(
            "Transferred {bytes} bytes from {} to {name}:{}",
            source.display(),
            parsed.destination
        );
    } else {
        return Err(format!("{} is not a regular file or directory", source.display()).into());
    }
    Ok(())
}

pub(super) fn pull(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = TransferArguments::parse(arguments, "pull", true)?;
    let name = resolve_name(store, parsed.requested.as_deref())?;
    let destination = PathBuf::from(&parsed.destination);
    if parsed.recursive {
        pull_tree(store, &name, &parsed.source, &destination)
    } else {
        let bytes = connect_agent(store, &name)?.get_file(&parsed.source, &destination)?;
        println!(
            "Transferred {bytes} bytes from {name}:{} to {}",
            parsed.source,
            destination.display()
        );
        Ok(())
    }
}

pub(super) fn copy(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let (source, destination) = match arguments {
        [source, destination] => (
            source.to_str().ok_or("copy source must be valid UTF-8")?,
            destination
                .to_str()
                .ok_or("copy destination must be valid UTF-8")?,
        ),
        _ => return Err("usage: lsw cp SOURCE DESTINATION".into()),
    };
    let source_is_windows = is_absolute_windows_path(source);
    let destination_is_windows = is_absolute_windows_path(destination);
    if source_is_windows == destination_is_windows {
        return Err(
            "lsw cp requires exactly one absolute Windows path (for example C:\\work\\file)".into(),
        );
    }
    let name = resolve_name(store, None)?;
    if destination_is_windows {
        let source = PathBuf::from(source);
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() {
            return Err(format!("{} must not be a symbolic link", source.display()).into());
        }
        if metadata.is_dir() {
            push_tree(store, &name, &source, destination, false, true)?;
        } else if metadata.is_file() {
            let bytes = connect_agent(store, &name)?.put_file(&source, destination)?;
            println!(
                "Transferred {bytes} bytes from {} to {name}:{destination}",
                source.display()
            );
        } else {
            return Err("copy source must be a regular file or directory".into());
        }
        return Ok(());
    }

    let destination = PathBuf::from(destination);
    match remote_entry_kind(store, &name, source)? {
        LocalEntryKind::Directory => pull_tree(store, &name, source, &destination),
        LocalEntryKind::File => {
            let bytes = connect_agent(store, &name)?.get_file(source, &destination)?;
            println!(
                "Transferred {bytes} bytes from {name}:{source} to {}",
                destination.display()
            );
            Ok(())
        }
    }
}

fn is_absolute_windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
        || value.starts_with("\\\\")
}

fn remote_entry_kind(
    store: &StateStore,
    name: &str,
    path: &str,
) -> Result<LocalEntryKind, Box<dyn std::error::Error>> {
    let script = r#"
$ErrorActionPreference='Stop'
$Item=Get-Item -LiteralPath $env:LSW_PATH -Force
if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'copy source is a reparse point' }
if ($Item.PSIsContainer) { [Console]::Out.Write('directory') } else { [Console]::Out.Write('file') }
"#;
    match remote_powershell(
        store,
        name,
        script,
        vec![("LSW_PATH".to_owned(), path.to_owned())],
    )?
    .as_slice()
    {
        b"directory" => Ok(LocalEntryKind::Directory),
        b"file" => Ok(LocalEntryKind::File),
        _ => Err("guest returned an invalid copy source type".into()),
    }
}

pub(super) fn sync(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = TransferArguments::parse_sync(arguments)?;
    let name = resolve_name(store, parsed.requested.as_deref())?;
    let source = PathBuf::from(&parsed.source);
    require_real_directory(&source)?;
    sync_host_to_guest(store, &name, &source, &parsed.destination, parsed.watch)?;
    Ok(())
}

pub(super) fn sync_host_to_guest(
    store: &StateStore,
    name: &str,
    source: &Path,
    destination: &str,
    watch: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    push_tree(store, name, source, destination, true, true)?;
    if !watch {
        return Ok(());
    }

    println!(
        "Watching {} and synchronizing changes to {name}:{} (remote deletions are preserved).",
        source.display(),
        destination
    );
    let mut previous = local_snapshot(source)?;
    loop {
        thread::sleep(WATCH_INTERVAL);
        let current = match local_snapshot(source) {
            Ok(current) => current,
            Err(error) => {
                eprintln!("lsw sync: {error}");
                continue;
            }
        };
        let mut next = previous.clone();
        next.files
            .retain(|relative, _| current.files.contains_key(relative));
        next.directories
            .retain(|relative| current.directories.contains(relative));
        for relative in current.directories.difference(&previous.directories) {
            let remote = join_windows_path(destination, relative)?;
            if let Err(error) = remote_create_directory(store, name, &remote) {
                eprintln!("lsw sync: could not create {relative}: {error}");
            } else {
                next.directories.insert(relative.clone());
                println!("Synchronized {relative}/");
            }
        }
        for (relative, stamp) in &current.files {
            if previous.files.get(relative) == Some(stamp) {
                continue;
            }
            let local = source.join(relative_path(relative)?);
            let remote = join_windows_path(destination, relative)?;
            if let Err(error) = put_file_replacing(store, name, &local, &remote) {
                eprintln!("lsw sync: could not update {relative}: {error}");
            } else {
                next.files.insert(relative.clone(), *stamp);
                println!("Synchronized {relative}");
            }
        }
        previous = next;
    }
}

pub(super) fn sync_host_to_guest_silent(
    store: &StateStore,
    name: &str,
    source: &Path,
    destination: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    push_tree(store, name, source, destination, true, false)
}

pub(super) fn sync_guest_to_host(
    store: &StateStore,
    name: &str,
    source: &str,
    destination: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    pull_tree_replacing(store, name, source, destination)
}

#[derive(Debug)]
struct TransferArguments {
    requested: Option<String>,
    source: String,
    destination: String,
    recursive: bool,
    watch: bool,
}

impl TransferArguments {
    fn parse(
        arguments: &[OsString],
        command: &str,
        allow_recursive: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut recursive = false;
        let mut positional = Vec::new();
        for argument in arguments {
            if argument == OsStr::new("--recursive") && allow_recursive {
                if recursive {
                    return Err("--recursive was supplied more than once".into());
                }
                recursive = true;
            } else if argument.to_string_lossy().starts_with('-') {
                return Err(format!(
                    "usage: lsw {command} [NAME] [--recursive] SOURCE DESTINATION"
                )
                .into());
            } else {
                positional.push(
                    argument
                        .to_str()
                        .ok_or("transfer arguments must be valid UTF-8")?
                        .to_owned(),
                );
            }
        }
        let (requested, source, destination) = match positional.as_slice() {
            [source, destination] => (None, source.clone(), destination.clone()),
            [name, source, destination] => {
                (Some(name.clone()), source.clone(), destination.clone())
            }
            _ => {
                return Err(
                    format!("usage: lsw {command} [NAME] [--recursive] SOURCE DESTINATION").into(),
                )
            }
        };
        let (windows_path, role) = match command {
            "pull" => (&source, "source"),
            "push" | "sync" => (&destination, "destination"),
            _ => return Err(format!("unknown transfer command {command}").into()),
        };
        if !is_absolute_windows_path(windows_path) {
            return Err(format!(
                "lsw {command} requires an absolute Windows {role} path (for example C:\\work\\file)"
            )
            .into());
        }
        Ok(Self {
            requested,
            source,
            destination,
            recursive,
            watch: false,
        })
    }

    fn parse_sync(arguments: &[OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut watch = false;
        let filtered = arguments
            .iter()
            .filter_map(|argument| {
                if argument == OsStr::new("--watch") {
                    if watch {
                        return Some(Err("--watch was supplied more than once".into()));
                    }
                    watch = true;
                    None
                } else {
                    Some(Ok(argument.clone()))
                }
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        let mut parsed = Self::parse(&filtered, "sync", false)?;
        parsed.watch = watch;
        Ok(parsed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LocalEntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalEntry {
    relative: String,
    path: PathBuf,
    kind: LocalEntryKind,
}

fn push_tree(
    store: &StateStore,
    name: &str,
    source: &Path,
    destination: &str,
    replace: bool,
    report: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    require_real_directory(source)?;
    remote_create_directory(store, name, destination)?;
    let entries = collect_local_tree(source)?;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for entry in entries {
        let remote = join_windows_path(destination, &entry.relative)?;
        match entry.kind {
            LocalEntryKind::Directory => remote_create_directory(store, name, &remote)?,
            LocalEntryKind::File => {
                let transferred = if replace {
                    put_file_replacing(store, name, &entry.path, &remote)?
                } else {
                    connect_agent(store, name)?.put_file(&entry.path, &remote)?
                };
                files += 1;
                bytes = bytes.saturating_add(transferred);
            }
        }
    }
    if report {
        println!(
            "Transferred {files} files ({bytes} bytes) from {} to {name}:{destination}",
            source.display()
        );
    }
    Ok(())
}

fn pull_tree(
    store: &StateStore,
    name: &str,
    source: &str,
    destination: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_local_directory(destination)?;
    let entries = remote_tree(store, name, source)?;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for entry in entries {
        let relative = relative_path(&entry.relative)?;
        let local = destination.join(relative);
        match entry.kind {
            LocalEntryKind::Directory => ensure_local_directory(&local)?,
            LocalEntryKind::File => {
                let parent = local.parent().ok_or("local file has no parent directory")?;
                ensure_local_directory(parent)?;
                let remote = join_windows_path(source, &entry.relative)?;
                bytes =
                    bytes.saturating_add(connect_agent(store, name)?.get_file(&remote, &local)?);
                files += 1;
            }
        }
    }
    println!(
        "Transferred {files} files ({bytes} bytes) from {name}:{source} to {}",
        destination.display()
    );
    Ok(())
}

fn pull_tree_replacing(
    store: &StateStore,
    name: &str,
    source: &str,
    destination: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    require_real_directory(destination)?;
    let entries = remote_tree(store, name, source)?;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for entry in entries {
        let relative = relative_path(&entry.relative)?;
        let local = destination.join(relative);
        match entry.kind {
            LocalEntryKind::Directory => ensure_local_directory(&local)?,
            LocalEntryKind::File => {
                let parent = local.parent().ok_or("local file has no parent directory")?;
                ensure_local_directory(parent)?;
                let remote = join_windows_path(source, &entry.relative)?;
                bytes = bytes.saturating_add(get_file_replacing(store, name, &remote, &local)?);
                files += 1;
            }
        }
    }
    println!(
        "Synchronized {files} files ({bytes} bytes) from {name}:{source} to {}",
        destination.display()
    );
    Ok(())
}

fn collect_local_tree(root: &Path) -> Result<Vec<LocalEntry>, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    collect_local_directory(root, root, &mut entries)?;
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(entries)
}

fn collect_local_directory(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<LocalEntry>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut children = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "{} is a symbolic link; tree transfer refuses links",
                path.display()
            )
            .into());
        }
        let relative = path.strip_prefix(root).expect("walk remains below root");
        let relative = normalized_relative_path(relative)?;
        if metadata.is_dir() {
            entries.push(LocalEntry {
                relative: relative.clone(),
                path: path.clone(),
                kind: LocalEntryKind::Directory,
            });
            collect_local_directory(root, &path, entries)?;
        } else if metadata.is_file() {
            entries.push(LocalEntry {
                relative,
                path,
                kind: LocalEntryKind::File,
            });
        } else {
            return Err("tree transfer accepts only regular files and directories".into());
        }
    }
    Ok(())
}

fn normalized_relative_path(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or("tree paths must be valid UTF-8")?;
                if !windows_component_is_safe(value) {
                    return Err(format!(
                        "{} cannot be represented as a safe Windows path component",
                        path.display()
                    )
                    .into());
                }
                parts.push(value.to_owned());
            }
            _ => return Err("tree path escaped its source root".into()),
        }
    }
    if parts.is_empty() {
        return Err("tree relative path must not be empty".into());
    }
    Ok(parts.join("/"))
}

fn relative_path(value: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut path = PathBuf::new();
    for component in value.split(['/', '\\']) {
        if !windows_component_is_safe(component) {
            return Err(format!("unsafe relative transfer path {value:?}").into());
        }
        path.push(component);
    }
    if path.as_os_str().is_empty() {
        return Err("relative transfer path must not be empty".into());
    }
    Ok(path)
}

fn windows_component_is_safe(component: &str) -> bool {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with([' ', '.'])
        || component
            .chars()
            .any(|character| character <= '\u{1f}' || "<>:\"/\\|?*".contains(character))
    {
        return false;
    }
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !matches!(
            stem.as_str(),
            "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
}

fn join_windows_path(root: &str, relative: &str) -> Result<String, Box<dyn std::error::Error>> {
    let relative = relative_path(relative)?;
    let relative = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("\\");
    Ok(format!(
        "{}\\{relative}",
        root.trim_end_matches(['/', '\\'])
    ))
}

fn require_real_directory(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("{} must be a real directory", path.display()).into());
    }
    Ok(())
}

fn ensure_local_directory(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(format!("{} must be a real directory", path.display()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            if let Some(parent) = parent {
                ensure_local_directory(parent)?;
            }
            fs::create_dir(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn remote_create_directory(
    store: &StateStore,
    name: &str,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    remote_powershell(
        store,
        name,
        r#"$ErrorActionPreference='Stop'; $Full=[IO.Path]::GetFullPath($env:LSW_PATH); $Root=[IO.Path]::GetPathRoot($Full); $Current=$Root; foreach ($Part in $Full.Substring($Root.Length).Split([char[]]'\/',[StringSplitOptions]::RemoveEmptyEntries)) { $Current=[IO.Path]::Combine($Current,$Part); if (Test-Path -LiteralPath $Current) { $Item=Get-Item -LiteralPath $Current -Force; if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw ('transfer path crosses a reparse point: '+$Item.FullName) } } }; $Directory=[IO.Directory]::CreateDirectory($Full); if (($Directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'transfer destination is a reparse point' }"#,
        vec![("LSW_PATH".to_owned(), path.to_owned())],
    )
    .map(|_| ())
}

fn put_file_replacing(
    store: &StateStore,
    name: &str,
    source: &Path,
    destination: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    remote_powershell(
        store,
        name,
        r#"$ErrorActionPreference='Stop'; $Full=[IO.Path]::GetFullPath($env:LSW_PATH); $Parent=[IO.Path]::GetDirectoryName($Full); $Root=[IO.Path]::GetPathRoot($Parent); $Current=$Root; foreach ($Part in $Parent.Substring($Root.Length).Split([char[]]'\/',[StringSplitOptions]::RemoveEmptyEntries)) { $Current=[IO.Path]::Combine($Current,$Part); $Item=Get-Item -LiteralPath $Current -Force; if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw ('transfer path crosses a reparse point: '+$Item.FullName) } }"#,
        vec![("LSW_PATH".to_owned(), destination.to_owned())],
    )?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = format!("{destination}.lsw-sync-{}-{nonce}", std::process::id());
    let bytes = connect_agent(store, name)?.put_file(source, &temporary)?;
    let result = remote_powershell(
        store,
        name,
        r#"$ErrorActionPreference='Stop'; if (Test-Path -LiteralPath $env:LSW_DEST -PathType Container) { throw 'sync destination is a directory' }; Move-Item -LiteralPath $env:LSW_TEMP -Destination $env:LSW_DEST -Force"#,
        vec![
            ("LSW_TEMP".to_owned(), temporary.clone()),
            ("LSW_DEST".to_owned(), destination.to_owned()),
        ],
    );
    if result.is_err() {
        let _ = remote_powershell(
            store,
            name,
            r#"Remove-Item -LiteralPath $env:LSW_TEMP -Force -ErrorAction SilentlyContinue"#,
            vec![("LSW_TEMP".to_owned(), temporary)],
        );
    }
    result.map(|_| bytes)
}

pub(super) fn set_guest_share_read_only(
    store: &StateStore,
    name: &str,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    remote_powershell(
        store,
        name,
        READ_ONLY_SHARE_ACL_SCRIPT,
        vec![("LSW_PATH".to_owned(), path.to_owned())],
    )
    .map(|_| ())
}

fn get_file_replacing(
    store: &StateStore,
    name: &str,
    source: &str,
    destination: &Path,
) -> Result<u64, Box<dyn std::error::Error>> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(format!(
                "{} must be a regular non-symlink file",
                destination.display()
            )
            .into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let temporary = destination.with_file_name(format!(
        ".lsw-download-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let bytes = connect_agent(store, name)?.get_file(source, &temporary)?;
    let result = fs::rename(&temporary, destination);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(bytes)
}

fn remote_tree(
    store: &StateStore,
    name: &str,
    root: &str,
) -> Result<Vec<RemoteEntry>, Box<dyn std::error::Error>> {
    let output = remote_powershell(
        store,
        name,
        r#"$ErrorActionPreference='Stop'; $Full=[IO.Path]::GetFullPath($env:LSW_ROOT); $Drive=[IO.Path]::GetPathRoot($Full); $Current=$Drive; foreach ($Part in $Full.Substring($Drive.Length).Split([char[]]'\/',[StringSplitOptions]::RemoveEmptyEntries)) { $Current=[IO.Path]::Combine($Current,$Part); $Item=Get-Item -LiteralPath $Current -Force; if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw ('remote source crosses a reparse point: '+$Item.FullName) } }; $Root=Get-Item -LiteralPath $Full -Force; if (-not $Root.PSIsContainer) { throw 'remote source is not a directory' }; $Prefix=$Root.FullName.TrimEnd('\')+'\'; Get-ChildItem -LiteralPath $Root.FullName -Force -Recurse | ForEach-Object { if (($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw ('reparse points are not supported: '+$_.FullName) }; $Relative=$_.FullName.Substring($Prefix.Length); $Hex=([BitConverter]::ToString([Text.Encoding]::UTF8.GetBytes($Relative))).Replace('-',''); if ($_.PSIsContainer) { [Console]::Out.WriteLine(("D`t{0}" -f $Hex)) } else { [Console]::Out.WriteLine(("F`t{0}`t{1}" -f $_.Length,$Hex)) } }"#,
        vec![("LSW_ROOT".to_owned(), root.to_owned())],
    )?;
    parse_remote_tree(&output)
}

#[derive(Debug, Eq, PartialEq)]
struct RemoteEntry {
    relative: String,
    kind: LocalEntryKind,
}

fn parse_remote_tree(output: &[u8]) -> Result<Vec<RemoteEntry>, Box<dyn std::error::Error>> {
    let output = std::str::from_utf8(output)?;
    let mut entries = Vec::new();
    let mut paths = BTreeSet::new();
    for line in output.lines().filter(|line| !line.is_empty()) {
        let fields = line.split('\t').collect::<Vec<_>>();
        let (kind, encoded) = match fields.as_slice() {
            ["D", encoded] => (LocalEntryKind::Directory, *encoded),
            ["F", length, encoded] => {
                length.parse::<u64>()?;
                (LocalEntryKind::File, *encoded)
            }
            _ => return Err("guest returned an invalid tree entry".into()),
        };
        let relative = String::from_utf8(decode_hex(encoded)?)?;
        relative_path(&relative)?;
        if !paths.insert(relative.clone()) {
            return Err(format!("guest returned duplicate tree path {relative:?}").into());
        }
        entries.push(RemoteEntry { relative, kind });
    }
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(entries)
}

fn decode_hex(value: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("guest returned invalid hexadecimal path data".into());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair)?;
            Ok(u8::from_str_radix(pair, 16)?)
        })
        .collect()
}

fn remote_powershell(
    store: &StateStore,
    name: &str,
    script: &str,
    variables: Vec<(String, String)>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: vec![
            "powershell.exe".to_owned(),
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ],
        working_directory: None,
    };
    let environment = ProcessEnvironment::new(variables)?;
    let process = connect_agent(store, name)?.run_capture_with_environment(
        &request,
        &[],
        REMOTE_OUTPUT_LIMIT,
        &environment,
    )?;
    if process.exit_code != 0 {
        return Err(format!(
            "guest PowerShell exited with {}: {}",
            process.exit_code,
            String::from_utf8_lossy(&process.stderr).trim()
        )
        .into());
    }
    Ok(process.stdout)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
    length: u64,
    modified_nanos: u128,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalSnapshot {
    directories: BTreeSet<String>,
    files: BTreeMap<String, FileStamp>,
}

fn local_snapshot(root: &Path) -> Result<LocalSnapshot, Box<dyn std::error::Error>> {
    let mut directories = BTreeSet::new();
    let mut files = BTreeMap::new();
    for entry in collect_local_tree(root)? {
        if entry.kind == LocalEntryKind::Directory {
            directories.insert(entry.relative);
        } else {
            let metadata = fs::metadata(&entry.path)?;
            let modified_nanos = metadata
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            files.insert(
                entry.relative,
                FileStamp {
                    length: metadata.len(),
                    modified_nanos,
                },
            );
        }
    }
    Ok(LocalSnapshot { directories, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_share_acl_keeps_the_agent_as_an_explicit_writer() {
        assert!(READ_ONLY_SHARE_ACL_SCRIPT.contains("SetAccessRuleProtection($true,$false)"));
        assert!(READ_ONLY_SHARE_ACL_SCRIPT.contains("WindowsIdentity]::GetCurrent().User"));
        assert!(READ_ONLY_SHARE_ACL_SCRIPT.contains("S-1-5-18"));
        assert!(READ_ONLY_SHARE_ACL_SCRIPT.contains("S-1-5-32-544"));
        assert!(READ_ONLY_SHARE_ACL_SCRIPT.contains("S-1-5-32-545"));
        assert!(READ_ONLY_SHARE_ACL_SCRIPT.contains("ReadAndExecute"));
        assert!(!READ_ONLY_SHARE_ACL_SCRIPT.contains("AccessControlType]::Deny"));
    }

    #[test]
    fn remote_tree_parser_is_binary_safe_and_rejects_traversal() {
        let output = b"D\t737263\nF\t3\t7372635C6D61696E2E7273\n";
        assert_eq!(
            parse_remote_tree(output).expect("tree should parse"),
            vec![
                RemoteEntry {
                    relative: "src".to_owned(),
                    kind: LocalEntryKind::Directory,
                },
                RemoteEntry {
                    relative: "src\\main.rs".to_owned(),
                    kind: LocalEntryKind::File,
                },
            ]
        );
        assert!(parse_remote_tree(b"F\t1\t2E2E5C657363617065\n").is_err());
        assert!(parse_remote_tree(b"D\t737263\nF\t1\t737263\n").is_err());
    }

    #[test]
    fn windows_join_accepts_only_relative_children() {
        assert_eq!(
            join_windows_path("C:\\src\\", "nested/main.rs").unwrap(),
            "C:\\src\\nested\\main.rs"
        );
        assert!(join_windows_path("C:\\src", "../escape").is_err());
        assert!(join_windows_path("C:\\src", "D:/escape").is_err());
        assert!(join_windows_path("C:\\src", "CON.txt").is_err());
        assert!(join_windows_path("C:\\src", "trailing.").is_err());
        assert!(normalized_relative_path(Path::new("literal\\backslash")).is_err());
    }

    #[test]
    fn transfer_argument_parser_keeps_instance_optional() {
        let parsed = TransferArguments::parse(
            &[
                "win-dev".into(),
                "--recursive".into(),
                "src".into(),
                "C:\\src".into(),
            ],
            "push",
            true,
        )
        .unwrap();
        assert_eq!(parsed.requested.as_deref(), Some("win-dev"));
        assert!(parsed.recursive);
        assert_eq!(parsed.source, "src");
    }

    #[test]
    fn explicit_transfers_reject_relative_windows_paths() {
        assert!(
            TransferArguments::parse(&["source".into(), "C:relative".into()], "push", true,)
                .is_err()
        );
        assert!(TransferArguments::parse(
            &["C:relative".into(), "destination".into()],
            "pull",
            true,
        )
        .is_err());
        assert!(TransferArguments::parse_sync(&["source".into(), "relative".into()]).is_err());
        assert!(
            TransferArguments::parse(&["source".into(), "C:/absolute".into()], "push", true,)
                .is_ok()
        );
    }

    #[test]
    fn copy_direction_requires_exactly_one_absolute_windows_path() {
        assert!(is_absolute_windows_path("C:\\work\\file.txt"));
        assert!(is_absolute_windows_path("d:/work/file.txt"));
        assert!(is_absolute_windows_path("\\\\server\\share\\file.txt"));
        assert!(!is_absolute_windows_path("C:file.txt"));
        assert!(!is_absolute_windows_path("./C:\\file.txt"));
        assert!(!is_absolute_windows_path("/home/user/file.txt"));
    }
}
