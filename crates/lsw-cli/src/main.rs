// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("the LSW 1.0 beta CLI currently requires a Unix host");

mod agent_client;
mod daemon_client;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use agent_client::AgentClient;
use daemon_client::DaemonClient;
use lsw_core::{
    CustomizationPlan, HostCapabilities, InstallSeedBuilder, InstallSeedOptions, InstanceManifest,
    InstanceSpec, InstanceState, LaunchPhase, LswError, NetworkMode, Provisioner, QemuPlanner,
    SessionKind, StartRequest, StateStore, WindowsProfile,
};

const AGENT_START_TIMEOUT: Duration = Duration::from_secs(90);

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("lsw: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<OsString>) -> Result<u8, Box<dyn std::error::Error>> {
    let store = StateStore::new(state_root()?);
    if arguments.is_empty() {
        return shell(&store, &[]);
    }
    let command = arguments[0].to_str().ok_or("command must be valid UTF-8")?;
    let remaining = &arguments[1..];

    match command {
        "help" | "--help" | "-h" => print_help(),
        "version" | "--version" | "-V" => println!("lsw {}", env!("CARGO_PKG_VERSION")),
        "doctor" => doctor(&store),
        "profile" => profile(remaining)?,
        "create" => create(&store, remaining)?,
        "prepare" => prepare(&store, remaining)?,
        "seed" => seed(&store, remaining)?,
        "list" => list(&store)?,
        "show" => show(&store, remaining)?,
        "plan" => plan(&store, remaining)?,
        "use" => select_default(&store, remaining)?,
        "daemon" => daemon(&store, remaining)?,
        "install" => install_instance(&store, remaining)?,
        "start" => start_instance(&store, remaining, LaunchPhase::Run)?,
        "status" => status(&store, remaining)?,
        "stop" => stop(&store, remaining)?,
        "shell" => return shell(&store, remaining),
        "exec" => return guest_command(&store, remaining, SessionKind::Exec),
        "run" => return guest_command(&store, remaining, SessionKind::Run),
        "push" => push_file(&store, remaining)?,
        "pull" => pull_file(&store, remaining)?,
        unknown => {
            return Err(format!("unknown command {unknown:?}; run `lsw help`").into());
        }
    }
    Ok(0)
}

fn seed(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let name = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or("usage: lsw seed NAME [OPTIONS]")?;
    let mut options = InstallSeedOptions::default();
    let mut execute = false;
    let mut index = 1;
    while index < arguments.len() {
        let option = arguments[index]
            .to_str()
            .ok_or("seed arguments must be valid UTF-8")?;
        match option {
            "--locale" => {
                options.locale = next_value(arguments, &mut index, option)?.to_owned();
            }
            "--unattended-index" => {
                options.unattended_image_index = Some(parse_number(arguments, &mut index, option)?);
            }
            "--agent" => {
                options.agent_binary = Some(absolute_path(Path::new(next_value(
                    arguments, &mut index, option,
                )?))?);
            }
            "--execute" => {
                if execute {
                    return Err("--execute was supplied more than once".into());
                }
                execute = true;
            }
            unknown => return Err(format!("unknown seed option {unknown:?}").into()),
        }
        index += 1;
    }

    let manifest = store.load(name)?;
    let instance_dir = store.instance_dir(name)?;
    let token = store.read_agent_token(name)?;
    let plan = InstallSeedBuilder::plan(&manifest, &instance_dir, &token, &options)?;
    println!("LSW installation seed plan for {name:?}");
    for line in plan.describe() {
        println!("  {line}");
    }
    if execute {
        InstallSeedBuilder::apply(&plan)?;
        println!(
            "Installation seed created at {}",
            plan.destination.display()
        );
    } else {
        println!("Dry-run only. Pass --execute to create the seed.");
    }
    Ok(())
}

fn doctor(store: &StateStore) {
    let capabilities = HostCapabilities::detect();
    println!("LSW host capability report");
    println!("  state root:  {}", store.root().display());
    println!("  KVM:         {}", yes_no(capabilities.kvm));
    println!(
        "  QEMU:        {}",
        display_optional(&capabilities.qemu_system)
    );
    println!(
        "  qemu-img:    {}",
        display_optional(&capabilities.qemu_img)
    );
    println!("  swtpm:       {}", display_optional(&capabilities.swtpm));
    println!(
        "  OVMF code:   {}",
        display_optional(&capabilities.ovmf_code)
    );
    println!(
        "  OVMF vars:   {}",
        display_optional(&capabilities.ovmf_vars)
    );
    println!(
        "  Secure code: {}",
        display_optional(&capabilities.ovmf_secure_code)
    );
    println!(
        "  Secure vars: {}",
        display_optional(&capabilities.ovmf_secure_vars)
    );

    let preparation_missing = capabilities.missing_for_preparation();
    let launch_missing = capabilities.missing_for_launch();
    if preparation_missing.is_empty() && launch_missing.is_empty() {
        println!("\nHost has the userspace components required to prepare and run a guest.");
    } else {
        if !preparation_missing.is_empty() {
            println!(
                "\nMissing preparation components: {}",
                preparation_missing.join(", ")
            );
        }
        if !launch_missing.is_empty() {
            println!("Missing launch components: {}", launch_missing.join(", "));
        }
        println!("Planning and state commands still work on this headless host.");
    }
    if !capabilities.kvm {
        println!("KVM acceleration is unavailable; QEMU would use its slow TCG fallback.");
    }
}

fn create(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = CreateArguments::parse(arguments)?;
    let profile = parsed.profile;
    let spec = InstanceSpec {
        name: parsed.name,
        source_iso: absolute_path(&parsed.iso)?,
        profile,
        cpus: parsed.cpus,
        memory_mib: parsed.memory_mib,
        disk_gib: parsed
            .disk_gib
            .unwrap_or_else(|| profile.default_disk_gib()),
        network: parsed.network,
        license_accepted: parsed.accept_license,
        allow_unsupported_requirements: parsed.allow_unsupported_requirements,
    };
    let manifest = InstanceManifest::new(spec)?;
    let instance_dir = store.create(&manifest)?;
    if store.default_name()?.is_none() {
        store.set_default(&manifest.spec.name)?;
    }

    println!("Created instance {:?}.", manifest.spec.name);
    println!(
        "  manifest: {}",
        instance_dir.join("instance.lsw").display()
    );
    println!("  profile:  {}", manifest.spec.profile);
    println!("  source:   {}", manifest.spec.source_iso.display());
    println!("No Windows binary was copied or distributed by LSW.");
    println!(
        "Run `lsw plan {}` to inspect the launch plan.",
        manifest.spec.name
    );
    Ok(())
}

fn select_default(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let name = required_exact_name(arguments, "use")?;
    store.set_default(name)?;
    println!("Default LSW instance is now {name:?}.");
    Ok(())
}

fn daemon(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments
        .first()
        .and_then(|value| value.to_str())
        .unwrap_or("status");
    if arguments.len() > 1 {
        return Err("usage: lsw daemon [start|status]".into());
    }
    let client = DaemonClient::new(store);
    match action {
        "start" => {
            client.ensure_running()?;
            println!("lswd is ready at {}", client.socket().display());
        }
        "status" => match client.request("PING") {
            Ok(lines) => {
                println!("lswd is ready at {}", client.socket().display());
                for line in lines {
                    println!("  {line}");
                }
            }
            Err(error) => {
                println!(
                    "lswd is not reachable at {}: {error}",
                    client.socket().display()
                );
            }
        },
        _ => return Err("usage: lsw daemon [start|status]".into()),
    }
    Ok(())
}

fn start_instance(
    store: &StateStore,
    arguments: &[OsString],
    phase: LaunchPhase,
) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, phase.to_string().as_str())?;
    let name = resolve_name(store, requested)?;
    start_named_instance(store, &name, phase)
}

fn start_named_instance(
    store: &StateStore,
    name: &str,
    phase: LaunchPhase,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = DaemonClient::new(store);
    for line in client.request_checked(&format!("START {name} {phase}"))? {
        println!("{line}");
    }
    if phase == LaunchPhase::Install {
        println!(
            "Installation/recovery display: {}",
            store
                .instance_dir(name)?
                .join("run/recovery-vnc.sock")
                .display()
        );
    }
    Ok(())
}

fn install_instance(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut requested = None;
    let mut options = InstallSeedOptions::default();
    let mut seed_option_seen = false;
    let mut without_agent = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index]
            .to_str()
            .ok_or("install arguments must be valid UTF-8")?;
        match argument {
            "--locale" => {
                options.locale = next_value(arguments, &mut index, argument)?.to_owned();
                seed_option_seen = true;
            }
            "--unattended-index" => {
                options.unattended_image_index =
                    Some(parse_number(arguments, &mut index, argument)?);
                seed_option_seen = true;
            }
            "--agent" => {
                options.agent_binary = Some(absolute_path(Path::new(next_value(
                    arguments, &mut index, argument,
                )?))?);
                seed_option_seen = true;
            }
            "--without-agent" => {
                if without_agent {
                    return Err("--without-agent was supplied more than once".into());
                }
                without_agent = true;
                seed_option_seen = true;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown install option {value:?}").into())
            }
            name => {
                if requested.replace(name).is_some() {
                    return Err("usage: lsw install [NAME] [OPTIONS]".into());
                }
            }
        }
        index += 1;
    }
    if without_agent && options.agent_binary.is_some() {
        return Err("--agent and --without-agent cannot be used together".into());
    }

    let name = resolve_name(store, requested)?;
    let instance_dir = store.instance_dir(&name)?;
    let seed = instance_dir.join("seed");
    match fs::symlink_metadata(&seed) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            if seed_option_seen {
                return Err(format!(
                    "{} already exists; install seed options cannot be changed implicitly",
                    seed.display()
                )
                .into());
            }
            println!("Using existing installation seed at {}", seed.display());
        }
        Ok(_) => {
            return Err(format!("{} must be a real directory", seed.display()).into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if options.agent_binary.is_none() && !without_agent {
                options.agent_binary = find_windows_agent();
                if options.agent_binary.is_none() {
                    return Err(
                        "lsw-agent.exe was not found; pass --agent PATH, set LSW_WINDOWS_AGENT, or explicitly use --without-agent"
                            .into(),
                    );
                }
            }
            let manifest = store.load(&name)?;
            let token = store.read_agent_token(&name)?;
            let plan = InstallSeedBuilder::plan(&manifest, &instance_dir, &token, &options)?;
            for line in plan.describe() {
                println!("  {line}");
            }
            InstallSeedBuilder::apply(&plan)?;
            println!("Installation seed created at {}", seed.display());
        }
        Err(error) => return Err(error.into()),
    }
    start_named_instance(store, &name, LaunchPhase::Install)
}

fn find_windows_agent() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("LSW_WINDOWS_AGENT") {
        return Some(PathBuf::from(configured));
    }
    let executable = env::current_exe().ok()?;
    let binary_directory = executable.parent()?;
    let candidates = [
        Some(binary_directory.join("lsw-agent.exe")),
        binary_directory
            .parent()
            .map(|prefix| prefix.join("libexec/lsw/lsw-agent.exe")),
    ];
    candidates.into_iter().flatten().find(|candidate| {
        fs::symlink_metadata(candidate).is_ok_and(|metadata| {
            metadata.file_type().is_file() && !metadata.file_type().is_symlink()
        })
    })
}

fn status(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "status")?;
    let name = resolve_name(store, requested)?;
    let client = DaemonClient::new(store);
    println!("instance={name}");
    for line in client.request_checked(&format!("STATUS {name}"))? {
        println!("{line}");
    }
    let manifest = store.load(&name)?;
    let token = store.read_agent_token(&name)?;
    let agent = AgentClient::connect(&manifest, &token)
        .and_then(AgentClient::probe)
        .is_ok();
    println!("AGENT={}", if agent { "ready" } else { "unavailable" });
    Ok(())
}

fn stop(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
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

fn shell(store: &StateStore, arguments: &[OsString]) -> Result<u8, Box<dyn std::error::Error>> {
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
    guest_exit_code(client.run(&request, true)?)
}

fn guest_command(
    store: &StateStore,
    arguments: &[OsString],
    kind: SessionKind,
) -> Result<u8, Box<dyn std::error::Error>> {
    let command_name = match kind {
        SessionKind::Run => "run",
        SessionKind::Exec => "exec",
        SessionKind::Shell => "shell",
    };
    let usage = format!("usage: lsw {command_name} [NAME] -- COMMAND [ARG ...]");
    let separator = arguments
        .iter()
        .position(|argument| argument == OsStr::new("--"))
        .ok_or_else(|| usage.clone())?;
    if separator > 1 || separator + 1 >= arguments.len() {
        return Err(usage.into());
    }
    let requested = arguments[..separator]
        .first()
        .map(|value| value.to_str().ok_or("instance name must be valid UTF-8"))
        .transpose()?;
    let name = resolve_name(store, requested)?;
    let argv = arguments[separator + 1..]
        .iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .ok_or("guest arguments must be valid UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let request = StartRequest {
        kind,
        argv,
        working_directory: None,
    };
    let client = connect_agent(store, &name)?;
    guest_exit_code(client.run(&request, true)?)
}

fn push_file(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let (requested, source, destination) = transfer_arguments(arguments, "push")?;
    let name = resolve_name(store, requested)?;
    let source = PathBuf::from(source);
    let destination = destination
        .to_str()
        .ok_or("guest destination must be valid UTF-8")?;
    let bytes = connect_agent(store, &name)?.put_file(&source, destination)?;
    println!(
        "Transferred {bytes} bytes from {} to {name}:{}",
        source.display(),
        destination
    );
    Ok(())
}

fn pull_file(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let (requested, source, destination) = transfer_arguments(arguments, "pull")?;
    let name = resolve_name(store, requested)?;
    let source = source.to_str().ok_or("guest source must be valid UTF-8")?;
    let destination = PathBuf::from(destination);
    let bytes = connect_agent(store, &name)?.get_file(source, &destination)?;
    println!(
        "Transferred {bytes} bytes from {name}:{} to {}",
        source,
        destination.display()
    );
    Ok(())
}

fn transfer_arguments<'a>(
    arguments: &'a [OsString],
    command: &str,
) -> Result<(Option<&'a str>, &'a OsStr, &'a OsStr), Box<dyn std::error::Error>> {
    match arguments {
        [source, destination] => Ok((None, source.as_os_str(), destination.as_os_str())),
        [name, source, destination] => Ok((
            Some(name.to_str().ok_or("instance name must be valid UTF-8")?),
            source.as_os_str(),
            destination.as_os_str(),
        )),
        _ => Err(format!("usage: lsw {command} [NAME] SOURCE DESTINATION").into()),
    }
}

fn connect_agent(
    store: &StateStore,
    name: &str,
) -> Result<AgentClient, Box<dyn std::error::Error>> {
    let mut manifest = store.load(name)?;
    let token = store.read_agent_token(name)?;
    if let Ok(client) = AgentClient::connect(&manifest, &token) {
        return Ok(client);
    }

    match manifest.state {
        InstanceState::Configured => {
            return Err(
                format!("instance {name:?} is not installed; run `lsw install {name}`").into(),
            )
        }
        InstanceState::Stopped | InstanceState::Failed => {
            println!("Starting {name:?}...");
            let daemon = DaemonClient::new(store);
            for line in daemon.request_checked(&format!("START {name} run"))? {
                println!("{line}");
            }
        }
        InstanceState::Suspended => {
            return Err("resume is not implemented in the beta supervisor".into())
        }
        InstanceState::Installing | InstanceState::Running => {}
    }

    println!("Waiting for the LSW guest agent...");
    let deadline = Instant::now() + AGENT_START_TIMEOUT;
    loop {
        manifest = store.load(name)?;
        if let Ok(client) = AgentClient::connect(&manifest, &token) {
            return Ok(client);
        }
        if manifest.state == InstanceState::Failed {
            return Err(format!("instance {name:?} failed while waiting for its agent").into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for the guest agent at {}; inspect the VM and {}",
                agent_client::address(&manifest),
                store.instance_dir(name)?.join("qemu.log").display()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn guest_exit_code(code: i32) -> Result<u8, Box<dyn std::error::Error>> {
    if (0..=255).contains(&code) {
        Ok(code as u8)
    } else {
        Err(
            format!("guest returned exit code {code}, which cannot be represented by the host")
                .into(),
        )
    }
}

fn resolve_name(
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

fn optional_name<'a>(
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

fn required_exact_name<'a>(
    arguments: &'a [OsString],
    command: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    optional_name(arguments, command)?.ok_or_else(|| format!("usage: lsw {command} NAME").into())
}

fn profile(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let profile = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or("usage: lsw profile PROFILE")?
        .parse::<WindowsProfile>()?;
    if arguments.len() != 1 {
        return Err("usage: lsw profile PROFILE".into());
    }
    let plan = CustomizationPlan::for_profile(profile);
    println!("LSW Windows profile: {}", plan.profile);
    println!("  servicing preserved: {}", profile.keeps_servicing());
    println!("  CompactOS requested: {}", plan.compact_os);
    if plan.remove_provisioned_appx_patterns.is_empty() {
        println!("  provisioned AppX removals: none");
    } else {
        println!("  provisioned AppX patterns removed locally:");
        for pattern in plan.remove_provisioned_appx_patterns {
            println!("    - {pattern}");
        }
    }
    println!("  explicitly preserved:");
    for component in plan.preserve_components {
        println!("    - {component}");
    }
    for warning in plan.warnings {
        println!("  warning: {warning}");
    }
    Ok(())
}

fn list(store: &StateStore) -> Result<(), LswError> {
    let instances = store.list()?;
    if instances.is_empty() {
        println!("No LSW instances configured.");
        return Ok(());
    }
    println!("NAME\tSTATE\tPROFILE\tCPUS\tMEMORY");
    for manifest in instances {
        println!(
            "{}\t{}\t{}\t{}\t{} MiB",
            manifest.spec.name,
            manifest.state,
            manifest.spec.profile,
            manifest.spec.cpus,
            manifest.spec.memory_mib
        );
    }
    Ok(())
}

fn prepare(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let name = required_name_with_flag(arguments, "prepare", "--execute")?;
    let execute = arguments
        .iter()
        .any(|argument| argument.as_os_str() == OsStr::new("--execute"));
    let manifest = store.load(name)?;
    let instance_dir = store.instance_dir(name)?;
    let provisioner = Provisioner::new(HostCapabilities::detect());
    let plan = provisioner.plan(&manifest, &instance_dir)?;

    println!("LSW preparation plan for {name:?}");
    if plan.steps.is_empty() {
        println!("  instance storage is already prepared");
    } else {
        for step in &plan.steps {
            println!("  - {}", step.describe());
        }
    }
    if !plan.missing_capabilities.is_empty() {
        println!("blocked: missing {}", plan.missing_capabilities.join(", "));
        println!("No preparation step was executed.");
        return Ok(());
    }
    if execute {
        provisioner.apply(&plan)?;
        println!("Preparation completed.");
    } else if !plan.steps.is_empty() {
        println!("Dry-run only. Pass --execute to apply this plan.");
    }
    Ok(())
}

fn show(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let name = required_name(arguments, "show", false)?;
    let manifest = store.load(name)?;
    let security = manifest.spec.profile.security();
    println!("name:                 {}", manifest.spec.name);
    println!("state:                {}", manifest.state);
    println!("profile:              {}", manifest.spec.profile);
    println!(
        "source ISO:           {}",
        manifest.spec.source_iso.display()
    );
    println!("CPUs:                 {}", manifest.spec.cpus);
    println!("memory:               {} MiB", manifest.spec.memory_mib);
    println!("disk:                 {} GiB", manifest.spec.disk_gib);
    println!("network:              {}", manifest.spec.network);
    println!("UEFI:                 {}", security.uefi);
    println!("guest Secure Boot:    {}", security.secure_boot);
    println!("vTPM:                 {}", security.vtpm);
    println!("test signing allowed: {}", security.test_signing_allowed);
    println!("custom driver allowed: {}", security.custom_driver_allowed);
    println!(
        "servicing preserved:  {}",
        manifest.spec.profile.keeps_servicing()
    );
    println!("agent host port:      {}", manifest.control_port);
    Ok(())
}

fn plan(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let name = required_name(arguments, "plan", true)?;
    let phase = if arguments
        .iter()
        .any(|argument| argument.as_os_str() == OsStr::new("--run"))
    {
        LaunchPhase::Run
    } else {
        LaunchPhase::Install
    };
    let manifest = store.load(name)?;
    let instance_dir = store.instance_dir(name)?;
    let plan =
        QemuPlanner::new(HostCapabilities::detect()).plan(&manifest, &instance_dir, phase)?;

    println!("LSW QEMU {} plan for {:?}", phase, name);
    for helper in &plan.helper_commands {
        println!("helper: {}", helper.display_command());
    }
    println!("qemu:   {}", plan.display_command());
    for note in plan.notes {
        println!("note: {note}");
    }
    if !plan.missing_capabilities.is_empty() {
        println!("blocked: missing {}", plan.missing_capabilities.join(", "));
        println!("This is a dry-run only; no process was started.");
    }
    Ok(())
}

fn required_name<'a>(
    arguments: &'a [OsString],
    command: &str,
    allow_run: bool,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    let name = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("usage: lsw {command} NAME"))?;
    if arguments
        .iter()
        .skip(1)
        .any(|argument| !allow_run || argument.as_os_str() != OsStr::new("--run"))
    {
        return Err(format!("unexpected argument to `lsw {command}`").into());
    }
    Ok(name)
}

fn required_name_with_flag<'a>(
    arguments: &'a [OsString],
    command: &str,
    allowed_flag: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    let name = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("usage: lsw {command} NAME [{allowed_flag}]"))?;
    let mut flag_seen = false;
    for argument in arguments.iter().skip(1) {
        if argument.as_os_str() != OsStr::new(allowed_flag) || flag_seen {
            return Err(format!("unexpected argument to `lsw {command}`").into());
        }
        flag_seen = true;
    }
    Ok(name)
}

fn state_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(configured) = env::var_os("LSW_STATE_DIR") {
        return Ok(PathBuf::from(configured));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set; configure LSW_STATE_DIR")?;
    Ok(PathBuf::from(home).join(".local/share/lsw"))
}

fn absolute_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn display_optional(value: &Option<PathBuf>) -> String {
    value
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "not found".to_owned())
}

fn print_help() {
    println!(concat!(
        "LSW - local Windows development runtime\n\n",
        "USAGE:\n",
        "  lsw doctor\n",
        "  lsw profile PROFILE\n",
        "  lsw create NAME --iso PATH --accept-license [OPTIONS]\n",
        "  lsw prepare NAME [--execute]\n",
        "  lsw seed NAME [--locale LOCALE] [--agent PATH] [--unattended-index N] [--execute]\n",
        "  lsw list\n",
        "  lsw show NAME\n",
        "  lsw use NAME\n",
        "  lsw plan NAME [--run]\n",
        "  lsw install [NAME] [--locale LOCALE] [--agent PATH]\n",
        "              [--unattended-index N] [--without-agent]\n",
        "  lsw start [NAME]\n",
        "  lsw status [NAME]\n",
        "  lsw stop [NAME] [--force]\n",
        "  lsw shell [NAME]\n",
        "  lsw exec [NAME] -- COMMAND [ARG ...]\n",
        "  lsw run [NAME] -- PROGRAM [ARG ...]\n",
        "  lsw push [NAME] HOST_FILE WINDOWS_PATH\n",
        "  lsw pull [NAME] WINDOWS_PATH HOST_FILE\n",
        "  lsw daemon [start|status]\n",
        "  lsw                    enter the default instance shell\n\n",
        "CREATE OPTIONS:\n",
        "  --profile PROFILE    standard, slim, ephemeral, or secure\n",
        "  --cpus COUNT         virtual CPU count (default: 2)\n",
        "  --memory MIB         guest memory (default: 4096)\n",
        "  --disk GIB           virtual disk size (profile default: 64)\n",
        "  --network MODE       nat (default) or offline\n",
        "  --accept-license     confirm acceptance of the supplied media's license\n",
        "  --allow-unsupported-requirements\n",
        "                       permit an explicitly unsupported small VM\n\n",
        "SEED SAFETY:\n",
        "  --unattended-index N  select image N and explicitly wipe the dedicated VM disk\n",
        "  without that option, Windows edition/disk selection remains interactive\n\n",
        "ENVIRONMENT:\n",
        "  LSW_STATE_DIR        override the local state directory\n",
        "  LSW_WINDOWS_AGENT    override the lsw-agent.exe used by `lsw install`\n",
        "  LSW_OVMF_CODE        override the OVMF code firmware path\n",
        "  LSW_OVMF_VARS        override the OVMF variable template path\n",
        "  LSW_OVMF_SECURE_CODE override Secure Boot-capable OVMF code\n",
        "  LSW_OVMF_SECURE_VARS override a key-enrolled Secure Boot variable template\n"
    ));
}

#[derive(Debug)]
struct CreateArguments {
    name: String,
    iso: PathBuf,
    profile: WindowsProfile,
    cpus: u16,
    memory_mib: u32,
    disk_gib: Option<u32>,
    network: NetworkMode,
    accept_license: bool,
    allow_unsupported_requirements: bool,
}

impl CreateArguments {
    fn parse(arguments: &[OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let name = arguments
            .first()
            .and_then(|value| value.to_str())
            .ok_or("usage: lsw create NAME --iso PATH --accept-license")?
            .to_owned();
        let mut iso = None;
        let mut profile = WindowsProfile::Standard;
        let mut cpus = 2;
        let mut memory_mib = 4096;
        let mut disk_gib = None;
        let mut network = NetworkMode::Nat;
        let mut accept_license = false;
        let mut allow_unsupported_requirements = false;
        let mut index = 1;

        while index < arguments.len() {
            let option = arguments[index]
                .to_str()
                .ok_or("command arguments must be valid UTF-8")?;
            match option {
                "--iso" => {
                    iso = Some(PathBuf::from(next_value(arguments, &mut index, option)?));
                }
                "--profile" => {
                    profile = next_value(arguments, &mut index, option)?.parse()?;
                }
                "--cpus" => cpus = parse_number(arguments, &mut index, option)?,
                "--memory" => memory_mib = parse_number(arguments, &mut index, option)?,
                "--disk" => disk_gib = Some(parse_number(arguments, &mut index, option)?),
                "--network" => network = next_value(arguments, &mut index, option)?.parse()?,
                "--accept-license" => accept_license = true,
                "--allow-unsupported-requirements" => allow_unsupported_requirements = true,
                unknown => return Err(format!("unknown create option {unknown:?}").into()),
            }
            index += 1;
        }

        Ok(Self {
            name,
            iso: iso.ok_or("--iso PATH is required")?,
            profile,
            cpus,
            memory_mib,
            disk_gib,
            network,
            accept_license,
            allow_unsupported_requirements,
        })
    }
}

fn next_value<'a>(
    arguments: &'a [OsString],
    index: &mut usize,
    option: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    *index += 1;
    arguments
        .get(*index)
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{option} requires a value").into())
}

fn parse_number<T>(
    arguments: &[OsString],
    index: &mut usize,
    option: &str,
) -> Result<T, Box<dyn std::error::Error>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = next_value(arguments, index, option)?;
    value
        .parse::<T>()
        .map_err(|error| format!("invalid value for {option}: {error}").into())
}
