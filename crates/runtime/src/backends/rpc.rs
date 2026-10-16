use anyhow::{Context, Result, ensure};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
pub const MAX_LINE: usize = 2 * 1024 * 1024;
pub struct Rpc {
    pub input: ChildStdin,
    pub output: BufReader<ChildStdout>,
    received: usize,
}
impl Rpc {
    pub fn new(process: &mut super::isolation::Process) -> Result<Self> {
        Ok(Self {
            input: process
                .child
                .stdin
                .take()
                .context("backend input missing")?,
            output: BufReader::new(
                process
                    .child
                    .stdout
                    .take()
                    .context("backend output missing")?,
            ),
            received: 0,
        })
    }
    pub async fn send(&mut self, value: Value) -> Result<()> {
        write(&mut self.input, serde_json::to_string(&value)?).await?;
        Ok(())
    }
    pub async fn receive(&mut self) -> Result<Value> {
        let line = read(&mut self.output)
            .await?
            .context("backend closed its protocol connection")?;
        self.received += line.len();
        ensure!(
            self.received <= 32 * 1024 * 1024,
            "backend exceeded its protocol output limit"
        );
        Ok(serde_json::from_str(&line)?)
    }
}
pub async fn write(input: &mut ChildStdin, line: String) -> std::io::Result<()> {
    if line.len() > MAX_LINE {
        return Err(std::io::Error::other("backend request exceeds its limit"));
    }
    input.write_all(line.as_bytes()).await?;
    input.write_all(b"\n").await?;
    input.flush().await
}
pub async fn read(output: &mut BufReader<ChildStdout>) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let available = output.fill_buf().await?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::other(
                    "backend closed a partial protocol frame",
                ))
            };
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if bytes.len() + count > MAX_LINE {
            return Err(std::io::Error::other(
                "backend protocol line exceeds its limit",
            ));
        }
        bytes.extend_from_slice(&available[..count]);
        output.consume(count);
        if bytes.last() == Some(&b'\n') {
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(std::io::Error::other);
        }
    }
}
