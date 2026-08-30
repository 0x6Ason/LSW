// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("lswg currently requires a Unix Wayland host");

#[cfg(unix)]
mod presenter;

#[cfg(unix)]
pub use presenter::present;
