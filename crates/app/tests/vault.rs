#![cfg(unix)]
//! The real CLI starts a separate broker process, reports honest locked state
//! without prompting, and stops the broker on request.
#[allow(dead_code)]
mod support;
use anyhow::Result;
use support::{binary, capture};
use tokio::process::Command;

fn vault(root: &std::path::Path, args: &[&str]) -> Result<Command> {
    let mut command = Command::new(binary()?);
    command.arg("--data-dir").arg(root).arg("vault").args(args);
    Ok(command)
}

#[tokio::test]
async fn broker_autostarts_reports_locked_state_and_stops() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let status = capture(vault(root, &["status"])?).await?;
    assert!(status.status.success(), "{status:?}");
    let text = String::from_utf8(status.stdout)?;
    assert!(text.contains("exists\tno") && text.contains("unlocked\tno"));
    assert!(text.contains("two-factor\tunknown while locked"));
    let socket = root.join("vault").join("broker.sock");
    assert!(socket.exists());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(root.join("vault"))?.permissions().mode() & 0o777,
        0o700
    );
    // Locked: listing fails with an explanation instead of returning nothing.
    let list = capture(vault(root, &["list"])?).await?;
    assert!(!list.status.success());
    assert!(String::from_utf8(list.stderr)?.contains("locked"));
    let stop = capture(vault(root, &["stop"])?).await?;
    assert!(stop.status.success(), "{stop:?}");
    for _ in 0..100 {
        if !socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!socket.exists());
    // Stopping again reports the absence and starts nothing.
    let again = capture(vault(root, &["stop"])?).await?;
    assert!(again.status.success(), "{again:?}");
    assert!(String::from_utf8(again.stdout)?.contains("not running"));
    assert!(!socket.exists());
    Ok(())
}

#[tokio::test]
async fn vault_import_preview_never_copies_unlocks_or_starts_a_gateway() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("target");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("gateway.stopped"), "deliberately stopped")?;
    let source = directory.path().join("source/secrets.json");
    let password = uuid::Uuid::new_v4().to_string();
    let mut original = a206_vault::Vault::locked(source.clone());
    original.unlock(&a206_vault::Secret::new(password.clone()))?;
    original.lock();
    let before = std::fs::read(&source)?;
    // Explicitly owned process; failed assertions kill it on drop. A successful
    // run requests shutdown and awaits it before dropping the private profile.
    let mut broker = vault(&root, &["broker"])?
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    tokio::time::timeout(support::TIMEOUT, async {
        loop {
            match a206_broker::Client::connect(&root, a206_broker::Role::User).await {
                Ok(_) => break,
                Err(a206_broker::ClientError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => panic!("test broker failed: {error:?}"),
            }
        }
    })
    .await?;
    let mut command = vault(&root, &["import"])?;
    command.arg(&source);
    let output = capture(command).await?;
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout)?;
    assert!(text.contains("SHA-256") && text.contains("Not imported"));
    assert!(!text.contains(&password));
    assert_eq!(std::fs::read(&source)?, before);
    assert!(!a206_broker::vault_path(&root).exists());
    assert!(!root.join("gateway.sock").exists());
    assert!(root.join("gateway.stopped").exists());
    let stop = capture(vault(&root, &["stop"])?).await?;
    assert!(stop.status.success(), "{stop:?}");
    assert!(
        tokio::time::timeout(support::TIMEOUT, broker.wait())
            .await??
            .success()
    );
    Ok(())
}
