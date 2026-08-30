// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("lsw-host currently requires a Unix host");

#[cfg(unix)]
mod agent;
#[cfg(unix)]
mod daemon;

#[cfg(unix)]
pub use agent::{
    agent_address, AgentClient, CapturedProcess, GuiWindowEvent, GuiWindowReader, GuiWindowSession,
    GuiWindowWriter,
};
#[cfg(unix)]
pub use daemon::DaemonClient;
