// SPDX-License-Identifier: GPL-3.0-or-later

//! Protocol, process-lifecycle, service, and configuration regression tests.

use super::*;

mod configuration;
mod unix_session;
mod windows_process;

#[cfg(unix)]
use windows_process::controlled_test_connection;
