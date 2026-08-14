// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("the LSW 1.0 beta CLI currently requires a Unix host");

mod agent_client;
mod daemon_client;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write as IoWrite};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_client::AgentClient;
use daemon_client::DaemonClient;
use lsw_core::{
    CustomizationPlan, HostCapabilities, InstallSeedBuilder, InstallSeedOptions, InstanceManifest,
    InstanceSpec, InstanceState, LaunchPhase, LswError, NetworkMode, PeImage, PeImportSymbol,
    PortForward, Provisioner, QemuBackend, QemuPlanner, SessionKind, StartRequest, StateStore,
    VmAccelerator, WindowsEdition, WindowsMediaInspector, WindowsProfile,
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
    if arguments.is_empty() {
        let store = StateStore::new(state_root()?);
        return shell(&store, &[]);
    }
    let command = arguments[0].to_str().ok_or("command must be valid UTF-8")?;
    let remaining = &arguments[1..];

    match command {
        "help" | "--help" | "-h" => {
            print_help();
            return Ok(0);
        }
        "version" | "--version" | "-V" => {
            println!("lsw {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        }
        "inspect" => {
            inspect_pe(remaining)?;
            return Ok(0);
        }
        "profile" => {
            profile(remaining)?;
            return Ok(0);
        }
        _ => {}
    }

    let store = StateStore::new(state_root()?);
    match command {
        "doctor" => doctor(&store, remaining)?,
        "bench" => bench(&store, remaining)?,
        "create" => create(&store, remaining)?,
        "prepare" => prepare(&store, remaining)?,
        "seed" => seed(&store, remaining)?,
        "list" => list(&store)?,
        "show" => show(&store, remaining)?,
        "config" => config(&store, remaining)?,
        "logs" => logs(&store, remaining)?,
        "diagnose" => diagnose(&store, remaining)?,
        "remove" => remove_instance(&store, remaining)?,
        "shutdown" => shutdown(&store, remaining)?,
        "view" => view(&store, remaining)?,
        "plan" => plan(&store, remaining)?,
        "use" => select_default(&store, remaining)?,
        "daemon" => daemon(&store, remaining)?,
        "install" => install_instance(&store, remaining)?,
        "start" => start_instance(&store, remaining, LaunchPhase::Run)?,
        "status" => status(&store, remaining)?,
        "suspend" => suspend(&store, remaining)?,
        "resume" => resume(&store, remaining)?,
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

fn doctor(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let fix = match arguments {
        [] => false,
        [flag] if flag == OsStr::new("--fix") => true,
        _ => return Err("usage: lsw doctor [--fix]".into()),
    };
    let mut capabilities = HostCapabilities::detect();
    if fix {
        let missing = capabilities.missing_for_install_workflow();
        if missing.is_empty() {
            println!("All beta.5 host dependencies are already installed.\n");
        } else {
            println!(
                "Installing missing beta.5 host dependencies: {}",
                missing.join(", ")
            );
            fix_host_dependencies()?;
            capabilities = HostCapabilities::detect();
        }
    }

    print_host_report(store, &capabilities);
    if fix {
        let missing = capabilities.missing_for_install_workflow();
        if !missing.is_empty() {
            return Err(format!(
                "host dependency repair completed but these components are still missing: {}",
                missing.join(", ")
            )
            .into());
        }
    }
    Ok(())
}

fn print_host_report(store: &StateStore, capabilities: &HostCapabilities) {
    let backend = QemuBackend::select(capabilities);
    println!("LSW host capability report");
    println!("  state root:  {}", store.root().display());
    println!("  platform:    {}", capabilities.platform);
    println!("  accelerator: {}", backend.accelerator());
    println!(
        "  KVM:         {}",
        yes_no(capabilities.accelerators.supports(VmAccelerator::Kvm))
    );
    println!(
        "  HVF:         {}",
        yes_no(capabilities.accelerators.supports(VmAccelerator::Hvf))
    );
    println!(
        "  WHPX:        {}",
        yes_no(capabilities.accelerators.supports(VmAccelerator::Whpx))
    );
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
        "  wimlib:      {}",
        display_optional(&capabilities.wimlib_imagex)
    );
    println!("  xorriso:     {}", display_optional(&capabilities.xorriso));
    println!(
        "  viewer:      {}",
        display_optional(&capabilities.remote_viewer)
    );
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
    if backend.accelerator() == VmAccelerator::Tcg {
        if let Some(native) = capabilities.platform.native_accelerator() {
            println!(
                "{} is unavailable; QEMU would use its slow TCG fallback.",
                native.capability_name()
            );
        } else {
            println!("No native accelerator is defined; QEMU would use its slow TCG fallback.");
        }
    }
}

fn fix_host_dependencies() -> Result<(), Box<dyn std::error::Error>> {
    let os_release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let distribution = os_release
        .lines()
        .find_map(|line| line.strip_prefix("ID="))
        .map(|value| value.trim_matches('"').to_owned())
        .unwrap_or_default();
    let like = os_release
        .lines()
        .find_map(|line| line.strip_prefix("ID_LIKE="))
        .map(|value| value.trim_matches('"').to_owned())
        .unwrap_or_default();
    let family = format!("{distribution} {like}");
    let elevated = command_prefix()?;

    if family.contains("debian") || family.contains("ubuntu") {
        run_package_command(&elevated, "apt-get", &["update"])?;
        run_package_command(
            &elevated,
            "apt-get",
            &[
                "install",
                "--yes",
                "qemu-system-x86",
                "qemu-utils",
                "ovmf",
                "swtpm",
                "wimtools",
                "xorriso",
                "virt-viewer",
            ],
        )?;
    } else if family.contains("fedora") || family.contains("rhel") {
        run_package_command(
            &elevated,
            "dnf",
            &[
                "install",
                "--assumeyes",
                "qemu-system-x86-core",
                "qemu-img",
                "edk2-ovmf",
                "swtpm",
                "wimlib-utils",
                "xorriso",
                "virt-viewer",
            ],
        )?;
    } else if family.contains("arch") {
        run_package_command(
            &elevated,
            "pacman",
            &[
                "--sync",
                "--needed",
                "--noconfirm",
                "qemu-desktop",
                "edk2-ovmf",
                "swtpm",
                "wimlib",
                "xorriso",
                "virt-viewer",
            ],
        )?;
    } else if family.contains("suse") {
        run_package_command(
            &elevated,
            "zypper",
            &[
                "--non-interactive",
                "install",
                "qemu-x86",
                "qemu-tools",
                "qemu-ovmf-x86_64",
                "swtpm",
                "wimlib",
                "xorriso",
                "virt-viewer",
            ],
        )?;
    } else {
        return Err(format!(
            "automatic dependency repair does not support distribution {distribution:?}; install QEMU, qemu-img, OVMF, swtpm, wimlib-imagex, xorriso, and remote-viewer manually"
        )
        .into());
    }
    Ok(())
}

fn command_prefix() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let output = Command::new("id").arg("-u").output()?;
    if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "0" {
        Ok(Vec::new())
    } else {
        Ok(vec!["sudo".to_owned()])
    }
}

fn run_package_command(
    prefix: &[String],
    program: &str,
    arguments: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    let status = if let Some(elevated) = prefix.first() {
        Command::new(elevated)
            .arg(program)
            .args(arguments)
            .status()?
    } else {
        Command::new(program).args(arguments).status()?
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed with {status}").into())
    }
}

fn inspect_pe(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let mut path = None;
    let mut json = false;
    let mut show_imports = false;

    for argument in arguments {
        let value = argument
            .to_str()
            .ok_or("inspect arguments must be valid UTF-8")?;
        match value {
            "--json" => {
                if json {
                    return Err("--json was supplied more than once".into());
                }
                json = true;
            }
            "--imports" => {
                if show_imports {
                    return Err("--imports was supplied more than once".into());
                }
                show_imports = true;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown inspect option {option:?}").into())
            }
            _ => {
                if path.replace(PathBuf::from(argument)).is_some() {
                    return Err("usage: lsw inspect FILE [--imports] [--json]".into());
                }
            }
        }
    }

    let path = path.ok_or("usage: lsw inspect FILE [--imports] [--json]")?;
    let image = PeImage::read(&path)?;
    if json {
        println!("{}", pe_json(&path, &image));
    } else {
        print_pe_report(&path, &image, show_imports);
    }
    Ok(())
}

fn print_pe_report(path: &Path, image: &PeImage, show_imports: bool) {
    let assessment = image.assess_for_beta();
    println!("LSW PE inspection");
    println!("  file:              {}", path.display());
    println!("  format:            {}", image.kind);
    println!("  machine:           {}", image.machine);
    println!("  subsystem:         {}", image.subsystem);
    println!("  image base:        0x{:016x}", image.image_base);
    println!("  entry point RVA:   0x{:08x}", image.entry_point_rva);
    println!("  image size:        {} bytes", image.size_of_image);
    println!("  DLL:               {}", yes_no(image.is_dll));
    println!("  managed (.NET):    {}", yes_no(image.is_managed));
    println!(
        "  certificate table: {}",
        if image.has_certificate_table {
            "present (signature not cryptographically verified)"
        } else {
            "absent"
        }
    );
    println!("  sections:          {}", image.sections.len());
    println!("  imported DLLs:     {}", image.imports.len());
    println!("  imported symbols:  {}", image.imported_symbol_count());
    println!("  beta assessment:   {}", assessment.level);
    for note in assessment.notes {
        println!("  note: {note}");
    }

    if image.imports.is_empty() {
        return;
    }
    println!("\nImports:");
    for import in &image.imports {
        println!("  {} ({} symbols)", import.dll, import.symbols.len());
        if show_imports {
            for symbol in &import.symbols {
                match symbol {
                    PeImportSymbol::Name { hint, name } => {
                        println!("    {name} (hint {hint})");
                    }
                    PeImportSymbol::Ordinal(value) => println!("    #{value}"),
                }
            }
        }
    }
    if !show_imports {
        println!("Pass --imports to list every imported symbol.");
    }
}

fn pe_json(path: &Path, image: &PeImage) -> String {
    let assessment = image.assess_for_beta();
    let mut output = String::new();
    output.push('{');
    push_json_field(&mut output, "file", &path.to_string_lossy(), false);
    push_json_field(&mut output, "format", &image.kind.to_string(), true);
    push_json_field(&mut output, "machine", &image.machine.to_string(), true);
    push_json_field(
        &mut output,
        "machine_code",
        &format!("0x{:04x}", image.machine.raw()),
        true,
    );
    push_json_field(&mut output, "subsystem", &image.subsystem.to_string(), true);
    write!(
        output,
        ",\"subsystem_code\":{},\"timestamp\":{},\"characteristics\":{},\"entry_point_rva\":{},\"image_base\":{},\"size_of_image\":{},\"is_dll\":{},\"is_managed\":{},\"has_certificate_table\":{}",
        image.subsystem.raw(),
        image.timestamp,
        image.characteristics,
        image.entry_point_rva,
        image.image_base,
        image.size_of_image,
        image.is_dll,
        image.is_managed,
        image.has_certificate_table
    )
    .expect("writing JSON to a String cannot fail");

    output.push_str(",\"assessment\":{");
    push_json_field(&mut output, "level", &assessment.level.to_string(), false);
    output.push_str(",\"notes\":[");
    for (index, note) in assessment.notes.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        push_json_string(&mut output, note);
    }
    output.push_str("]}");

    output.push_str(",\"sections\":[");
    for (index, section) in image.sections.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push('{');
        push_json_field(&mut output, "name", &section.name, false);
        write!(
            output,
            ",\"virtual_address\":{},\"virtual_size\":{},\"raw_offset\":{},\"raw_size\":{},\"characteristics\":{}",
            section.virtual_address,
            section.virtual_size,
            section.raw_offset,
            section.raw_size,
            section.characteristics
        )
        .expect("writing JSON to a String cannot fail");
        output.push('}');
    }
    output.push(']');

    output.push_str(",\"imports\":[");
    for (import_index, import) in image.imports.iter().enumerate() {
        if import_index != 0 {
            output.push(',');
        }
        output.push('{');
        push_json_field(&mut output, "dll", &import.dll, false);
        output.push_str(",\"symbols\":[");
        for (symbol_index, symbol) in import.symbols.iter().enumerate() {
            if symbol_index != 0 {
                output.push(',');
            }
            match symbol {
                PeImportSymbol::Name { hint, name } => {
                    output.push('{');
                    push_json_field(&mut output, "kind", "name", false);
                    push_json_field(&mut output, "name", name, true);
                    write!(output, ",\"hint\":{hint}")
                        .expect("writing JSON to a String cannot fail");
                    output.push('}');
                }
                PeImportSymbol::Ordinal(value) => {
                    write!(output, "{{\"kind\":\"ordinal\",\"ordinal\":{value}}}")
                        .expect("writing JSON to a String cannot fail");
                }
            }
        }
        output.push_str("]}");
    }
    output.push_str("]}");
    output
}

fn push_json_field(output: &mut String, name: &str, value: &str, comma: bool) {
    if comma {
        output.push(',');
    }
    push_json_string(output, name);
    output.push(':');
    push_json_string(output, value);
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            control if control <= '\u{1f}' => {
                write!(output, "\\u{:04x}", control as u32)
                    .expect("writing JSON to a String cannot fail");
            }
            other => output.push(other),
        }
    }
    output.push('"');
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
        port_forwards: parsed.port_forwards,
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
        println!("The installation display is ready; use `lsw view {name}` to reopen it.");
    }
    Ok(())
}

fn install_instance(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let parsed = InstallArguments::parse(arguments)?;
    if parsed.without_agent && parsed.seed.agent_binary.is_some() {
        return Err("--agent and --without-agent cannot be used together".into());
    }
    if parsed.seed.unattended_image_index.is_some() && parsed.edition.is_some() {
        return Err("--edition and --unattended-index cannot be used together".into());
    }

    if let Some(iso) = parsed.iso.clone() {
        return install_new_instance(store, parsed, iso);
    }
    if parsed.create_option_seen {
        return Err(
            "--profile, --cpus, --memory, --disk, --network, and --publish require --iso".into(),
        );
    }

    let name = resolve_name(store, parsed.requested.as_deref())?;
    let manifest = store.load(&name)?;
    ensure_install_dependencies(
        manifest.spec.profile,
        parsed.edition.is_some(),
        !parsed.no_viewer,
    )?;
    let mut options = parsed.seed;
    if let Some(requested) = parsed.edition.as_deref() {
        let edition =
            select_windows_edition(store, &manifest.spec.source_iso, Some(requested), true)?
                .expect("a required edition selection must return an edition");
        options.unattended_image_name = Some(edition.name);
    }

    let instance_dir = store.instance_dir(&name)?;
    let seed = instance_dir.join("seed");
    match fs::symlink_metadata(&seed) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            if parsed.seed_option_seen {
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
            if options.agent_binary.is_none() && !parsed.without_agent {
                options.agent_binary = find_windows_agent();
                if options.agent_binary.is_none() {
                    return Err(
                        "lsw-agent.exe was not found; pass --agent PATH, set LSW_WINDOWS_AGENT, or explicitly use --without-agent"
                            .into(),
                    );
                }
            }
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
    start_named_instance(store, &name, LaunchPhase::Install)?;
    if !parsed.no_viewer {
        launch_installation_viewer(store, &name)?;
    }
    Ok(())
}

fn install_new_instance(
    store: &StateStore,
    parsed: InstallArguments,
    iso: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = parsed
        .requested
        .as_deref()
        .ok_or("usage: lsw install NAME --iso PATH --edition EDITION [OPTIONS]")?;
    ensure_install_dependencies(parsed.profile, true, !parsed.no_viewer)?;
    let iso = absolute_path(&iso)?;

    let edition = select_windows_edition(store, &iso, parsed.edition.as_deref(), true)?
        .expect("a new one-shot install must select an edition");
    let mut options = parsed.seed;
    options.unattended_image_name = Some(edition.name.clone());
    if options.agent_binary.is_none() && !parsed.without_agent {
        options.agent_binary = find_windows_agent();
        if options.agent_binary.is_none() {
            return Err(
                "lsw-agent.exe was not found; pass --agent PATH, set LSW_WINDOWS_AGENT, or explicitly use --without-agent"
                    .into(),
            );
        }
    }

    let spec = InstanceSpec {
        name: name.to_owned(),
        source_iso: iso,
        profile: parsed.profile,
        cpus: parsed.cpus,
        memory_mib: parsed.memory_mib,
        disk_gib: parsed
            .disk_gib
            .unwrap_or_else(|| parsed.profile.default_disk_gib()),
        network: parsed.network,
        port_forwards: parsed.port_forwards,
        license_accepted: true,
        allow_unsupported_requirements: parsed.allow_unsupported_requirements,
    };
    let manifest = InstanceManifest::new(spec)?;
    let instance_dir = store.create(&manifest)?;
    if store.default_name()?.is_none() {
        store.set_default(name)?;
    }
    println!("Created instance {name:?} using {}.", edition.name);
    println!(
        "You are responsible for the license, product key, and activation of the Windows media you supplied."
    );

    let provisioner = Provisioner::new(HostCapabilities::detect());
    let preparation = provisioner.plan(&manifest, &instance_dir)?;
    provisioner.apply(&preparation)?;
    println!("Prepared disk, firmware variables, runtime directories, and vTPM state.");

    let token = store.read_agent_token(name)?;
    let seed_plan = InstallSeedBuilder::plan(&manifest, &instance_dir, &token, &options)?;
    for line in seed_plan.describe() {
        println!("  {line}");
    }
    InstallSeedBuilder::apply(&seed_plan)?;
    println!(
        "Installation seed created at {}",
        seed_plan.destination.display()
    );
    start_named_instance(store, name, LaunchPhase::Install)?;
    if !parsed.no_viewer {
        launch_installation_viewer(store, name)?;
    }
    Ok(())
}

fn ensure_install_dependencies(
    profile: WindowsProfile,
    needs_media_tools: bool,
    needs_viewer: bool,
) -> Result<HostCapabilities, Box<dyn std::error::Error>> {
    let mut capabilities = HostCapabilities::detect();
    let mut missing = capabilities.missing_for_profile_launch(profile);
    missing.extend(capabilities.missing_for_profile_preparation(profile));
    if needs_media_tools && capabilities.wimlib_imagex.is_none() {
        missing.push("wimlib-imagex");
    }
    if needs_media_tools && capabilities.xorriso.is_none() {
        missing.push("xorriso");
    }
    if needs_viewer && capabilities.remote_viewer.is_none() {
        missing.push("remote-viewer");
    }
    missing.sort_unstable();
    missing.dedup();
    if !missing.is_empty() {
        println!("Missing host dependencies: {}", missing.join(", "));
        println!("Attempting the same package repair as `lsw doctor --fix`...");
        fix_host_dependencies()?;
        capabilities = HostCapabilities::detect();
        let mut remaining = capabilities.missing_for_profile_launch(profile);
        remaining.extend(capabilities.missing_for_profile_preparation(profile));
        if needs_media_tools && capabilities.wimlib_imagex.is_none() {
            remaining.push("wimlib-imagex");
        }
        if needs_media_tools && capabilities.xorriso.is_none() {
            remaining.push("xorriso");
        }
        if needs_viewer && capabilities.remote_viewer.is_none() {
            remaining.push("remote-viewer");
        }
        remaining.sort_unstable();
        remaining.dedup();
        if !remaining.is_empty() {
            return Err(format!(
                "required install dependencies remain unavailable: {}",
                remaining.join(", ")
            )
            .into());
        }
    }
    Ok(capabilities)
}

fn select_windows_edition(
    store: &StateStore,
    iso: &Path,
    requested: Option<&str>,
    require_selection: bool,
) -> Result<Option<WindowsEdition>, Box<dyn std::error::Error>> {
    if requested.is_none() && !require_selection {
        return Ok(None);
    }
    let editions = WindowsMediaInspector::new(HostCapabilities::detect())
        .inspect(iso, &store.root().join("run"))?;
    let selected = if let Some(requested) = requested {
        let matches = editions
            .iter()
            .filter(|edition| edition.matches(requested))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [edition] => Some((*edition).clone()),
            [] => {
                return Err(format!(
                    "edition {requested:?} is not present in {}; available editions: {}",
                    iso.display(),
                    editions
                        .iter()
                        .map(|edition| edition.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into())
            }
            _ => {
                return Err(format!(
                    "edition {requested:?} is ambiguous; use one of the full names shown below"
                )
                .into())
            }
        }
    } else if editions.len() == 1 {
        editions.first().cloned()
    } else {
        None
    };

    println!("Windows editions found in {}:", iso.display());
    for edition in &editions {
        let marker = if selected.as_ref() == Some(edition) {
            " (selected)"
        } else {
            ""
        };
        println!("  - {}{}", edition.name, marker);
    }
    if let Some(selected) = selected {
        Ok(Some(selected))
    } else {
        Err(
            "the ISO contains multiple editions; pass --edition NAME (for example, --edition pro)"
                .into(),
        )
    }
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

fn view(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "view")?;
    let name = resolve_name(store, requested)?;
    launch_viewer(store, &name, true)
}

fn launch_installation_viewer(
    store: &StateStore,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    launch_viewer(store, name, false)
}

fn launch_viewer(
    store: &StateStore,
    name: &str,
    explicit: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("DISPLAY").is_none() && env::var_os("WAYLAND_DISPLAY").is_none() {
        let message = format!(
            "no graphical desktop was detected; run `lsw view {name}` from a graphical session"
        );
        if explicit {
            return Err(message.into());
        }
        println!("Viewer not opened: {message}.");
        return Ok(());
    }

    let capabilities = HostCapabilities::detect();
    let viewer = env::var_os("LSW_INSTALL_VIEWER")
        .map(PathBuf::from)
        .or(capabilities.remote_viewer);
    let Some(viewer) = viewer else {
        let message = "remote-viewer was not found; run `lsw doctor --fix`";
        if explicit {
            return Err(message.into());
        }
        println!("Viewer not opened: {message}.");
        return Ok(());
    };

    let instance_dir = store.instance_dir(name)?;
    let socket = instance_dir.join("run/recovery-vnc.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if !socket.exists() {
        return Err(format!("the installation display for {name:?} is not ready").into());
    }
    let socket_text = socket
        .to_str()
        .filter(|value| !value.contains(['\r', '\n']))
        .ok_or("the installation display path cannot be represented safely")?;
    let connection = instance_dir.join("run/installation-viewer.vv");
    fs::write(
        &connection,
        format!(
            "[virt-viewer]\ntype=vnc\nunix-path={socket_text}\ntitle=LSW installation - {name}\n"
        ),
    )?;
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600))?;
    Command::new(viewer)
        .arg(&connection)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    println!("Opened the LSW installation viewer for {name:?}.");
    Ok(())
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

fn suspend(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "suspend")?;
    let name = resolve_name(store, requested)?;
    let client = DaemonClient::new(store);
    for line in client.request_checked(&format!("SUSPEND {name}"))? {
        println!("{line}");
    }
    Ok(())
}

fn resume(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let requested = optional_name(arguments, "resume")?;
    let name = resolve_name(store, requested)?;
    let client = DaemonClient::new(store);
    for line in client.request_checked(&format!("RESUME {name}"))? {
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
            println!("Resuming {name:?}...");
            let daemon = DaemonClient::new(store);
            for line in daemon.request_checked(&format!("RESUME {name}"))? {
                println!("{line}");
            }
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

fn config(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or("usage: lsw config get [NAME] | lsw config set NAME KEY=VALUE [KEY=VALUE ...]")?;
    match action {
        "get" => {
            let requested = optional_name(&arguments[1..], "config get")?;
            let name = resolve_name(store, requested)?;
            let manifest = store.load(&name)?;
            println!("name={}", manifest.spec.name);
            println!("profile={}", manifest.spec.profile);
            println!("cpus={}", manifest.spec.cpus);
            println!("memory.max={}", format_memory(manifest.spec.memory_mib));
            println!("disk.max={}GiB", manifest.spec.disk_gib);
            println!("network={}", manifest.spec.network);
            println!(
                "idle-timeout={}",
                format_duration(manifest.idle_timeout_seconds)
            );
        }
        "set" => {
            let name = arguments
                .get(1)
                .and_then(|value| value.to_str())
                .ok_or("usage: lsw config set NAME KEY=VALUE [KEY=VALUE ...]")?;
            if arguments.len() < 3 {
                return Err("usage: lsw config set NAME KEY=VALUE [KEY=VALUE ...]".into());
            }
            let mut manifest = store.load(name)?;
            if matches!(
                manifest.state,
                InstanceState::Installing | InstanceState::Running | InstanceState::Suspended
            ) {
                return Err(format!(
                    "instance {name:?} is {}; shut it down before changing runtime limits",
                    manifest.state
                )
                .into());
            }
            for assignment in &arguments[2..] {
                let assignment = assignment
                    .to_str()
                    .ok_or("configuration assignments must be valid UTF-8")?;
                let (key, value) = assignment
                    .split_once('=')
                    .ok_or("configuration assignments must use KEY=VALUE syntax")?;
                match key {
                    "memory.max" => manifest.spec.memory_mib = parse_memory_mib(value)?,
                    "idle-timeout" => {
                        manifest.idle_timeout_seconds = parse_duration_seconds(value)?
                    }
                    _ => {
                        return Err(format!(
                            "unknown configuration key {key:?}; supported keys are memory.max and idle-timeout"
                        )
                        .into())
                    }
                }
            }
            store.update(&manifest)?;
            println!("Updated configuration for {name:?}.");
            println!("memory.max={}", format_memory(manifest.spec.memory_mib));
            println!(
                "idle-timeout={}",
                format_duration(manifest.idle_timeout_seconds)
            );
        }
        _ => {
            return Err(
                "usage: lsw config get [NAME] | lsw config set NAME KEY=VALUE [KEY=VALUE ...]"
                    .into(),
            )
        }
    }
    Ok(())
}

fn parse_memory_mib(value: &str) -> Result<u32, Box<dyn std::error::Error>> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("GiB") {
        (number, 1024_u64)
    } else if let Some(number) = value.strip_suffix("MiB") {
        (number, 1_u64)
    } else {
        (value, 1_u64)
    };
    let amount = number.parse::<u64>()?;
    u32::try_from(
        amount
            .checked_mul(multiplier)
            .ok_or("memory value is too large")?,
    )
    .map_err(|_| "memory value is too large".into())
}

fn format_memory(memory_mib: u32) -> String {
    if memory_mib % 1024 == 0 {
        format!("{}GiB", memory_mib / 1024)
    } else {
        format!("{memory_mib}MiB")
    }
}

fn parse_duration_seconds(value: &str) -> Result<u64, Box<dyn std::error::Error>> {
    if value == "0" || value == "off" {
        return Ok(0);
    }
    let (number, multiplier) = if let Some(number) = value.strip_suffix('s') {
        (number, 1_u64)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_u64)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 60 * 60)
    } else if let Some(number) = value.strip_suffix('d') {
        (number, 24 * 60 * 60)
    } else {
        return Err("duration must use s, m, h, or d (for example, 10m)".into());
    };
    number
        .parse::<u64>()?
        .checked_mul(multiplier)
        .ok_or_else(|| "duration is too large".into())
}

fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        "off".to_owned()
    } else if seconds % (24 * 60 * 60) == 0 {
        format!("{}d", seconds / (24 * 60 * 60))
    } else if seconds % (60 * 60) == 0 {
        format!("{}h", seconds / (60 * 60))
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn logs(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let mut requested = None;
    let mut follow = false;
    let mut lines = 200_usize;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index]
            .to_str()
            .ok_or("log arguments must be valid UTF-8")?;
        match argument {
            "--follow" | "-f" => follow = true,
            "--lines" | "-n" => lines = parse_number(arguments, &mut index, argument)?,
            value if value.starts_with('-') => {
                return Err(format!("unknown logs option {value:?}").into())
            }
            name => {
                if requested.replace(name).is_some() {
                    return Err("usage: lsw logs [NAME] [--lines N] [--follow]".into());
                }
            }
        }
        index += 1;
    }
    let name = resolve_name(store, requested)?;
    let path = store.instance_dir(&name)?.join("qemu.log");
    if !path.is_file() {
        return Err(format!("no QEMU log exists yet for instance {name:?}").into());
    }
    let (text, mut offset) = read_log_tail(&path, lines)?;
    print!("{text}");
    std::io::stdout().flush()?;
    if follow {
        loop {
            thread::sleep(Duration::from_millis(250));
            let mut file = fs::File::open(&path)?;
            let length = file.metadata()?.len();
            if length < offset {
                offset = 0;
            }
            if length > offset {
                file.seek(SeekFrom::Start(offset))?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                print!("{}", String::from_utf8_lossy(&bytes));
                std::io::stdout().flush()?;
                offset = length;
            }
        }
    }
    Ok(())
}

fn read_log_tail(path: &Path, lines: usize) -> Result<(String, u64), Box<dyn std::error::Error>> {
    const MAX_TAIL_BYTES: u64 = 1024 * 1024;
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(MAX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut selected = text.lines().rev().take(lines).collect::<Vec<_>>();
    selected.reverse();
    let mut output = selected.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    Ok((output, length))
}

fn remove_instance(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let name = required_exact_name(arguments, "remove")?;
    store.remove(name)?;
    println!("Removed instance {name:?} and its local virtual disk.");
    Ok(())
}

fn shutdown(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let mut all = false;
    let mut force = false;
    let mut requested = None;
    for argument in arguments {
        let argument = argument
            .to_str()
            .ok_or("shutdown arguments must be valid UTF-8")?;
        match argument {
            "--all" => all = true,
            "--force" => force = true,
            value if value.starts_with('-') => {
                return Err(format!("unknown shutdown option {value:?}").into())
            }
            name => {
                if requested.replace(name).is_some() {
                    return Err("usage: lsw shutdown [NAME | --all] [--force]".into());
                }
            }
        }
    }
    if all && requested.is_some() {
        return Err("NAME and --all cannot be used together".into());
    }
    let names = if all {
        store
            .list()?
            .into_iter()
            .filter(|manifest| {
                matches!(
                    manifest.state,
                    InstanceState::Installing | InstanceState::Running | InstanceState::Suspended
                )
            })
            .map(|manifest| manifest.spec.name)
            .collect::<Vec<_>>()
    } else {
        vec![resolve_name(store, requested)?]
    };
    if names.is_empty() {
        println!("No active LSW instances.");
        return Ok(());
    }
    let client = DaemonClient::new(store);
    let mode = if force { "force" } else { "graceful" };
    let mut failures = Vec::new();
    for name in names {
        match client.request_checked(&format!("STOP {name} {mode}")) {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
            }
            Err(error) => failures.push(format!("{name}: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "some instances could not be shut down: {}",
            failures.join("; ")
        )
        .into())
    }
}

fn diagnose(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let name = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or("usage: lsw diagnose NAME [--bundle] [--output PATH]")?;
    let mut bundle = false;
    let mut output = None;
    let mut index = 1;
    while index < arguments.len() {
        let argument = arguments[index]
            .to_str()
            .ok_or("diagnose arguments must be valid UTF-8")?;
        match argument {
            "--bundle" => bundle = true,
            "--output" => {
                output = Some(PathBuf::from(next_value(arguments, &mut index, argument)?));
                bundle = true;
            }
            _ => return Err(format!("unknown diagnose option {argument:?}").into()),
        }
        index += 1;
    }
    let manifest = store.load(name)?;
    let instance_dir = store.instance_dir(name)?;
    println!("LSW diagnosis for {name:?}");
    println!("  state: {}", manifest.state);
    println!("  instance directory: {}", instance_dir.display());
    println!("  QEMU log: {}", instance_dir.join("qemu.log").display());
    if !bundle {
        println!("Pass --bundle to create a redacted support archive.");
        return Ok(());
    }

    store.initialize()?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let diagnostics_root = store.root().join("run");
    fs::create_dir_all(&diagnostics_root)?;
    fs::set_permissions(&diagnostics_root, fs::Permissions::from_mode(0o700))?;
    let staging = diagnostics_root.join(format!(
        "diagnose-{name}-{}-{timestamp}",
        std::process::id()
    ));
    fs::create_dir(&staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
    let result = (|| -> Result<PathBuf, Box<dyn std::error::Error>> {
        let encoded = manifest.encode()?;
        let redacted = encoded
            .lines()
            .map(|line| {
                if line.starts_with("source_iso=") {
                    "source_iso=<redacted>".to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(
            staging.join("instance.redacted.lsw"),
            format!("{redacted}\n"),
        )?;
        fs::write(
            staging.join("host.txt"),
            diagnostic_host_text(store, &HostCapabilities::detect()),
        )?;
        let daemon = DaemonClient::new(store);
        let status = daemon
            .request(&format!("STATUS {name}"))
            .map(|lines| lines.join("\n"))
            .unwrap_or_else(|error| format!("unavailable: {error}"));
        fs::write(staging.join("status.txt"), format!("{status}\n"))?;
        if let Ok(plan) = QemuPlanner::new(HostCapabilities::detect()).plan(
            &manifest,
            &instance_dir,
            if manifest.state == InstanceState::Configured {
                LaunchPhase::Install
            } else {
                LaunchPhase::Run
            },
        ) {
            let mut command = plan.display_command();
            if let Some(value) = manifest.spec.source_iso.to_str() {
                command = command.replace(value, "<WINDOWS_ISO>");
            }
            if let Some(value) = instance_dir.to_str() {
                command = command.replace(value, "<INSTANCE_DIR>");
            }
            fs::write(staging.join("qemu-plan.txt"), format!("{command}\n"))?;
        }
        let redactions = [
            (manifest.spec.source_iso.as_path(), "<WINDOWS_ISO>"),
            (instance_dir.as_path(), "<INSTANCE_DIR>"),
            (store.root(), "<STATE_ROOT>"),
        ];
        for filename in ["qemu.log", "helper.log", "swtpm.log"] {
            copy_diagnostic_tail(
                &instance_dir.join(filename),
                &staging.join(filename),
                &redactions,
            )?;
        }

        let output =
            absolute_path(&output.unwrap_or_else(|| {
                PathBuf::from(format!("lsw-diagnose-{name}-{timestamp}.tar.gz"))
            }))?;
        if output.exists() {
            return Err(format!("refusing to replace existing {}", output.display()).into());
        }
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(&output)
            .args(["-C"])
            .arg(&staging)
            .arg(".")
            .status()?;
        if !status.success() {
            return Err(format!("tar failed with {status}").into());
        }
        fs::set_permissions(&output, fs::Permissions::from_mode(0o600))?;
        Ok(output)
    })();
    let _ = fs::remove_dir_all(&staging);
    let output = result?;
    println!("Created redacted diagnostic bundle: {}", output.display());
    Ok(())
}

fn diagnostic_host_text(_store: &StateStore, capabilities: &HostCapabilities) -> String {
    let backend = QemuBackend::select(capabilities);
    format!(
        concat!(
            "lsw_version={}\nstate_root={}\nplatform={}\naccelerator={}\n",
            "kvm={}\nqemu={}\nqemu_img={}\nswtpm={}\nwimlib={}\nxorriso={}\nviewer={}\n"
        ),
        env!("CARGO_PKG_VERSION"),
        "<redacted>",
        capabilities.platform,
        backend.accelerator(),
        yes_no(capabilities.accelerators.supports(VmAccelerator::Kvm)),
        display_optional(&capabilities.qemu_system),
        display_optional(&capabilities.qemu_img),
        display_optional(&capabilities.swtpm),
        display_optional(&capabilities.wimlib_imagex),
        display_optional(&capabilities.xorriso),
        display_optional(&capabilities.remote_viewer),
    )
}

fn copy_diagnostic_tail(
    source: &Path,
    destination: &Path,
    redactions: &[(&Path, &str)],
) -> Result<(), Box<dyn std::error::Error>> {
    if !fs::symlink_metadata(source)
        .map(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let mut file = fs::File::open(source)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(4 * 1024 * 1024)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    for (path, replacement) in redactions {
        if let Some(path) = path.to_str().filter(|path| !path.is_empty()) {
            text = text.replace(path, replacement);
        }
    }
    fs::write(destination, text)?;
    Ok(())
}

fn bench(store: &StateStore, arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
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
        println!("LSW beta.5 performance baseline");
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

fn push_optional_number(output: &mut String, key: &str, value: Option<u128>) {
    write!(output, ",\"{key}\":").expect("writing to a String cannot fail");
    if let Some(value) = value {
        write!(output, "{value}").expect("writing to a String cannot fail");
    } else {
        output.push_str("null");
    }
}

fn format_microseconds(value: Option<u128>) -> String {
    value
        .map(|value| format!("{value} us"))
        .unwrap_or_else(|| "unavailable".to_owned())
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
    println!(
        "idle timeout:         {}",
        format_duration(manifest.idle_timeout_seconds)
    );
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
        "  lsw doctor [--fix]\n",
        "  lsw bench [NAME] [--json]\n",
        "  lsw inspect FILE [--imports] [--json]\n",
        "  lsw profile PROFILE\n",
        "  lsw create NAME --iso PATH --accept-license [OPTIONS]\n",
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
        "  lsw install NAME --iso PATH --edition EDITION [--profile PROFILE] [OPTIONS]\n",
        "  lsw install [NAME] [--locale LOCALE] [--edition EDITION] [--agent PATH]\n",
        "              [--unattended-index N] [--without-agent] [--no-viewer]\n",
        "  lsw view [NAME]\n",
        "  lsw start [NAME]\n",
        "  lsw status [NAME]\n",
        "  lsw suspend [NAME]\n",
        "  lsw resume [NAME]\n",
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
        "  --publish HOST:GUEST publish a TCP guest port on host loopback; repeatable\n",
        "  --accept-license     confirm acceptance of the supplied media's license\n",
        "  --allow-unsupported-requirements\n",
        "                       permit an explicitly unsupported small VM\n\n",
        "ONE-SHOT INSTALL:\n",
        "  --edition EDITION   select an edition by its ISO name or a friendly alias such as pro\n",
        "  --no-viewer         do not open the installation viewer (for headless automation)\n",
        "  supplying --iso records responsibility for the user-provided Windows license\n\n",
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

#[derive(Debug)]
struct InstallArguments {
    requested: Option<String>,
    iso: Option<PathBuf>,
    edition: Option<String>,
    profile: WindowsProfile,
    cpus: u16,
    memory_mib: u32,
    disk_gib: Option<u32>,
    network: NetworkMode,
    port_forwards: Vec<PortForward>,
    allow_unsupported_requirements: bool,
    seed: InstallSeedOptions,
    without_agent: bool,
    no_viewer: bool,
    seed_option_seen: bool,
    create_option_seen: bool,
}

impl InstallArguments {
    fn parse(arguments: &[OsString]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut parsed = Self {
            requested: None,
            iso: None,
            edition: None,
            profile: WindowsProfile::Standard,
            cpus: 2,
            memory_mib: 4096,
            disk_gib: None,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            allow_unsupported_requirements: false,
            seed: InstallSeedOptions::default(),
            without_agent: false,
            no_viewer: false,
            seed_option_seen: false,
            create_option_seen: false,
        };
        let mut index = 0;
        while index < arguments.len() {
            let argument = arguments[index]
                .to_str()
                .ok_or("install arguments must be valid UTF-8")?;
            match argument {
                "--iso" => {
                    if parsed.iso.is_some() {
                        return Err("--iso was supplied more than once".into());
                    }
                    parsed.iso = Some(PathBuf::from(next_value(arguments, &mut index, argument)?));
                }
                "--edition" => {
                    if parsed.edition.is_some() {
                        return Err("--edition was supplied more than once".into());
                    }
                    parsed.edition = Some(next_value(arguments, &mut index, argument)?.to_owned());
                    parsed.seed_option_seen = true;
                }
                "--profile" => {
                    parsed.profile = next_value(arguments, &mut index, argument)?.parse()?;
                    parsed.create_option_seen = true;
                }
                "--cpus" => {
                    parsed.cpus = parse_number(arguments, &mut index, argument)?;
                    parsed.create_option_seen = true;
                }
                "--memory" => {
                    parsed.memory_mib = parse_number(arguments, &mut index, argument)?;
                    parsed.create_option_seen = true;
                }
                "--disk" => {
                    parsed.disk_gib = Some(parse_number(arguments, &mut index, argument)?);
                    parsed.create_option_seen = true;
                }
                "--network" => {
                    parsed.network = next_value(arguments, &mut index, argument)?.parse()?;
                    parsed.create_option_seen = true;
                }
                "--publish" => {
                    parsed
                        .port_forwards
                        .push(next_value(arguments, &mut index, argument)?.parse()?);
                    parsed.create_option_seen = true;
                }
                "--locale" => {
                    parsed.seed.locale = next_value(arguments, &mut index, argument)?.to_owned();
                    parsed.seed_option_seen = true;
                }
                "--unattended-index" => {
                    parsed.seed.unattended_image_index =
                        Some(parse_number(arguments, &mut index, argument)?);
                    parsed.seed_option_seen = true;
                }
                "--agent" => {
                    parsed.seed.agent_binary = Some(absolute_path(Path::new(next_value(
                        arguments, &mut index, argument,
                    )?))?);
                    parsed.seed_option_seen = true;
                }
                "--without-agent" => {
                    if parsed.without_agent {
                        return Err("--without-agent was supplied more than once".into());
                    }
                    parsed.without_agent = true;
                    parsed.seed_option_seen = true;
                }
                "--no-viewer" => {
                    if parsed.no_viewer {
                        return Err("--no-viewer was supplied more than once".into());
                    }
                    parsed.no_viewer = true;
                }
                "--allow-unsupported-requirements" => {
                    parsed.allow_unsupported_requirements = true;
                    parsed.create_option_seen = true;
                }
                "--accept-license" => {}
                value if value.starts_with('-') => {
                    return Err(format!("unknown install option {value:?}").into())
                }
                name => {
                    if parsed.requested.replace(name.to_owned()).is_some() {
                        return Err("usage: lsw install [NAME] [OPTIONS]".into());
                    }
                }
            }
            index += 1;
        }
        Ok(parsed)
    }
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
    port_forwards: Vec<PortForward>,
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
        let mut port_forwards = Vec::new();
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
                "--publish" => {
                    port_forwards.push(next_value(arguments, &mut index, option)?.parse()?);
                }
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
            port_forwards,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_accepts_repeated_tcp_publish_options() {
        let arguments = [
            "win-dev",
            "--iso",
            "windows.iso",
            "--accept-license",
            "--publish",
            "8080:80",
            "--publish",
            "8443:443",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        let parsed = CreateArguments::parse(&arguments).expect("create options should parse");
        assert_eq!(
            parsed.port_forwards,
            vec![
                PortForward::new(8080, 80).expect("ports should be valid"),
                PortForward::new(8443, 443).expect("ports should be valid"),
            ]
        );
    }

    #[test]
    fn create_rejects_malformed_tcp_publish_option() {
        let arguments = [
            "win-dev",
            "--iso",
            "windows.iso",
            "--accept-license",
            "--publish",
            "8080",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        assert!(CreateArguments::parse(&arguments).is_err());
    }

    #[test]
    fn one_shot_install_parses_the_beta_five_beginner_flow() {
        let arguments = [
            "win-dev",
            "--iso",
            "Windows11.iso",
            "--edition",
            "pro",
            "--profile",
            "slim",
            "--no-viewer",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        let parsed = InstallArguments::parse(&arguments).expect("install options should parse");
        assert_eq!(parsed.requested.as_deref(), Some("win-dev"));
        assert_eq!(parsed.iso, Some(PathBuf::from("Windows11.iso")));
        assert_eq!(parsed.edition.as_deref(), Some("pro"));
        assert_eq!(parsed.profile, WindowsProfile::Slim);
        assert!(parsed.no_viewer);
    }

    #[test]
    fn runtime_configuration_units_are_strict_and_stable() {
        assert_eq!(parse_memory_mib("4GiB").expect("memory should parse"), 4096);
        assert_eq!(
            parse_memory_mib("4608MiB").expect("memory should parse"),
            4608
        );
        assert!(parse_memory_mib("4GB").is_err());
        assert_eq!(
            parse_duration_seconds("10m").expect("duration should parse"),
            600
        );
        assert_eq!(format_duration(600), "10m");
        assert!(parse_duration_seconds("ten minutes").is_err());
    }

    #[test]
    fn pe_json_strings_escape_protocol_sensitive_characters() {
        let mut output = String::new();
        push_json_string(
            &mut output,
            "quote=\" slash=\\ line=\n tab=\t control=\u{0001} 中文",
        );
        assert_eq!(
            output,
            "\"quote=\\\" slash=\\\\ line=\\n tab=\\t control=\\u0001 中文\""
        );
    }
}
