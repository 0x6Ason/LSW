// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::control_media::prepare_control_media;
use super::io::{
    path_is_missing, require_regular_file, run_control_command, set_private_directory_permissions,
    set_private_file_permissions,
};
use super::{
    WinPeDismBackend, WinPeDismProgress, WinPeDismRunResult, WinPeDismVmPlan, MAX_DISM_LOG_BYTES,
    MAX_STATUS_LOG_BYTES, MIN_APPLIED_DISK_BYTES, WINPE_STATUS_TAG,
};
use crate::{HostCapabilities, LswError, Result};

impl WinPeDismBackend {
    /// Runs a WinPE phase and requires its exact private-volume completion marker.
    pub fn run_vm(plan: &WinPeDismVmPlan, timeout: Duration) -> Result<WinPeDismRunResult> {
        Self::run_vm_with_progress(plan, timeout, |_| {})
    }

    /// Runs a WinPE phase while reporting trusted guest stages and DISM percentages.
    pub fn run_vm_with_progress<F>(
        plan: &WinPeDismVmPlan,
        timeout: Duration,
        mut on_progress: F,
    ) -> Result<WinPeDismRunResult>
    where
        F: FnMut(&WinPeDismProgress),
    {
        if timeout.is_zero() {
            return Err(LswError::InvalidValue {
                field: "WinPE timeout",
                reason: "must be greater than zero".to_owned(),
            });
        }
        if !plan.missing_capabilities.is_empty() {
            return Err(LswError::MissingCapabilities(
                plan.missing_capabilities.clone(),
            ));
        }
        crate::Provisioner::new(HostCapabilities::unavailable(plan.backend.platform()))
            .apply(&plan.host_preparation)?;
        prepare_status_volume(&plan.status_log, &plan.dism_log)?;
        let uses_qmp = plan
            .invocation
            .arguments
            .iter()
            .any(|argument| argument.to_string_lossy() == "-qmp");
        if uses_qmp {
            prepare_control_media(&plan.control_media)?;
        }
        prepare_output_file(&plan.serial_log)?;
        let stdout = prepare_output_file(&plan.qemu_log)?;
        let stderr = stdout.try_clone()?;
        let started = Instant::now();
        let mut child = Command::new(&plan.invocation.program)
            .args(&plan.invocation.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()?;

        let mut observed_status_events = 0_usize;
        let mut current_stage = Some("starting-winpe".to_owned());
        let mut last_percent = None;
        on_progress(&WinPeDismProgress {
            phase: plan.phase,
            stage: "starting-winpe".to_owned(),
            percent: None,
            elapsed: started.elapsed(),
        });

        let status = loop {
            report_winpe_progress(
                plan,
                started,
                &mut observed_status_events,
                &mut current_stage,
                &mut last_percent,
                &mut on_progress,
            )?;
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(LswError::InvalidValue {
                    field: "WinPE DISM run",
                    reason: format!(
                        "{:?} phase exceeded {} seconds; inspect {}, {}, and {}",
                        plan.phase,
                        timeout.as_secs(),
                        plan.status_log.display(),
                        plan.serial_log.display(),
                        plan.qemu_log.display()
                    ),
                });
            }
            // The terminal renderer updates once per second. Matching that
            // cadence avoids repeatedly rereading a growing guest DISM log.
            thread::sleep(Duration::from_secs(1));
        };
        report_winpe_progress(
            plan,
            started,
            &mut observed_status_events,
            &mut current_stage,
            &mut last_percent,
            &mut on_progress,
        )?;
        if !status.success() {
            return Err(LswError::ExternalCommandFailed {
                program: PathBuf::from(&plan.invocation.program),
                status: status.code(),
            });
        }

        let status_events = read_status_events(&plan.status_log)?;
        if status_events
            .iter()
            .any(|event| event.contains(plan.phase.failure_marker()))
        {
            return Err(LswError::InvalidValue {
                field: "WinPE DISM run",
                reason: format!(
                    "{:?} phase reported failure; inspect {} and {}",
                    plan.phase,
                    plan.status_log.display(),
                    plan.qemu_log.display()
                ),
            });
        }
        if !status_events
            .iter()
            .any(|event| event.contains(plan.phase.completion_marker()))
        {
            return Err(LswError::InvalidValue {
                field: "WinPE DISM run",
                reason: format!(
                    "{:?} phase exited without completion marker {:?}; inspect {}",
                    plan.phase,
                    plan.phase.completion_marker(),
                    plan.status_log.display()
                ),
            });
        }
        if let Some(target_disk) = &plan.target_disk {
            validate_applied_target(&plan.qemu_img, target_disk)?;
        }
        Ok(WinPeDismRunResult {
            phase: plan.phase,
            elapsed: started.elapsed(),
            status_events,
        })
    }
}
fn prepare_output_file(path: &Path) -> Result<fs::File> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            return Err(LswError::InvalidValue {
                field: "WinPE log",
                reason: format!("{} is not a regular file", path.display()),
            })
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    set_private_file_permissions(path)?;
    Ok(file)
}

fn read_status_events(path: &Path) -> Result<Vec<String>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(LswError::InvalidValue {
            field: "WinPE status log",
            reason: format!("{} is not a regular file", path.display()),
        });
    }
    if metadata.len() > MAX_STATUS_LOG_BYTES {
        return Err(LswError::InvalidValue {
            field: "WinPE status log",
            reason: format!("{} exceeds 1 MiB", path.display()),
        });
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(path)?
        .take(MAX_STATUS_LOG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(|line| line.trim_matches(|character: char| character.is_control()))
        .filter(|line| line.contains("LSW-WINPE-DISM "))
        .map(str::to_owned)
        .collect())
}

fn report_winpe_progress<F>(
    plan: &WinPeDismVmPlan,
    started: Instant,
    observed_status_events: &mut usize,
    current_stage: &mut Option<String>,
    last_percent: &mut Option<u8>,
    on_progress: &mut F,
) -> Result<()>
where
    F: FnMut(&WinPeDismProgress),
{
    let mut emitted = false;
    let events = read_status_events(&plan.status_log)?;
    if events.len() < *observed_status_events {
        *observed_status_events = 0;
    }
    for event in events.iter().skip(*observed_status_events) {
        let Some(stage) = event.strip_prefix("LSW-WINPE-DISM ") else {
            continue;
        };
        let stage = stage.trim().to_owned();
        *current_stage = Some(stage.clone());
        *last_percent = None;
        on_progress(&WinPeDismProgress {
            phase: plan.phase,
            stage,
            percent: None,
            elapsed: started.elapsed(),
        });
        emitted = true;
    }
    *observed_status_events = events.len();

    if let Some((dism_stage, percent)) = read_dism_progress(&plan.dism_log)? {
        if current_stage.as_deref() == Some(dism_stage.as_str())
            && last_percent.as_ref() != Some(&percent)
        {
            *last_percent = Some(percent);
            on_progress(&WinPeDismProgress {
                phase: plan.phase,
                stage: dism_stage,
                percent: Some(percent),
                elapsed: started.elapsed(),
            });
            emitted = true;
        }
    }
    if !emitted {
        if let Some(stage) = current_stage.as_ref() {
            on_progress(&WinPeDismProgress {
                phase: plan.phase,
                stage: stage.clone(),
                percent: *last_percent,
                elapsed: started.elapsed(),
            });
        }
    }
    Ok(())
}

pub(super) fn read_dism_progress(path: &Path) -> Result<Option<(String, u8)>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(LswError::InvalidValue {
            field: "WinPE DISM log",
            reason: format!("{} is not a regular file", path.display()),
        });
    }
    if metadata.len() > MAX_DISM_LOG_BYTES {
        return Err(LswError::InvalidValue {
            field: "WinPE DISM log",
            reason: format!("{} exceeds 8 MiB", path.display()),
        });
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(path)?
        .take(MAX_DISM_LOG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let log = String::from_utf8_lossy(&bytes);
    let Some(stage_offset) = log.rfind("LSW-DISM-STAGE ") else {
        return Ok(None);
    };
    let stage_and_output = &log[stage_offset + "LSW-DISM-STAGE ".len()..];
    let Some(stage_end) = stage_and_output.find(['\r', '\n']) else {
        return Ok(None);
    };
    let stage = stage_and_output[..stage_end].trim();
    if stage.is_empty() {
        return Ok(None);
    }
    let output = &stage_and_output[stage_end..];
    let Some(command_offset) = output.find("LSW-DISM-COMMAND ") else {
        return Ok(None);
    };
    let command_output = &output[command_offset + "LSW-DISM-COMMAND ".len()..];
    let percent = command_output
        .match_indices('%')
        .filter_map(|(offset, _)| parse_percentage_before(&command_output[..offset]))
        .next_back();
    Ok(percent.map(|percent| (stage.to_owned(), percent)))
}

fn parse_percentage_before(value: &str) -> Option<u8> {
    let candidate = value
        .chars()
        .rev()
        .skip_while(|character| character.is_ascii_whitespace())
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let percent = candidate.parse::<f32>().ok()?;
    (percent.is_finite() && (0.0..=100.0).contains(&percent)).then_some(percent.floor() as u8)
}

fn validate_applied_target(qemu_img: &Path, target_disk: &Path) -> Result<()> {
    require_regular_file(target_disk, "applied target disk")?;
    let length = fs::metadata(target_disk)?.len();
    if length < MIN_APPLIED_DISK_BYTES {
        return Err(LswError::InvalidValue {
            field: "WinPE DISM apply",
            reason: format!(
                "{} is only {length} bytes after apply; refusing a false completion marker",
                target_disk.display()
            ),
        });
    }
    run_control_command(
        qemu_img,
        &["check".into(), target_disk.as_os_str().to_owned()],
        None,
    )
}

fn prepare_status_volume(status_log: &Path, dism_log: &Path) -> Result<()> {
    let directory = status_log.parent().ok_or_else(|| LswError::InvalidValue {
        field: "WinPE status volume",
        reason: "status log has no parent directory".to_owned(),
    })?;
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(LswError::InvalidValue {
                field: "WinPE status volume",
                reason: format!("{} is not a real directory", directory.display()),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(directory)?;
            set_private_directory_permissions(directory)?;
        }
        Err(error) => return Err(error.into()),
    }

    let tag = directory.join(WINPE_STATUS_TAG);
    if path_is_missing(&tag, "WinPE status tag")? {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tag)?;
        set_private_file_permissions(&tag)?;
        file.write_all(b"LSW private WinPE status volume\r\n")?;
        file.sync_all()?;
    } else {
        require_regular_file(&tag, "WinPE status tag")?;
    }
    prepare_output_file(status_log)?;
    prepare_output_file(dism_log)?;
    Ok(())
}
