// SPDX-License-Identifier: GPL-3.0-or-later

//! Compile-time fallback for unsupported agent platforms.
//!
//! Supported production targets use a real platform owner. This no-op owner
//! exists only so unsupported targets remain explicit at compile time.

use std::io;
use std::process::{Child, Command};

pub(super) struct Prepared;

impl Prepared {
    pub(super) fn new(_command: &mut Command) -> io::Result<Self> {
        Ok(Self)
    }

    pub(super) fn attach_and_start(self, _child: &Child) -> io::Result<Owner> {
        Ok(Owner)
    }
}

pub(super) struct Owner;

impl Owner {
    pub(super) fn terminate(&self, _exit_code: i32) -> io::Result<()> {
        Ok(())
    }
}
