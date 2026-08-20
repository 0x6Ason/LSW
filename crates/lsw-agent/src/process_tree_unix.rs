// SPDX-License-Identifier: GPL-3.0-or-later

//! Unix process-tree ownership implemented with a dedicated process group.
//!
//! This is lifecycle ownership rather than a sandbox: child code may leave
//! its process group, but ordinary descendants are reclaimed with the session.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::os::raw::c_int;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

const SIGKILL: c_int = 9;
// ESRCH is 3 on the Unix platforms supported by Rust. It means the group
// is already empty, which is a successful teardown outcome.
const ESRCH: i32 = 3;

extern "C" {
    fn kill(pid: c_int, signal: c_int) -> c_int;
}

pub(super) struct Prepared;

impl Prepared {
    pub(super) fn new(command: &mut Command) -> io::Result<Self> {
        // `process_group(0)` arranges setpgid(0, 0) after fork and before
        // exec. This closes the ordinary spawn-before-ownership race, but
        // does not prevent guest code from later creating a new session or
        // process group; process-group ownership is not a sandbox.
        command.process_group(0);
        Ok(Self)
    }

    pub(super) fn attach_and_start(self, child: &Child) -> io::Result<Owner> {
        let process_group = c_int::try_from(child.id()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "child process id exceeds pid_t")
        })?;
        Ok(Owner { process_group })
    }
}

pub(super) struct Owner {
    process_group: c_int,
}

impl Owner {
    pub(super) fn terminate(&self, _exit_code: i32) -> io::Result<()> {
        // A negative pid addresses the process group. process_group is a
        // validated positive child pid, so negation cannot overflow.
        // SAFETY: `kill` does not dereference pointers; the validated negative
        // PID targets only the session's process group.
        if unsafe { kill(-self.process_group, SIGKILL) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}
