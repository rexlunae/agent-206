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
pub fn ssh_command(endpoint: &str) -> Result<Command> {
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
    let mut command = Command::new("ssh");
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
    if let Some(port) = url.port() {
        command.arg("-p").arg(port.to_string());
    }
    if !url.username().is_empty() {
        command.arg("-l").arg(url.username());
    }
    command.arg("--").arg(host);
    Ok(command)
}
pub fn gateway_command(root: &Path, remote: Option<&str>) -> Result<Command> {
    let mut command = if let Some(remote) = remote {
        let mut command = ssh_command(remote)?;
        // Fixed command text: OpenSSH executes remote arguments through a shell.
        command.arg("agent206 gateway --stdio");
        command
    } else {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--data-dir")
            .arg(root)
            .args(["gateway", "--stdio"]);
        command
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
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
