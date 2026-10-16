//! Process output is bounded while reading, before allocating an untrusted result.
use anyhow::{Context, Result, ensure};
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
pub async fn output(command: &mut Command, limit: usize, timeout: Duration) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().context("process stdout unavailable")?;
    let operation = async {
        let mut bytes = vec![];
        stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= limit, "process output exceeds its limit");
        ensure!(child.wait().await?.success(), "process failed");
        Ok(bytes)
    };
    tokio::time::timeout(timeout, operation).await?
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn stops_an_unbounded_producer() {
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do printf data; done"]);
        assert!(
            output(&mut command, 1024, Duration::from_secs(2))
                .await
                .is_err()
        );
    }
}
