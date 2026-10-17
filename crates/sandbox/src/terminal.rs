//! The macOS terminal launcher: it starts a new session, makes its stdin
//! (a pseudo-terminal the gateway opened) the controlling terminal of that
//! session, and then becomes `sandbox-exec`, which becomes the runtime.
//! Everything in the session shares that terminal, so its line discipline
//! delivers interrupt and suspend keys to the runtime's process group.
//!
//! On Linux the in-sandbox helper does the same inside bubblewrap's new
//! session (`helper.rs`), since a terminal acquired outside would not
//! survive it.
use crate::SandboxError;
use std::{ffi::OsString, os::unix::process::CommandExt};

pub(crate) fn run(
    mut args: impl Iterator<Item = OsString>,
) -> Result<std::process::ExitCode, SandboxError> {
    let program = args.next().ok_or(SandboxError::Usage)?;
    rustix::process::setsid().map_err(|errno| SandboxError::Terminal(errno.into()))?;
    rustix::process::ioctl_tiocsctty(std::io::stdin())
        .map_err(|errno| SandboxError::Terminal(errno.into()))?;
    // Returns only when the program could not be started.
    let source = std::process::Command::new(&program).args(args).exec();
    Err(SandboxError::Launch { program, source })
}
