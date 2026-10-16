//! Fail closed outside supported OS isolation. Clearing environment is not a sandbox.
use a206_core::*;
use anyhow::{Context, Result, ensure};
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
    pub fn launch(
        config: &BackendConfig,
        thread: &Thread,
        workspace: Option<&Path>,
        state_root: &Path,
        port: u16,
        token: &str,
    ) -> Result<Self> {
        let scratch = tempfile::Builder::new().prefix("a206-backend-").tempdir()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        let scratch_root = std::fs::canonicalize(scratch.path())?;
        let root = std::fs::canonicalize(&config.runtime_root)
            .context("backend installation directory is missing")?;
        let executable =
            std::fs::canonicalize(&config.executable).context("backend executable is missing")?;
        ensure!(
            executable.is_file() && executable.starts_with(&root),
            "executable must be a file inside its installation directory"
        );
        let state_root = std::fs::canonicalize(state_root)?;
        ensure!(
            !state_root.starts_with(&root) && !root.starts_with(&state_root),
            "backend installation must be separate from private application state"
        );
        if let Some(home) = std::env::var_os("HOME") {
            ensure!(
                !Path::new(&home).starts_with(&root),
                "backend installation directory must not expose the user's home directory"
            );
        }
        let script = config
            .adapter_script
            .as_ref()
            .map(std::fs::canonicalize)
            .transpose()
            .context("Claude ACP adapter entry point is missing")?;
        if let Some(script) = &script {
            ensure!(
                script.is_file() && script.starts_with(&root),
                "adapter script must be inside its installation directory"
            );
        }
        let project = workspace.map(std::fs::canonicalize).transpose()?;
        if let Some(project) = &project {
            ensure!(
                !project.starts_with(&root) && !root.starts_with(project),
                "backend installation and project directories must not overlap"
            );
        }
        let workspace = if thread
            .grant
            .capabilities
            .contains(&Capability::ReadWorkspace)
        {
            let path = project.context("project access requires an associated project")?;
            ensure!(
                path.is_dir() && !path.starts_with(&state_root),
                "invalid backend workspace"
            );
            Some(path)
        } else {
            None
        };
        // Start outside the user's project so engines cannot discover ambient project
        // settings, hooks, plugins or MCP servers during their own initialization.
        let cwd = scratch_root.join("workspace");
        std::fs::create_dir(&cwd)?;
        let home = scratch_root.join("home");
        std::fs::create_dir(&home)?;
        let tmp = scratch_root.join("tmp");
        std::fs::create_dir(&tmp)?;
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
                        .context("missing executable parent")?
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
                std::fs::create_dir(home.join(".codex"))?;
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
                std::fs::write(home.join(".codex/config.toml"), toml::to_string(&settings)?)?;
                command.args(["app-server"]).env("A206_PROXY_TOKEN", token);
            }
            CodingBackend::ClaudeAgent => {
                command
                    .arg(script.context("Claude ACP adapter script is required")?)
                    .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
                    // This is a revocable proxy capability, never the upstream API key.
                    .env("ANTHROPIC_API_KEY", token)
                    .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
            }
            CodingBackend::Native => anyhow::bail!("Agent 206 has no external runtime"),
        }
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .spawn()
            .context("could not start isolated backend")?;
        #[cfg(unix)]
        let group = Some(nix::unistd::Pid::from_raw(i32::try_from(
            child.id().context("backend has no process ID")?,
        )?));
        Ok(Self {
            child,
            cwd,
            workspace,
            _scratch: scratch,
            #[cfg(unix)]
            group,
        })
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
    let quote = |path: &Path| -> Result<String> {
        Ok(serde_json::to_string(
            path.to_str().context("backend paths must be UTF-8")?,
        )?)
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
    if let Some(home) = std::env::var_os("HOME") {
        for suffix in [
            ".ssh",
            ".aws",
            ".config/gcloud",
            ".codex",
            ".claude",
            ".agent206",
            "Library/Keychains",
        ] {
            policy.push_str(&format!(
                "(deny file-read* file-write* (subpath {}))\n",
                quote(&PathBuf::from(&home).join(suffix))?
            ));
        }
    }
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &policy]).arg(executable);
    Ok(command)
}
#[cfg(not(target_os = "macos"))]
fn sandbox_command(
    _executable: &Path,
    _root: &Path,
    _scratch: &Path,
    _cwd: &Path,
    _state: &Path,
    _thread: &Thread,
    _port: u16,
) -> Result<Command> {
    anyhow::bail!(
        "external coding backends are disabled on this platform until equivalent OS filesystem and network isolation is implemented"
    )
}
