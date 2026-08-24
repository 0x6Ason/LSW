// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use lsw_core::{FolderShareTransport, ProcessEnvironment, SessionKind, StartRequest, StateStore};

use super::{connect_agent, resolve_name, transfer};

const DEFAULT_SIZE_MIB: u32 = 1024;
const DEFAULT_SMALL_FILES: u32 = 4096;
const OUTPUT_LIMIT: usize = 64 * 1024;
const GUEST_LOCAL_ROOT: &str = "C:\\ProgramData\\LSW\\file-bench-local";
const MIRROR_ROOT: &str = "C:\\ProgramData\\LSW\\file-bench-mirror";
const BENCHMARK_SCRIPT: &str = r#"
$ErrorActionPreference='Stop'
$Root=[IO.Path]::GetFullPath($env:LSW_BENCH_ROOT)
$SizeMiB=[int]$env:LSW_BENCH_SIZE_MIB
$SmallFiles=[int]$env:LSW_BENCH_SMALL_FILES
if ($SizeMiB -lt 1 -or $SmallFiles -lt 1) { throw 'invalid benchmark dimensions' }
Remove-Item -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $Root | Out-Null
try {
    $Payload=Join-Path $Root 'sequential.bin'
    $Buffer=New-Object byte[] (1MB)
    $Watch=[Diagnostics.Stopwatch]::StartNew()
    $Stream=[IO.File]::Open($Payload,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
    try {
        for ($Index=0; $Index -lt $SizeMiB; $Index++) { $Stream.Write($Buffer,0,$Buffer.Length) }
        $Stream.Flush($true)
    } finally { $Stream.Dispose() }
    $Watch.Stop()
    [Console]::Out.WriteLine('SEQ_WRITE_MS='+$Watch.ElapsedMilliseconds)

    $Watch.Restart()
    $Stream=[IO.File]::OpenRead($Payload)
    try { while ($Stream.Read($Buffer,0,$Buffer.Length) -gt 0) {} } finally { $Stream.Dispose() }
    $Watch.Stop()
    [Console]::Out.WriteLine('SEQ_READ_MS='+$Watch.ElapsedMilliseconds)

    $Tree=Join-Path $Root 'tree'
    New-Item -ItemType Directory -Path $Tree | Out-Null
    $Watch.Restart()
    for ($Index=0; $Index -lt $SmallFiles; $Index++) {
        $Bucket=Join-Path $Tree ('d'+($Index % 64).ToString('D2'))
        if (-not [IO.Directory]::Exists($Bucket)) { [IO.Directory]::CreateDirectory($Bucket) | Out-Null }
        [IO.File]::WriteAllText((Join-Path $Bucket ('f'+$Index.ToString('D6')+'.txt')),('lsw-'+$Index))
    }
    $Watch.Stop()
    [Console]::Out.WriteLine('TREE_CREATE_MS='+$Watch.ElapsedMilliseconds)

    $Watch.Restart()
    $Count=(Get-ChildItem -LiteralPath $Tree -File -Recurse -Force | Measure-Object).Count
    $Watch.Stop()
    if ($Count -ne $SmallFiles) { throw 'metadata walk returned the wrong file count' }
    [Console]::Out.WriteLine('METADATA_WALK_MS='+$Watch.ElapsedMilliseconds)

    $Watch.Restart()
    $Hash=[Security.Cryptography.SHA256]::Create()
    try {
        foreach ($File in Get-ChildItem -LiteralPath $Tree -File -Recurse -Force) {
            $Bytes=[IO.File]::ReadAllBytes($File.FullName)
            $null=$Hash.ComputeHash($Bytes)
        }
    } finally { $Hash.Dispose() }
    $Watch.Stop()
    [Console]::Out.WriteLine('SYNTHETIC_BUILD_MS='+$Watch.ElapsedMilliseconds)

    $Git=Get-Command git.exe -ErrorAction SilentlyContinue
    if ($null -eq $Git) {
        [Console]::Out.WriteLine('GIT_STATUS_MS=unavailable')
    } else {
        & $Git.Source -C $Root init --quiet
        $Watch.Restart()
        & $Git.Source -C $Root status --porcelain --untracked-files=all | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'git status failed' }
        $Watch.Stop()
        [Console]::Out.WriteLine('GIT_STATUS_MS='+$Watch.ElapsedMilliseconds)
    }
} finally {
    Remove-Item -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue
}
"#;

#[derive(Clone, Debug)]
struct TargetResult {
    name: &'static str,
    available: bool,
    values: BTreeMap<String, Option<u64>>,
}

pub(super) fn command(
    store: &StateStore,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut json = false;
    let mut size_mib = DEFAULT_SIZE_MIB;
    let mut small_files = DEFAULT_SMALL_FILES;
    let mut requested = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index]
            .to_str()
            .ok_or("file benchmark arguments must be valid UTF-8")?;
        match argument {
            "--json" => json = true,
            "--size-mib" => {
                index += 1;
                size_mib = parse_dimension(arguments.get(index), argument, 1, 4096)?;
            }
            "--small-files" => {
                index += 1;
                small_files = parse_dimension(arguments.get(index), argument, 1, 100_000)?;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown file benchmark option {value:?}").into())
            }
            name => {
                if requested.replace(name.to_owned()).is_some() {
                    return Err(usage().into());
                }
            }
        }
        index += 1;
    }
    let name = resolve_name(store, requested.as_deref())?;
    let manifest = store.load(&name)?;
    let guest_local = benchmark_windows_target(
        store,
        &name,
        "guest-local",
        GUEST_LOCAL_ROOT,
        size_mib,
        small_files,
    )?;
    let live = if manifest
        .folder_shares
        .iter()
        .any(|share| share.transport == FolderShareTransport::LiveSmb)
        && connect_agent(store, &name)?.live_share_status()?.mapped
    {
        benchmark_windows_target(
            store,
            &name,
            "live-smb",
            "L:\\.lsw-file-bench",
            size_mib,
            small_files,
        )?
    } else {
        TargetResult {
            name: "live-smb",
            available: false,
            values: BTreeMap::new(),
        }
    };
    let mirror_sync_ms = benchmark_agent_mirror(store, &name, size_mib, small_files)?;

    if json {
        println!(
            "{}",
            render_json(
                &name,
                size_mib,
                small_files,
                &guest_local,
                &live,
                mirror_sync_ms
            )
        );
    } else {
        println!("LSW file benchmark for {name:?}");
        println!("  dataset: {size_mib} MiB sequential file, {small_files} small files");
        print_target(&guest_local);
        print_target(&live);
        println!("  agent-mirror dataset sync: {mirror_sync_ms} ms");
        println!("Use --json to save the machine-readable result.");
    }
    Ok(())
}

fn benchmark_windows_target(
    store: &StateStore,
    instance: &str,
    target: &'static str,
    root: &str,
    size_mib: u32,
    small_files: u32,
) -> Result<TargetResult, Box<dyn std::error::Error>> {
    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: vec![
            "powershell.exe".to_owned(),
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            BENCHMARK_SCRIPT.to_owned(),
        ],
        working_directory: None,
    };
    let environment = ProcessEnvironment::new(vec![
        ("LSW_BENCH_ROOT".to_owned(), root.to_owned()),
        ("LSW_BENCH_SIZE_MIB".to_owned(), size_mib.to_string()),
        ("LSW_BENCH_SMALL_FILES".to_owned(), small_files.to_string()),
    ])?;
    let process = connect_agent(store, instance)?.run_capture_with_environment(
        &request,
        &[],
        OUTPUT_LIMIT,
        &environment,
    )?;
    if process.exit_code != 0 {
        return Err(format!(
            "{target} benchmark failed with {}: {}",
            process.exit_code,
            String::from_utf8_lossy(&process.stderr).trim()
        )
        .into());
    }
    Ok(TargetResult {
        name: target,
        available: true,
        values: parse_metrics(&process.stdout)?,
    })
}

fn benchmark_agent_mirror(
    store: &StateStore,
    name: &str,
    size_mib: u32,
    small_files: u32,
) -> Result<u64, Box<dyn std::error::Error>> {
    remove_remote_benchmark_root(store, name)?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root = std::env::temp_dir().join(format!("lsw-file-bench-{nonce}"));
    let result = (|| {
        fs::create_dir(&root)?;
        let payload = fs::File::create(root.join("sequential.bin"))?;
        payload.set_len(u64::from(size_mib) * 1024 * 1024)?;
        let tree = root.join("tree");
        fs::create_dir(&tree)?;
        for index in 0..small_files {
            let bucket = tree.join(format!("d{:02}", index % 64));
            fs::create_dir_all(&bucket)?;
            let mut file = fs::File::create(bucket.join(format!("f{index:06}.txt")))?;
            write!(file, "lsw-{index}")?;
        }
        let started = Instant::now();
        transfer::sync_host_to_guest_silent(store, name, &root, MIRROR_ROOT)?;
        Ok(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
    })();
    let local_cleanup = fs::remove_dir_all(&root);
    let remote_cleanup = remove_remote_benchmark_root(store, name);
    match (result, local_cleanup, remote_cleanup) {
        (Ok(value), Ok(()), Ok(())) => Ok(value),
        (Err(error), _, _) => Err(error),
        (_, Err(error), _) => Err(error.into()),
        (_, _, Err(error)) => Err(error),
    }
}

fn remove_remote_benchmark_root(
    store: &StateStore,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: vec![
            "powershell.exe".to_owned(),
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "Remove-Item -LiteralPath 'C:\\ProgramData\\LSW\\file-bench-mirror' -Recurse -Force -ErrorAction SilentlyContinue".to_owned(),
        ],
        working_directory: None,
    };
    let process = connect_agent(store, name)?.run_capture(&request, &[], OUTPUT_LIMIT)?;
    if process.exit_code == 0 {
        Ok(())
    } else {
        Err("could not clean the guest mirror benchmark directory".into())
    }
}

fn parse_metrics(
    output: &[u8],
) -> Result<BTreeMap<String, Option<u64>>, Box<dyn std::error::Error>> {
    let output = std::str::from_utf8(output)?;
    let mut values = BTreeMap::new();
    for line in output.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or("benchmark returned a malformed metric")?;
        let value = if value == "unavailable" {
            None
        } else {
            Some(value.parse()?)
        };
        if values.insert(key.to_owned(), value).is_some() {
            return Err("benchmark returned a duplicate metric".into());
        }
    }
    for required in [
        "SEQ_WRITE_MS",
        "SEQ_READ_MS",
        "TREE_CREATE_MS",
        "METADATA_WALK_MS",
        "SYNTHETIC_BUILD_MS",
        "GIT_STATUS_MS",
    ] {
        if !values.contains_key(required) {
            return Err(format!("benchmark omitted {required}").into());
        }
    }
    Ok(values)
}

fn print_target(result: &TargetResult) {
    if !result.available {
        println!("  {}: unavailable (no mounted live share)", result.name);
        return;
    }
    println!("  {}:", result.name);
    for (key, value) in &result.values {
        match value {
            Some(value) => println!("    {}: {value} ms", key.to_ascii_lowercase()),
            None => println!("    {}: unavailable", key.to_ascii_lowercase()),
        }
    }
}

fn render_json(
    instance: &str,
    size_mib: u32,
    small_files: u32,
    guest_local: &TargetResult,
    live: &TargetResult,
    mirror_sync_ms: u64,
) -> String {
    format!(
        "{{\"schema\":1,\"instance\":{},\"dataset\":{{\"sequential_mib\":{size_mib},\"small_files\":{small_files}}},\"guest_local\":{},\"live_smb\":{},\"agent_mirror\":{{\"available\":true,\"dataset_sync_ms\":{mirror_sync_ms}}}}}",
        json_string(instance),
        target_json(guest_local),
        target_json(live),
    )
}

fn target_json(result: &TargetResult) -> String {
    let mut output = format!("{{\"available\":{}", result.available);
    for (key, value) in &result.values {
        output.push_str(&format!(",\"{}\":", key.to_ascii_lowercase()));
        match value {
            Some(value) => output.push_str(&value.to_string()),
            None => output.push_str("null"),
        }
    }
    output.push('}');
    output
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '\"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            character if character.is_control() => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => output.push(character),
        }
    }
    output.push('\"');
    output
}

fn parse_dimension(
    value: Option<&OsString>,
    option: &str,
    minimum: u32,
    maximum: u32,
) -> Result<u32, Box<dyn std::error::Error>> {
    let value = value
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{option} requires a value"))?
        .parse::<u32>()?;
    if !(minimum..=maximum).contains(&value) {
        return Err(format!("{option} must be between {minimum} and {maximum}").into());
    }
    Ok(value)
}

fn usage() -> &'static str {
    "usage: lsw bench files [NAME] [--json] [--size-mib N] [--small-files N]"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchmark_metrics_are_strict_and_machine_readable() {
        let values = parse_metrics(
            b"SEQ_WRITE_MS=1\nSEQ_READ_MS=2\nTREE_CREATE_MS=3\nMETADATA_WALK_MS=4\nSYNTHETIC_BUILD_MS=5\nGIT_STATUS_MS=unavailable\n",
        )
        .expect("metrics should parse");
        let target = TargetResult {
            name: "guest-local",
            available: true,
            values,
        };
        let json = render_json("windows", 1024, 4096, &target, &target, 9);
        assert!(json.contains("\"schema\":1"));
        assert!(json.contains("\"git_status_ms\":null"));
        assert!(json.contains("\"dataset_sync_ms\":9"));
    }
}
