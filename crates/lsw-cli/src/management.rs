// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) fn media(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let action = arguments.first().and_then(|value| value.to_str()).ok_or(
        "usage: lsw media <resolve|published-sha256> [--language LANGUAGE] [--request-file PATH]",
    )?;
    if action != "resolve" && action != "published-sha256" {
        return Err("usage: lsw media <resolve|published-sha256> [--language LANGUAGE] [--request-file PATH]".into());
    }
    let mut language = "English";
    let mut language_seen = false;
    let mut request_file = None;
    let mut index = 1;
    while index < arguments.len() {
        let option = arguments[index]
            .to_str()
            .ok_or("media options must be valid UTF-8")?;
        match option {
            "--language" if index + 1 < arguments.len() => {
                if language_seen {
                    return Err("--language was supplied more than once".into());
                }
                language_seen = true;
                index += 1;
                language = arguments[index]
                    .to_str()
                    .ok_or("language must be valid UTF-8")?;
            }
            "--request-file" if action == "resolve" && index + 1 < arguments.len() => {
                index += 1;
                let path = PathBuf::from(&arguments[index]);
                if request_file.replace(path).is_some() {
                    return Err("--request-file was supplied more than once".into());
                }
            }
            _ => {
                return Err("usage: lsw media <resolve|published-sha256> [--language LANGUAGE] [--request-file PATH]".into());
            }
        }
        index += 1;
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
    if let Some(path) = request_file {
        write_media_request(
            &path,
            resolved.download_url.expose(),
            &resolved.expected_sha256,
            &resolved.filename,
        )?;
        println!("REQUEST_FILE={}", path.display());
    }
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

pub(super) fn write_media_request(
    path: &Path,
    url: &str,
    sha256: &str,
    filename: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if !path.is_absolute() {
        return Err("media request file path must be absolute".into());
    }
    let parent = path.parent().ok_or("media request file has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    if !parent_metadata.file_type().is_dir() || parent_metadata.file_type().is_symlink() {
        return Err("media request file parent must be a real directory".into());
    }
    if [url, sha256, filename]
        .iter()
        .any(|value| value.is_empty() || value.contains(['\r', '\n']))
    {
        return Err("media request contains an invalid field".into());
    }
    let contents = format!(
        "version=1\nurl={url}\nsha256={}\nfilename={filename}\n",
        sha256.to_ascii_lowercase()
    );
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    if let Err(error) = (|| -> std::io::Result<()> {
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    })() {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

pub(super) fn list(store: &StateStore) -> Result<(), LswError> {
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

pub(super) fn config(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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
            println!("memory.min={}", format_memory(manifest.memory_min_mib));
            println!("disk.max={}GiB", manifest.spec.disk_gib);
            println!("network={}", manifest.spec.network);
            println!(
                "idle-timeout={}",
                format_duration(manifest.idle_timeout_seconds)
            );
            println!("idle-policy={}", manifest.idle_policy);
            println!(
                "hibernate-timeout={}",
                format_duration(manifest.hibernate_timeout_seconds)
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
                    "memory.min" => manifest.memory_min_mib = parse_memory_mib(value)?,
                    "idle-timeout" => {
                        manifest.idle_timeout_seconds = parse_duration_seconds(value)?
                    }
                    "hibernate-timeout" => {
                        manifest.hibernate_timeout_seconds = parse_duration_seconds(value)?
                    }
                    "idle-policy" => manifest.idle_policy = value.parse::<IdlePolicy>()?,
                    _ => {
                        return Err(format!(
                            "unknown configuration key {key:?}; supported keys are memory.max, memory.min, idle-timeout, hibernate-timeout, and idle-policy"
                        )
                        .into())
                    }
                }
            }
            store.update(&manifest)?;
            println!("Updated configuration for {name:?}.");
            println!("memory.max={}", format_memory(manifest.spec.memory_mib));
            println!("memory.min={}", format_memory(manifest.memory_min_mib));
            println!(
                "idle-timeout={}",
                format_duration(manifest.idle_timeout_seconds)
            );
            println!("idle-policy={}", manifest.idle_policy);
            println!(
                "hibernate-timeout={}",
                format_duration(manifest.hibernate_timeout_seconds)
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

pub(super) fn parse_memory_mib(value: &str) -> Result<u32, Box<dyn std::error::Error>> {
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

pub(super) fn format_memory(memory_mib: u32) -> String {
    if memory_mib % 1024 == 0 {
        format!("{}GiB", memory_mib / 1024)
    } else {
        format!("{memory_mib}MiB")
    }
}

pub(super) fn parse_duration_seconds(value: &str) -> Result<u64, Box<dyn std::error::Error>> {
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

pub(super) fn format_duration(seconds: u64) -> String {
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

pub(super) fn logs(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn read_log_tail(
    path: &Path,
    lines: usize,
) -> Result<(String, u64), Box<dyn std::error::Error>> {
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

pub(super) fn remove_instance(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let name = required_exact_name(arguments, "remove")?;
    store.remove(name)?;
    println!("Removed instance {name:?} and its local virtual disk.");
    Ok(())
}

pub(super) fn shutdown(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn diagnose(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
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

pub(super) fn diagnostic_host_text(_store: &StateStore, capabilities: &HostCapabilities) -> String {
    let backend = QemuBackend::select(capabilities);
    format!(
        concat!(
            "lsw_version={}\nstate_root={}\nplatform={}\naccelerator={}\n",
            "kvm={}\nqemu={}\nqemu_img={}\nsetsid={}\nswtpm={}\nsmbd={}\naria2c={}\nwimlib={}\nxorriso={}\nseven_zip={}\nviewer={}\n"
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
        display_optional(&capabilities.smbd),
        display_optional(&capabilities.aria2c),
        display_optional(&capabilities.wimlib_imagex),
        display_optional(&capabilities.xorriso),
        display_optional(&capabilities.seven_zip),
        display_optional(&capabilities.remote_viewer),
    )
}

pub(super) fn copy_diagnostic_tail(
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
