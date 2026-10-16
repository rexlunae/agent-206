//! One user-private gateway process per state directory, shared by all clients.
use anyhow::{Context, Result};
use std::{path::Path, process::Stdio};
pub fn is_stopped(root: &Path) -> bool {
    root.join("gateway.stopped").exists()
}
pub fn mark_stopped(root: &Path) -> Result<()> {
    let marker = tempfile::NamedTempFile::new_in(root)?;
    marker.as_file().sync_all()?;
    marker.persist(root.join("gateway.stopped"))?;
    #[cfg(unix)]
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}
/// Persist stop for an absent service, serialized with startup by its store lock.
/// A locked profile must instead acknowledge cleanup through the live gateway.
pub(crate) fn stop_if_offline(root: &Path) -> Result<bool> {
    let lock = crate::store::profile_lock(root)?;
    match fs2::FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {
            mark_stopped(root)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    }
}
#[cfg(unix)]
pub async fn daemon(root: &Path, explicit: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;
    anyhow::ensure!(
        explicit || !is_stopped(root),
        "gateway was deliberately stopped; use gateway start"
    );
    let gateway = crate::gateway::Gateway::open(root)?;
    // Repeat under the acquired store lock: an automatic starter can race stop.
    anyhow::ensure!(
        explicit || !is_stopped(root),
        "gateway was deliberately stopped; use gateway start"
    );
    tokio::spawn(refresh_catalogs(gateway.clone()));
    let path = root.join("gateway.sock");
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    if is_stopped(root) {
        std::fs::remove_file(root.join("gateway.stopped"))?;
    }
    loop {
        let stop = gateway.stop_signal();
        let (stream, _) = tokio::select! { accepted = listener.accept() => accepted?, _ = stop.cancelled() => break };
        let (read, write) = stream.into_split();
        let gateway = gateway.clone();
        tokio::spawn(async move {
            if let Err(error) = crate::gateway::serve_connection(gateway, read, write).await {
                eprintln!("gateway client disconnected: {error}");
            }
        });
    }
    std::fs::remove_file(&path)?;
    Ok(())
}
#[cfg(unix)]
pub async fn connect(root: &Path, start: bool, explicit: bool) -> Result<tokio::net::UnixStream> {
    let path = root.join("gateway.sock");
    if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
        return Ok(stream);
    }
    anyhow::ensure!(
        start && (explicit || !is_stopped(root)),
        "gateway is stopped or unavailable; use gateway start"
    );
    // Concurrent starters are serialized by the store's advisory lock.
    let log_dir = root.to_owned();
    std::fs::create_dir_all(&log_dir)?;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o700))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_dir.join("gateway.log"))?;
    log.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .arg("--data-dir")
        .arg(root)
        .args(["gateway", "--daemon"]);
    if explicit {
        command.arg("--start");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .context("start gateway service")?;
    for _ in 0..100 {
        if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
            tokio::spawn(async move {
                let _exit_status = child.wait().await;
            });
            return Ok(stream);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let _kill = child.kill().await;
    anyhow::bail!(
        "gateway did not become ready; inspect {}",
        root.join("gateway.log").display()
    )
}
#[cfg(unix)]
pub async fn stdio(root: &Path, start: bool, explicit: bool) -> Result<()> {
    let stream = connect(root, start, explicit).await?;
    let (mut reader, mut writer) = stream.into_split();
    // Tokio stdin uses a blocking read that cannot be cancelled. Use readiness
    // on the pipe/TTY so a stopped gateway can actually close its stdio bridge.
    let stdin = NonblockingInput::open()?;
    let mut input = tokio::spawn(async move {
        use std::io::Read;
        use tokio::io::AsyncWriteExt;
        let input = async_io::Async::new_nonblocking(stdin)?;
        let mut bytes = [0; 8192];
        loop {
            let count = input
                .read_with(|input| (&input.file).read(&mut bytes))
                .await?;
            if count == 0 {
                writer.shutdown().await?;
                break;
            }
            writer.write_all(&bytes[..count]).await?;
        }
        Ok::<_, std::io::Error>(())
    });
    let mut output =
        tokio::spawn(async move { tokio::io::copy(&mut reader, &mut tokio::io::stdout()).await });
    tokio::select! {
        result = &mut input => { result??; output.await??; },
        result = &mut output => { input.abort(); let _cancelled = input.await; result??; }
    }
    Ok(())
}
#[cfg(unix)]
struct NonblockingInput {
    file: std::fs::File,
    flags: nix::fcntl::OFlag,
}
#[cfg(unix)]
impl NonblockingInput {
    fn open() -> Result<Self> {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let file = std::fs::File::open("/dev/stdin")?;
        let flags = OFlag::from_bits_truncate(fcntl(&file, FcntlArg::F_GETFL)?);
        fcntl(&file, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        Ok(Self { file, flags })
    }
}
#[cfg(unix)]
impl std::os::fd::AsRawFd for NonblockingInput {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.file)
    }
}
#[cfg(unix)]
impl std::os::fd::AsFd for NonblockingInput {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.file)
    }
}
#[cfg(unix)]
impl Drop for NonblockingInput {
    fn drop(&mut self) {
        let _restore = nix::fcntl::fcntl(&self.file, nix::fcntl::FcntlArg::F_SETFL(self.flags));
    }
}
#[cfg(not(unix))]
pub async fn daemon(_root: &Path, _explicit: bool) -> Result<()> {
    anyhow::bail!("Windows named-pipe gateway transport is not implemented")
}
#[cfg(not(unix))]
pub async fn stdio(_root: &Path, _start: bool, _explicit: bool) -> Result<()> {
    anyhow::bail!("Windows named-pipe gateway transport is not implemented")
}

/// Refresh cached facts from provider APIs. Failed refreshes retain the expired cache.
pub async fn refresh_catalogs(gateway: crate::gateway::Gateway) {
    let mut attempted = std::collections::HashMap::<a206_core::Id, u64>::new();
    loop {
        if let Ok(snapshot) = gateway.snapshot() {
            let now = crate::provider::now();
            for provider in snapshot.settings.providers {
                let fresh = snapshot
                    .catalogs
                    .iter()
                    .any(|c| c.provider == provider.id && !c.stale(now));
                if !fresh
                    && attempted
                        .get(&provider.id)
                        .is_none_or(|last| now.saturating_sub(*last) >= 300)
                {
                    attempted.insert(provider.id, now);
                    gateway
                        .request(a206_protocol::Request {
                            id: a206_core::Id::new_v4(),
                            body: a206_core::Command::RefreshCatalog {
                                provider: provider.id,
                            },
                        })
                        .await;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_stop_is_serialized_with_startup_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("profile");
        let lock = crate::store::profile_lock(&root).unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        assert!(!stop_if_offline(&root).unwrap());
        assert!(!is_stopped(&root));
        drop(lock);
        assert!(stop_if_offline(&root).unwrap());
        assert!(is_stopped(&root));
        assert!(stop_if_offline(&root).unwrap());
    }
}
