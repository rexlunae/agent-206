//! Fail closed outside supported OS isolation. Clearing environment is not a sandbox.
//! `a206_sandbox` confines the runtime: bubblewrap namespaces with Landlock
//! inside on Linux, a Seatbelt profile on macOS. Other platforms refuse.
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
    control: Option<crate::sandbox::Control>,
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
        let control = crate::sandbox::Control::bind(&scratch_root.join("control"), Some(port))?;
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
        // Only the Linux readiness check below needs `mut`.
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
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
/// `a206_sandbox`: bubblewrap and Landlock on Linux, Seatbelt on macOS.
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
    let mut hidden = a206_sandbox::hidden_home_paths();
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
        network: a206_sandbox::NetworkAccess::Proxy(port),
        stderr: false,
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
