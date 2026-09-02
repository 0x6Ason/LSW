// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) fn bench(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    if arguments
        .first()
        .is_some_and(|argument| argument == "files")
    {
        return file_bench::command(store, &arguments[1..]);
    }
    let mut json = false;
    let mut requested = None;
    for argument in arguments {
        let argument = argument
            .to_str()
            .ok_or("bench arguments must be valid UTF-8")?;
        match argument {
            "--json" => json = true,
            value if value.starts_with('-') => {
                return Err(format!("unknown bench option {value:?}").into())
            }
            name => {
                if requested.replace(name).is_some() {
                    return Err("usage: lsw bench [NAME] [--json]".into());
                }
            }
        }
    }

    let started = Instant::now();
    let capabilities = HostCapabilities::detect();
    let capability_scan_us = started.elapsed().as_micros();
    let started = Instant::now();
    let daemon_ready = DaemonClient::new(store).request("PING").is_ok();
    let daemon_ping_us = daemon_ready.then_some(started.elapsed().as_micros());

    let name = requested
        .map(str::to_owned)
        .or_else(|| store.default_name().ok().flatten());
    let mut manifest_load_us = None;
    let mut agent_probe_us = None;
    if let Some(name) = name.as_deref() {
        let started = Instant::now();
        if let Ok(manifest) = store.load(name) {
            manifest_load_us = Some(started.elapsed().as_micros());
            let started = Instant::now();
            let ready = store
                .read_agent_token(name)
                .ok()
                .and_then(|token| AgentClient::connect(&manifest, &token).ok())
                .and_then(|client| client.probe().ok())
                .is_some();
            if ready {
                agent_probe_us = Some(started.elapsed().as_micros());
            }
        }
    }

    if json {
        let mut output = String::from("{");
        write!(
            output,
            "\"version\":\"{}\",\"accelerator\":\"{}\",\"capability_scan_us\":{}",
            env!("CARGO_PKG_VERSION"),
            QemuBackend::select(&capabilities).accelerator(),
            capability_scan_us
        )?;
        push_optional_number(&mut output, "daemon_ping_us", daemon_ping_us);
        push_optional_number(&mut output, "manifest_load_us", manifest_load_us);
        push_optional_number(&mut output, "agent_probe_us", agent_probe_us);
        output.push_str(
            ",\"targets\":{\"warm_shell_p95_ms\":300,\"cold_boot_p95_ms\":15000,\"resume_p95_ms\":3000}}",
        );
        println!("{output}");
    } else {
        println!("LSW performance baseline");
        println!(
            "  accelerator: {}",
            QemuBackend::select(&capabilities).accelerator()
        );
        println!("  capability scan: {capability_scan_us} us");
        println!("  daemon ping: {}", format_microseconds(daemon_ping_us));
        println!("  manifest load: {}", format_microseconds(manifest_load_us));
        println!("  agent probe: {}", format_microseconds(agent_probe_us));
        println!("Use --json to record a machine-readable baseline.");
    }
    Ok(())
}

pub(super) fn push_optional_number(output: &mut String, key: &str, value: Option<u128>) {
    write!(output, ",\"{key}\":").expect("writing to a String cannot fail");
    if let Some(value) = value {
        write!(output, "{value}").expect("writing to a String cannot fail");
    } else {
        output.push_str("null");
    }
}

pub(super) fn format_microseconds(value: Option<u128>) -> String {
    value
        .map(|value| format!("{value} us"))
        .unwrap_or_else(|| "unavailable".to_owned())
}

pub(super) fn prepare(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn show(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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
    println!("minimum memory:       {} MiB", manifest.memory_min_mib);
    println!("disk:                 {} GiB", manifest.spec.disk_gib);
    println!(
        "idle timeout:         {}",
        format_duration(manifest.idle_timeout_seconds)
    );
    println!("idle policy:          {}", manifest.idle_policy);
    println!(
        "hibernate timeout:    {}",
        format_duration(manifest.hibernate_timeout_seconds)
    );
    println!(
        "default Windows user: {}",
        manifest.default_user.as_deref().unwrap_or("not registered")
    );
    println!(
        "Windows user role:    {}",
        manifest
            .default_user_role
            .map_or("not registered".to_owned(), |role| role.to_string())
    );
    println!(
        "sealed base image:    {}",
        manifest.base_image_key.as_deref().unwrap_or("none")
    );
    println!("folder shares:        {}", manifest.folder_shares.len());
    println!("network:              {}", manifest.spec.network);
    if manifest.spec.port_forwards.is_empty() {
        println!("published TCP ports:  none");
    } else {
        println!("published TCP ports:");
        for forward in &manifest.spec.port_forwards {
            println!(
                "  127.0.0.1:{} -> guest:{}",
                forward.host_port, forward.guest_port
            );
        }
    }
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

pub(super) fn plan(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn required_name<'a>(
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

pub(super) fn required_name_with_flag<'a>(
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

pub(super) fn state_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(configured) = env::var_os("LSW_STATE_DIR") {
        return Ok(PathBuf::from(configured));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set; configure LSW_STATE_DIR")?;
    Ok(PathBuf::from(home).join(".local/share/lsw"))
}

pub(super) fn absolute_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

pub(super) fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

pub(super) fn display_optional(value: &Option<PathBuf>) -> String {
    value
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "not found".to_owned())
}

pub(super) fn print_help() {
    println!(concat!(
        "LSW - local Windows development runtime\n\n",
        "USAGE:\n",
        "  lsw doctor [--fix]\n",
        "  lsw bench [NAME] [--json]\n",
        "  lsw inspect FILE [--imports] [--json]\n",
        "  lsw profile PROFILE\n",
        "  lsw media resolve [--language LANGUAGE] [--request-file PATH]\n",
        "  lsw path <--windows|-w|--unix|-u> PATH\n",
        "  lsw completion <bash|zsh|fish|powershell>\n",
        "  lsw create NAME --iso PATH --accept-windows-license [OPTIONS]\n",
        "  lsw prepare NAME [--execute]\n",
        "  lsw seed NAME [--locale LOCALE] [--agent PATH] [--unattended-index N] [--execute]\n",
        "  lsw list\n",
        "  lsw show NAME\n",
        "  lsw config get [NAME]\n",
        "  lsw config set NAME memory.max=4GiB idle-timeout=10m\n",
        "  lsw logs [NAME] [--lines N] [--follow]\n",
        "  lsw diagnose NAME [--bundle] [--output PATH]\n",
        "  lsw remove NAME\n",
        "  lsw shutdown [NAME | --all] [--force]\n",
        "  lsw use NAME\n",
        "  lsw plan NAME [--run]\n",
        "  lsw install NAME --iso PATH --edition EDITION [--profile PROFILE] [--accept-windows-license] [OPTIONS]\n",
        "  lsw install [NAME] [--locale LOCALE] [--edition EDITION] [--agent PATH]\n",
        "              [--unattended-index N] [--without-agent] [--viewer]\n",
        "              [--accept-windows-license] [--defer-user-setup]\n",
        "  lsw user setup [NAME] [--username USER] [--password-stdin] [--administrator]\n",
        "  lsw user add [NAME] [--username USER] [--password-stdin] [--administrator]\n",
        "  lsw user promote [NAME]\n",
        "  lsw user demote [NAME]\n",
        "  lsw sudo <status|enable|disable> [NAME]\n",
        "  lsw license status [NAME]\n",
        "  lsw license activate [NAME] [--key-stdin | --online]\n",
        "  lsw license open [NAME]\n",
        "  lsw view [NAME]\n",
        "  lsw start [NAME]\n",
        "  lsw status [NAME]\n",
        "  lsw suspend [NAME]\n",
        "  lsw resume [NAME]\n",
        "  lsw hibernate [NAME]\n",
        "  lsw memory <reclaim|restore> [NAME]\n",
        "  lsw trim [NAME]\n",
        "  lsw compact NAME\n",
        "  lsw stop [NAME] [--force]\n",
        "  lsw shell [NAME]\n",
        "  lsw exec [NAME] [--cwd PATH] [-e KEY=VALUE] -- COMMAND [ARG ...]\n",
        "  lsw run [NAME] [--cwd PATH] [-e KEY=VALUE] [--detach|--gui] [--translate-files] -- PROGRAM [ARG ...]\n",
        "  lsw app install [NAME] [--title TITLE] [--cwd PATH] [-e KEY=VALUE] -- PROGRAM.exe [ARG ...]\n",
        "  lsw app <list|remove ID>\n",
        "  lsw cp SOURCE DESTINATION\n",
        "  lsw push [NAME] [--recursive] HOST_PATH WINDOWS_PATH\n",
        "  lsw pull [NAME] [--recursive] WINDOWS_PATH HOST_PATH\n",
        "  lsw sync [NAME] [--watch] HOST_DIRECTORY WINDOWS_DIRECTORY\n",
        "  lsw share [PATH]\n",
        "  lsw unshare SHARE\n",
        "  lsw share add [NAME] SHARE HOST_PATH GUEST_PATH (--read-only|--read-write)\n",
        "  lsw share <list|remove|sync|watch> [NAME] [SHARE]\n",
        "  lsw image <list|seal NAME|verify KEY>\n",
        "  lsw clone SOURCE NAME\n",
        "  lsw daemon <enable|disable|start|status|diagnose>\n",
        "  lsw                    enter the default instance shell\n\n",
        "CREATE OPTIONS:\n",
        "  --profile PROFILE    vanilla or slim (default: slim)\n",
        "  --cpus COUNT         virtual CPU count (default: 2)\n",
        "  --memory MIB         guest memory (default: 4096)\n",
        "  --disk GIB           virtual disk size (profile default: 64)\n",
        "  --network MODE       nat (default) or offline\n",
        "  --publish HOST:GUEST publish a TCP guest port on host loopback; repeatable\n",
        "  --publish auto:GUEST allocate an available host loopback port\n",
        "  --accept-windows-license\n",
        "                       confirm acceptance of the applicable Microsoft Windows license\n",
        "  --accept-license     compatibility alias for --accept-windows-license\n",
        "  --allow-unsupported-requirements\n",
        "                       permit an explicitly unsupported small VM\n\n",
        "ONE-SHOT INSTALL:\n",
        "  --language LANGUAGE download that language from Microsoft (default: English)\n",
        "  --edition EDITION   select an edition by its ISO name or a friendly alias such as pro\n",
        "  --viewer            open the optional installation viewer (headless by default)\n",
        "  --no-viewer         keep headless installation explicit (compatibility option)\n",
        "  --defer-user-setup  skip permanent desktop-user registration for later automation\n",
        "  interactive new installs ask [y/N]; automation must pass --accept-windows-license\n",
        "  the flag accepts Windows terms only; LSW remains GPL-3.0-or-later\n\n",
        "SEED SAFETY:\n",
        "  --unattended-index N  select image N and explicitly wipe the dedicated VM disk\n",
        "  without that option, Windows edition/disk selection remains interactive\n\n",
        "ENVIRONMENT:\n",
        "  LSW_STATE_DIR        override the local state directory\n",
        "  LSW_WINDOWS_AGENT    override the lsw-agent.exe used by `lsw install`\n",
        "  LSW_INSTALL_VIEWER   override the remote-viewer executable\n",
        "  LSW_OVMF_CODE        override the OVMF code firmware path\n",
        "  LSW_OVMF_VARS        override the OVMF variable template path\n",
        "  LSW_OVMF_SECURE_CODE override Secure Boot-capable OVMF code\n",
        "  LSW_OVMF_SECURE_VARS override a key-enrolled Secure Boot variable template\n"
    ));
}
