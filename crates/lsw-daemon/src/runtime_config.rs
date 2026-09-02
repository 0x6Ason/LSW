// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

pub(super) fn state_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(configured) = env::var_os("LSW_STATE_DIR") {
        return Ok(PathBuf::from(configured));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set; configure LSW_STATE_DIR")?;
    Ok(PathBuf::from(home).join(".local/share/lsw"))
}

pub(super) fn socket_path(store: &StateStore) -> PathBuf {
    if let Some(configured) = env::var_os("LSW_DAEMON_SOCKET") {
        return PathBuf::from(configured);
    }
    store.root().join("run/lswd.sock")
}

pub(super) fn daemon_idle_exit() -> Result<Duration, Box<dyn std::error::Error>> {
    let seconds = match env::var("LSW_DAEMON_IDLE_SECONDS") {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| "LSW_DAEMON_IDLE_SECONDS must be an integer")?,
        Err(env::VarError::NotPresent) => return Ok(DEFAULT_IDLE_EXIT),
        Err(error) => return Err(error.into()),
    };
    if !(1..=3600).contains(&seconds) {
        return Err("LSW_DAEMON_IDLE_SECONDS must be between 1 and 3600".into());
    }
    Ok(Duration::from_secs(seconds))
}

pub(super) fn current_rss_kib() -> Option<u64> {
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_ascii_whitespace()
                .next()?
                .parse()
                .ok()
        })
}
