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
    Ok(())
}
