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
pub async fn spawn(
    server: &McpServerConfig,
    project_root: Option<&Path>,
    state_root: &Path,
    limits: Limits,
) -> Result<Session, McpError> {
    let (launch, stdout, stdin) = launch(server, project_root, state_root).await?;
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
    let mut command = sandbox_command(server, &placement, &scratch_root, &cwd)?;
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
    command
        .args(&server.args)
        .env_clear()
        .current_dir(&cwd)
        // Entry variables first, so the fixed ones below always win; `validate`
        // also refuses entries that try to set them.
        .envs(&server.env)
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("PATH", format!("{bin}:/usr/bin:/bin"))
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

#[cfg(target_os = "macos")]
fn sandbox_command(
    server: &McpServerConfig,
    placement: &Placement,
    scratch: &Path,
    _cwd: &Path,
) -> Result<Command, McpError> {
    // A JSON string literal is also a valid SBPL string literal.
    let quote = |path: &Path| -> Result<String, McpError> {
        path.to_str()
            .map(|path| serde_json::Value::from(path).to_string())
            .ok_or(McpError::NonUtf8Path)
    };
    let root = quote(&placement.root)?;
    let scratch_quoted = quote(scratch)?;
    let mut policy = format!(
        r#"(version 1)
(deny default)
(allow process-fork)
(allow process-exec (subpath {root}))
(allow signal (target self))
(allow sysctl-read)
(allow system-mac-syscall (mac-policy-name "vnguard"))
(allow system-mac-syscall (require-all (mac-policy-name "Sandbox") (mac-syscall-number 67)))
(allow file-read* (literal "/"))
(allow file-read-metadata (literal "/var") (literal "/tmp") (literal "/etc"))
(allow mach-lookup (global-name "com.apple.system.logger") (global-name "com.apple.system.opendirectoryd"))
(allow file-map-executable file-read* (subpath "/System/Library") (subpath "/System/iOSSupport/System/Library") (subpath "/usr/lib") (subpath "/usr/share") (subpath "/Library/Apple") (subpath "/private/var/db/dyld") (literal "/dev/null") (literal "/dev/urandom") (literal "/dev/random") (subpath {root}) (subpath {scratch_quoted}))
(allow file-write* (subpath {scratch_quoted}) (literal "/dev/null"))
"#
    );
    let mut ancestors = std::collections::BTreeSet::new();
    let mut traversed = vec![
        placement.root.as_path(),
        scratch,
        Path::new("/System/Library"),
        Path::new("/System/iOSSupport/System/Library"),
        Path::new("/usr/lib"),
        Path::new("/usr/share"),
        Path::new("/Library/Apple"),
        Path::new("/private/var/db/dyld"),
    ];
    if let Some(workspace) = &placement.workspace {
        traversed.push(workspace);
    }
    for path in traversed {
        ancestors.extend(path.ancestors().skip(1));
    }
    for ancestor in ancestors {
        policy.push_str(&format!(
            "(allow file-read-metadata (literal {}))\n",
            quote(ancestor)?
        ));
    }
    let holds = |c| server.capabilities.contains(&c);
    if let Some(workspace) = &placement.workspace {
        let workspace = quote(workspace)?;
        policy.push_str(&format!("(allow file-read* (subpath {workspace}))\n"));
        if holds(Capability::WriteWorkspace) {
            policy.push_str(&format!("(allow file-write* (subpath {workspace}))\n"));
        }
    }
    if holds(Capability::Execute) {
        policy.push_str(
            "(allow process-exec file-read* file-map-executable (subpath \"/bin\") (subpath \"/usr/bin\"))\n",
        );
    }
    if holds(Capability::Network) {
        policy.push_str(
            r#"(allow network-outbound)
(allow system-socket)
(allow mach-lookup (global-name "com.apple.dnssd.service") (global-name "com.apple.SystemConfiguration.configd") (global-name "com.apple.trustd.agent"))
(allow file-read* (subpath "/private/etc/ssl") (literal "/private/etc/resolv.conf") (literal "/private/etc/hosts") (literal "/private/etc/services"))
"#,
        );
    }
    // Seatbelt rules are ordered: exclusions must follow broad workspace grants.
    policy.push_str(&format!(
        "(deny file-read* file-write* (subpath {}))\n",
        quote(&placement.state)?
    ));
    // Ambient agent configuration and common credential stores stay hidden even
    // inside a broad workspace.
    if let Some(home) = std::env::var_os("HOME") {
        for suffix in a206_sandbox::HIDDEN_HOME_PATHS {
            policy.push_str(&format!(
                "(deny file-read* file-write* (subpath {}))\n",
                quote(&PathBuf::from(&home).join(suffix))?
            ));
        }
    }
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &policy]).arg(&placement.executable);
    Ok(command)
}
/// Bubblewrap and Landlock (`a206_sandbox`), with the same grants as the
/// macOS profile. The helper is this executable, which must call
/// `a206_sandbox::dispatch` first.
#[cfg(target_os = "linux")]
fn sandbox_command(
    server: &McpServerConfig,
    placement: &Placement,
    scratch: &Path,
    cwd: &Path,
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
        hidden: &hidden,
        cwd,
        network: if holds(Capability::Network) {
            a206_sandbox::NetworkAccess::Host
        } else {
            a206_sandbox::NetworkAccess::None
        },
        // MCP servers' stderr is discarded on every platform.
        stderr: false,
    };
    Ok(Command::from(profile.command().map_err(McpError::Sandbox)?))
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sandbox_command(
    _server: &McpServerConfig,
    _placement: &Placement,
    _scratch: &Path,
    _cwd: &Path,
) -> Result<Command, McpError> {
    Err(McpError::Unsupported)
}
