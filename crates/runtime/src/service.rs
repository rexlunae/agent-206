//! One user-private gateway process per state directory, shared by all clients.
use anyhow::{Context, Result};
use std::{path::Path, process::Stdio};
#[cfg(unix)]
pub async fn daemon(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;
    let gateway = crate::gateway::Gateway::open(root)?;
    tokio::spawn(refresh_catalogs(gateway.clone()));
    let path = root.join("gateway.sock");
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
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
pub async fn connect(root: &Path) -> Result<tokio::net::UnixStream> {
    let path = root.join("gateway.sock");
    if let Ok(stream) = tokio::net::UnixStream::connect(&path).await {
        return Ok(stream);
    }
    // Concurrent starters are serialized by the store's advisory lock.
    let log_dir = root.to_owned();
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("gateway.log"))?;
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("--data-dir")
        .arg(root)
        .args(["gateway", "--daemon"])
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
    anyhow::bail!("gateway did not become ready; inspect gateway.log in the data directory")
}
#[cfg(unix)]
pub async fn stdio(root: &Path) -> Result<()> {
    let stream = connect(root).await?;
    let (mut reader, mut writer) = stream.into_split();
    let input =
        tokio::spawn(async move { tokio::io::copy(&mut tokio::io::stdin(), &mut writer).await });
    let output =
        tokio::spawn(async move { tokio::io::copy(&mut reader, &mut tokio::io::stdout()).await });
    tokio::select! { result = input => { result??; }, result = output => { result??; } }
    Ok(())
}
#[cfg(not(unix))]
pub async fn daemon(_root: &Path) -> Result<()> {
    anyhow::bail!("Windows named-pipe gateway transport is not implemented")
}
#[cfg(not(unix))]
pub async fn stdio(_root: &Path) -> Result<()> {
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
