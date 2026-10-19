//! One user-private gateway process per state directory, shared by all clients.
use a206_core::{ErrorKind, Ignore};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
/// Why the gateway service could not be started, reached or stopped.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("could not lock the gateway profile")]
    Lock(#[source] std::io::Error),
    #[error("could not record the stop request")]
    MarkStopped(#[source] std::io::Error),
    #[error("gateway was deliberately stopped; use gateway start")]
    Stopped,
    #[error("gateway is stopped or unavailable; use gateway start")]
    NotRunning,
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error("could not publish the gateway socket")]
    Socket(#[source] std::io::Error),
    #[error("could not accept a gateway client")]
    Accept(#[source] std::io::Error),
    #[error("could not prepare the gateway log")]
    Log(#[source] std::io::Error),
    #[error("could not locate the agent206 executable")]
    CurrentExe(#[source] std::io::Error),
    #[error("could not start the gateway service")]
    Spawn(#[source] std::io::Error),
    #[error("gateway did not become ready; inspect {}", log.display())]
    NotReady { log: PathBuf },
    #[error("could not open standard input")]
    Stdin(#[source] std::io::Error),
    #[cfg(unix)]
    #[error("could not configure standard input")]
    StdinMode(#[source] nix::errno::Errno),
    #[error("gateway stdio bridge failed")]
    Bridge(#[source] std::io::Error),
    #[error("gateway stdio bridge task failed")]
    Task(#[from] tokio::task::JoinError),
    #[error("Windows named-pipe gateway transport is not implemented")]
    Unsupported,
}
impl crate::error::Classified for ServiceError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Stopped | Self::NotRunning | Self::NotReady { .. } | Self::Unsupported => {
                ErrorKind::Unavailable
            }
            Self::MarkStopped(_) => ErrorKind::Storage,
            Self::CurrentExe(_) | Self::Task(_) => ErrorKind::Internal,
            // `transparent` hides the store error from the chain.
            Self::Store(error) => return crate::error::classify(error),
            #[cfg(unix)]
            Self::StdinMode(_) => ErrorKind::Internal,
            Self::Lock(_)
            | Self::Socket(_)
            | Self::Accept(_)
            | Self::Log(_)
            | Self::Spawn(_)
            | Self::Stdin(_)
            | Self::Bridge(_) => return None,
        })
    }
}
type Result<T, E = ServiceError> = std::result::Result<T, E>;
pub fn is_stopped(root: &Path) -> bool {
    root.join("gateway.stopped").exists()
}
pub fn mark_stopped(root: &Path) -> std::io::Result<()> {
    let marker = tempfile::NamedTempFile::new_in(root)?;
    marker.as_file().sync_all()?;
    marker
        .persist(root.join("gateway.stopped"))
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}
/// Persist stop for an absent service, serialized with startup by its store lock.
/// A locked profile must instead acknowledge cleanup through the live gateway.
pub(crate) fn stop_if_offline(root: &Path) -> Result<bool> {
    let lock = crate::store::profile_lock(root).map_err(ServiceError::Lock)?;
    match crate::store::Held::try_lock(lock) {
        Ok(_held) => {
            mark_stopped(root).map_err(ServiceError::MarkStopped)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(ServiceError::Lock(error)),
    }
}
pub(crate) async fn is_ready(root: &Path) -> bool {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(root.join("gateway.sock"))
            .await
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        let _root = root;
        false
    }
}
/// Startup and user stop share a short-lived cross-process gate. Unlike the
/// store lock, this can be held by a controller while a legacy daemon exits.
pub(crate) async fn lifecycle_guard(root: &Path) -> Result<crate::store::Held> {
    loop {
        let lock =
            crate::store::lock_file(root, "gateway.lifecycle.lock").map_err(ServiceError::Lock)?;
        match crate::store::Held::try_lock(lock) {
            Ok(held) => return Ok(held),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(error) => return Err(ServiceError::Lock(error)),
        }
    }
}
#[cfg(unix)]
pub async fn daemon(root: &Path, explicit: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;
    let lifecycle = lifecycle_guard(root).await?;
    crate::error::check(explicit || !is_stopped(root), ServiceError::Stopped)?;
    let gateway = crate::gateway::Gateway::open(root)?;
    // Repeat under the acquired store lock: an automatic starter can race stop.
    crate::error::check(explicit || !is_stopped(root), ServiceError::Stopped)?;
    let refresh = tokio::spawn(refresh_catalogs(gateway.clone()));
    // Ends when the gateway's stop signal fires; awaited after the drain.
    let messengers = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.supervise_messengers().await }
    });
    // Ends when the gateway's stop signal fires; awaited after the drain.
    let schedule = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.supervise_schedule().await }
    });
    let path = root.join("gateway.sock");
    if path.exists() {
        std::fs::remove_file(&path).map_err(ServiceError::Socket)?;
    }
    let listener = UnixListener::bind(&path).map_err(ServiceError::Socket)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(ServiceError::Socket)?;
    if is_stopped(root) {
        std::fs::remove_file(root.join("gateway.stopped")).map_err(ServiceError::MarkStopped)?;
    }
    drop(lifecycle);
    let mut connections = tokio::task::JoinSet::new();
    let stop = gateway.stop_signal();
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted.map_err(ServiceError::Accept)?,
            _ = stop.cancelled() => break,
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    crate::error::handle("gateway_connection", &error);
                }
                continue;
            }
        };
        // gateway.sock is mode 0600; refuse other users at the kernel level too.
        match stream.peer_cred() {
            Ok(peer) if peer.uid() == nix::unistd::geteuid().as_raw() => {}
            Ok(peer) => {
                tracing::warn!(
                    uid = peer.uid(),
                    "refused a gateway connection from another user"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "could not identify a gateway client"
                );
                continue;
            }
        }
        let (read, write) = stream.into_split();
        let gateway = gateway.clone();
        connections.spawn(async move {
            if let Err(error) = crate::gateway::serve_connection(gateway, read, write).await {
                crate::error::handle("gateway_connection", &error);
            }
        });
    }
    drop(listener);
    std::fs::remove_file(&path).map_err(ServiceError::Socket)?;
    refresh.abort();
    // Stop accepting new clients, but flush replies for already accepted work,
    // including independent Shutdown requests queued behind the first one.
    // Retain profile ownership until this bounded drain finishes.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(result) = connections.join_next().await {
            if let Err(error) = result {
                crate::error::handle("gateway_connection", &error);
            }
        }
    })
    .await
    // Connections still open after the bounded drain are aborted below.
    .ignore();
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    if let Err(error) = messengers.await {
        crate::error::handle("messenger", &error);
    }
    if let Err(error) = schedule.await {
        crate::error::handle("schedule_jobs", &error);
    }
    Ok(())
}
#[cfg(unix)]
pub async fn connect(root: &Path, start: bool, explicit: bool) -> Result<tokio::net::UnixStream> {
    let path = root.join("gateway.sock");
    if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
        return Ok(stream);
    }
    crate::error::check(
        start && (explicit || !is_stopped(root)),
        ServiceError::NotRunning,
    )?;
    // Concurrent starters are serialized by the store's advisory lock.
    let log_dir = root.to_owned();
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let log = std::fs::create_dir_all(&log_dir)
        .and_then(|()| std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o700)))
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(log_dir.join("gateway.log"))
        })
        .and_then(|log| {
            log.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            Ok(log)
        })
        .map_err(ServiceError::Log)?;
    let mut command =
        tokio::process::Command::new(std::env::current_exe().map_err(ServiceError::CurrentExe)?);
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
        .map_err(ServiceError::Spawn)?;
    for _ in 0..100 {
        if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
            tokio::spawn(async move {
                // Reaps the detached daemon; its exit is reported through its own log.
                child.wait().await.ignore();
            });
            return Ok(stream);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // The readiness failure below is the error reported; the kill is cleanup.
    child.kill().await.ignore();
    Err(ServiceError::NotReady {
        log: root.join("gateway.log"),
    })
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
        let input = async_io::Async::new_nonblocking(stdin).map_err(ServiceError::Stdin)?;
        let mut bytes = [0; 8192];
        loop {
            let count = input
                .read_with(|input| (&input.file).read(&mut bytes))
                .await
                .map_err(ServiceError::Stdin)?;
            if count == 0 {
                writer.shutdown().await.map_err(ServiceError::Bridge)?;
                break;
            }
            writer
                .write_all(&bytes[..count])
                .await
                .map_err(ServiceError::Bridge)?;
        }
        Ok::<_, ServiceError>(())
    });
    let mut output = tokio::spawn(async move {
        tokio::io::copy(&mut reader, &mut tokio::io::stdout())
            .await
            .map_err(ServiceError::Bridge)
    });
    tokio::select! {
        result = &mut input => { result??; output.await??; },
        // The aborted input task's cancellation is expected; the output result is what matters.
        result = &mut output => { input.abort(); input.await.ignore(); result??; }
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
        let file = std::fs::File::open("/dev/stdin").map_err(ServiceError::Stdin)?;
        let flags = OFlag::from_bits_truncate(
            fcntl(&file, FcntlArg::F_GETFL).map_err(ServiceError::StdinMode)?,
        );
        fcntl(&file, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
            .map_err(ServiceError::StdinMode)?;
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
        // Restoring stdin flags on exit is best effort; Drop has no caller to tell.
        nix::fcntl::fcntl(&self.file, nix::fcntl::FcntlArg::F_SETFL(self.flags)).ignore();
    }
}
#[cfg(not(unix))]
pub async fn daemon(_root: &Path, _explicit: bool) -> Result<()> {
    Err(ServiceError::Unsupported)
}
#[cfg(not(unix))]
pub async fn stdio(_root: &Path, _start: bool, _explicit: bool) -> Result<()> {
    Err(ServiceError::Unsupported)
}

/// Refresh cached facts from provider APIs. Failed refreshes retain the expired cache.
pub async fn refresh_catalogs(gateway: crate::gateway::Gateway) {
    let paired = gateway.paired_signal();
    loop {
        if let Err(error) = gateway.refresh_stale_catalogs().await {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "catalog refresh pass failed"
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
            _ = paired.notified() => {}
        }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn automatic_start_waits_for_legacy_stop_handoff() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("profile");
        let handoff = lifecycle_guard(&root).await.unwrap();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let starter = tokio::spawn({
            let entered = entered.clone();
            let root = root.clone();
            async move {
                entered.notify_one();
                daemon(&root, false).await
            }
        });
        // On this current-thread runtime the starter has reached its first
        // suspension. It must wait at the handoff gate, before claiming a store
        // or publishing a socket, even though the legacy store lock is now free.
        entered.notified().await;
        assert!(!root.join("gateway.sock").exists());
        assert!(!root.join("state.sqlite3").exists());
        assert!(stop_if_offline(&root).unwrap());
        drop(handoff);
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), starter)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        assert!(is_stopped(&root));
        assert!(!root.join("gateway.sock").exists());
    }
}
