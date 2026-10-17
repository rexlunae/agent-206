//! The gateway's side of the sandbox, shared by every process it confines: the
//! helper's readiness report and, for runtimes that need their credential
//! proxy, the Unix socket that carries proxy connections out of the network
//! namespace.
use a206_core::{ErrorKind, ErrorReport};

/// Why a sandboxed process did not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("could not prepare the sandbox's control sockets")]
    Setup(#[source] std::io::Error),
    #[error("could not wait for the sandbox")]
    Wait(#[source] std::io::Error),
    #[error("the sandbox could not start its process")]
    Failed(#[source] ErrorReport),
    #[error("the sandbox exited ({status}) before its process started: {diagnostic}")]
    Exited {
        status: std::process::ExitStatus,
        /// Bubblewrap's own explanation, as it printed it.
        diagnostic: String,
    },
    #[error("could not read the sandbox helper's report")]
    Report(#[source] std::io::Error),
    #[error("the sandbox helper sent a malformed report")]
    Malformed(#[source] rmp_serde::decode::Error),
    #[error("the sandbox helper speaks version {version}, not this gateway's")]
    Version { version: u32 },
    #[error("the sandbox did not report readiness in time")]
    Timeout(#[source] tokio::time::error::Elapsed),
}
impl StartError {
    /// The client-facing kind; `None` defers to the helper's own report.
    pub fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Setup(_) | Self::Wait(_) => ErrorKind::Internal,
            Self::Exited { .. } => ErrorKind::Unavailable,
            Self::Failed(_) => return None,
            Self::Report(_) | Self::Malformed(_) | Self::Version { .. } => ErrorKind::Protocol,
            Self::Timeout(_) => ErrorKind::Timeout,
        })
    }
}

#[cfg(target_os = "linux")]
pub use linux::Control;

#[cfg(target_os = "linux")]
mod linux {
    use super::StartError;
    use crate::{MAX_STATUS, SPEC_VERSION, Status};
    use a206_core::Ignore;
    use std::path::Path;
    use tokio::{
        io::AsyncReadExt,
        net::{TcpStream, UnixListener},
        process::Child,
        task::{JoinHandle, JoinSet},
    };
    type Result<T> = std::result::Result<T, StartError>;
    /// How long the helper may take to report that the process started.
    const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    /// How long a report may trail the helper's exit.
    const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
    /// Most bubblewrap diagnostics kept when the sandbox fails to start.
    const MAX_DIAGNOSTIC: u64 = 4096;
    pub struct Control {
        status: UnixListener,
        bridge: Option<JoinHandle<()>>,
    }
    impl Drop for Control {
        fn drop(&mut self) {
            // Aborting is the whole teardown; `stop` also awaits it.
            if let Some(bridge) = &self.bridge {
                bridge.abort();
            }
        }
    }
    impl Control {
        /// Listen in a new, private `control` directory, which the sandbox
        /// mounts read-only so the confined process cannot replace the
        /// sockets. With a proxy `port`, connections to `proxy.sock` are
        /// carried to that loopback port.
        pub fn bind(control: &Path, port: Option<u16>) -> Result<Self> {
            std::fs::create_dir(control).map_err(StartError::Setup)?;
            let bridge = match port {
                Some(port) => {
                    let proxy = UnixListener::bind(control.join("proxy.sock"))
                        .map_err(StartError::Setup)?;
                    Some(tokio::spawn(bridge(proxy, port)))
                }
                None => None,
            };
            let status =
                UnixListener::bind(control.join("status.sock")).map_err(StartError::Setup)?;
            Ok(Self { status, bridge })
        }
        /// Wait until the helper reports the process started inside both
        /// layers. Bubblewrap's diagnostics are read from `child`'s stderr,
        /// when it is piped, if the sandbox exits first.
        pub async fn ready(&self, child: &mut Child) -> Result<()> {
            let report = async {
                let (stream, _) = self.status.accept().await.map_err(StartError::Report)?;
                let mut bytes = Vec::new();
                stream
                    .take(MAX_STATUS)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(StartError::Report)?;
                rmp_serde::from_slice::<Status>(&bytes).map_err(StartError::Malformed)
            };
            let mut report = std::pin::pin!(report);
            let decide = |status: Result<Status>| match status? {
                Status::Ready { version } if version == SPEC_VERSION => Ok(()),
                Status::Ready { version } => Err(StartError::Version { version }),
                Status::Failed(report) => Err(StartError::Failed(report)),
            };
            // `Child::wait` closes stdin, which the process still needs.
            let stdin = child.stdin.take();
            // The deadline covers only waiting; an exit is then explained in full.
            let first = tokio::time::timeout(READY_TIMEOUT, async {
                tokio::select! {
                    biased;
                    status = &mut report => Ok(status),
                    exit = child.wait() => Err(exit),
                }
            })
            .await;
            let result = match first {
                Err(elapsed) => Err(StartError::Timeout(elapsed)),
                Ok(Ok(status)) => decide(status),
                Ok(Err(exit)) => match exit.map_err(StartError::Wait) {
                    Err(error) => Err(error),
                    Ok(status) => {
                        // A helper that reported its failure and then exited has
                        // that report queued; it explains more than the exit.
                        match tokio::time::timeout(EXIT_GRACE, &mut report).await {
                            Ok(report) => decide(report),
                            Err(_) => {
                                let mut diagnostic = Vec::new();
                                if let Some(stderr) = child.stderr.take() {
                                    // Whatever arrived is the diagnostic; a read error, or a
                                    // descendant still holding stderr open, leaves it partial.
                                    let mut stderr = stderr.take(MAX_DIAGNOSTIC);
                                    let read = stderr.read_to_end(&mut diagnostic);
                                    tokio::time::timeout(EXIT_GRACE, read).await.ignore();
                                }
                                Err(StartError::Exited {
                                    status,
                                    diagnostic: String::from_utf8_lossy(&diagnostic)
                                        .trim()
                                        .to_owned(),
                                })
                            }
                        }
                    }
                },
            };
            child.stdin = stdin;
            result
        }
        pub async fn stop(mut self) {
            if let Some(mut bridge) = self.bridge.take() {
                bridge.abort();
                // Cancellation is the expected outcome; a panic was already reported.
                (&mut bridge).await.ignore();
            }
        }
    }
    /// Carry each connection from inside the sandbox to the loopback proxy.
    /// Connection tasks end with the bridge, which owns them.
    async fn bridge(listener: UnixListener, port: u16) {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((mut inside, _)) => {
                        connections.spawn(async move {
                            let result = async {
                                let mut proxy = TcpStream::connect(("127.0.0.1", port)).await?;
                                tokio::io::copy_bidirectional(&mut inside, &mut proxy).await
                            };
                            if let Err(error) = result.await {
                                tracing::warn!(error = &error as &dyn std::error::Error, "sandbox proxy bridge failed");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(error = &error as &dyn std::error::Error, "sandbox proxy bridge stopped");
                        return;
                    }
                },
                Some(_) = connections.join_next() => {}
            }
        }
    }
}
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{Control, StartError};
    use crate::{SPEC_VERSION, SandboxError, Status};
    use a206_core::{ErrorKind, Id};
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn shell(script: &str) -> tokio::process::Child {
        tokio::process::Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    async fn send(control: &std::path::Path, status: &Status) {
        let mut stream = tokio::net::UnixStream::connect(control.join("status.sock"))
            .await
            .unwrap();
        stream
            .write_all(&rmp_serde::to_vec_named(status).unwrap())
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_early_exit_carries_bubblewraps_diagnostic_and_keeps_stdin() {
        let directory = tempfile::tempdir().unwrap();
        let control = Control::bind(&directory.path().join("control"), Some(1)).unwrap();
        let diagnostic = Id::new_v4().to_string();
        let mut child = shell(&format!("echo {diagnostic} >&2; exit 3"));
        let error = control.ready(&mut child).await.unwrap_err();
        let StartError::Exited {
            status,
            diagnostic: reported,
        } = &error
        else {
            panic!("{error:?}");
        };
        assert_eq!(status.code(), Some(3));
        assert_eq!(*reported, diagnostic);
        assert_eq!(error.kind(), Some(ErrorKind::Unavailable));
        assert!(child.stdin.is_some());
    }

    #[tokio::test]
    async fn a_descendant_holding_stderr_cannot_stall_the_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let control = Control::bind(&directory.path().join("control"), Some(1)).unwrap();
        let diagnostic = Id::new_v4().to_string();
        // The descendant outlives any reasonable wait; it ends on its own.
        let mut child = shell(&format!("echo {diagnostic} >&2; sleep 60 & exit 3"));
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            control.ready(&mut child),
        )
        .await
        .expect("an open stderr must not hold up the result")
        .unwrap_err();
        let StartError::Exited {
            diagnostic: reported,
            ..
        } = &error
        else {
            panic!("{error:?}");
        };
        assert_eq!(*reported, diagnostic);
    }

    #[tokio::test]
    async fn the_helpers_failure_reaches_the_client_intact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let control = Control::bind(&path, Some(1)).unwrap();
        let mut child = shell("read line");
        let sent = SandboxError::NotEnforced.report();
        send(&path, &Status::Failed(sent.clone())).await;
        let error = control.ready(&mut child).await.unwrap_err();
        let StartError::Failed(received) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*received, sent);
        // The kind is the helper's, which travels inside the report.
        assert_eq!(error.kind(), None);
        assert!(child.stdin.is_some());
        control.stop().await;
    }

    #[tokio::test]
    async fn a_report_sent_before_exiting_wins_over_the_exit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let control = Control::bind(&path, Some(1)).unwrap();
        let sent = SandboxError::NotEnforced.report();
        send(&path, &Status::Failed(sent.clone())).await;
        let mut child = shell("exit 1");
        let stdin = child.stdin.take();
        child.wait().await.unwrap();
        child.stdin = stdin;
        // Both the report and the exit are ready before readiness is polled.
        let error = control.ready(&mut child).await.unwrap_err();
        let StartError::Failed(received) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*received, sent);
    }

    #[tokio::test]
    async fn readiness_requires_this_gateways_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let control = Control::bind(&path, Some(1)).unwrap();
        let mut child = shell("read line");
        send(
            &path,
            &Status::Ready {
                version: SPEC_VERSION + 1,
            },
        )
        .await;
        assert!(matches!(
            control.ready(&mut child).await,
            Err(StartError::Version { .. })
        ));
        let path = directory.path().join("ready");
        let control = Control::bind(&path, Some(1)).unwrap();
        send(
            &path,
            &Status::Ready {
                version: SPEC_VERSION,
            },
        )
        .await;
        control.ready(&mut child).await.unwrap();
    }

    #[tokio::test]
    async fn the_bridge_relays_both_ways_to_the_loopback_proxy() {
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = proxy.local_addr().unwrap().port();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let control = Control::bind(&path, Some(port)).unwrap();
        let mut inside = tokio::net::UnixStream::connect(path.join("proxy.sock"))
            .await
            .unwrap();
        let request = Id::new_v4().to_string();
        inside.write_all(request.as_bytes()).await.unwrap();
        let (mut upstream, _) = proxy.accept().await.unwrap();
        let mut received = vec![0; request.len()];
        upstream.read_exact(&mut received).await.unwrap();
        assert_eq!(received, request.as_bytes());
        let response = Id::new_v4().to_string();
        upstream.write_all(response.as_bytes()).await.unwrap();
        drop(upstream);
        let mut returned = String::new();
        inside.read_to_string(&mut returned).await.unwrap();
        assert_eq!(returned, response);
        control.stop().await;
    }

    #[tokio::test]
    async fn without_a_proxy_there_is_no_way_out() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let control = Control::bind(&path, None).unwrap();
        assert!(path.join("status.sock").exists());
        assert!(!path.join("proxy.sock").exists());
        control.stop().await;
    }
}
