// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt::Write as FmtWrite;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use md4::{Digest, Md4};

use crate::store::write_atomic;
use crate::{
    FolderShareTransport, InstanceManifest, LswError, Result, LIVE_SMB_CONFIG_FILE,
    LIVE_SMB_RUNTIME_DIRECTORY,
};

const LIVE_SMB_PASSWORD_FILE: &str = "smbpasswd";
const LIVE_SMB_USERNAME_MAP_FILE: &str = "username.map";
const LIVE_SMB_CLIENT_USER: &str = "lsw";

pub fn prepare_live_share_runtime(
    manifest: &InstanceManifest,
    instance_dir: &Path,
    credential: &str,
) -> Result<()> {
    let runtime = instance_dir.join("run").join(LIVE_SMB_RUNTIME_DIRECTORY);
    let Some(share) = manifest
        .folder_shares
        .iter()
        .find(|share| share.transport == FolderShareTransport::LiveSmb)
    else {
        remove_runtime_if_present(&runtime)?;
        return Ok(());
    };
    if credential.len() != 64
        || !credential
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(LswError::InvalidValue {
            field: "live folder credential",
            reason: "must contain 64 lowercase hexadecimal characters".to_owned(),
        });
    }
    require_real_directory(instance_dir, "instance directory")?;
    require_real_directory(&instance_dir.join("run"), "runtime directory")?;
    remove_runtime_if_present(&runtime)?;
    ensure_private_runtime_directory(&runtime)?;

    let metadata = fs::metadata(instance_dir)?;
    let uid = metadata.uid();
    let unix_user = account_name_for_uid(uid)?;
    let share_root = canonical_share_root(&share.host_path)?;
    let config = samba_config(&runtime, &share_root, &unix_user)?;
    let username_map = format!("{unix_user} = {LIVE_SMB_CLIENT_USER}\n");
    let password = smb_password_entry(&unix_user, uid, credential);

    for path in [
        runtime.join(LIVE_SMB_CONFIG_FILE),
        runtime.join(LIVE_SMB_PASSWORD_FILE),
        runtime.join(LIVE_SMB_USERNAME_MAP_FILE),
    ] {
        require_missing_or_regular_file(&path)?;
    }
    write_atomic(
        &runtime.join(LIVE_SMB_USERNAME_MAP_FILE),
        username_map.as_bytes(),
    )?;
    write_atomic(&runtime.join(LIVE_SMB_PASSWORD_FILE), password.as_bytes())?;
    write_atomic(&runtime.join(LIVE_SMB_CONFIG_FILE), config.as_bytes())?;
    Ok(())
}

fn canonical_share_root(path: &Path) -> Result<PathBuf> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if metadata.file_type().is_symlink() {
            return Err(LswError::InvalidValue {
                field: "live folder share root",
                reason: format!("{} crosses a symbolic link", path.display()),
            });
        }
    }
    let root = fs::canonicalize(path)?;
    require_real_directory(&root, "live folder share root")?;
    Ok(root)
}

fn account_name_for_uid(uid: u32) -> Result<String> {
    let passwd = fs::read_to_string("/etc/passwd")?;
    let account = passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        let candidate_uid = fields.next()?.parse::<u32>().ok()?;
        (candidate_uid == uid).then(|| name.to_owned())
    });
    let account = account.ok_or_else(|| LswError::InvalidValue {
        field: "live folder host account",
        reason: format!("could not resolve uid {uid} through /etc/passwd"),
    })?;
    if account.is_empty()
        || !account
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err(LswError::InvalidValue {
            field: "live folder host account",
            reason: "the resolved Unix account name is not safe for a private Samba mapping"
                .to_owned(),
        });
    }
    Ok(account)
}

fn samba_config(runtime: &Path, share_root: &Path, unix_user: &str) -> Result<String> {
    let runtime = samba_value(runtime)?;
    let share_root = samba_value(share_root)?;
    Ok(format!(
        "[global]\n\
         \tprivate dir = {runtime}\n\
         \tpid directory = {runtime}\n\
         \tlock directory = {runtime}\n\
         \tstate directory = {runtime}\n\
         \tcache directory = {runtime}\n\
         \tncalrpc dir = {runtime}/ncalrpc\n\
         \tlog file = {runtime}/log.smbd\n\
         \tmax log size = 1024\n\
         \tserver role = standalone server\n\
         \tsecurity = user\n\
         \tmap to guest = Never\n\
         \tpassdb backend = smbpasswd:{runtime}/{LIVE_SMB_PASSWORD_FILE}\n\
         \tusername map = {runtime}/{LIVE_SMB_USERNAME_MAP_FILE}\n\
         \tserver min protocol = SMB2_10\n\
         \tserver signing = mandatory\n\
         \tserver smb encrypt = required\n\
         \tload printers = no\n\
         \tprinting = bsd\n\
         \tdisable spoolss = yes\n\
         \tusershare max shares = 0\n\
         \tdns proxy = no\n\
         \tlocal master = no\n\
         \n\
         [qemu]\n\
         \tpath = {share_root}\n\
         \tread only = no\n\
         \tguest ok = no\n\
         \tbrowseable = no\n\
         \tvalid users = {unix_user}\n\
         \tforce user = {unix_user}\n\
         \tfollow symlinks = no\n\
         \twide links = no\n\
         \tsmb encrypt = required\n"
    ))
}

fn samba_value(path: &Path) -> Result<String> {
    let value = path.to_str().ok_or_else(|| LswError::InvalidValue {
        field: "live folder Samba path",
        reason: format!("{} is not valid UTF-8", path.display()),
    })?;
    if value.trim() != value || value.contains(['\r', '\n', '\0', '%', '#', ';', '"', '\\']) {
        return Err(LswError::InvalidValue {
            field: "live folder Samba path",
            reason: format!(
                "{} contains syntax that is unsafe in a private Samba configuration",
                path.display()
            ),
        });
    }
    Ok(value.to_owned())
}

fn smb_password_entry(unix_user: &str, uid: u32, credential: &str) -> String {
    let mut utf16 = Vec::with_capacity(credential.len() * 2);
    for unit in credential.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let digest = Md4::digest(&utf16);
    utf16.fill(0);
    let mut nt_hash = String::with_capacity(32);
    for byte in digest {
        write!(nt_hash, "{byte:02X}").expect("writing to a String cannot fail");
    }
    let changed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    format!(
        "{unix_user}:{uid}:XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX:{nt_hash}:[UX         ]:LCT-{changed:08X}:\n"
    )
}

fn ensure_private_runtime_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(LswError::InvalidValue {
                field: "live folder runtime",
                reason: format!("{} must be a real directory", path.display()),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn require_real_directory(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        return Ok(());
    }
    Err(LswError::InvalidValue {
        field,
        reason: format!("{} must be a real directory", path.display()),
    })
}

fn require_missing_or_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(LswError::InvalidValue {
            field: "live folder credential file",
            reason: format!("{} must be a regular non-symlink file", path.display()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_runtime_if_present(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(LswError::InvalidValue {
            field: "live folder runtime",
            reason: format!("refusing to remove unexpected path {}", path.display()),
        });
    }
    remove_runtime_children(path)?;
    fs::remove_dir(path)?;
    Ok(())
}

fn remove_runtime_children(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(LswError::InvalidValue {
                field: "live folder runtime",
                reason: format!(
                    "refusing to remove symbolic link {}",
                    entry.path().display()
                ),
            });
        }
        if metadata.file_type().is_dir() {
            remove_runtime_children(&entry.path())?;
            fs::remove_dir(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{FolderShare, FolderShareMode, InstanceSpec, NetworkMode, WindowsProfile};
    use std::net::{Ipv4Addr, TcpListener};
    use std::os::fd::OwnedFd;
    use std::process::{Command, Stdio};
    use std::thread;

    use super::*;

    #[test]
    fn nt_hash_matches_the_samba_password_format() {
        assert!(smb_password_entry("user", 1000, "password")
            .contains(":8846F7EAEE8FB117AD06BDD830B7586C:"));
    }

    #[test]
    fn samba_paths_allow_spaces_but_reject_configuration_syntax() {
        assert_eq!(
            samba_value(Path::new("/tmp/LSW Shared")).expect("spaces should be retained"),
            "/tmp/LSW Shared"
        );
        for path in ["/tmp/%U", "/tmp/#root", "/tmp/root;guest ok=yes"] {
            assert!(samba_value(Path::new(path)).is_err());
        }
    }

    #[test]
    fn runtime_contains_only_private_authenticated_configuration() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lsw-samba-test-{nonce}"));
        let share = root.join("host share");
        fs::create_dir_all(root.join("run")).expect("runtime root should be created");
        fs::create_dir(&share).expect("share should be created");
        let iso = root.join("windows.iso");
        fs::write(&iso, b"iso").expect("ISO fixture should be written");
        let mut manifest = InstanceManifest::new(InstanceSpec {
            name: "windows".to_owned(),
            source_iso: iso,
            profile: WindowsProfile::Vanilla,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 64,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            license_accepted: true,
            allow_unsupported_requirements: false,
        })
        .expect("manifest should be valid");
        manifest.folder_shares.push(FolderShare {
            name: "linux".to_owned(),
            host_path: share,
            guest_path: "L:\\".to_owned(),
            mode: FolderShareMode::ReadWrite,
            transport: FolderShareTransport::LiveSmb,
        });
        let credential = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        prepare_live_share_runtime(&manifest, &root, credential)
            .expect("authenticated runtime should be prepared");
        let runtime = root.join("run").join(LIVE_SMB_RUNTIME_DIRECTORY);
        let config = fs::read_to_string(runtime.join(LIVE_SMB_CONFIG_FILE))
            .expect("configuration should be readable");
        let password = fs::read_to_string(runtime.join(LIVE_SMB_PASSWORD_FILE))
            .expect("password database should be readable");
        assert!(config.contains("map to guest = Never"));
        assert!(config.contains("server signing = mandatory"));
        assert!(config.contains("server smb encrypt = required"));
        assert!(config.contains("follow symlinks = no"));
        assert!(!config.contains(credential));
        assert!(!password.contains(credential));
        match std::process::Command::new("testparm")
            .args(["-s", "--parameter-name=server role"])
            .arg(runtime.join(LIVE_SMB_CONFIG_FILE))
            .output()
        {
            Ok(output) => assert!(
                output.status.success(),
                "Samba rejected the generated configuration: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("could not validate the Samba configuration: {error}"),
        }
        verify_local_authenticated_connection(&runtime, credential);
        for file in [
            LIVE_SMB_CONFIG_FILE,
            LIVE_SMB_PASSWORD_FILE,
            LIVE_SMB_USERNAME_MAP_FILE,
        ] {
            assert_eq!(
                fs::metadata(runtime.join(file))
                    .expect("runtime file should exist")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        manifest.folder_shares.clear();
        prepare_live_share_runtime(&manifest, &root, credential)
            .expect("disabled runtime should be removed");
        assert!(!runtime.exists());
        fs::remove_dir_all(root).expect("fixture should be removable");
    }

    fn verify_local_authenticated_connection(runtime: &Path, credential: &str) {
        if Command::new("smbd").arg("--version").output().is_err()
            || Command::new("smbclient").arg("--version").output().is_err()
            || Command::new("setsid").arg("--version").output().is_err()
        {
            return;
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("a Samba test port should be available");
        let port = listener
            .local_addr()
            .expect("the test port should have an address")
            .port();
        let runtime = runtime.to_owned();
        let server = thread::spawn(move || {
            let (stream, _) = listener
                .accept()
                .expect("the local Samba test connection should arrive");
            let input = stream
                .try_clone()
                .expect("the accepted socket should be cloneable");
            let input: OwnedFd = input.into();
            let output: OwnedFd = stream.into();
            Command::new("setsid")
                .arg("smbd")
                .arg("-l")
                .arg(&runtime)
                .arg("-s")
                .arg(runtime.join(LIVE_SMB_CONFIG_FILE))
                .stdin(Stdio::from(input))
                .stdout(Stdio::from(output))
                .stderr(Stdio::piped())
                .output()
                .expect("the inetd-mode Samba server should run")
        });
        let client = Command::new("smbclient")
            .arg("//127.0.0.1/qemu")
            .args(["-p", &port.to_string(), "-U", &format!("lsw%{credential}")])
            .args(["-c", "ls"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("the local Samba client should run");
        let server = server
            .join()
            .expect("the inetd-mode Samba server should finish");
        assert!(
            client.status.success(),
            "authenticated Samba login failed with {}: {}{}; server: {}",
            client.status,
            String::from_utf8_lossy(&client.stdout),
            String::from_utf8_lossy(&client.stderr),
            String::from_utf8_lossy(&server.stderr),
        );
    }
}
