//! Explicit process ownership for local/SSH Joshua servers. No pkill, no fixed model catalog.
use a206_core::{Id, ProviderConfig};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::{Child, Command};
#[derive(Default)]
pub struct Manager {
    child: Option<Child>,
    label: Option<String>,
}
pub fn models_directory() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?).join("models"))
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
            if let Some(status) = child.try_wait()? {
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
        ensure!(
            self.child.is_none(),
            "stop the current managed process before switching models"
        );
        ensure!(
            !label.trim().is_empty() && valid_filename(&model_file) && port > 1024,
            "provide a connection name, simple model filename and unprivileged port"
        );
        // Reserve-check the local endpoint so we never claim an unrelated running service.
        let probe = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .context("API port is already in use")?;
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
                .context("model file does not exist in ~/models")?;
            ensure!(
                model.starts_with(models.canonicalize()?) && model.is_file(),
                "model file escapes ~/models"
            );
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
            .open(root.join("joshua.log"))?;
        drop(probe);
        let child = command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .context("could not start Joshua")?;
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
            child.kill().await?;
            child.wait().await?;
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
