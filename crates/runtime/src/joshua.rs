//! Explicit process ownership for local/SSH Joshua servers. No pkill, no fixed model catalog.
use a206_core::{ErrorKind, Id, ProviderConfig};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::{Child, Command};
#[derive(Debug, thiserror::Error)]
pub enum JoshuaError {
    #[error("HOME is unavailable")]
    NoHome,
    #[error("stop the current managed process before switching models")]
    AlreadyRunning,
    #[error("provide a connection name, simple model filename and unprivileged port")]
    InvalidRequest,
    #[error("API port {port} is already in use")]
    PortInUse {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid SSH destination")]
    Endpoint(#[from] crate::transport::SshEndpointError),
    #[error("model file in ~/models is missing or unreadable")]
    MissingModel(#[source] std::io::Error),
    #[error("model file escapes ~/models")]
    ModelEscapes,
    #[error("could not open ~/models")]
    ModelsDirectory(#[source] std::io::Error),
    #[error("could not open joshua.log")]
    Log(#[source] std::io::Error),
    #[error("could not start Joshua")]
    Spawn(#[source] std::io::Error),
    #[error("could not check the managed process")]
    Wait(#[source] std::io::Error),
    #[error("could not stop the managed process")]
    Stop(#[source] std::io::Error),
}
impl crate::error::Classified for JoshuaError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::AlreadyRunning | Self::PortInUse { .. } => ErrorKind::Conflict,
            Self::InvalidRequest | Self::Endpoint(_) | Self::ModelEscapes => {
                ErrorKind::InvalidRequest
            }
            // Absent and unreadable files need different recovery; the I/O cause decides.
            Self::MissingModel(_) => return None,
            Self::NoHome
            | Self::ModelsDirectory(_)
            | Self::Log(_)
            | Self::Spawn(_)
            | Self::Wait(_)
            | Self::Stop(_) => ErrorKind::Unavailable,
        })
    }
}
type Result<T, E = JoshuaError> = std::result::Result<T, E>;
#[derive(Default)]
pub struct Manager {
    child: Option<Child>,
    label: Option<String>,
}
pub fn models_directory() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").ok_or(JoshuaError::NoHome)?).join("models"))
}
fn valid_filename(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
impl Manager {
    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }
    pub fn status(&mut self) -> Result<String> {
        if let Some(child) = &mut self.child {
            if let Some(status) = child.try_wait().map_err(JoshuaError::Wait)? {
                self.child = None;
                return Ok(format!("Managed process exited: {status}"));
            }
            return Ok(format!(
                "{} is running (owned process {})",
                self.label.as_deref().unwrap_or("Managed service"),
                child.id().unwrap_or_default()
            ));
        }
        Ok("No managed Joshua process is running.".into())
    }
    pub async fn start(
        &mut self,
        label: String,
        model_file: String,
        port: u16,
        remote: Option<String>,
        root: &Path,
    ) -> Result<ProviderConfig> {
        self.status()?;
        if self.child.is_some() {
            return Err(JoshuaError::AlreadyRunning);
        }
        if label.trim().is_empty() || !valid_filename(&model_file) || port <= 1024 {
            return Err(JoshuaError::InvalidRequest);
        }
        // Reserve-check the local endpoint so we never claim an unrelated running service.
        let probe = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .map_err(|source| JoshuaError::PortInUse { port, source })?;
        let mut command = if let Some(remote) = remote {
            let mut command = crate::transport::ssh_command(&remote)?;
            // Options must precede the destination. Build the known forwarding option there.
            let original: Vec<std::ffi::OsString> =
                command.as_std().get_args().map(ToOwned::to_owned).collect();
            command = Command::new("ssh");
            command.args([
                "-o",
                "ExitOnForwardFailure=yes",
                "-L",
                &format!("127.0.0.1:{port}:127.0.0.1:{port}"),
            ]);
            command.args(original);
            // Filename validated above; the only expansion is the remote user's HOME.
            command.arg(format!(
                "exec joshua serve --model \"$HOME/models/{model_file}\" --addr 127.0.0.1:{port}"
            ));
            command
        } else {
            let models = models_directory()?;
            let model = models
                .join(model_file)
                .canonicalize()
                .map_err(JoshuaError::MissingModel)?;
            let models = models
                .canonicalize()
                .map_err(JoshuaError::ModelsDirectory)?;
            if !model.starts_with(models) || !model.is_file() {
                return Err(JoshuaError::ModelEscapes);
            }
            let mut command = Command::new("joshua");
            command
                .arg("serve")
                .arg("--model")
                .arg(model)
                .arg("--addr")
                .arg(format!("127.0.0.1:{port}"));
            command
        };
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("joshua.log"))
            .map_err(JoshuaError::Log)?;
        drop(probe);
        let child = command
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(JoshuaError::Log)?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(JoshuaError::Spawn)?;
        self.child = Some(child);
        self.label = Some(label.clone());
        Ok(ProviderConfig {
            id: Id::new_v4(),
            label,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            credential: None,
            api: Default::default(),
        })
    }
    pub async fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            child.kill().await.map_err(JoshuaError::Stop)?;
            child.wait().await.map_err(JoshuaError::Wait)?;
        }
        self.label = None;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_argument_cannot_escape_or_inject() {
        for name in ["../x", "-x", "x;echo", "x$(id)", "x\ny", "x/y"] {
            assert!(!valid_filename(name));
        }
    }
}
