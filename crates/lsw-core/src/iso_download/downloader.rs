// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Download implementation selected for an ISO transfer.
pub enum IsoDownloadEngine {
    /// External aria2c with at most four CDN connections.
    Aria2,
    /// Built-in four-range resumable HTTP downloader.
    Native,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// A measurable or indeterminate stage in the ISO download pipeline.
pub enum IsoDownloadProgressStage {
    /// Bytes are being transferred from Microsoft's CDN.
    Transferring,
    /// Native range files are being assembled into one ISO.
    Assembling,
    /// Downloaded or cached bytes are being hashed locally.
    Verifying,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Live progress from an official ISO download or cache verification.
pub struct IsoDownloadProgress {
    /// Operation currently being performed.
    pub stage: IsoDownloadProgressStage,
    /// Bytes completed when the selected engine can measure them faithfully.
    pub completed_bytes: Option<u64>,
    /// Total bytes expected when known.
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Verified result of an ISO download.
pub struct IsoDownloadReport {
    /// Final caller-visible ISO path.
    pub destination: PathBuf,
    /// Engine that transferred or verified the ISO.
    pub engine: IsoDownloadEngine,
    /// Final ISO size in bytes.
    pub bytes: u64,
    /// Lowercase SHA-256 calculated locally.
    pub sha256: String,
    /// Number of Microsoft URL resolutions used, including refreshes.
    pub resolutions: usize,
}

#[derive(Clone, Debug)]
/// Selects aria2c when available and otherwise performs native ranged downloads.
pub struct IsoDownloader {
    aria2c: Option<PathBuf>,
    agent: ureq::Agent,
}

impl IsoDownloader {
    /// Creates a downloader from detected host capabilities.
    pub fn new(capabilities: &HostCapabilities) -> Self {
        Self {
            aria2c: capabilities.aria2c.clone(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(20))
                .timeout_read(Duration::from_secs(60))
                .timeout_write(Duration::from_secs(60))
                .redirects(0)
                .build(),
        }
    }

    /// Reports the engine that the next transfer will use.
    pub fn engine(&self) -> IsoDownloadEngine {
        if self.aria2c.is_some() {
            IsoDownloadEngine::Aria2
        } else {
            IsoDownloadEngine::Native
        }
    }

    /// Resolves and downloads an ISO, verifying it before atomic promotion.
    pub fn download(
        &self,
        resolver: &MicrosoftIsoResolver,
        request: &MicrosoftIsoRequest,
        destination: &Path,
    ) -> Result<IsoDownloadReport> {
        let initial = resolver.resolve(request)?;
        self.download_resolved(resolver, request, initial, destination)
    }

    /// Downloads an already-resolved ISO and refreshes expired URLs as needed.
    ///
    /// A refresh must identify the same product, SKU, filename, and digest;
    /// otherwise partial bytes are never combined.
    pub fn download_resolved(
        &self,
        resolver: &MicrosoftIsoResolver,
        request: &MicrosoftIsoRequest,
        initial: ResolvedWindowsIso,
        destination: &Path,
    ) -> Result<IsoDownloadReport> {
        self.download_resolved_with_progress(resolver, request, initial, destination, |_| {})
    }

    /// Downloads an already-resolved ISO while reporting transfer and hash progress.
    pub fn download_resolved_with_progress<F>(
        &self,
        resolver: &MicrosoftIsoResolver,
        request: &MicrosoftIsoRequest,
        initial: ResolvedWindowsIso,
        destination: &Path,
        mut on_progress: F,
    ) -> Result<IsoDownloadReport>
    where
        F: FnMut(&IsoDownloadProgress),
    {
        let parent = destination.parent().ok_or_else(|| LswError::InvalidValue {
            field: "ISO destination",
            reason: "destination has no parent directory".to_owned(),
        })?;
        require_real_directory(parent, "ISO destination directory")?;
        validate_destination(destination)?;

        if let Some(report) =
            existing_verified_iso(destination, &initial, self.engine(), &mut on_progress)?
        {
            return Ok(report);
        }
        let temporary = download_temporary_path(destination)?;
        let engine = self.engine();
        let mut resolved = initial.clone();
        let mut last_error = None;

        for attempt in 0..MAX_DOWNLOAD_ATTEMPTS {
            if attempt > 0 {
                resolved = resolver.resolve(request)?;
                ensure_same_iso(&initial, &resolved)?;
            }
            let result = match &self.aria2c {
                Some(aria2c) => {
                    download_with_aria2(aria2c, &resolved, &temporary, &mut on_progress)
                }
                None => download_with_ranges(&self.agent, &resolved, &temporary, &mut on_progress),
            };
            match result {
                Ok(()) => {
                    let actual = sha256_file_with_progress(&temporary, &mut on_progress)?;
                    if !actual.eq_ignore_ascii_case(&resolved.expected_sha256) {
                        return Err(LswError::InvalidValue {
                            field: "Windows ISO SHA-256",
                            reason: format!(
                                "downloaded ISO hash {actual} does not match Microsoft's expected {}",
                                resolved.expected_sha256
                            ),
                        });
                    }
                    let bytes = fs::metadata(&temporary)?.len();
                    promote_download(&temporary, destination)?;
                    cleanup_download_sidecars(&temporary);
                    return Ok(IsoDownloadReport {
                        destination: destination.to_owned(),
                        engine,
                        bytes,
                        sha256: actual,
                        resolutions: attempt + 1,
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "download failed without an error".to_owned(),
        }))
    }
}

pub(super) fn download_with_aria2<F>(
    aria2c: &Path,
    resolved: &ResolvedWindowsIso,
    temporary: &Path,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(&IsoDownloadProgress),
{
    validate_microsoft_cdn_url(&resolved.download_url.0)?;
    let parent = temporary.parent().ok_or_else(|| LswError::InvalidValue {
        field: "ISO download",
        reason: "temporary destination has no parent".to_owned(),
    })?;
    let filename = temporary
        .file_name()
        .ok_or_else(|| LswError::InvalidValue {
            field: "ISO download",
            reason: "temporary destination has no filename".to_owned(),
        })?;
    let filename = filename.to_str().ok_or_else(|| LswError::InvalidValue {
        field: "ISO download",
        reason: "aria2 requires a UTF-8 destination filename".to_owned(),
    })?;
    if filename.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
        return Err(LswError::InvalidValue {
            field: "ISO download",
            reason: "aria2 destination filename contains a line break".to_owned(),
        });
    }
    validate_partial_file(temporary)?;

    let mut child = Command::new(aria2c)
        .arg("--input-file=-")
        .arg("--continue=true")
        .arg("--max-connection-per-server=4")
        .arg("--split=4")
        .arg("--min-split-size=16M")
        .arg("--max-tries=2")
        .arg("--retry-wait=2")
        .arg("--connect-timeout=20")
        .arg("--timeout=60")
        .arg("--file-allocation=none")
        .arg("--allow-overwrite=false")
        .arg("--auto-file-renaming=false")
        .arg("--console-log-level=warn")
        .arg("--summary-interval=0")
        .arg("--quiet=true")
        .current_dir(parent)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // Global --out is ignored when aria2 reads URIs from an input file.
        // Keep the signed URL off argv and provide the output name as a
        // per-URI option on the following indented line.
        stdin.write_all(resolved.download_url.expose().as_bytes())?;
        stdin.write_all(format!("\n  out={filename}\n").as_bytes())?;
    }
    on_progress(&IsoDownloadProgress {
        stage: IsoDownloadProgressStage::Transferring,
        completed_bytes: None,
        total_bytes: None,
    });
    let mut last_heartbeat = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(1) {
            on_progress(&IsoDownloadProgress {
                stage: IsoDownloadProgressStage::Transferring,
                completed_bytes: None,
                total_bytes: None,
            });
            last_heartbeat = Instant::now();
        }
        thread::sleep(Duration::from_millis(200));
    };
    if !status.success() {
        return Err(LswError::ExternalCommandFailed {
            program: aria2c.to_owned(),
            status: status.code(),
        });
    }
    require_regular_file(temporary, "aria2 ISO download")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ByteRange {
    pub(super) start: u64,
    pub(super) end: u64,
}

impl ByteRange {
    fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

pub(super) fn split_ranges(length: u64) -> Result<Vec<ByteRange>> {
    if length == 0 {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "Microsoft CDN reported an empty file".to_owned(),
        });
    }
    let connections = usize::try_from(length.min(MAX_CDN_CONNECTIONS as u64)).unwrap_or(1);
    let chunk = length.div_ceil(connections as u64);
    Ok((0..connections)
        .map(|index| {
            let start = index as u64 * chunk;
            ByteRange {
                start,
                end: (start + chunk - 1).min(length - 1),
            }
        })
        .filter(|range| range.start <= range.end)
        .collect())
}

fn download_with_ranges<F>(
    agent: &ureq::Agent,
    resolved: &ResolvedWindowsIso,
    temporary: &Path,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(&IsoDownloadProgress),
{
    validate_microsoft_cdn_url(&resolved.download_url.0)?;
    let length = cdn_content_length(agent, &resolved.download_url)?;
    let ranges = split_ranges(length)?;
    let part_paths = ranges
        .iter()
        .enumerate()
        .map(|(index, _)| sidecar_path(temporary, &format!("part{index}")))
        .collect::<Result<Vec<_>>>()?;

    let mut downloaded = part_paths
        .iter()
        .zip(&ranges)
        .map(|(path, range)| partial_file_length(path, range.len()))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum::<u64>();
    on_progress(&IsoDownloadProgress {
        stage: IsoDownloadProgressStage::Transferring,
        completed_bytes: Some(downloaded),
        total_bytes: Some(length),
    });

    thread::scope(|scope| -> Result<()> {
        let (progress_sender, progress_receiver) = mpsc::channel();
        let mut handles = Vec::new();
        for (range, path) in ranges.iter().copied().zip(part_paths.iter().cloned()) {
            let agent = agent.clone();
            let url = resolved.download_url.clone();
            let progress_sender = progress_sender.clone();
            handles.push(
                scope.spawn(move || download_range(&agent, &url, range, &path, &progress_sender)),
            );
        }
        drop(progress_sender);
        for bytes in progress_receiver {
            downloaded = downloaded.saturating_add(bytes).min(length);
            on_progress(&IsoDownloadProgress {
                stage: IsoDownloadProgressStage::Transferring,
                completed_bytes: Some(downloaded),
                total_bytes: Some(length),
            });
        }
        let mut first_error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Ok(Err(_)) => {}
                Err(_) if first_error.is_none() => {
                    first_error = Some(LswError::InvalidValue {
                        field: "Windows ISO download",
                        reason: "a range worker panicked".to_owned(),
                    })
                }
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    })?;

    let assembly = sidecar_path(temporary, "assembling")?;
    let mut output = create_private_truncated_file(&assembly)?;
    let mut assembled = 0_u64;
    on_progress(&IsoDownloadProgress {
        stage: IsoDownloadProgressStage::Assembling,
        completed_bytes: Some(0),
        total_bytes: Some(length),
    });
    let mut buffer = vec![0_u8; 1024 * 1024];
    for (path, range) in part_paths.iter().zip(&ranges) {
        require_regular_file(path, "ISO range part")?;
        let metadata = fs::metadata(path)?;
        if metadata.len() != range.len() {
            return Err(LswError::InvalidValue {
                field: "Windows ISO download",
                reason: "a completed range has the wrong size".to_owned(),
            });
        }
        let mut part = fs::File::open(path)?;
        loop {
            let bytes = part.read(&mut buffer)?;
            if bytes == 0 {
                break;
            }
            output.write_all(&buffer[..bytes])?;
            assembled += bytes as u64;
            on_progress(&IsoDownloadProgress {
                stage: IsoDownloadProgressStage::Assembling,
                completed_bytes: Some(assembled.min(length)),
                total_bytes: Some(length),
            });
        }
    }
    if assembled != length {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "assembled ISO has the wrong size".to_owned(),
        });
    }
    output.flush()?;
    output.sync_all()?;
    drop(output);
    if fs::symlink_metadata(temporary).is_ok() {
        validate_partial_file(temporary)?;
        fs::remove_file(temporary)?;
    }
    fs::rename(assembly, temporary)?;
    set_private_file_permissions(temporary)
}

fn cdn_content_length(agent: &ureq::Agent, url: &SecretDownloadUrl) -> Result<u64> {
    let response = agent
        .head(url.expose())
        .set("User-Agent", MICROSOFT_USER_AGENT)
        .call()
        .map_err(|error| download_http_error("checking ISO size", error))?;
    validate_final_cdn_response(&response)?;
    response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|length| *length > 0)
        .ok_or_else(|| LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "Microsoft CDN returned no valid Content-Length".to_owned(),
        })
}

fn download_range(
    agent: &ureq::Agent,
    url: &SecretDownloadUrl,
    range: ByteRange,
    path: &Path,
    progress_sender: &mpsc::Sender<u64>,
) -> Result<()> {
    let existing = partial_file_length(path, range.len())?;
    if existing == range.len() {
        return Ok(());
    }
    let start = range.start + existing;
    let response = agent
        .get(url.expose())
        .set("User-Agent", MICROSOFT_USER_AGENT)
        .set("Accept-Encoding", "identity")
        .set("Range", &format!("bytes={start}-{}", range.end))
        .call()
        .map_err(|error| download_http_error("downloading an ISO range", error))?;
    validate_final_cdn_response(&response)?;
    if response.status() != 206 {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: format!(
                "Microsoft CDN did not honor a Range request (HTTP {})",
                response.status()
            ),
        });
    }
    let expected_content_range = format!("bytes {start}-{}/", range.end);
    if !response
        .header("Content-Range")
        .is_some_and(|value| value.starts_with(&expected_content_range))
    {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: "Microsoft CDN returned an unexpected Content-Range".to_owned(),
        });
    }
    let remaining = range.len() - existing;
    let mut output = OpenOptions::new().create(true).append(true).open(path)?;
    set_private_file_permissions(path)?;
    let mut reader = response.into_reader().take(remaining + 1);
    let mut written = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let bytes = reader.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        output.write_all(&buffer[..bytes])?;
        written += bytes as u64;
        let _ = progress_sender.send(bytes as u64);
    }
    output.flush()?;
    output.sync_all()?;
    if written != remaining {
        return Err(LswError::InvalidValue {
            field: "Windows ISO download",
            reason: format!(
                "Microsoft CDN range ended early (expected {remaining} bytes, received {written})"
            ),
        });
    }
    Ok(())
}

fn validate_final_cdn_response(response: &ureq::Response) -> Result<()> {
    let url = Url::parse(response.get_url()).map_err(|_| LswError::InvalidValue {
        field: "Windows ISO download",
        reason: "Microsoft CDN returned an invalid final URL".to_owned(),
    })?;
    validate_microsoft_cdn_url(&url)
}

fn download_http_error(context: &'static str, error: ureq::Error) -> LswError {
    let reason = match error {
        ureq::Error::Status(status, _) => format!("{context}: CDN returned HTTP {status}"),
        ureq::Error::Transport(transport) => {
            format!("{context}: network transport {:?}", transport.kind())
        }
    };
    LswError::InvalidValue {
        field: "Windows ISO download",
        reason,
    }
}

pub(super) fn existing_verified_iso<F>(
    destination: &Path,
    resolved: &ResolvedWindowsIso,
    engine: IsoDownloadEngine,
    on_progress: &mut F,
) -> Result<Option<IsoDownloadReport>>
where
    F: FnMut(&IsoDownloadProgress),
{
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!("{} is not a regular file", destination.display()),
        });
    }
    let actual = sha256_file_with_progress(destination, on_progress)?;
    if !actual.eq_ignore_ascii_case(&resolved.expected_sha256) {
        return Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!(
                "{} already exists but its SHA-256 does not match Microsoft's expected hash",
                destination.display()
            ),
        });
    }
    Ok(Some(IsoDownloadReport {
        destination: destination.to_owned(),
        engine,
        bytes: metadata.len(),
        sha256: actual,
        resolutions: 1,
    }))
}

fn ensure_same_iso(initial: &ResolvedWindowsIso, refreshed: &ResolvedWindowsIso) -> Result<()> {
    if initial.product_id == refreshed.product_id
        && initial.sku_id == refreshed.sku_id
        && initial.filename == refreshed.filename
        && initial
            .expected_sha256
            .eq_ignore_ascii_case(&refreshed.expected_sha256)
    {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "Microsoft ISO refresh",
            reason: "Microsoft changed the selected ISO while refreshing an expired URL; refusing to combine downloads"
                .to_owned(),
        })
    }
}

/// Calculates a lowercase SHA-256 for a regular, non-symlink file.
pub fn sha256_file(path: &Path) -> Result<String> {
    sha256_file_with_progress(path, &mut |_| {})
}

pub(super) fn sha256_file_with_progress<F>(path: &Path, on_progress: &mut F) -> Result<String>
where
    F: FnMut(&IsoDownloadProgress),
{
    require_regular_file(path, "SHA-256 input")?;
    let total = fs::metadata(path)?.len();
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut completed = 0_u64;
    on_progress(&IsoDownloadProgress {
        stage: IsoDownloadProgressStage::Verifying,
        completed_bytes: Some(0),
        total_bytes: Some(total),
    });
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        digest.update(&buffer[..bytes]);
        completed += bytes as u64;
        on_progress(&IsoDownloadProgress {
            stage: IsoDownloadProgressStage::Verifying,
            completed_bytes: Some(completed),
            total_bytes: Some(total),
        });
    }
    Ok(hex_bytes(&digest.finalize()))
}

fn validate_destination(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!("{} must not be a symbolic link", path.display()),
        }),
        Ok(metadata) if !metadata.file_type().is_file() => Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!("{} is not a regular file", path.display()),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn promote_download(temporary: &Path, destination: &Path) -> Result<()> {
    require_regular_file(temporary, "completed ISO download")?;
    set_private_file_permissions(temporary)?;
    match fs::hard_link(temporary, destination) {
        Ok(()) => {
            fs::remove_file(temporary)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(LswError::InvalidValue {
            field: "ISO destination",
            reason: format!(
                "{} appeared while the ISO was downloading; refusing to overwrite it",
                destination.display()
            ),
        }),
        Err(error) => Err(error.into()),
    }
}

fn validate_partial_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(LswError::InvalidValue {
            field: "ISO partial download",
            reason: format!("{} is not a regular file", path.display()),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn partial_file_length(path: &Path, expected: u64) -> Result<u64> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= expected =>
        {
            Ok(metadata.len())
        }
        Ok(metadata) if metadata.file_type().is_symlink() => Err(LswError::InvalidValue {
            field: "ISO range part",
            reason: format!("{} must not be a symbolic link", path.display()),
        }),
        Ok(_) => Err(LswError::InvalidValue {
            field: "ISO range part",
            reason: format!("{} has an invalid type or size", path.display()),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn download_temporary_path(destination: &Path) -> Result<PathBuf> {
    let filename = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| LswError::InvalidValue {
            field: "ISO destination",
            reason: "filename must be valid UTF-8".to_owned(),
        })?;
    Ok(destination.with_file_name(format!(".{filename}.lsw-download")))
}

fn sidecar_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| LswError::InvalidValue {
            field: "ISO partial download",
            reason: "filename must be valid UTF-8".to_owned(),
        })?;
    Ok(path.with_file_name(format!("{filename}.{suffix}")))
}

fn cleanup_download_sidecars(temporary: &Path) {
    for index in 0..MAX_CDN_CONNECTIONS {
        if let Ok(path) = sidecar_path(temporary, &format!("part{index}")) {
            let _ = remove_private_regular_file(&path);
        }
    }
    for suffix in ["assembling", "aria2"] {
        if let Ok(path) = sidecar_path(temporary, suffix) {
            let _ = remove_private_regular_file(&path);
        }
    }
}

fn remove_private_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(LswError::InvalidValue {
            field: "ISO partial download",
            reason: format!("refusing to remove non-regular {}", path.display()),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn create_private_truncated_file(path: &Path) -> Result<fs::File> {
    validate_partial_file(path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    set_private_file_permissions(path)?;
    Ok(file)
}

fn require_real_directory(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a real directory", path.display()),
        })
    }
}

fn require_regular_file(path: &Path, field: &'static str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field,
            reason: format!("{} is not a regular file", path.display()),
        })
    }
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
