//! The runtime's use of the Linux sandbox (`a206_sandbox`): its readiness
//! check, and re-running this executable confined for its own workers.
use a206_core::ErrorKind;

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
pub use linux::Confined;

#[cfg(target_os = "linux")]
mod linux {
    use super::{ConfineError, Control};
    use std::path::Path;
    use tokio::process::Child;
    /// This executable, re-run inside bubblewrap and Landlock with only the
    /// system libraries and itself visible, no network, nothing else to
    /// execute and an empty private directory as its working directory.
    pub struct Confined {
        /// Piped stdin and stdout; the environment is empty.
        pub child: Child,
        control: Control,
        _scratch: tempfile::TempDir,
    }
    impl Confined {
        /// Start once the helper reports both layers in place. `args` gets
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
            };
            let mut command = tokio::process::Command::from(profile.command()?);
            let control = Control::bind(&control, None)?;
            command
                .args(args(&root))
                .env_clear()
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                // Bubblewrap and the helper report setup failures here; the
                // confined process's own stderr is discarded.
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let mut child = command.spawn().map_err(ConfineError::Spawn)?;
            // Dropping `child` on failure kills bubblewrap and the sandbox with it.
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
