// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) fn user_command(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().and_then(|value| value.to_str()) {
        Some("setup") => user_setup::command(store, &arguments[1..]),
        Some("add") => user_setup::add(store, &arguments[1..]),
        Some("promote") => user_setup::set_role(
            store,
            &arguments[1..],
            lsw_core::WindowsUserRole::Administrator,
        ),
        Some("demote") => user_setup::set_role(
            store,
            &arguments[1..],
            lsw_core::WindowsUserRole::Standard,
        ),
        _ => Err(
            "usage: lsw user <setup|add> [NAME] [--username USER] [--password-stdin] [--administrator] | lsw user <promote|demote> [NAME]"
                .into(),
        ),
    }
}

pub(super) fn image_command(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments
        .first()
        .and_then(|value| value.to_str())
        .ok_or("usage: lsw image <list|seal NAME|verify KEY>")?;
    let capabilities = HostCapabilities::detect();
    let manager = ImageManager::new(store, &capabilities);
    match action {
        "list" if arguments.len() == 1 => {
            let images = manager.list()?;
            if images.is_empty() {
                println!("No sealed base images.");
            } else {
                println!("KEY\tDISK");
                for image in images {
                    println!("{}\t{}", image.key, image.disk.display());
                }
            }
        }
        "seal" if arguments.len() == 2 => {
            let name = arguments[1]
                .to_str()
                .ok_or("instance name must be valid UTF-8")?;
            let agent = find_windows_agent().ok_or(
                "lsw-agent.exe was not found; set LSW_WINDOWS_AGENT before sealing an image",
            )?;
            let manifest = store.load(name)?;
            if !matches!(
                manifest.state,
                InstanceState::Stopped | InstanceState::Hibernated
            ) {
                return Err(format!(
                    "instance {name:?} must be stopped or hibernated before sealing"
                )
                .into());
            }
            if manifest.default_user.is_some() {
                return Err("seal a pristine instance before permanent user registration".into());
            }
            println!("Registering the private identity disk before credential rotation...");
            manager.stage_instance_identity(name)?;
            connect_agent(store, name)?.probe()?;
            request_graceful_stop_and_wait(store, name, Duration::from_secs(5 * 60))?;
            println!("Retiring the installed instance token before capturing the shared base...");
            manager.rotate_instance_identity(name)?;
            connect_agent(store, name)?.probe()?;
            request_graceful_stop_and_wait(store, name, Duration::from_secs(5 * 60))?;
            let mut manifest = store.load(name)?;
            let image = manager.seal(&manifest, &agent)?;
            manager.rotate_instance_identity(name)?;
            manifest.base_image_key = Some(image.key.clone());
            store.update(&manifest)?;
            println!("Sealed base image for {name:?}.");
            println!("IMAGE={}", image.key);
            println!("DISK={}", image.disk.display());
        }
        "verify" if arguments.len() == 2 => {
            let key = arguments[1]
                .to_str()
                .ok_or("sealed image key must be valid UTF-8")?;
            let image = manager.verify(key)?;
            println!("Verified sealed image {}.", image.key);
            println!("BASE_SHA256={}", image.base_disk_sha256);
        }
        _ => return Err("usage: lsw image <list|seal NAME|verify KEY>".into()),
    }
    Ok(())
}

pub(super) fn request_graceful_stop_and_wait(
    store: &StateStore,
    name: &str,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    DaemonClient::new(store).request_checked(&format!("STOP {name} graceful"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match store.load(name)?.state {
            InstanceState::Stopped => return Ok(()),
            InstanceState::Failed => {
                return Err(
                    format!("instance {name:?} failed while preparing its sealed image").into(),
                )
            }
            _ if Instant::now() >= deadline => {
                return Err(format!("timed out waiting for {name:?} to stop before sealing").into())
            }
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
}

pub(super) fn clone_instance(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let (source, target) = match arguments {
        [source, target] => (
            source.to_str().ok_or("source name must be valid UTF-8")?,
            target.to_str().ok_or("target name must be valid UTF-8")?,
        ),
        _ => return Err("usage: lsw clone SOURCE NAME".into()),
    };
    let capabilities = HostCapabilities::detect();
    let directory = ImageManager::new(store, &capabilities).clone_instance(source, target)?;
    if store.default_name()?.is_none() {
        store.set_default(target)?;
    }
    println!("Created linked clone {target:?} from {source:?}.");
    println!("INSTANCE={}", directory.display());
    println!("Run `lsw start {target}` to boot it with its private identity.");
    Ok(())
}

pub(super) fn seed(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn doctor(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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
            println!("All required LSW host dependencies are already installed.\n");
        } else {
            let mut requested = missing;
            if install_aria2 {
                requested.push("aria2c (optional download accelerator)");
            }
            println!("Installing LSW host dependencies: {}", requested.join(", "));
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

pub(super) fn print_host_report(store: &StateStore, capabilities: &HostCapabilities) {
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
    println!("  smbd:        {}", display_optional(&capabilities.smbd));
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

pub(super) fn fix_host_dependencies(needs_viewer: bool) -> Result<(), Box<dyn std::error::Error>> {
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
            "samba",
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
            "samba",
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
            "samba",
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
            "samba",
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
            "automatic dependency repair does not support distribution {distribution:?}; install QEMU, qemu-img, OVMF, swtpm, Samba (smbd), util-linux, aria2, wimlib-imagex, xorriso, and 7z manually{}",
            if needs_viewer { ", plus remote-viewer" } else { "" }
        )
        .into());
    }
    Ok(())
}

pub(super) fn command_prefix() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let output = Command::new("id").arg("-u").output()?;
    if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "0" {
        Ok(Vec::new())
    } else {
        Ok(vec!["sudo".to_owned()])
    }
}

pub(super) fn run_package_command(
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

pub(super) fn inspect_pe(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn print_pe_report(path: &Path, image: &PeImage, show_imports: bool) {
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

pub(super) fn pe_json(path: &Path, image: &PeImage) -> String {
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

pub(super) fn push_json_field(output: &mut String, name: &str, value: &str, comma: bool) {
    if comma {
        output.push(',');
    }
    push_json_string(output, name);
    output.push(':');
    push_json_string(output, value);
}

pub(super) fn push_json_string(output: &mut String, value: &str) {
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

pub(super) fn create(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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
    let capabilities = HostCapabilities::detect();
    ImageManager::new(store, &capabilities).stage_instance_identity(&manifest.spec.name)?;
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

pub(super) fn select_default(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let name = required_exact_name(arguments, "use")?;
    store.set_default(name)?;
    println!("Default LSW instance is now {name:?}.");
    Ok(())
}

pub(super) fn daemon(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments
        .first()
        .and_then(|value| value.to_str())
        .unwrap_or("status");
    if arguments.len() > 1 {
        return Err("usage: lsw daemon [enable|disable|start|status|diagnose]".into());
    }
    let client = DaemonClient::new(store);
    match action {
        "start" => {
            client.ensure_running()?;
            println!("lswd is ready at {}", client.socket().display());
        }
        "enable" => {
            run_systemctl_user(&["enable", "--now", "lswd.socket"])?;
            println!("Enabled optional LSW socket activation for this user.");
        }
        "disable" => {
            run_systemctl_user(&["disable", "--now", "lswd.socket", "lswd.service"])?;
            println!("Disabled LSW background socket activation for this user.");
        }
        "diagnose" => {
            print_systemd_state();
            match client.request("PING") {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                }
                Err(error) => println!("DAEMON=unavailable ({error})"),
            }
            println!("Idle daemon RSS gate: below 30720 KiB.");
            println!("With only lswd.socket waiting, lswd.service must be inactive.");
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
        _ => return Err("usage: lsw daemon [enable|disable|start|status|diagnose]".into()),
    }
    Ok(())
}

pub(super) fn run_systemctl_user(arguments: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl --user {} failed with {status}",
            arguments.join(" ")
        )
        .into())
    }
}

pub(super) fn print_systemd_state() {
    for unit in ["lswd.socket", "lswd.service"] {
        for property in ["is-enabled", "is-active"] {
            let output = Command::new("systemctl")
                .args(["--user", property, unit])
                .output();
            let value = output
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unavailable".to_owned());
            println!("SYSTEMD_{unit}_{property}={value}");
        }
    }
}
