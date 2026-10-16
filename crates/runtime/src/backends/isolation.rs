//! Fail closed outside supported OS isolation. Clearing environment is not a sandbox.
//! macOS uses a generated Seatbelt profile; Linux uses `a206_sandbox`
//! (bubblewrap namespaces with Landlock inside). Other platforms refuse.
use super::{BackendError, Result, check};
use a206_core::*;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::{Child, Command};
pub struct Process {
    pub child: Child,
    pub cwd: PathBuf,
    pub workspace: Option<PathBuf>,
    _scratch: tempfile::TempDir,
    #[cfg(unix)]
    group: Option<nix::unistd::Pid>,
    #[cfg(target_os = "linux")]
    control: Option<linux::Control>,
}
impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Some(group) = self.group.take() {
                // Cleanup: the group may already have exited, and there is no caller to report to.
                nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL).ignore();
            }
        }
    }
}
impl Process {
    pub async fn launch(
        config: &BackendConfig,
        thread: &Thread,
        workspace: Option<&Path>,
        state_root: &Path,
        port: u16,
        token: &str,
    ) -> Result<Self> {
        let scratch = tempfile::Builder::new()
            .prefix("a206-backend-")
            .tempdir()
            .map_err(BackendError::Setup)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(BackendError::Setup)?;
        }
        let scratch_root = std::fs::canonicalize(scratch.path()).map_err(BackendError::Setup)?;
        let missing = |what| move |source| BackendError::Missing { what, source };
        let root = std::fs::canonicalize(&config.runtime_root)
            .map_err(missing("backend installation directory"))?;
        let executable =
            std::fs::canonicalize(&config.executable).map_err(missing("backend executable"))?;
        check(
            executable.is_file() && executable.starts_with(&root),
            BackendError::Placement("executable must be a file inside its installation directory"),
        )?;
        let state_root = std::fs::canonicalize(state_root).map_err(missing("application state"))?;
        check(
            !state_root.starts_with(&root) && !root.starts_with(&state_root),
            BackendError::Placement(
                "backend installation must be separate from private application state",
            ),
        )?;
        if let Some(home) = std::env::var_os("HOME") {
            check(
                !Path::new(&home).starts_with(&root),
                BackendError::Placement(
                    "backend installation directory must not expose the user's home directory",
                ),
            )?;
        }
        let script = config
            .adapter_script
            .as_ref()
            .map(std::fs::canonicalize)
            .transpose()
            .map_err(missing("Claude ACP adapter entry point"))?;
        if let Some(script) = &script {
            check(
                script.is_file() && script.starts_with(&root),
                BackendError::Placement("adapter script must be inside its installation directory"),
            )?;
        }
        let project = workspace
            .map(std::fs::canonicalize)
            .transpose()
            .map_err(missing("project directory"))?;
        if let Some(project) = &project {
            check(
                !project.starts_with(&root) && !root.starts_with(project),
                BackendError::Placement(
                    "backend installation and project directories must not overlap",
                ),
            )?;
        }
        let workspace = if thread
            .grant
            .capabilities
            .contains(&Capability::ReadWorkspace)
        {
            let path = project.ok_or(BackendError::Placement(
                "project access requires an associated project",
            ))?;
            check(
                path.is_dir() && !path.starts_with(&state_root),
                BackendError::Placement("invalid backend workspace"),
            )?;
            Some(path)
        } else {
            None
        };
        // Start outside the user's project so engines cannot discover ambient project
        // settings, hooks, plugins or MCP servers during their own initialization.
        let cwd = scratch_root.join("workspace");
        std::fs::create_dir(&cwd).map_err(BackendError::Setup)?;
        let home = scratch_root.join("home");
        std::fs::create_dir(&home).map_err(BackendError::Setup)?;
        let tmp = scratch_root.join("tmp");
        std::fs::create_dir(&tmp).map_err(BackendError::Setup)?;
        #[cfg(target_os = "linux")]
        let control = linux::Control::bind(&scratch_root.join("control"), port)?;
        let mut command = sandbox_command(
            &executable,
            &root,
            &scratch_root,
            workspace.as_deref().unwrap_or(&cwd),
            &state_root,
            thread,
            port,
        )?;
        command
            .env_clear()
            .current_dir(&cwd)
            .env("HOME", &home)
            .env("TMPDIR", &tmp)
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin",
                    executable
                        .parent()
                        .ok_or(BackendError::Placement(
                            "executable must be a file inside its installation directory",
                        ))?
                        .display()
                ),
            )
            .env("LANG", "en_US.UTF-8")
            .env("NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .env("CODEX_HOME", home.join(".codex"))
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match config.kind {
            CodingBackend::Codex => {
                std::fs::create_dir(home.join(".codex")).map_err(BackendError::Setup)?;
                let base_url = format!("http://127.0.0.1:{port}/v1");
                let settings = toml::toml! {
                    model_provider = "agent206"
                    [model_providers.agent206]
                    name = "Agent 206 scoped proxy"
                    base_url = (base_url)
                    wire_api = "responses"
                    env_key = "A206_PROXY_TOKEN"
                    requires_openai_auth = false
                    [features]
                    multi_agent = false
                    apps = false
                    web_search = false
                };
                let settings = toml::to_string(&settings).map_err(BackendError::Configuration)?;
                std::fs::write(home.join(".codex/config.toml"), settings)
                    .map_err(BackendError::Setup)?;
                command.args(["app-server"]).env("A206_PROXY_TOKEN", token);
            }
            CodingBackend::ClaudeAgent => {
                command
                    .arg(script.ok_or(BackendError::Adapter)?)
                    .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
                    // This is a revocable proxy capability, never the upstream API key.
                    .env("ANTHROPIC_API_KEY", token)
                    .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
            }
            CodingBackend::Native => return Err(BackendError::Native),
        }
        #[cfg(unix)]
        command.process_group(0);
        // Bubblewrap and the helper report setup failures here before the
        // runtime starts; the runtime's own stderr stays discarded.
        #[cfg(target_os = "linux")]
        command.stderr(Stdio::piped());
        let child = command.spawn().map_err(BackendError::Spawn)?;
        #[cfg(unix)]
        let group = Some(nix::unistd::Pid::from_raw(
            child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .ok_or(BackendError::ProcessId)?,
        ));
        let mut process = Self {
            child,
            cwd,
            workspace,
            _scratch: scratch,
            #[cfg(unix)]
            group,
            #[cfg(target_os = "linux")]
            control: Some(control),
        };
        // Dropping `process` on failure tears the sandbox down.
        #[cfg(target_os = "linux")]
        if let Some(control) = &process.control {
            control.ready(&mut process.child).await?;
        }
        Ok(process)
    }
    pub async fn stop(&mut self) {
        #[cfg(unix)]
        {
            if let Some(group) = self.group.take() {
                // Cleanup: the group may already have exited, and there is no caller to report to.
                nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL).ignore();
            }
        }
        // Best-effort teardown of a process that may already have exited.
        self.child.kill().await.ignore();
        self.child.wait().await.ignore();
        #[cfg(target_os = "linux")]
        if let Some(control) = self.control.take() {
            control.stop().await;
        }
    }
}
#[cfg(target_os = "macos")]
fn sandbox_command(
    executable: &Path,
    root: &Path,
    scratch: &Path,
    cwd: &Path,
    state: &Path,
    thread: &Thread,
    port: u16,
) -> Result<Command> {
    // A JSON string literal is also a valid SBPL string literal.
    let quote = |path: &Path| -> Result<String> {
        path.to_str()
            .map(|path| serde_json::Value::from(path).to_string())
            .ok_or(BackendError::NonUtf8Path)
    };
    let mut policy = format!(
        r#"(version 1)
(deny default)
(allow process-fork)
(allow process-exec (subpath {}))
(allow signal (target self))
(allow sysctl-read)
(allow system-mac-syscall (mac-policy-name "vnguard"))
(allow system-mac-syscall (require-all (mac-policy-name "Sandbox") (mac-syscall-number 67)))
(allow file-read* (literal "/"))
(allow file-read-metadata (literal "/var") (literal "/tmp"))
(allow mach-lookup (global-name "com.apple.system.logger") (global-name "com.apple.system.opendirectoryd"))
(allow file-map-executable file-read* (subpath "/System/Library") (subpath "/System/iOSSupport/System/Library") (subpath "/usr/lib") (subpath "/usr/share") (subpath "/Library/Apple") (subpath "/private/var/db/dyld") (literal "/dev/null") (literal "/dev/urandom") (literal "/dev/random") (subpath {}) (subpath {}))
(allow file-write* (subpath {}) (literal "/dev/null"))
(allow network-outbound (remote ip "localhost:{}"))
"#,
        quote(root)?,
        quote(root)?,
        quote(scratch)?,
        quote(scratch)?,
        port
    );
    // Runtime traversal needs metadata for exact ancestors, never arbitrary files
    // elsewhere in the user's filesystem. Descendants stay within the read grants.
    let mut ancestors = std::collections::BTreeSet::new();
    for path in [
        root,
        scratch,
        cwd,
        Path::new("/System/Library"),
        Path::new("/System/iOSSupport/System/Library"),
        Path::new("/usr/lib"),
        Path::new("/usr/share"),
        Path::new("/Library/Apple"),
        Path::new("/private/var/db/dyld"),
    ] {
        ancestors.extend(path.ancestors().skip(1));
    }
    for ancestor in ancestors {
        policy.push_str(&format!(
            "(allow file-read-metadata (literal {}))\n",
            quote(ancestor)?
        ));
    }
    if thread
        .grant
        .capabilities
        .contains(&Capability::ReadWorkspace)
    {
        policy.push_str(&format!("(allow file-read* (subpath {}))\n", quote(cwd)?));
    }
    if thread
        .grant
        .capabilities
        .contains(&Capability::WriteWorkspace)
    {
        policy.push_str(&format!("(allow file-write* (subpath {}))\n", quote(cwd)?));
    }
    if thread.grant.capabilities.contains(&Capability::Execute) {
        policy.push_str(
            "(allow process-exec file-read* (subpath \"/bin\") (subpath \"/usr/bin\"))\n",
        );
    }
    // Seatbelt rules are ordered: exclusions must follow broad workspace grants.
    policy.push_str(&format!(
        "(deny file-read* file-write* (subpath {}))\n",
        quote(state)?
    ));
    // Deny ambient agent configuration and common credential stores even inside a broad workspace.
    for hidden in hidden_home_paths() {
        policy.push_str(&format!(
            "(deny file-read* file-write* (subpath {}))\n",
            quote(&hidden)?
        ));
    }
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &policy]).arg(executable);
    Ok(command)
}
/// Credential stores and agent configuration that stay hidden from a runtime
/// even when its granted project contains them. A symlinked home resolves to
/// the canonical paths the grants are compared with.
#[cfg(unix)]
fn hidden_home_paths() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        // A home that cannot be resolved is still denied by its literal path.
        .map(|home| std::fs::canonicalize(&home).unwrap_or_else(|_| PathBuf::from(home)))
        .map(|home| {
            a206_sandbox::HIDDEN_HOME_PATHS
                .iter()
                .map(|suffix| PathBuf::from(&home).join(suffix))
                .collect()
        })
        .unwrap_or_default()
}
#[cfg(target_os = "linux")]
fn sandbox_command(
    executable: &Path,
    root: &Path,
    scratch: &Path,
    cwd: &Path,
    state: &Path,
    thread: &Thread,
    port: u16,
) -> Result<Command> {
    let helper = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(BackendError::Helper)?;
    let capabilities = &thread.grant.capabilities;
    let mut hidden = hidden_home_paths();
    hidden.push(state.to_path_buf());
    let control = scratch.join("control");
    let start = scratch.join("workspace");
    let profile = a206_sandbox::Profile {
        helper: &helper,
        executable,
        root,
        scratch,
        control: &control,
        workspace: capabilities
            .contains(&Capability::ReadWorkspace)
            .then(|| (cwd, capabilities.contains(&Capability::WriteWorkspace))),
        execute: capabilities.contains(&Capability::Execute),
        hidden: &hidden,
        cwd: &start,
        port,
    };
    Ok(Command::from(profile.command()?))
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sandbox_command(
    _executable: &Path,
    _root: &Path,
    _scratch: &Path,
    _cwd: &Path,
    _state: &Path,
    _thread: &Thread,
    _port: u16,
) -> Result<Command> {
    Err(BackendError::Unsupported)
}
#[cfg(target_os = "linux")]
mod linux {
    //! The gateway's side of the Linux sandbox: the Unix socket that carries
    //! proxy connections out of the network namespace, and the helper's report.
    use super::{BackendError, Result};
    use a206_core::Ignore;
    use a206_sandbox::{MAX_STATUS, SPEC_VERSION, Status};
    use std::path::Path;
    use tokio::{
        io::AsyncReadExt,
        net::{TcpStream, UnixListener},
        process::Child,
        task::{JoinHandle, JoinSet},
    };
    /// How long the helper may take to report that the runtime started.
    const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    /// Most bubblewrap diagnostics kept when the sandbox fails to start.
    const MAX_DIAGNOSTIC: u64 = 4096;
    pub struct Control {
        status: UnixListener,
        bridge: JoinHandle<()>,
    }
    impl Drop for Control {
        fn drop(&mut self) {
            // Aborting is the whole teardown; `stop` also awaits it.
            self.bridge.abort();
        }
    }
    impl Control {
        /// Listen in a new, private `control` directory, which the sandbox
        /// mounts read-only so the runtime cannot replace the sockets.
        pub fn bind(control: &Path, port: u16) -> Result<Self> {
            std::fs::create_dir(control).map_err(BackendError::Setup)?;
            let proxy =
                UnixListener::bind(control.join("proxy.sock")).map_err(BackendError::Setup)?;
            let status =
                UnixListener::bind(control.join("status.sock")).map_err(BackendError::Setup)?;
            Ok(Self {
                status,
                bridge: tokio::spawn(bridge(proxy, port)),
            })
        }
        /// Wait until the helper reports the runtime started inside both layers.
        pub async fn ready(&self, child: &mut Child) -> Result<()> {
            let report = async {
                let (stream, _) = self
                    .status
                    .accept()
                    .await
                    .map_err(BackendError::SandboxReport)?;
                let mut bytes = Vec::new();
                stream
                    .take(MAX_STATUS)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(BackendError::SandboxReport)?;
                rmp_serde::from_slice::<Status>(&bytes).map_err(BackendError::SandboxMalformed)
            };
            // `Child::wait` closes stdin, which the runtime still needs.
            let stdin = child.stdin.take();
            let result = tokio::select! {
                status = tokio::time::timeout(READY_TIMEOUT, report) => {
                    match status.map_err(BackendError::SandboxTimeout)?? {
                        Status::Ready { version } if version == SPEC_VERSION => Ok(()),
                        Status::Ready { version } => Err(BackendError::SandboxVersion { version }),
                        Status::Failed(report) => Err(BackendError::SandboxFailed(report)),
                    }
                }
                status = child.wait() => {
                    let status = status.map_err(BackendError::Spawn)?;
                    let mut diagnostic = Vec::new();
                    if let Some(stderr) = child.stderr.take() {
                        // Whatever arrived is the diagnostic; a read error leaves it partial.
                        stderr.take(MAX_DIAGNOSTIC).read_to_end(&mut diagnostic).await.ignore();
                    }
                    Err(BackendError::SandboxExited {
                        status,
                        diagnostic: String::from_utf8_lossy(&diagnostic).trim().to_owned(),
                    })
                }
            };
            child.stdin = stdin;
            result
        }
        pub async fn stop(mut self) {
            self.bridge.abort();
            // Cancellation is the expected outcome; a panic was already reported.
            (&mut self.bridge).await.ignore();
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
    use super::{BackendError, linux::Control};
    use a206_core::{ErrorKind, Id};
    use a206_sandbox::{SPEC_VERSION, SandboxError, Status};
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
        let control = Control::bind(&directory.path().join("control"), 1).unwrap();
        let diagnostic = Id::new_v4().to_string();
        let mut child = shell(&format!("echo {diagnostic} >&2; exit 3"));
        let error = control.ready(&mut child).await.unwrap_err();
        let BackendError::SandboxExited {
            status,
            diagnostic: reported,
        } = &error
        else {
            panic!("{error:?}");
        };
        assert_eq!(status.code(), Some(3));
        assert_eq!(*reported, diagnostic);
        assert_eq!(crate::error::report(&error).kind, ErrorKind::Unavailable);
        assert!(child.stdin.is_some());
    }

    #[tokio::test]
    async fn the_helpers_failure_reaches_the_client_intact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let control = Control::bind(&path, 1).unwrap();
        let mut child = shell("read line");
        let sent = SandboxError::NotEnforced.report();
        send(&path, &Status::Failed(sent.clone())).await;
        let error = control.ready(&mut child).await.unwrap_err();
        let BackendError::SandboxFailed(received) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*received, sent);
        let report = crate::error::report(&error);
        assert_eq!(report.kind, ErrorKind::Unavailable);
        assert_eq!(report.causes().next().unwrap().message, sent.message);
        assert!(child.stdin.is_some());
        control.stop().await;
    }

    #[tokio::test]
    async fn readiness_requires_this_gateways_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control");
        let control = Control::bind(&path, 1).unwrap();
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
            Err(BackendError::SandboxVersion { .. })
        ));
        let path = directory.path().join("ready");
        let control = Control::bind(&path, 1).unwrap();
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
        let control = Control::bind(&path, port).unwrap();
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
}
