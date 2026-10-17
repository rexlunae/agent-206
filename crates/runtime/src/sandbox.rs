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
    use super::{ConfineError, Control};
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
            // Bubblewrap and the helper report setup failures on stderr; the
            // confined process's own stderr is discarded. On macOS nothing
            // reads it, so the profile's discarded stderr stands.
            #[cfg(target_os = "linux")]
            command.stderr(std::process::Stdio::piped());
            let mut child = command.spawn().map_err(ConfineError::Spawn)?;
            // Dropping `child` on failure kills the sandbox and its process.
            control.ready(&mut child).await?;
            Ok(Self {
                child,
                control,
                _scratch: scratch,
            })
        }
        pub async fn stop(self) {
            self.control.stop().await;
        }
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
}
