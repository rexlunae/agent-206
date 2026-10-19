//! MCP servers are untrusted programs. Each runs in its own process group under
//! OS isolation with a private home, no inherited environment and only the
//! authority its entry declares. Clearing the environment is not a sandbox, so
//! platforms without an isolation facility refuse to start servers.
use crate::{Limits, McpError, Session, bounded::LineLimited};
use a206_core::{Capability, Ignore, McpServerConfig};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
};
use tokio::process::{Child, Command};

/// A running server process. Dropping it kills the server's process group.
pub struct Launch {
    child: Child,
    #[cfg(unix)]
    group: Mutex<Option<nix::unistd::Pid>>,
    /// Dropping it stops listening for the sandbox helper.
    #[cfg(target_os = "linux")]
    control: a206_sandbox::Control,
    _scratch: tempfile::TempDir,
}
impl Launch {
    /// Kill the server and any processes it started.
    pub fn kill(&self) {
        #[cfg(unix)]
        if let Some(group) = self.group.lock().ok().and_then(|mut group| group.take()) {
            // The group may already have exited, and there is no caller to report to.
            nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL).ignore();
        }
    }
}
impl Drop for Launch {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Start `server` in its sandbox and initialize an MCP session with it.
/// `project_root` is the bound project's directory, for workspace access;
/// `state_root` is private application state the server must never see.
/// `programs` are canonical directories mounted read-only and executable,
/// with their programs on `PATH`; the caller checks them first and passes
/// them only to an entry with `program_directories` set.
pub async fn spawn(
    server: &McpServerConfig,
    project_root: Option<&Path>,
    state_root: &Path,
    programs: &[PathBuf],
    limits: Limits,
) -> Result<Session, McpError> {
    let (launch, stdout, stdin) = launch(server, project_root, state_root, programs).await?;
    Session::start(
        (LineLimited::new(stdout, limits.max_line_bytes), stdin),
        Some(launch),
        limits,
    )
    .await
}

struct Placement {
    executable: PathBuf,
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux")),
        expect(dead_code, reason = "read by the sandbox profile")
    )]
    root: PathBuf,
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "linux")),
        expect(dead_code, reason = "read by the sandbox profile")
    )]
    state: PathBuf,
    workspace: Option<PathBuf>,
}

fn place(
    server: &McpServerConfig,
    project_root: Option<&Path>,
    state_root: &Path,
) -> Result<Placement, McpError> {
    let missing = |what| move |source| McpError::Missing { what, source };
    let root = std::fs::canonicalize(&server.runtime_root)
        .map_err(missing("MCP server installation directory"))?;
    let executable =
        std::fs::canonicalize(&server.executable).map_err(missing("MCP server executable"))?;
    if !executable.is_file() || !executable.starts_with(&root) {
        return Err(McpError::Placement(
            "MCP server executable must be a file inside its installation directory",
        ));
    }
    let state = std::fs::canonicalize(state_root).map_err(missing("application state"))?;
    if state.starts_with(&root) || root.starts_with(&state) {
        return Err(McpError::Placement(
            "MCP server installation must be separate from private application state",
        ));
    }
    if let Some(home) = std::env::var_os("HOME")
        && Path::new(&home).starts_with(&root)
    {
        return Err(McpError::Placement(
            "MCP server installation directory must not expose the user's home directory",
        ));
    }
    let workspace = if server.capabilities.contains(&Capability::ReadWorkspace) {
        let project = project_root.ok_or(McpError::ProjectRequired)?;
        let project = std::fs::canonicalize(project).map_err(missing("project directory"))?;
        if !project.is_dir()
            || project.starts_with(&state)
            || project.starts_with(&root)
            || root.starts_with(&project)
        {
            return Err(McpError::Placement(
                "MCP server installation and project directories must not overlap private state or each other",
            ));
        }
        Some(project)
    } else {
        None
    };
    Ok(Placement {
        executable,
        root,
        state,
        workspace,
    })
}

type Pipes = (
    Launch,
    tokio::process::ChildStdout,
    tokio::process::ChildStdin,
);

/// Cancelling this kills the server it was starting.
async fn launch(
    server: &McpServerConfig,
    project_root: Option<&Path>,
    state_root: &Path,
    programs: &[PathBuf],
) -> Result<Pipes, McpError> {
    let placement = place(server, project_root, state_root)?;
    let scratch = tempfile::Builder::new()
        .prefix("a206-mcp-")
        .tempdir()
        .map_err(McpError::Setup)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(McpError::Setup)?;
    }
    let scratch_root = std::fs::canonicalize(scratch.path()).map_err(McpError::Setup)?;
    let home = scratch_root.join("home");
    let tmp = scratch_root.join("tmp");
    let cwd = placement
        .workspace
        .clone()
        .unwrap_or_else(|| scratch_root.join("workspace"));
    for dir in [&home, &tmp] {
        std::fs::create_dir(dir).map_err(McpError::Setup)?;
    }
    if placement.workspace.is_none() {
        std::fs::create_dir(&cwd).map_err(McpError::Setup)?;
    }
    let mut command = sandbox_command(server, &placement, &scratch_root, &cwd, programs)?;
    #[cfg(target_os = "linux")]
    let control = a206_sandbox::Control::bind(&scratch_root.join("control"), None)
        .map_err(McpError::SandboxStart)?;
    let bin = placement
        .executable
        .parent()
        .ok_or(McpError::Placement(
            "MCP server executable must be a file inside its installation directory",
        ))?
        .to_str()
        .ok_or(McpError::NonUtf8Path)?;
    let mut path = std::ffi::OsString::from(bin);
    for directory in a206_sandbox::program_search_path(programs) {
        path.push(":");
        path.push(directory);
    }
    path.push(":/usr/bin:/bin");
    command
        .args(&server.args)
        .env_clear()
        .current_dir(&cwd)
        // Entry variables first, so the fixed ones below always win; `validate`
        // also refuses entries that try to set them.
        .envs(&server.env)
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("PATH", path)
        .env("LANG", "en_US.UTF-8")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    // Bubblewrap and the helper report setup failures here before the server
    // starts; the server's own stderr stays discarded.
    #[cfg(target_os = "linux")]
    command.stderr(Stdio::piped());
    let child = command.spawn().map_err(McpError::Spawn)?;
    #[cfg(unix)]
    let group = nix::unistd::Pid::from_raw(
        child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .ok_or(McpError::ProcessId)?,
    );
    // Dropping `launch` on failure kills whatever the sandbox started.
    let mut launch = Launch {
        child,
        #[cfg(unix)]
        group: Mutex::new(Some(group)),
        #[cfg(target_os = "linux")]
        control,
        _scratch: scratch,
    };
    #[cfg(target_os = "linux")]
    launch
        .control
        .ready(&mut launch.child)
        .await
        .map_err(McpError::SandboxStart)?;
    // Both are piped above, so the handles are always present.
    let (Some(stdout), Some(stdin)) = (launch.child.stdout.take(), launch.child.stdin.take())
    else {
        unreachable!("stdio is piped")
    };
    Ok((launch, stdout, stdin))
}

/// `a206_sandbox`: bubblewrap and Landlock on Linux, where the helper is this
/// executable, which must call `a206_sandbox::dispatch` first; Seatbelt on
/// macOS.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sandbox_command(
    server: &McpServerConfig,
    placement: &Placement,
    scratch: &Path,
    cwd: &Path,
    programs: &[PathBuf],
) -> Result<Command, McpError> {
    let helper = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(McpError::Helper)?;
    let holds = |c| server.capabilities.contains(&c);
    let mut hidden = a206_sandbox::hidden_home_paths();
    hidden.push(placement.state.clone());
    let control = scratch.join("control");
    let profile = a206_sandbox::Profile {
        helper: &helper,
        executable: &placement.executable,
        root: &placement.root,
        scratch,
        control: &control,
        workspace: placement
            .workspace
            .as_deref()
            .map(|workspace| (workspace, holds(Capability::WriteWorkspace))),
        execute: holds(Capability::Execute),
        programs,
        hidden: &hidden,
        cwd,
        network: if holds(Capability::Network) {
            a206_sandbox::NetworkAccess::Host
        } else {
            a206_sandbox::NetworkAccess::None
        },
        // MCP servers' stderr is discarded on every platform.
        stderr: false,
        terminal: None,
    };
    Ok(Command::from(profile.command().map_err(McpError::Sandbox)?))
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sandbox_command(
    _server: &McpServerConfig,
    _placement: &Placement,
    _scratch: &Path,
    _cwd: &Path,
    _programs: &[PathBuf],
) -> Result<Command, McpError> {
    Err(McpError::Unsupported)
}
