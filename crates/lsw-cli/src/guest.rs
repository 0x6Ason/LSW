// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) fn start_instance(
    store: &StateStore,
    arguments: &[OsString],
    phase: LaunchPhase,
) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, phase.to_string().as_str())?;
    let name = resolve_name(store, requested)?;
    start_named_instance(store, &name, phase)
}

pub(super) fn start_named_instance(
    store: &StateStore,
    name: &str,
    phase: LaunchPhase,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = DaemonClient::new(store);
    for line in client.request_checked(&format!("START {name} {phase}"))? {
        println!("{line}");
    }
    if phase == LaunchPhase::Install {
        println!("The installation display is ready; use `lsw view {name}` to reopen it.");
    }
    Ok(())
}

pub(super) fn view(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "view")?;
    let name = resolve_name(store, requested)?;
    launch_viewer(store, &name, true)
}

pub(super) fn launch_installation_viewer(
    store: &StateStore,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    launch_viewer(store, name, false)
}

pub(super) fn launch_viewer(
    store: &StateStore,
    name: &str,
    explicit: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    viewer::launch(store, name, explicit)
}

pub(super) fn status(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "status")?;
    let name = resolve_name(store, requested)?;
    let client = DaemonClient::new(store);
    println!("instance={name}");
    for line in client.request_checked(&format!("STATUS {name}"))? {
        println!("{line}");
    }
    let manifest = store.load(&name)?;
    let token = store.read_agent_token(&name)?;
    match AgentClient::connect(&manifest, &token).and_then(AgentClient::probe) {
        Ok(()) => println!("AGENT=ready"),
        Err(error) => {
            println!("AGENT=unavailable");
            println!(
                "AGENT_ERROR={}",
                error.to_string().replace(['\r', '\n'], " ")
            );
        }
    }
    Ok(())
}

pub(super) fn stop(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut requested = None;
    let mut force = false;
    for argument in arguments {
        let argument = argument.to_str().ok_or("arguments must be valid UTF-8")?;
        if argument == "--force" {
            if force {
                return Err("--force was supplied more than once".into());
            }
            force = true;
        } else if requested.replace(argument).is_some() {
            return Err("usage: lsw stop [NAME] [--force]".into());
        }
    }
    let name = resolve_name(store, requested)?;
    let mode = if force { "force" } else { "graceful" };
    let client = DaemonClient::new(store);
    for line in client.request_checked(&format!("STOP {name} {mode}"))? {
        println!("{line}");
    }
    Ok(())
}

pub(super) fn suspend(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "suspend")?;
    let name = resolve_name(store, requested)?;
    let client = DaemonClient::new(store);
    for line in client.request_checked(&format!("SUSPEND {name}"))? {
        println!("{line}");
    }
    Ok(())
}

pub(super) fn resume(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "resume")?;
    let name = resolve_name(store, requested)?;
    let client = DaemonClient::new(store);
    for line in client.request_checked(&format!("RESUME {name}"))? {
        println!("{line}");
    }
    Ok(())
}

pub(super) fn hibernate(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "hibernate")?;
    let name = resolve_name(store, requested)?;
    let client = DaemonClient::new(store);
    for line in client.request_checked(&format!("HIBERNATE {name}"))? {
        println!("{line}");
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let manifest = store.load(&name)?;
        if manifest.state == InstanceState::Hibernated {
            println!("Instance {name:?} hibernated; QEMU is no longer resident.");
            return Ok(());
        }
        if manifest.state == InstanceState::Failed {
            return Err(format!("instance {name:?} failed while hibernating").into());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {name:?} to hibernate").into());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

pub(super) fn memory_command(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or("usage: lsw memory <reclaim|restore> [NAME]")?;
    let requested = optional_name(&arguments[1..], "memory reclaim|restore")?;
    let name = resolve_name(store, requested)?;
    let manifest = store.load(&name)?;
    let target = match action {
        "reclaim" => manifest.memory_min_mib,
        "restore" => manifest.spec.memory_mib,
        _ => return Err("usage: lsw memory <reclaim|restore> [NAME]".into()),
    };
    for line in DaemonClient::new(store).request_checked(&format!("BALLOON {name} {target}"))? {
        println!("{line}");
    }
    Ok(())
}

pub(super) fn trim_instance(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "trim")?;
    let name = resolve_name(store, requested)?;
    connect_agent(store, &name)?.trim()?;
    println!("Guest TRIM completed for {name:?}.");
    Ok(())
}

pub(super) fn compact_instance(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let name = required_exact_name(arguments, "compact")?;
    let manifest = store.load(name)?;
    if !matches!(
        manifest.state,
        InstanceState::Stopped | InstanceState::Hibernated
    ) {
        return Err(
            format!("instance {name:?} must be stopped or hibernated before compaction").into(),
        );
    }
    let qemu_img = HostCapabilities::detect()
        .qemu_img
        .ok_or("qemu-img is required for compaction")?;
    let instance_dir = store.instance_dir(name)?;
    let disk = instance_dir.join("disk.qcow2");
    let metadata = fs::symlink_metadata(&disk)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!("{} must be a regular non-symlink file", disk.display()).into());
    }
    let temporary = instance_dir.join(format!("disk.compact-{}.qcow2", std::process::id()));
    if temporary.exists() {
        return Err(format!("refusing to replace {}", temporary.display()).into());
    }
    let mut command = Command::new(qemu_img);
    command.args(["convert", "-p", "-O", "qcow2"]);
    if let Some(key) = &manifest.base_image_key {
        let base = fs::canonicalize(store.root().join("images").join(key).join("base.qcow2"))?;
        command.args(["-F", "qcow2", "-B"]).arg(base);
    }
    let status = command.arg(&disk).arg(&temporary).status()?;
    if !status.success() {
        let _ = fs::remove_file(&temporary);
        return Err(format!("qemu-img compaction failed with {status}").into());
    }
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    if let Err(error) = fs::rename(&temporary, &disk) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    println!("Compacted storage for {name:?}. Run `lsw trim {name}` before the next compaction for best results.");
    Ok(())
}

pub(super) fn shell(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<u8, Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "shell")?;
    let name = resolve_name(store, requested)?;
    let request = StartRequest {
        kind: SessionKind::Shell,
        argv: vec![
            "pwsh.exe".to_owned(),
            "pwsh".to_owned(),
            "powershell.exe".to_owned(),
            "powershell".to_owned(),
            "cmd.exe".to_owned(),
            "cmd".to_owned(),
        ],
        working_directory: None,
    };
    let client = connect_agent(store, &name)?;
    if !client.supports_conpty() {
        eprintln!("{PIPE_SHELL_NOTICE}");
    }
    guest_exit_code(client.run(&request, true)?)
}

pub(super) fn guest_command(
    store: &StateStore,
    arguments: &[OsString],
    kind: SessionKind,
) -> Result<u8, Box<dyn std::error::Error>> {
    let parsed = GuestCommandArguments::parse(arguments, kind)?;
    let name = resolve_name(store, parsed.requested.as_deref())?;
    if parsed.gui {
        let manifest = store.load(&name)?;
        let user_name = manifest.default_user.clone().ok_or_else(|| {
            format!(
                "instance {name:?} has no registered Windows desktop user; run `lsw user setup {name}`"
            )
        })?;
        let mut request = parsed.request;
        if parsed.translate_files {
            translate_gui_file_arguments(&manifest, &mut request.argv)?;
        }
        let mount_live_share = manifest
            .folder_shares
            .iter()
            .any(|share| share.transport == FolderShareTransport::LiveSmb);
        let request = GuiStartRequest {
            user_name,
            request,
            environment: parsed.environment,
            mount_live_share,
        };
        connect_agent(store, &name)?.probe()?;
        return guest_exit_code(lswg_launcher::present(store, &name, &request)?);
    }
    let client = connect_agent(store, &name)?;
    if parsed.detached {
        let process_id = client.run_detached(&parsed.request, &parsed.environment)?;
        println!("Started detached process {process_id} in {name:?}.");
        Ok(0)
    } else {
        guest_exit_code(client.run_with_environment(&parsed.request, true, &parsed.environment)?)
    }
}

#[derive(Debug)]
pub(super) struct GuestCommandArguments {
    pub(super) requested: Option<String>,
    pub(super) request: StartRequest,
    pub(super) environment: ProcessEnvironment,
    pub(super) detached: bool,
    pub(super) gui: bool,
    pub(super) translate_files: bool,
}

impl GuestCommandArguments {
    pub(super) fn parse(
        arguments: &[OsString],
        kind: SessionKind,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let command_name = match kind {
            SessionKind::Run => "run",
            SessionKind::Exec => "exec",
            SessionKind::Shell => "shell",
        };
        let usage = format!(
            "usage: lsw {command_name} [NAME] [--cwd PATH] [--env KEY=VALUE]{} -- COMMAND [ARG ...]",
            if kind == SessionKind::Run {
                " [--detach|--gui] [--translate-files]"
            } else {
                ""
            }
        );
        let separator = arguments
            .iter()
            .position(|argument| argument == OsStr::new("--"))
            .ok_or_else(|| usage.clone())?;
        if separator + 1 >= arguments.len() {
            return Err(usage.into());
        }
        let mut requested = None;
        let mut working_directory = None;
        let mut environment = Vec::new();
        let mut detached = false;
        let mut gui = false;
        let mut translate_files = false;
        let mut index = 0;
        while index < separator {
            let argument = arguments[index]
                .to_str()
                .ok_or("guest options must be valid UTF-8")?;
            match argument {
                "--cwd" | "-C" => {
                    index += 1;
                    let value = arguments
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or_else(|| usage.clone())?;
                    if value.is_empty() || working_directory.replace(value.to_owned()).is_some() {
                        return Err(
                            "--cwd requires one non-empty path and may appear only once".into()
                        );
                    }
                }
                "--env" | "-e" => {
                    index += 1;
                    let value = arguments
                        .get(index)
                        .and_then(|value| value.to_str())
                        .ok_or_else(|| usage.clone())?;
                    environment.push(parse_guest_environment(value)?);
                }
                "--detach" if kind == SessionKind::Run => {
                    if detached {
                        return Err("--detach was supplied more than once".into());
                    }
                    detached = true;
                }
                "--gui" if kind == SessionKind::Run => {
                    if gui {
                        return Err("--gui was supplied more than once".into());
                    }
                    gui = true;
                }
                "--translate-files" if kind == SessionKind::Run => {
                    if translate_files {
                        return Err("--translate-files was supplied more than once".into());
                    }
                    translate_files = true;
                }
                _ if argument.starts_with("--cwd=") => {
                    let value = argument.trim_start_matches("--cwd=");
                    if value.is_empty() || working_directory.replace(value.to_owned()).is_some() {
                        return Err(
                            "--cwd requires one non-empty path and may appear only once".into()
                        );
                    }
                }
                _ if argument.starts_with("--env=") => {
                    environment.push(parse_guest_environment(
                        argument.trim_start_matches("--env="),
                    )?);
                }
                _ if argument.starts_with('-') => return Err(usage.into()),
                _ => {
                    if requested.replace(argument.to_owned()).is_some() {
                        return Err(usage.into());
                    }
                }
            }
            index += 1;
        }
        let argv = arguments[separator + 1..]
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .ok_or("guest arguments must be valid UTF-8")
            })
            .collect::<Result<Vec<_>, _>>()?;
        if detached && gui {
            return Err("--detach and --gui are mutually exclusive".into());
        }
        if translate_files && !gui {
            return Err("--translate-files requires --gui".into());
        }
        if gui && !argv[0].to_ascii_lowercase().ends_with(".exe") {
            return Err("--gui requires a PROGRAM.exe command".into());
        }
        Ok(Self {
            requested,
            request: StartRequest {
                kind,
                argv,
                working_directory,
            },
            environment: ProcessEnvironment::new(environment)?,
            detached,
            gui,
            translate_files,
        })
    }
}

pub(super) fn translate_gui_file_arguments(
    manifest: &InstanceManifest,
    arguments: &mut [String],
) -> Result<(), Box<dyn std::error::Error>> {
    let live_root = manifest
        .folder_shares
        .iter()
        .find(|share| share.transport == FolderShareTransport::LiveSmb)
        .map(|share| fs::canonicalize(&share.host_path))
        .transpose()?;
    for argument in arguments.iter_mut().skip(1) {
        let path = Path::new(argument);
        if !path.is_absolute() {
            continue;
        }
        if !path.exists() {
            return Err(format!("host file {} does not exist", path.display()).into());
        }
        let Some(live_root) = &live_root else {
            return Err(format!(
                "host file {} is not available to Windows; run `lsw share` first or omit --translate-files",
                path.display()
            )
            .into());
        };
        let canonical = fs::canonicalize(path)?;
        let relative = canonical.strip_prefix(live_root).map_err(|_| {
            format!(
                "host file {} is outside the live folder {}; share a common parent before launching it",
                path.display(),
                live_root.display()
            )
        })?;
        let components = relative
            .components()
            .map(|component| {
                let component = component
                    .as_os_str()
                    .to_str()
                    .ok_or("host file path is not valid UTF-8")?;
                validate_live_windows_component(component)?;
                Ok(component)
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        *argument = if components.is_empty() {
            "L:\\".to_owned()
        } else {
            format!("L:\\{}", components.join("\\"))
        };
    }
    Ok(())
}

pub(super) fn validate_live_windows_component(
    value: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if value.is_empty()
        || value.ends_with([' ', '.'])
        || is_reserved_windows_component(value)
        || value
            .chars()
            .any(|character| character <= '\u{1f}' || "<>:\"|?*".contains(character))
    {
        return Err(format!(
            "host file component {value:?} cannot be represented through Windows Linux (L:)"
        )
        .into());
    }
    Ok(())
}

pub(super) fn is_reserved_windows_component(value: &str) -> bool {
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
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

pub(super) fn parse_guest_environment(
    value: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let (name, value) = value.split_once('=').ok_or("--env requires KEY=VALUE")?;
    if name.is_empty() {
        return Err("--env requires a non-empty variable name".into());
    }
    Ok((name.to_owned(), value.to_owned()))
}

pub(super) fn connect_agent(
    store: &StateStore,
    name: &str,
) -> Result<AgentClient, Box<dyn std::error::Error>> {
    let mut manifest = store.load(name)?;
    let token = store.read_agent_token(name)?;
    if let Ok(client) = AgentClient::connect(&manifest, &token) {
        let _ = DaemonClient::new(store).request_checked(&format!("ACTIVITY {name}"));
        return Ok(client);
    }

    match manifest.state {
        InstanceState::Configured => {
            return Err(
                format!("instance {name:?} is not installed; run `lsw install {name}`").into(),
            )
        }
        InstanceState::Stopped | InstanceState::Hibernated | InstanceState::Failed => {
            eprintln!("Starting {name:?}...");
            let daemon = DaemonClient::new(store);
            for line in daemon.request_checked(&format!("START {name} run"))? {
                eprintln!("{line}");
            }
        }
        InstanceState::Suspended => {
            eprintln!("Resuming {name:?}...");
            let daemon = DaemonClient::new(store);
            for line in daemon.request_checked(&format!("RESUME {name}"))? {
                eprintln!("{line}");
            }
        }
        InstanceState::Installing | InstanceState::Running => {}
    }

    let mut progress = ProgressRenderer::stderr();
    progress.update(ProgressEvent::stage(
        1,
        1,
        "Starting Windows",
        "waiting for the guest agent",
    ));
    let deadline = Instant::now() + AGENT_START_TIMEOUT;
    loop {
        manifest = store.load(name)?;
        if let Ok(client) = AgentClient::connect(&manifest, &token) {
            progress.update(ProgressEvent::measured(
                1,
                1,
                "Starting Windows",
                "guest agent ready",
                1,
                1,
            ));
            progress.finish();
            return Ok(client);
        }
        if manifest.state == InstanceState::Failed {
            progress.finish();
            return Err(format!("instance {name:?} failed while waiting for its agent").into());
        }
        if Instant::now() >= deadline {
            progress.finish();
            return Err(format!(
                "timed out waiting for the guest agent at {}; inspect the VM and {}",
                agent_address(&manifest),
                store.instance_dir(name)?.join("qemu.log").display()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

pub(super) fn guest_exit_code(code: i32) -> Result<u8, Box<dyn std::error::Error>> {
    if (0..=255).contains(&code) {
        Ok(code as u8)
    } else {
        let windows_code = code as u32;
        eprintln!(
            "lsw: guest exit code {windows_code} (0x{windows_code:08X}) cannot be represented by a Unix shell; returning 255"
        );
        Ok(255)
    }
}

pub(super) fn resolve_name(
    store: &StateStore,
    requested: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(configured) = env::var("LSW_DEFAULT_INSTANCE")
        .ok()
        .filter(|_| requested.is_none())
    {
        return Ok(store.resolve_name(Some(&configured))?);
    }
    Ok(store.resolve_name(requested)?)
}

pub(super) fn optional_name<'a>(
    arguments: &'a [OsString],
    command: &str,
) -> Result<Option<&'a str>, Box<dyn std::error::Error>> {
    if arguments.len() > 1 {
        return Err(format!("usage: lsw {command} [NAME]").into());
    }
    arguments
        .first()
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| "instance name must be valid UTF-8".into())
        })
        .transpose()
}

pub(super) fn required_exact_name<'a>(
    arguments: &'a [OsString],
    command: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    optional_name(arguments, command)?.ok_or_else(|| format!("usage: lsw {command} NAME").into())
}
