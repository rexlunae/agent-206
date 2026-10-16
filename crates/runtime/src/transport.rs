//! OpenSSH transport adapted from RustyClaw's gateway/ssh_connection.rs.
use a206_protocol::{decode_payload, encode, payload_size};
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::Command,
};

pub async fn read_frame<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Option<T>> {
    let mut header = [0; 10];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut header[1..])
        .await
        .context("truncated frame header")?;
    let mut payload = vec![0; payload_size(&header)?];
    reader
        .read_exact(&mut payload)
        .await
        .context("truncated frame body")?;
    Ok(Some(decode_payload(&payload)?))
}
pub async fn write_frame<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<()> {
    writer.write_all(&encode(value)?).await?;
    writer.flush().await?;
    Ok(())
}
/// A validated `ssh://[user@]host[:port]` endpoint.
pub struct SshEndpoint {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}
pub fn parse_ssh_endpoint(endpoint: &str) -> Result<SshEndpoint> {
    let url = url::Url::parse(endpoint)?;
    ensure!(
        url.scheme() == "ssh"
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && (url.path().is_empty() || url.path() == "/"),
        "expected ssh://[user@]host[:port]"
    );
    let host = url.host_str().context("SSH host missing")?;
    ensure!(!host.starts_with('-'), "invalid SSH host");
    ensure!(
        url.username()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid SSH username"
    );
    Ok(SshEndpoint {
        user: (!url.username().is_empty()).then(|| url.username().to_owned()),
        host: host.to_owned(),
        port: url.port(),
    })
}
pub fn ssh_command(endpoint: &str) -> Result<Command> {
    ssh_base(endpoint, None, false)
}
/// `ssh -G`: print the configuration OpenSSH would use for an endpoint, without connecting.
pub fn ssh_config_command(endpoint: &str) -> Result<Command> {
    ssh_base(endpoint, None, true)
}
/// Gateway SSH also trusts host keys the user confirmed during setup, kept in
/// the profile's own known_hosts after the user's `~/.ssh/known_hosts`.
pub fn gateway_ssh_command(root: &Path, endpoint: &str) -> Result<Command> {
    ssh_base(endpoint, Some(&crate::connection::known_hosts(root)), false)
}
fn ssh_base(endpoint: &str, known_hosts: Option<&Path>, print_config: bool) -> Result<Command> {
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
    if let Some(file) = known_hosts {
        let file = file.to_str().context("profile path is not valid UTF-8")?;
        ensure!(
            !file.contains('"') && !file.contains('\n'),
            "profile path cannot be passed to OpenSSH"
        );
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
pub fn gateway_command(
    root: &Path,
    remote: Option<&str>,
    start: bool,
    explicit: bool,
) -> Result<Command> {
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
        let mut command = Command::new(std::env::current_exe()?);
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
