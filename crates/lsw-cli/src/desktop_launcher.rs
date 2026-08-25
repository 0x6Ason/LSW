// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lsw_core::{GuiIconRequest, GuiStartRequest, ProcessEnvironment, StartRequest, StateStore};

use super::{connect_agent, parse_guest_environment, resolve_name};

const USAGE: &str = "usage: lsw app install [NAME] [--title TITLE] [--cwd PATH] [-e KEY=VALUE] -- PROGRAM.exe [ARG ...] | lsw app <list|remove ID>";

pub(super) fn command(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().and_then(|value| value.to_str()) {
        Some("install") => install(store, &arguments[1..]),
        Some("list") if arguments.len() == 1 => list(),
        Some("remove") if arguments.len() == 2 => remove(
            arguments[1]
                .to_str()
                .ok_or("launcher ID must be valid UTF-8")?,
        ),
        _ => Err(USAGE.into()),
    }
}

fn install(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = InstallArguments::parse(arguments)?;
    let name = resolve_name(store, parsed.requested.as_deref())?;
    let manifest = store.load(&name)?;
    let user_name = manifest.default_user.ok_or_else(|| {
        format!(
            "instance {name:?} has no registered Windows desktop user; run `lsw user setup {name}`"
        )
    })?;
    let title = parsed
        .title
        .unwrap_or_else(|| default_title(&parsed.request.argv[0]));
    validate_title(&title)?;
    let title_slug = slug(&title);
    let id = format!(
        "lsw-{}-{}",
        name,
        if title_slug.is_empty() {
            "app"
        } else {
            &title_slug
        }
    );
    validate_id(&id)?;

    let icon = connect_agent(store, &name)?.gui_icon(&GuiIconRequest {
        user_name,
        program: parsed.request.argv[0].clone(),
    })?;
    let roots = LauncherRoots::discover()?;
    fs::create_dir_all(&roots.applications)?;
    fs::create_dir_all(&roots.icons)?;
    let icon_path = roots.icons.join(format!("{id}.ico"));
    let desktop_path = roots.applications.join(format!("{id}.desktop"));
    let icon_existed = validate_existing_launcher(&desktop_path, &icon_path, &id)?;
    atomic_write(&icon_path, &icon, 0o644)?;

    let executable = env::current_exe()?;
    let entry = desktop_entry(
        &title,
        &id,
        &name,
        &icon_path,
        &executable,
        &parsed.request,
        &parsed.environment,
    )?;
    if let Err(error) = atomic_write(&desktop_path, entry.as_bytes(), 0o600) {
        if !icon_existed {
            let _ = fs::remove_file(&icon_path);
        }
        return Err(error);
    }
    println!("Installed desktop launcher {id:?} for {title:?}.");
    println!("Launcher: {}", desktop_path.display());
    println!("Dropped host files are translated through the approved Linux (L:) share.");
    Ok(())
}

fn list() -> Result<(), Box<dyn std::error::Error>> {
    let roots = LauncherRoots::discover()?;
    let Ok(entries) = fs::read_dir(roots.applications) else {
        println!("No LSW desktop launchers.");
        return Ok(());
    };
    let mut ids = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| {
            name.strip_prefix("lsw-")
                .and_then(|_| name.strip_suffix(".desktop"))
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    ids.sort();
    if ids.is_empty() {
        println!("No LSW desktop launchers.");
    } else {
        println!("ID");
        for id in ids {
            println!("{id}");
        }
    }
    Ok(())
}

fn remove(id: &str) -> Result<(), Box<dyn std::error::Error>> {
    validate_id(id)?;
    if !id.starts_with("lsw-") {
        return Err("launcher ID must begin with lsw-".into());
    }
    let roots = LauncherRoots::discover()?;
    let desktop = roots.applications.join(format!("{id}.desktop"));
    let icon = roots.icons.join(format!("{id}.ico"));
    let _ = validate_existing_launcher(&desktop, &icon, id)?;
    let mut removed = false;
    for path in [&desktop, &icon] {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                fs::remove_file(path)?;
                removed = true;
            }
            Ok(_) => {
                return Err(format!(
                    "refusing to remove non-regular launcher path {}",
                    path.display()
                )
                .into())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if !removed {
        return Err(format!("desktop launcher {id:?} does not exist").into());
    }
    println!("Removed desktop launcher {id:?}.");
    Ok(())
}

struct InstallArguments {
    requested: Option<String>,
    title: Option<String>,
    request: StartRequest,
    environment: ProcessEnvironment,
}

impl InstallArguments {
    fn parse(arguments: &[OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let separator = arguments
            .iter()
            .position(|argument| argument == "--")
            .ok_or(USAGE)?;
        if separator + 1 >= arguments.len() {
            return Err(USAGE.into());
        }
        let mut requested = None;
        let mut title = None;
        let mut working_directory = None;
        let mut environment = Vec::new();
        let mut index = 0;
        while index < separator {
            let argument = arguments[index]
                .to_str()
                .ok_or("launcher options must be valid UTF-8")?;
            match argument {
                "--title" => {
                    index += 1;
                    let value = arguments
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or(USAGE)?;
                    if value.is_empty() || title.replace(value.to_owned()).is_some() {
                        return Err("--title requires one non-empty value".into());
                    }
                }
                "--cwd" | "-C" => {
                    index += 1;
                    let value = arguments
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or(USAGE)?;
                    if value.is_empty() || working_directory.replace(value.to_owned()).is_some() {
                        return Err("--cwd requires one non-empty path".into());
                    }
                }
                "--env" | "-e" => {
                    index += 1;
                    let value = arguments
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or(USAGE)?;
                    environment.push(parse_guest_environment(value)?);
                }
                _ if argument.starts_with("--title=") => {
                    let value = argument.trim_start_matches("--title=");
                    if value.is_empty() || title.replace(value.to_owned()).is_some() {
                        return Err("--title requires one non-empty value".into());
                    }
                }
                _ if argument.starts_with("--cwd=") => {
                    let value = argument.trim_start_matches("--cwd=");
                    if value.is_empty() || working_directory.replace(value.to_owned()).is_some() {
                        return Err("--cwd requires one non-empty path".into());
                    }
                }
                _ if argument.starts_with("--env=") => environment.push(parse_guest_environment(
                    argument.trim_start_matches("--env="),
                )?),
                _ if argument.starts_with('-') => return Err(USAGE.into()),
                _ => {
                    if requested.replace(argument.to_owned()).is_some() {
                        return Err(USAGE.into());
                    }
                }
            }
            index += 1;
        }
        let argv = arguments[separator + 1..]
            .iter()
            .map(|argument| {
                argument
                    .to_str()
                    .map(str::to_owned)
                    .ok_or("launcher command must be valid UTF-8")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let request = StartRequest {
            kind: lsw_core::SessionKind::Run,
            argv,
            working_directory,
        };
        let environment = ProcessEnvironment::new(environment)?;
        GuiStartRequest {
            user_name: "validation-user".to_owned(),
            request: request.clone(),
            environment: environment.clone(),
            mount_live_share: false,
        }
        .encode()?;
        Ok(Self {
            requested,
            title,
            request,
            environment,
        })
    }
}

struct LauncherRoots {
    applications: PathBuf,
    icons: PathBuf,
}

impl LauncherRoots {
    fn discover() -> Result<Self, Box<dyn std::error::Error>> {
        let data = if let Some(path) = env::var_os("XDG_DATA_HOME") {
            PathBuf::from(path)
        } else {
            PathBuf::from(env::var_os("HOME").ok_or("HOME is not set")?).join(".local/share")
        };
        if !data.is_absolute() {
            return Err("XDG_DATA_HOME must be an absolute path".into());
        }
        Ok(Self {
            applications: data.join("applications"),
            icons: data.join("icons/lsw"),
        })
    }
}

fn desktop_entry(
    title: &str,
    id: &str,
    instance: &str,
    icon: &Path,
    lsw: &Path,
    request: &StartRequest,
    environment: &ProcessEnvironment,
) -> Result<String, Box<dyn std::error::Error>> {
    let icon = icon.to_str().ok_or("icon path is not valid UTF-8")?;
    let lsw = lsw
        .to_str()
        .ok_or("LSW executable path is not valid UTF-8")?;
    let mut command = vec![
        lsw.to_owned(),
        "run".to_owned(),
        instance.to_owned(),
        "--gui".to_owned(),
        "--translate-files".to_owned(),
    ];
    if let Some(directory) = &request.working_directory {
        command.push("--cwd".to_owned());
        command.push(directory.clone());
    }
    for (name, value) in &environment.variables {
        command.push("--env".to_owned());
        command.push(format!("{name}={value}"));
    }
    command.push("--".to_owned());
    command.extend(request.argv.iter().cloned());
    let mut exec = command
        .iter()
        .map(|argument| desktop_exec_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    exec.push_str(" %F");
    Ok(format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName={}\nComment=Run {} in LSW instance {}\nTryExec={}\nExec={}\nIcon={}\nTerminal=false\nStartupNotify=true\nCategories=Utility;\nX-LSW-Launcher={}\nX-LSW-Instance={}\nX-LSW-Program={}\n",
        desktop_value(title),
        desktop_value(title),
        desktop_value(instance),
        desktop_value(lsw),
        exec,
        desktop_value(icon),
        desktop_value(id),
        desktop_value(instance),
        desktop_value(&request.argv[0]),
    ))
}

fn desktop_exec_quote(value: &str) -> String {
    let escaped = value
        .replace('%', "%%")
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$");
    format!("\"{escaped}\"")
}

fn desktop_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "")
}

fn default_title(program: &str) -> String {
    program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(program)
        .strip_suffix(".exe")
        .or_else(|| program.strip_suffix(".EXE"))
        .unwrap_or(program)
        .to_owned()
}

fn validate_title(title: &str) -> Result<(), Box<dyn std::error::Error>> {
    if title.trim() != title
        || title.is_empty()
        || title.len() > 128
        || title.contains(['\0', '\r', '\n'])
    {
        return Err("launcher title must contain 1-128 characters without surrounding whitespace or line breaks".into());
    }
    Ok(())
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut hyphen = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            hyphen = false;
        } else if !hyphen && !output.is_empty() {
            output.push('-');
            hyphen = true;
        }
    }
    output.trim_end_matches('-').chars().take(48).collect()
}

fn validate_id(id: &str) -> Result<(), Box<dyn std::error::Error>> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("launcher ID must contain lowercase ASCII letters, digits, or hyphens".into());
    }
    Ok(())
}

fn validate_existing_launcher(
    desktop: &Path,
    icon: &Path,
    id: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let managed = match fs::symlink_metadata(desktop) {
        Ok(metadata) if metadata.file_type().is_file() && metadata.len() <= 64 * 1024 => {
            fs::read_to_string(desktop)?.lines().any(|line| {
                line.strip_prefix("X-LSW-Launcher=")
                    .is_some_and(|value| value == id)
            })
        }
        Ok(metadata) if metadata.file_type().is_file() => false,
        Ok(_) => {
            return Err(format!(
                "refusing to replace non-regular launcher path {}",
                desktop.display()
            )
            .into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if desktop.exists() && !managed {
        return Err(format!(
            "refusing to overwrite desktop entry not owned by LSW: {}",
            desktop.display()
        )
        .into());
    }
    match fs::symlink_metadata(icon) {
        Ok(metadata) if metadata.file_type().is_file() && managed => Ok(true),
        Ok(metadata) if metadata.file_type().is_file() => Err(format!(
            "refusing to overwrite icon not owned by an existing LSW launcher: {}",
            icon.display()
        )
        .into()),
        Ok(_) => Err(format!(
            "refusing to replace non-regular launcher icon {}",
            icon.display()
        )
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn atomic_write(
    destination: &Path,
    contents: &[u8],
    mode: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let parent = destination.parent().ok_or("launcher path has no parent")?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        destination
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or("launcher filename is not valid UTF-8")?,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, destination)?;
        fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_exec_is_quoted_and_keeps_file_field_code_outside_quotes() {
        let request = StartRequest {
            kind: lsw_core::SessionKind::Run,
            argv: vec!["C:\\Program Files\\Viewer\\view.exe".to_owned()],
            working_directory: Some("C:\\Work Tree".to_owned()),
        };
        let entry = desktop_entry(
            "Viewer",
            "lsw-win-dev-viewer",
            "win-dev",
            Path::new("/home/test/.local/share/icons/lsw/viewer.ico"),
            Path::new("/usr/bin/lsw"),
            &request,
            &ProcessEnvironment::new(vec![("MODE".to_owned(), "dev value".to_owned())]).unwrap(),
        )
        .unwrap();
        assert!(entry.contains("Terminal=false"));
        assert!(entry.contains("\"--gui\""));
        assert!(entry.contains("\"--translate-files\""));
        assert!(entry.contains(" %F\n"));
        assert!(!entry.contains("\"%F\""));
    }

    #[test]
    fn launcher_parser_requires_an_exe_and_supports_cwd_and_environment() {
        let arguments = [
            "win-dev",
            "--title",
            "Viewer",
            "--cwd=C:\\Work",
            "-e",
            "MODE=dev",
            "--",
            "viewer.exe",
        ]
        .map(OsString::from);
        let parsed = InstallArguments::parse(&arguments).unwrap();
        assert_eq!(parsed.requested.as_deref(), Some("win-dev"));
        assert_eq!(parsed.title.as_deref(), Some("Viewer"));
        assert_eq!(
            parsed.request.working_directory.as_deref(),
            Some("C:\\Work")
        );
        assert_eq!(parsed.environment.variables[0].0, "MODE");
        let invalid = [OsString::from("--"), OsString::from("viewer")];
        assert!(InstallArguments::parse(&invalid).is_err());
    }

    #[test]
    fn launcher_update_refuses_unmanaged_desktop_files() {
        let root = env::temp_dir().join(format!(
            "lsw-launcher-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let desktop = root.join("lsw-win-viewer.desktop");
        let icon = root.join("lsw-win-viewer.ico");
        fs::write(&desktop, "[Desktop Entry]\nName=Unrelated\n").unwrap();
        fs::write(&icon, [0, 0, 1, 0]).unwrap();
        assert!(validate_existing_launcher(&desktop, &icon, "lsw-win-viewer").is_err());
        fs::write(&desktop, "[Desktop Entry]\nX-LSW-Launcher=lsw-win-viewer\n").unwrap();
        assert!(validate_existing_launcher(&desktop, &icon, "lsw-win-viewer").unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
