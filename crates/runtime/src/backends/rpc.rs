use super::{BackendError, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
/// The longest line a backend may send.
pub const MAX_LINE: usize = 2 * 1024 * 1024;
/// The longest request line sent to a backend: a prompt's images and its
/// transcript, with room for JSON escaping and framing.
pub const MAX_REQUEST: usize = super::IMAGE_BUDGET + 2 * super::TRANSCRIPT_LIMIT + 1024 * 1024;
pub struct Rpc {
    pub input: ChildStdin,
    pub output: BufReader<ChildStdout>,
    received: usize,
}
impl Rpc {
    pub fn new(process: &mut super::isolation::Process) -> Result<Self> {
        let (Some(input), Some(output)) = (process.child.stdin.take(), process.child.stdout.take())
        else {
            return Err(BackendError::Closed);
        };
        Ok(Self {
            input,
            output: BufReader::new(output),
            received: 0,
        })
    }
    pub async fn send(&mut self, value: Value) -> Result<()> {
        let line = serde_json::to_string(&value).map_err(BackendError::Json)?;
        write(&mut self.input, line).await.map_err(BackendError::Io)
    }
    pub async fn receive(&mut self) -> Result<Value> {
        let line = read(&mut self.output)
            .await
            .map_err(BackendError::Io)?
            .ok_or(BackendError::Closed)?;
        self.received += line.len();
        if self.received > 32 * 1024 * 1024 {
            return Err(BackendError::OutputLimit);
        }
        serde_json::from_str(&line).map_err(BackendError::Json)
    }
}
pub async fn write(input: &mut ChildStdin, line: String) -> std::io::Result<()> {
    if line.len() > MAX_REQUEST {
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
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_prompt_with_its_full_image_budget_is_written_and_a_larger_one_refused() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "cat > /dev/null"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let prompt = "A".repeat(super::super::IMAGE_BUDGET + super::super::TRANSCRIPT_LIMIT);
        assert!(prompt.len() > MAX_LINE);
        write(&mut input, prompt).await.unwrap();
        let error = write(&mut input, "A".repeat(MAX_REQUEST + 1))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "backend request exceeds its limit");
        drop(input);
        child.wait().await.unwrap();
    }
}
