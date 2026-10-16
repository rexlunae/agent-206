use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

pub const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CAPTURE: u64 = 4 * 1024 * 1024;

pub async fn capture(mut command: Command) -> Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().context("missing stdout")?;
    let stderr = child.stderr.take().context("missing stderr")?;
    let result = tokio::time::timeout(TIMEOUT, async {
        let (stdout, stderr, status) =
            tokio::try_join!(read_bounded(stdout), read_bounded(stderr), async {
                Ok::<_, anyhow::Error>(child.wait().await?)
            })?;
        Ok::<_, anyhow::Error>(std::process::Output {
            status,
            stdout,
            stderr,
        })
    })
    .await;
    match result {
        Ok(Ok(output)) => Ok(output),
        error => {
            child.start_kill()?;
            tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
            match error {
                Ok(Err(error)) => Err(error),
                _ => anyhow::bail!("subprocess exceeded {TIMEOUT:?}"),
            }
        }
    }
}
async fn read_bounded(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(MAX_CAPTURE + 1).read_to_end(&mut bytes).await?;
    ensure!(
        bytes.len() as u64 <= MAX_CAPTURE,
        "subprocess output exceeds its limit"
    );
    Ok(bytes)
}
pub fn checked(output: std::process::Output) -> Result<String> {
    ensure!(
        output.status.success(),
        "command failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

/// Own the gateway explicitly so a failing assertion cannot leave an auto-started daemon.
pub struct App {
    daemon: std::process::Child,
    pub root: PathBuf,
    binary: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    _directory: tempfile::TempDir,
}
impl Drop for App {
    fn drop(&mut self) {
        // Lifecycle tests may replace the original daemon. Stop only this private
        // test profile through its socket before reaping the child we started.
        use std::io::{Read, Write};
        if let Ok(mut stream) =
            std::os::unix::net::UnixStream::connect(self.root.join("gateway.sock"))
        {
            let _timeout = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _timeout = stream.set_write_timeout(Some(Duration::from_secs(2)));
            if let Ok(frame) = a206_protocol::encode(&a206_protocol::Request {
                id: uuid::Uuid::new_v4(),
                body: a206_core::Command::Shutdown,
            }) {
                let _write = stream.write_all(&frame);
                let mut header = [0; 10];
                let _reply = stream.read_exact(&mut header);
            }
        }
        let _kill = self.daemon.kill();
        let _reap = self.daemon.wait();
    }
}
impl App {
    pub async fn start(
        directory: tempfile::TempDir,
        environment: BTreeMap<OsString, OsString>,
    ) -> Result<Self> {
        let binary = PathBuf::from(
            std::env::var_os("A206_TEST_BINARY")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_agent206").into()),
        )
        .canonicalize()?;
        let root = directory.path().join("state");
        std::fs::create_dir(&root)?;
        let log = std::fs::File::create(root.join("gateway.log"))?;
        let daemon = std::process::Command::new(&binary)
            .arg("--data-dir")
            .arg(&root)
            .args(["gateway", "--daemon"])
            .envs(&environment)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?;
        let mut app = Self {
            daemon,
            root,
            binary,
            environment,
            _directory: directory,
        };
        tokio::time::timeout(TIMEOUT, async {
            loop {
                ensure!(
                    app.daemon.try_wait()?.is_none(),
                    "gateway exited during startup"
                );
                if tokio::net::UnixStream::connect(app.root.join("gateway.sock"))
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("gateway startup timed out")??;
        Ok(app)
    }
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .arg("--data-dir")
            .arg(&self.root)
            .args(args)
            .envs(&self.environment);
        command
    }
    pub async fn cli(&self, args: &[&str]) -> Result<String> {
        checked(capture(self.command(args)).await?)
    }
    pub async fn snapshot(&self) -> Result<a206_core::Snapshot> {
        let a206_core::Reply::Snapshot(snapshot) = toml::from_str(&self.cli(&["snapshot"]).await?)?
        else {
            anyhow::bail!("expected snapshot");
        };
        Ok(*snapshot)
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        self.cli(&["stop-gateway"]).await?;
        let status = tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Some(status) = self.daemon.try_wait()? {
                    break Ok::<_, std::io::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        ensure!(status.success(), "gateway shutdown failed: {status}");
        Ok(())
    }
}
