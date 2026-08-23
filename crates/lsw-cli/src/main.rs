// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("the LSW 1.0 beta CLI currently requires a Unix host");

mod agent_client;
mod arguments;
mod completion;
mod daemon_client;
mod installation;
mod license;
mod path_translation;
mod progress;
mod transfer;

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
use arguments::{
    next_value, parse_number, resolve_port_forwards, CreateArguments, InstallArguments,
};
use daemon_client::DaemonClient;
use installation::install_instance;
use license::{license, show_activation_notice_once};
use lsw_core::{
    CustomizationPlan, HostCapabilities, InstallSeedBuilder, InstallSeedOptions, InstanceManifest,
    InstanceSpec, InstanceState, LaunchPhase, LswError, MicrosoftIsoRequest, MicrosoftIsoResolver,
    PeImage, PeImportSymbol, ProcessEnvironment, Provisioner, QemuBackend, QemuPlanner,
    SessionKind, StartRequest, StateStore, VmAccelerator, WindowsProfile,
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
        "media" => {
            media(remaining)?;
            return Ok(0);
        }
        "path" => {
            path_translation::command(remaining)?;
            return Ok(0);
        }
        "completion" => {
            completion::command(remaining)?;
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
        "license" => license(&store, remaining)?,
        "start" => start_instance(&store, remaining, LaunchPhase::Run)?,
        "status" => status(&store, remaining)?,
        "suspend" => suspend(&store, remaining)?,
        "resume" => resume(&store, remaining)?,
        "stop" => stop(&store, remaining)?,
        "shell" => return shell(&store, remaining),
        "exec" => return guest_command(&store, remaining, SessionKind::Exec),
        "run" => return guest_command(&store, remaining, SessionKind::Run),
        "push" => transfer::push(&store, remaining)?,
        "pull" => transfer::pull(&store, remaining)?,
        "sync" => transfer::sync(&store, remaining)?,
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
        let install_aria2 = capabilities.aria2c.is_none();
        if missing.is_empty() && !install_aria2 {
            println!("All beta.6 host dependencies are already installed.\n");
        } else {
            let mut requested = missing;
            if install_aria2 {
                requested.push("aria2c (optional download accelerator)");
            }
            println!(
                "Installing beta.6 host dependencies: {}",
                requested.join(", ")
            );
            fix_host_dependencies(false)?;
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
    println!("  setsid:      {}", display_optional(&capabilities.setsid));
    println!("  swtpm:       {}", display_optional(&capabilities.swtpm));
    println!("  aria2c:      {}", display_optional(&capabilities.aria2c));
    println!(
        "  wimlib:      {}",
        display_optional(&capabilities.wimlib_imagex)
    );
    println!("  xorriso:     {}", display_optional(&capabilities.xorriso));
    println!(
        "  7z:          {}",
        display_optional(&capabilities.seven_zip)
    );
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

fn fix_host_dependencies(needs_viewer: bool) -> Result<(), Box<dyn std::error::Error>> {
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
        let mut packages = vec![
            "install",
            "--yes",
            "qemu-system-x86",
            "qemu-utils",
            "util-linux",
            "ovmf",
            "swtpm",
            "aria2",
            "wimtools",
            "xorriso",
            "7zip",
        ];
        if needs_viewer {
            packages.push("virt-viewer");
        }
        run_package_command(&elevated, "apt-get", &packages)?;
    } else if family.contains("fedora") || family.contains("rhel") {
        let mut packages = vec![
            "install",
            "--assumeyes",
            "qemu-system-x86-core",
            "qemu-img",
            "util-linux",
            "edk2-ovmf",
            "swtpm",
            "aria2",
            "wimlib-utils",
            "xorriso",
            "p7zip",
            "p7zip-plugins",
        ];
        if needs_viewer {
            packages.push("virt-viewer");
        }
        run_package_command(&elevated, "dnf", &packages)?;
    } else if family.contains("arch") {
        let mut packages = vec![
            "--sync",
            "--needed",
            "--noconfirm",
            "qemu-desktop",
            "util-linux",
            "edk2-ovmf",
            "swtpm",
            "aria2",
            "wimlib",
            "xorriso",
            "7zip",
        ];
        if needs_viewer {
            packages.push("virt-viewer");
        }
        run_package_command(&elevated, "pacman", &packages)?;
    } else if family.contains("suse") {
        let mut packages = vec![
            "--non-interactive",
            "install",
            "qemu-x86",
            "qemu-tools",
            "util-linux",
            "qemu-ovmf-x86_64",
            "swtpm",
            "aria2",
            "wimlib",
            "xorriso",
            "7zip",
        ];
        if needs_viewer {
            packages.push("virt-viewer");
        }
        run_package_command(&elevated, "zypper", &packages)?;
    } else {
        return Err(format!(
            "automatic dependency repair does not support distribution {distribution:?}; install QEMU, qemu-img, OVMF, swtpm, util-linux, aria2, wimlib-imagex, xorriso, and 7z manually{}",
            if needs_viewer { ", plus remote-viewer" } else { "" }
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
    let port_forwards = resolve_port_forwards(&parsed.port_forwards, &parsed.name)?;
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
        port_forwards,
        license_accepted: parsed.accept_windows_license,
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
        let message = "remote-viewer was not found; install the optional virt-viewer package";
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
    let parsed = GuestCommandArguments::parse(arguments, kind)?;
    let name = resolve_name(store, parsed.requested.as_deref())?;
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
struct GuestCommandArguments {
    requested: Option<String>,
    request: StartRequest,
    environment: ProcessEnvironment,
    detached: bool,
}

impl GuestCommandArguments {
    fn parse(
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
                " [--detach]"
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
        Ok(Self {
            requested,
            request: StartRequest {
                kind,
                argv,
                working_directory,
            },
            environment: ProcessEnvironment::new(environment)?,
            detached,
        })
    }
}

fn parse_guest_environment(value: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let (name, value) = value.split_once('=').ok_or("--env requires KEY=VALUE")?;
    if name.is_empty() {
        return Err("--env requires a non-empty variable name".into());
    }
    Ok((name.to_owned(), value.to_owned()))
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
        let windows_code = code as u32;
        eprintln!(
            "lsw: guest exit code {windows_code} (0x{windows_code:08X}) cannot be represented by a Unix shell; returning 255"
        );
        Ok(255)
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
    let plan = CustomizationPlan::for_profile(profile)?;
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

fn media(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or("usage: lsw media <resolve|published-sha256> [--language LANGUAGE]")?;
    if action != "resolve" && action != "published-sha256" {
        return Err("usage: lsw media <resolve|published-sha256> [--language LANGUAGE]".into());
    }
    let mut language = "English";
    match &arguments[1..] {
        [] => {}
        [option, value] if option == "--language" => {
            language = value.to_str().ok_or("language must be valid UTF-8")?;
        }
        _ => {
            return Err("usage: lsw media <resolve|published-sha256> [--language LANGUAGE]".into());
        }
    }
    let request = MicrosoftIsoRequest {
        language: language.to_owned(),
    };
    let resolver = MicrosoftIsoResolver::new();
    if action == "published-sha256" {
        println!("SHA256={}", resolver.published_sha256(&request)?);
        return Ok(());
    }
    let resolved = resolver.resolve(&request)?;
    println!("PRODUCT_ID={}", resolved.product_id);
    println!("SKU_ID={}", resolved.sku_id);
    println!("LANGUAGE={}", resolved.language);
    println!("ARCHITECTURE={}", resolved.architecture);
    println!("FILENAME={}", resolved.filename);
    println!("SHA256={}", resolved.expected_sha256);
    if let Some(expires) = resolved.expires_at {
        println!("URL_EXPIRES={expires}");
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
        for (source, filename) in [
            ("winpe-prepare-qemu.log", "winpe-prepare-qemu.log"),
            ("winpe-apply-qemu.log", "winpe-apply-qemu.log"),
            ("run/winpe-prepare-serial.log", "winpe-prepare-serial.log"),
            ("run/winpe-apply-serial.log", "winpe-apply-serial.log"),
            (
                "run/winpe-prepare-status/status.log",
                "winpe-prepare-status.log",
            ),
            (
                "run/winpe-apply-status/status.log",
                "winpe-apply-status.log",
            ),
            (
                "run/winpe-prepare-status/dism.log",
                "winpe-prepare-dism.log",
            ),
            ("run/winpe-apply-status/dism.log", "winpe-apply-dism.log"),
        ] {
            copy_diagnostic_tail(
                &instance_dir.join(source),
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
            "kvm={}\nqemu={}\nqemu_img={}\nsetsid={}\nswtpm={}\naria2c={}\nwimlib={}\nxorriso={}\nseven_zip={}\nviewer={}\n"
        ),
        env!("CARGO_PKG_VERSION"),
        "<redacted>",
        capabilities.platform,
        backend.accelerator(),
        yes_no(capabilities.accelerators.supports(VmAccelerator::Kvm)),
        display_optional(&capabilities.qemu_system),
        display_optional(&capabilities.qemu_img),
        display_optional(&capabilities.setsid),
        display_optional(&capabilities.swtpm),
        display_optional(&capabilities.aria2c),
        display_optional(&capabilities.wimlib_imagex),
        display_optional(&capabilities.xorriso),
        display_optional(&capabilities.seven_zip),
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
        println!("LSW beta.6 performance baseline");
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
        "  lsw media resolve [--language LANGUAGE]\n",
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
        "              [--accept-windows-license]\n",
        "  lsw license status [NAME]\n",
        "  lsw license activate [NAME] [--key-stdin | --online]\n",
        "  lsw license open [NAME]\n",
        "  lsw view [NAME]\n",
        "  lsw start [NAME]\n",
        "  lsw status [NAME]\n",
        "  lsw suspend [NAME]\n",
        "  lsw resume [NAME]\n",
        "  lsw stop [NAME] [--force]\n",
        "  lsw shell [NAME]\n",
        "  lsw exec [NAME] [--cwd PATH] [-e KEY=VALUE] -- COMMAND [ARG ...]\n",
        "  lsw run [NAME] [--cwd PATH] [-e KEY=VALUE] [--detach] -- PROGRAM [ARG ...]\n",
        "  lsw push [NAME] [--recursive] HOST_PATH WINDOWS_PATH\n",
        "  lsw pull [NAME] [--recursive] WINDOWS_PATH HOST_PATH\n",
        "  lsw sync [NAME] [--watch] HOST_DIRECTORY WINDOWS_DIRECTORY\n",
        "  lsw daemon [start|status]\n",
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

#[cfg(test)]
mod tests;
