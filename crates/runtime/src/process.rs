//! Process output is bounded while reading, before allocating an untrusted result.
use a206_core::ErrorKind;
use std::{
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("could not start the process")]
    Spawn(#[source] std::io::Error),
    #[error("could not read the process output")]
    Read(#[source] std::io::Error),
    #[error("process output exceeds its {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error("process failed ({status})")]
    Failed { status: ExitStatus },
    #[error("process did not finish in time")]
    Timeout(#[source] tokio::time::error::Elapsed),
}
impl crate::error::Classified for ProcessError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Spawn(_) | Self::Read(_) => ErrorKind::Unavailable,
            Self::TooLarge { .. } => ErrorKind::InvalidRequest,
            Self::Failed { .. } => ErrorKind::Other,
            Self::Timeout(_) => ErrorKind::Timeout,
        })
    }
}

pub async fn output(
    command: &mut Command,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>, ProcessError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(ProcessError::Spawn)?;
    // Piped above, so the handle is always present.
    let Some(stdout) = child.stdout.take() else {
        unreachable!("stdout is piped")
    };
    let operation = async {
        let mut bytes = vec![];
        stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(ProcessError::Read)?;
        if bytes.len() > limit {
            return Err(ProcessError::TooLarge { limit });
        }
        let status = child.wait().await.map_err(ProcessError::Read)?;
        if !status.success() {
            return Err(ProcessError::Failed { status });
        }
        Ok(bytes)
    };
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(ProcessError::Timeout)?
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
