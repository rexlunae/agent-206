//! OpenSSH transport adapted from RustyClaw's gateway/ssh_connection.rs.
use a206_core::ErrorKind;
use serde::{Serialize, de::DeserializeOwned};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::Command,
};

/// A frame that could not be read or written on a gateway channel.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("gateway channel failed")]
    Io(#[from] std::io::Error),
    #[error("truncated frame {part}")]
    Truncated {
        part: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Protocol(#[from] a206_protocol::Error),
}
impl crate::error::Classified for FrameError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Io(_) => ErrorKind::Unavailable,
            Self::Truncated { .. } | Self::Protocol(_) => ErrorKind::Protocol,
        })
    }
}

pub async fn read_frame<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Option<T>, FrameError> {
    let mut header = [0; 10];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut header[1..])
        .await
        .map_err(|source| FrameError::Truncated {
            part: "header",
            source,
        })?;
    let mut payload = vec![0; a206_protocol::payload_size(&header)?];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|source| FrameError::Truncated {
            part: "body",
            source,
        })?;
    Ok(Some(a206_protocol::decode_payload(&payload)?))
}
pub async fn write_frame<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<(), FrameError> {
    writer.write_all(&a206_protocol::encode(value)?).await?;
    writer.flush().await?;
    Ok(())
}
/// A validated `ssh://[user@]host[:port]` endpoint.
pub struct SshEndpoint {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}
/// Why an SSH endpoint or command was refused before running OpenSSH.
#[derive(Debug, thiserror::Error)]
pub enum SshEndpointError {
    #[error("not a URL")]
    Url(#[from] url::ParseError),
    #[error("expected ssh://[user@]host[:port] with no password, path, query or fragment")]
    Shape,
    #[error("SSH host missing")]
    MissingHost,
    #[error("SSH host may not start with '-'")]
    OptionLikeHost,
    #[error("SSH username may contain only letters, digits, '.', '_' and '-'")]
    Username,
    #[error("profile path {} cannot be passed to OpenSSH", path.display())]
    ProfilePath { path: std::path::PathBuf },
}
impl crate::error::Classified for SshEndpointError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(ErrorKind::InvalidRequest)
    }
}
pub fn parse_ssh_endpoint(endpoint: &str) -> Result<SshEndpoint, SshEndpointError> {
    let url = url::Url::parse(endpoint)?;
    if !(url.scheme() == "ssh"
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (url.path().is_empty() || url.path() == "/"))
    {
        return Err(SshEndpointError::Shape);
    }
    let host = url.host_str().ok_or(SshEndpointError::MissingHost)?;
    if host.starts_with('-') {
        return Err(SshEndpointError::OptionLikeHost);
    }
    if !url
        .username()
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(SshEndpointError::Username);
    }
    Ok(SshEndpoint {
        user: (!url.username().is_empty()).then(|| url.username().to_owned()),
        host: host.to_owned(),
        port: url.port(),
    })
}
pub fn ssh_command(endpoint: &str) -> Result<Command, SshEndpointError> {
    ssh_base(endpoint, None, false)
}
/// `ssh -G`: print the configuration OpenSSH would use for an endpoint, without connecting.
pub fn ssh_config_command(endpoint: &str) -> Result<Command, SshEndpointError> {
    ssh_base(endpoint, None, true)
}
/// Gateway SSH also trusts host keys the user confirmed during setup, kept in
/// the profile's own known_hosts after the user's `~/.ssh/known_hosts`.
pub fn gateway_ssh_command(root: &Path, endpoint: &str) -> Result<Command, SshEndpointError> {
    ssh_base(endpoint, Some(&crate::connection::known_hosts(root)), false)
}
fn ssh_base(
    endpoint: &str,
    known_hosts: Option<&Path>,
    print_config: bool,
) -> Result<Command, SshEndpointError> {
    let endpoint = parse_ssh_endpoint(endpoint)?;
    let mut command = Command::new("ssh");
    if print_config {
        command.arg("-G");
    }
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ConnectTimeout=10",
    ]);
    if let Some(path) = known_hosts {
        let file = path
            .to_str()
            .filter(|file| !file.contains('"') && !file.contains('\n'))
            .ok_or_else(|| SshEndpointError::ProfilePath {
                path: path.to_owned(),
            })?;
        command
            .arg("-o")
            .arg(format!("UserKnownHostsFile=~/.ssh/known_hosts \"{file}\""));
    }
    if let Some(port) = endpoint.port {
        command.arg("-p").arg(port.to_string());
    }
    if let Some(user) = endpoint.user {
        command.arg("-l").arg(user);
    }
    command.arg("--").arg(endpoint.host);
    Ok(command)
}
/// Why the gateway command could not be built.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(transparent)]
    Endpoint(#[from] SshEndpointError),
    #[error("could not locate the agent206 executable")]
    CurrentExe(#[source] std::io::Error),
}
impl crate::error::Classified for LaunchError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Endpoint(_) => ErrorKind::InvalidRequest,
            Self::CurrentExe(_) => ErrorKind::Internal,
        })
    }
}
pub fn gateway_command(
    root: &Path,
    remote: Option<&str>,
    start: bool,
    explicit: bool,
) -> Result<Command, LaunchError> {
    let mut command = if let Some(remote) = remote {
        let mut command = gateway_ssh_command(root, remote)?;
        // Fixed command text: OpenSSH executes remote arguments through a shell.
        command.arg(if explicit {
            "agent206 gateway --stdio --start"
        } else if start {
            "agent206 gateway --stdio"
        } else {
            "agent206 gateway --stdio --no-start"
        });
        command
    } else {
        let mut command = Command::new(std::env::current_exe().map_err(LaunchError::CurrentExe)?);
        command
            .arg("--data-dir")
            .arg(root)
            .args(["gateway", "--stdio"]);
        if explicit {
            command.arg("--start");
        } else if !start {
            command.arg("--no-start");
        }
        command
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ssh_rejects_credentials_and_injection() {
        for input in [
            "http://localhost",
            "ssh://u:p@localhost",
            "ssh://-evil",
            "ssh://a%3Bb@host",
            "ssh://host/command",
        ] {
            assert!(ssh_command(input).is_err(), "{input}");
        }
        assert!(ssh_command("ssh://user@localhost:22").is_ok());
    }
}
