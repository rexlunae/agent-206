//! The runtime's use of the OS sandbox (`a206_sandbox`: bubblewrap and
//! Landlock on Linux, Seatbelt on macOS): its readiness check, and re-running
//! this executable confined for its own workers.
use a206_core::ErrorKind;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use a206_sandbox::Control;
pub use a206_sandbox::StartError;

/// Why this executable could not be re-run inside the sandbox.
#[derive(Debug, thiserror::Error)]
pub enum ConfineError {
    #[error("could not locate this executable")]
    CurrentExe(#[source] std::io::Error),
    #[error("could not prepare the sandbox's private directory")]
    Setup(#[source] std::io::Error),
    #[error("could not prepare OS isolation")]
    Sandbox(#[from] a206_sandbox::SandboxError),
    #[error("could not start the sandbox")]
    Spawn(#[source] std::io::Error),
    #[error(transparent)]
    Start(#[from] StartError),
}
impl crate::error::Classified for ConfineError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::CurrentExe(_) | Self::Setup(_) => ErrorKind::Internal,
            Self::Spawn(_) => ErrorKind::Unavailable,
            // The sandbox's own error knows its kind.
            Self::Sandbox(_) => return None,
            Self::Start(error) => return crate::error::classify(error),
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use confined::Confined;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod confined {
    #[cfg(target_os = "macos")]
    use super::macos;
    use super::{ConfineError, Control, StartError};
    use std::path::Path;
    use tokio::process::Child;
    /// This executable, re-run inside the OS sandbox with only the system
    /// libraries and itself visible, no network, nothing else to
    /// execute and an empty private directory as its working directory.
    pub struct Confined {
        /// Piped stdin and stdout; the environment is empty.
        pub child: Child,
        control: Control,
        _scratch: tempfile::TempDir,
        /// The start of `sandbox-exec`'s stderr, which the confined process
        /// shares, collected until it closes.
        #[cfg(target_os = "macos")]
        stderr: Option<tokio::task::JoinHandle<Vec<u8>>>,
    }
    impl Confined {
        /// Start once the sandbox is in place: on Linux, once the helper
        /// reports both layers; on macOS `sandbox-exec` applies the policy
        /// before it runs this executable, or fails without running it. `args` gets
        /// the private directory and returns the arguments to run with.
        pub async fn spawn_self(
            args: impl FnOnce(&Path) -> Vec<std::ffi::OsString>,
        ) -> std::result::Result<Self, ConfineError> {
            let scratch = tempfile::Builder::new()
                .prefix("a206-confined-")
                .tempdir()
                .map_err(ConfineError::Setup)?;
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700))
                    .map_err(ConfineError::Setup)?;
            }
            let root = std::fs::canonicalize(scratch.path()).map_err(ConfineError::Setup)?;
            let executable = std::env::current_exe()
                .and_then(std::fs::canonicalize)
                .map_err(ConfineError::CurrentExe)?;
            let control = root.join("control");
            let profile = a206_sandbox::Profile {
                helper: &executable,
                executable: &executable,
                // Only this file is executable, never its directory.
                root: &executable,
                scratch: &root,
                control: &control,
                workspace: None,
                execute: false,
                hidden: &[],
                cwd: &root,
                network: a206_sandbox::NetworkAccess::None,
                stderr: false,
            };
            let mut command = tokio::process::Command::from(profile.command()?);
            let control = Control::bind(&control, None)?;
            command
                .args(args(&root))
                .env_clear()
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true);
            // Bubblewrap and the helper, or `sandbox-exec`, report setup
            // failures on stderr.
            command.stderr(std::process::Stdio::piped());
            let mut child = command.spawn().map_err(ConfineError::Spawn)?;
            // Dropping `child` on failure kills the sandbox and its process.
            control.ready(&mut child).await?;
            // On macOS readiness proves nothing yet: `sandbox-exec` may still
            // refuse the policy. Keep its diagnostic for `startup_failure`.
            // The task ends when the pipe closes, which the process's exit
            // or kill guarantees; `startup_failure` awaits it.
            #[cfg(target_os = "macos")]
            let stderr = child
                .stderr
                .take()
                .map(|pipe| tokio::spawn(macos::drain(pipe)));
            Ok(Self {
                child,
                control,
                _scratch: scratch,
                #[cfg(target_os = "macos")]
                stderr,
            })
        }
        /// Why the sandbox, rather than this executable, ended the process,
        /// once it has stopped answering. Linux confirms the start before
        /// `spawn_self` returns, so only macOS can report one here: there
        /// `sandbox-exec` exits with its own diagnostic when it refuses the
        /// policy or cannot run the executable.
        pub async fn startup_failure(&mut self) -> Option<StartError> {
            #[cfg(target_os = "linux")]
            return None;
            #[cfg(target_os = "macos")]
            {
                // A process that closed its pipes but keeps running is this
                // executable's own failure, not the sandbox's.
                let status = tokio::time::timeout(macos::EXIT_GRACE, self.child.wait())
                    .await
                    .ok();
                // Unactionable here: the caller reports the exit it saw.
                let status = status?.ok()?;
                let diagnostic = self.stderr.take()?.await.ok()?;
                macos::refused(status, &diagnostic)
            }
        }
        pub async fn stop(self) {
            self.control.stop().await;
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::StartError;
    use tokio::io::AsyncReadExt;
    /// How long a process that stopped answering may take to exit.
    pub const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
    /// Most of `sandbox-exec`'s stderr kept; the rest is read and discarded so
    /// the pipe never fills.
    const MAX_DIAGNOSTIC: u64 = 4096;
    /// Every message `sandbox-exec` prints about itself starts with its name.
    const LAUNCHER: &str = "sandbox-exec";

    pub async fn drain(mut pipe: impl tokio::io::AsyncRead + Unpin) -> Vec<u8> {
        let mut kept = Vec::new();
        let drained = async {
            (&mut pipe)
                .take(MAX_DIAGNOSTIC)
                .read_to_end(&mut kept)
                .await?;
            tokio::io::copy(&mut pipe, &mut tokio::io::sink()).await
        };
        // A read failure only ends the diagnostic early; what was kept counts.
        if let Err(error) = drained.await {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "confined stderr closed early"
            );
        }
        kept
    }
    /// The start failure `sandbox-exec` reported, if it, not the process it
    /// runs, wrote the diagnostic and failed.
    pub fn refused(status: std::process::ExitStatus, diagnostic: &[u8]) -> Option<StartError> {
        let diagnostic = String::from_utf8_lossy(diagnostic).trim().to_owned();
        (!status.success() && diagnostic.starts_with(LAUNCHER))
            .then_some(StartError::Exited { status, diagnostic })
    }
}

#[cfg(test)]
mod tests {
    use super::StartError;
    use a206_core::ErrorKind;
    use a206_sandbox::SandboxError;

    #[test]
    fn the_helpers_failure_keeps_its_kind_for_the_client() {
        let sent = SandboxError::NotEnforced.report();
        let report = crate::error::report(&StartError::Failed(sent.clone()));
        assert_eq!(report.kind, ErrorKind::Unavailable);
        assert_eq!(report.causes().next().unwrap().message, sent.message);
    }

    /// A refused policy reaches the caller as `sandbox-exec`'s own diagnostic.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_refused_seatbelt_policy_keeps_its_diagnostic() {
        let mut child = tokio::process::Command::new(a206_sandbox::SANDBOX_EXEC)
            .args(["-p", "(version 1) (not-an-operation)", "/usr/bin/true"])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let diagnostic = super::macos::drain(child.stderr.take().unwrap()).await;
        let status = child.wait().await.unwrap();
        let error = super::macos::refused(status, &diagnostic).expect("a sandbox start failure");
        let report = crate::error::report(&error);
        assert_eq!(report.kind, ErrorKind::Unavailable);
        assert!(
            report.message.contains("sandbox-exec"),
            "{}",
            report.message
        );
        // A process's own failure is not the sandbox's.
        assert!(super::macos::refused(status, b"worker panicked").is_none());
    }
}
