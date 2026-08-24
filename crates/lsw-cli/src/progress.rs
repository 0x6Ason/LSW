// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgressEvent {
    pub step: u8,
    pub total_steps: u8,
    pub label: String,
    pub detail: String,
    pub completed: Option<u64>,
    pub total: Option<u64>,
}

impl ProgressEvent {
    pub fn stage(
        step: u8,
        total_steps: u8,
        label: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            step,
            total_steps,
            label: label.into(),
            detail: detail.into(),
            completed: None,
            total: None,
        }
    }

    pub fn measured(
        step: u8,
        total_steps: u8,
        label: impl Into<String>,
        detail: impl Into<String>,
        completed: u64,
        total: u64,
    ) -> Self {
        Self {
            step,
            total_steps,
            label: label.into(),
            detail: detail.into(),
            completed: Some(completed),
            total: Some(total),
        }
    }

    fn key(&self) -> (u8, &str, &str) {
        (self.step, &self.label, &self.detail)
    }

    fn percent(&self) -> Option<u8> {
        let (completed, total) = (self.completed?, self.total?);
        if total == 0 {
            return None;
        }
        Some(((completed.min(total) as u128 * 100) / total as u128) as u8)
    }
}

pub struct ProgressRenderer {
    destination: ProgressDestination,
    terminal: bool,
    current_step: Option<u8>,
    current_label: String,
    current_detail: String,
    stage_started: Instant,
    last_rendered: Instant,
    last_log_bucket: Option<u8>,
    line_open: bool,
    spinner_frame: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProgressDestination {
    Stdout,
    Stderr,
}

impl ProgressRenderer {
    pub fn new() -> Self {
        Self::with_destination(ProgressDestination::Stdout)
    }

    pub fn stderr() -> Self {
        Self::with_destination(ProgressDestination::Stderr)
    }

    fn with_destination(destination: ProgressDestination) -> Self {
        let now = Instant::now();
        Self {
            destination,
            terminal: match destination {
                ProgressDestination::Stdout => io::stdout().is_terminal(),
                ProgressDestination::Stderr => io::stderr().is_terminal(),
            },
            current_step: None,
            current_label: String::new(),
            current_detail: String::new(),
            stage_started: now,
            last_rendered: now.checked_sub(Duration::from_secs(1)).unwrap_or(now),
            last_log_bucket: None,
            line_open: false,
            spinner_frame: 0,
        }
    }

    fn print(&self, value: &str) {
        match self.destination {
            ProgressDestination::Stdout => {
                print!("{value}");
                let _ = io::stdout().flush();
            }
            ProgressDestination::Stderr => {
                eprint!("{value}");
                let _ = io::stderr().flush();
            }
        }
    }

    fn println(&self, value: &str) {
        match self.destination {
            ProgressDestination::Stdout => println!("{value}"),
            ProgressDestination::Stderr => eprintln!("{value}"),
        }
    }

    pub fn update(&mut self, event: ProgressEvent) {
        let changed = self.current_step.is_none()
            || event.key()
                != (
                    self.current_step.unwrap_or_default(),
                    self.current_label.as_str(),
                    self.current_detail.as_str(),
                );
        if changed {
            self.close_line();
            self.current_step = Some(event.step);
            self.current_label.clone_from(&event.label);
            self.current_detail.clone_from(&event.detail);
            self.stage_started = Instant::now();
            self.last_log_bucket = None;
            self.spinner_frame = 0;
        }

        let percent = event.percent();
        if self.terminal {
            if !changed && self.last_rendered.elapsed() < Duration::from_secs(1) {
                return;
            }
            let line =
                format_progress_line(&event, self.stage_started.elapsed(), self.spinner_frame);
            self.print(&format!("\r\x1b[2K{line}"));
            self.line_open = true;
            self.last_rendered = Instant::now();
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
        } else {
            let bucket = percent.map(|percent| percent / 10);
            if changed || percent == Some(100) || bucket != self.last_log_bucket {
                self.println(&format_progress_line(
                    &event,
                    self.stage_started.elapsed(),
                    0,
                ));
                self.last_log_bucket = bucket;
            }
        }
    }

    pub fn finish(&mut self) {
        self.close_line();
        self.current_step = None;
        self.current_label.clear();
        self.current_detail.clear();
    }

    fn close_line(&mut self) {
        if self.terminal && self.line_open {
            self.println("");
            self.line_open = false;
        }
    }
}

impl Drop for ProgressRenderer {
    fn drop(&mut self) {
        self.close_line();
    }
}

fn format_progress_line(event: &ProgressEvent, elapsed: Duration, spinner_frame: usize) -> String {
    let prefix = format!("[{}/{}] {}", event.step, event.total_steps, event.label);
    let elapsed = format_duration(elapsed);
    let suffix = match event.percent() {
        Some(percent) => {
            let filled = usize::from(percent) * 8 / 100;
            let bar = format!("{}{}", "#".repeat(filled), "-".repeat(8 - filled));
            let bytes = match (event.completed, event.total) {
                (Some(completed), Some(total)) if total >= 1024 => {
                    format!("  {}", format_byte_pair(completed, total))
                }
                _ => String::new(),
            };
            format!(" [{bar}] {percent:>3}%{bytes} {elapsed}")
        }
        None => {
            let spinner = ['|', '/', '-', '\\'][spinner_frame % 4];
            format!(" {spinner} {elapsed}")
        }
    };
    format_bounded_line(&prefix, &event.detail, &suffix)
}

fn format_bounded_line(prefix: &str, detail: &str, suffix: &str) -> String {
    const MAX_COLUMNS: usize = 78;

    if detail.is_empty() {
        return format!("{prefix}{suffix}");
    }

    let fixed_columns = prefix.chars().count() + suffix.chars().count() + 2;
    let detail_columns = MAX_COLUMNS.saturating_sub(fixed_columns);
    if detail_columns == 0 {
        return format!("{prefix}{suffix}");
    }

    let detail = truncate_with_ellipsis(detail, detail_columns);
    format!("{prefix}: {detail}{suffix}")
}

fn truncate_with_ellipsis(value: &str, max_columns: usize) -> String {
    if value.chars().count() <= max_columns {
        return value.to_owned();
    }
    if max_columns <= 3 {
        return value.chars().take(max_columns).collect();
    }
    format!(
        "{}...",
        value.chars().take(max_columns - 3).collect::<String>()
    )
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

fn format_byte_pair(completed: u64, total: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if total >= GIB {
        format!(
            "{:.1}/{:.1} GiB",
            completed as f64 / GIB as f64,
            total as f64 / GIB as f64
        )
    } else {
        format!(
            "{:.0}/{:.0} MiB",
            completed as f64 / MIB as f64,
            total as f64 / MIB as f64
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_progress_can_be_routed_to_stderr() {
        let progress = ProgressRenderer::stderr();
        assert_eq!(progress.destination, ProgressDestination::Stderr);
    }

    #[test]
    fn measured_progress_is_bounded_and_human_readable() {
        let line = format_progress_line(
            &ProgressEvent::measured(
                2,
                8,
                "Downloading Windows ISO",
                "Microsoft CDN",
                3 * 1024 * 1024 * 1024,
                6 * 1024 * 1024 * 1024,
            ),
            Duration::from_secs(63),
            0,
        );
        assert!(line.contains("[2/8] Downloading Windows ISO"));
        assert!(line.contains(" 50%"));
        assert!(line.contains("3.0/6.0 GiB"));
        assert!(line.ends_with("01:03"));
    }

    #[test]
    fn indeterminate_progress_uses_a_spinner_without_a_fake_percentage() {
        let line = format_progress_line(
            &ProgressEvent::stage(7, 8, "Completing Windows setup", "specialize"),
            Duration::from_secs(5),
            1,
        );
        assert!(line.contains("specialize / 00:05"));
        assert!(!line.contains('%'));
    }

    #[test]
    fn long_stage_details_do_not_wrap_an_eighty_column_terminal() {
        let line = format_progress_line(
            &ProgressEvent::measured(
                6,
                8,
                "Applying Windows image",
                "applying Windows to target disk",
                39,
                100,
            ),
            Duration::from_secs(161),
            0,
        );
        assert!(line.chars().count() <= 78, "{line}");
        assert!(line.contains(" 39%"));
        assert!(line.ends_with("02:41"));
    }
}
