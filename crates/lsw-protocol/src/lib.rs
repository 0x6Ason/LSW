// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

mod codec;
mod constants;
mod error;
mod frame;
mod gui;
mod integration;
mod role;
mod session;
mod transfer;
mod user;

pub use codec::constant_time_token_eq;
pub use constants::*;
pub use error::{ProtocolError, Result};
pub use frame::*;
pub use gui::*;
pub use integration::*;
pub use role::{validate_windows_user_name, WindowsUserRole};
pub use session::*;
pub use transfer::*;
pub use user::*;

#[cfg(test)]
mod tests;
