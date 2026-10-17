//! Native command execution for the run engine: one [`ToolHost`] tool that
//! runs a shell command inside the OS sandbox (`a206_sandbox`: bubblewrap and
//! Landlock on Linux, Seatbelt on macOS).
//!
//! The tool declares only [`Capability::Execute`], and its host policy is
//! `Ask`, so every call waits for the user's approval of that exact command
//! and runs only in a conversation granted Execute. Inside the sandbox the
//! command holds nothing beyond the conversation's scope: the project is
//! readable with `ReadWorkspace` and writable with `WriteWorkspace`, the host
//! network is shared only with `Network`, and gateway state and the
//! credential stores under `$HOME` stay hidden even inside the project. The
//! environment is empty apart from a private home, temporary directory and a
//! fixed `PATH`.
//!
//! macOS has no PID namespace, so the shell's process group is killed when it
//! exits; a process that leaves that group (with `setsid`) can outlive the
//! call there, though it stays inside the same Seatbelt policy.
//!
//! Platforms without that isolation do not offer the tool: they report it
//! unavailable rather than run commands unconfined.
use crate::tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec};
use a206_core::{Capability, ErrorKind, ErrorReport, ToolPermission, UnavailableTools};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// The tool's model-facing name, as RustyClaw named it.
pub const NAME: &str = "execute_command";
/// The source named when the tool is unavailable.
pub const SOURCE: &str = "native exec";
/// The command's locale; macOS has no `C.UTF-8`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const LOCALE: &str = if cfg!(target_os = "macos") {
    "en_US.UTF-8"
} else {
    "C.UTF-8"
};
/// Seconds a command may run when the call names no timeout.
pub const DEFAULT_TIMEOUT: u64 = 30;
/// Longest timeout a call may ask for.
pub const MAX_TIMEOUT: u64 = 600;
/// Longest command accepted.
pub const MAX_COMMAND: usize = 16 * 1024;
/// Output kept from each of stdout and stderr; the rest is read and dropped
/// so a chatty command cannot stall on a full pipe.
pub const MAX_STREAM: usize = crate::tools::MAX_OUTPUT / 2 - 256;

/// Why a command did not run to completion.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("command execution requires the Execute grant and approval of this exact call")]
    Denied,
    #[error("the project is inside protected gateway or credential state")]
    ProtectedWorkspace,
    #[error("`{0}` must be a string")]
    NotAString(&'static str),
    #[error("`command` is required")]
    MissingCommand,
    #[error("the command is empty, longer than {MAX_COMMAND} bytes or contains a NUL byte")]
    Command,
    #[error("`timeout_secs` must be a whole number from 1 to {MAX_TIMEOUT}")]
    Timeout,
    #[error("`working_dir` must be a relative path inside the project")]
    WorkingDir,
    #[error("`working_dir` needs a project and the ReadWorkspace grant")]
    NoWorkspace,
    #[error("unknown argument `{0}`")]
    Unknown(String),
    #[error("{} is missing or inaccessible", path.display())]
    Missing {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not locate this executable, which confines commands")]
    Helper(#[source] std::io::Error),
    #[error("could not prepare the command's private directory")]
    Setup(#[source] std::io::Error),
    #[error("could not prepare OS isolation for the command")]
    Sandbox(#[source] a206_sandbox::SandboxError),
    #[error("the command's sandbox did not start")]
    Start(#[source] a206_sandbox::StartError),
    #[error("could not start the command's sandbox")]
    Spawn(#[source] std::io::Error),
    #[error("could not read the command's output")]
    Output(#[source] std::io::Error),
    #[error("the command did not finish within {seconds} s and was stopped")]
    TimedOut { seconds: u64 },
    #[error(
        "commands are disabled on this platform until equivalent OS filesystem and network isolation is implemented"
    )]
    Unsupported,
}
impl crate::error::Classified for ExecError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Denied | Self::ProtectedWorkspace => ErrorKind::PermissionDenied,
            Self::NotAString(_)
            | Self::MissingCommand
            | Self::Command
            | Self::Timeout
            | Self::WorkingDir
            | Self::NoWorkspace
            | Self::Unknown(_) => ErrorKind::InvalidRequest,
            // Absent and unreadable paths need different recovery.
            Self::Missing { .. } => return None,
            Self::Helper(_) | Self::Setup(_) | Self::Output(_) => ErrorKind::Internal,
            // The sandbox's own errors know their kinds.
            Self::Sandbox(_) | Self::Start(_) => return None,
            Self::Spawn(_) | Self::Unsupported => ErrorKind::Unavailable,
            Self::TimedOut { .. } => ErrorKind::Timeout,
        })
    }
}

/// One validated call.
#[derive(Debug, PartialEq, Eq)]
struct Call {
    command: String,
    working_dir: Option<PathBuf>,
    timeout: u64,
}

fn parse(arguments: &Map<String, Value>) -> Result<Call, ExecError> {
    let text = |key: &'static str| match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ExecError::NotAString(key)),
    };
    if let Some(unknown) = arguments
        .keys()
        .find(|key| !matches!(key.as_str(), "command" | "working_dir" | "timeout_secs"))
    {
        return Err(ExecError::Unknown(unknown.clone()));
    }
    let command = text("command")?.ok_or(ExecError::MissingCommand)?;
    if command.trim().is_empty() || command.len() > MAX_COMMAND || command.contains('\0') {
        return Err(ExecError::Command);
    }
    let working_dir = match text("working_dir")? {
        None => None,
        Some(dir) => {
            let dir = PathBuf::from(dir);
            let inside = dir.components().all(|c| {
                matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            });
            if !inside || dir.as_os_str().is_empty() {
                return Err(ExecError::WorkingDir);
            }
            Some(dir)
        }
    };
    let timeout = match arguments.get("timeout_secs") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT,
        Some(value) => value
            .as_u64()
            .filter(|t| (1..=MAX_TIMEOUT).contains(t))
            .ok_or(ExecError::Timeout)?,
    };
    Ok(Call {
        command,
        working_dir,
        timeout,
    })
}

/// Runs commands for native runs. `state_root` is the gateway's private
/// state, which commands never see.
pub struct ExecHost {
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    state_root: PathBuf,
}
impl ExecHost {
    pub fn new(state_root: &Path) -> Self {
        Self {
            state_root: state_root.to_owned(),
        }
    }
    fn spec() -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: format!(
                "Run a shell command with `sh -c` in an OS sandbox and return its exit status, \
                 stdout and stderr (each cut to {MAX_STREAM} bytes). It starts in the project \
                 directory, or a private empty one without project access. It can read the \
                 project only with read access, change it only with write access and use the \
                 network only with network access. Nothing else on the computer is visible, \
                 and the environment is empty. Every call needs the user's approval."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command, run with sh -c."
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Directory to start in, relative to the project root."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT,
                        "description": format!("Seconds before the command is stopped (default {DEFAULT_TIMEOUT}).")
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            capabilities: [Capability::Execute].into(),
            permission: ToolPermission::Ask,
            whole_output: false,
        }
    }
}
impl ToolHost for ExecHost {
    fn list<'a>(&'a self, _scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            if cfg!(any(target_os = "linux", target_os = "macos")) {
                ToolListing {
                    tools: vec![Self::spec()],
                    unavailable: vec![],
                }
            } else {
                ToolListing {
                    tools: vec![],
                    unavailable: vec![UnavailableTools {
                        source: SOURCE.into(),
                        error: crate::error::report(&ExecError::Unsupported),
                    }],
                }
            }
        })
    }
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            let result = async {
                if request.name != NAME
                    || !request.approved
                    || !scope.capabilities.contains(&Capability::Execute)
                {
                    return Err(ExecError::Denied);
                }
                let call = parse(&request.arguments)?;
                self.run(scope, call).await
            };
            result.await.map_err(|error| crate::error::report(&error))
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl ExecHost {
    async fn run(&self, _scope: &ToolScope, _call: Call) -> Result<ToolOutput, ExecError> {
        Err(ExecError::Unsupported)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ExecHost {
    /// Cancel-safe: dropping the future kills the sandbox and everything in it.
    async fn run(&self, scope: &ToolScope, call: Call) -> Result<ToolOutput, ExecError> {
        use a206_core::Ignore;
        use std::process::Stdio;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(call.timeout);
        let holds = |c| scope.capabilities.contains(&c);
        let workspace = match &scope.workspace {
            Some(workspace) if holds(Capability::ReadWorkspace) => Some(
                std::fs::canonicalize(workspace).map_err(|source| ExecError::Missing {
                    path: workspace.clone(),
                    source,
                })?,
            ),
            _ => None,
        };
        let scratch = tempfile::Builder::new()
            .prefix("a206-exec-")
            .tempdir()
            .map_err(ExecError::Setup)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(ExecError::Setup)?;
        }
        let root = std::fs::canonicalize(scratch.path()).map_err(ExecError::Setup)?;
        let (home, tmp) = (root.join("home"), root.join("tmp"));
        for dir in [&home, &tmp, &root.join("workspace")] {
            std::fs::create_dir(dir).map_err(ExecError::Setup)?;
        }
        let cwd = match (&workspace, &call.working_dir) {
            (Some(workspace), Some(dir)) => {
                let cwd = workspace.join(dir);
                // A symlink must not lead the start directory out of the project.
                let resolved = std::fs::canonicalize(&cwd)
                    .map_err(|source| ExecError::Missing { path: cwd, source })?;
                if !resolved.starts_with(workspace) || !resolved.is_dir() {
                    return Err(ExecError::WorkingDir);
                }
                resolved
            }
            (Some(workspace), None) => workspace.clone(),
            (None, Some(_)) => return Err(ExecError::NoWorkspace),
            (None, None) => root.join("workspace"),
        };
        let helper = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(ExecError::Helper)?;
        let shell = std::fs::canonicalize("/bin/sh").map_err(|source| ExecError::Missing {
            path: "/bin/sh".into(),
            source,
        })?;
        let mut hidden = a206_sandbox::hidden_home_paths();
        hidden.push(
            std::fs::canonicalize(&self.state_root).unwrap_or_else(|_| self.state_root.clone()),
        );
        if let Some(workspace) = &workspace {
            reject_protected_workspace(workspace, &hidden)?;
        }
        let control = root.join("control");
        let profile = a206_sandbox::Profile {
            helper: &helper,
            executable: &shell,
            // Only the shell itself; other programs come with `execute`.
            root: &shell,
            scratch: &root,
            control: &control,
            workspace: workspace
                .as_deref()
                .map(|workspace| (workspace, holds(Capability::WriteWorkspace))),
            // The tool's own declaration: the call was refused without Execute.
            execute: true,
            hidden: &hidden,
            cwd: &cwd,
            network: if holds(Capability::Network) {
                a206_sandbox::NetworkAccess::Host
            } else {
                a206_sandbox::NetworkAccess::None
            },
            stderr: true,
        };
        let mut command =
            tokio::process::Command::from(profile.command().map_err(ExecError::Sandbox)?);
        let control = a206_sandbox::Control::bind(&control, None).map_err(ExecError::Start)?;
        command
            .args(["-c", &call.command])
            .env_clear()
            .current_dir(&cwd)
            .env("HOME", &home)
            .env("TMPDIR", &tmp)
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", LOCALE)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Bubblewrap reports setup failures here before the command starts.
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(ExecError::Spawn)?;
        let group = Group(std::sync::Mutex::new(
            child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .map(nix::unistd::Pid::from_raw),
        ));
        let finished = tokio::time::timeout_at(deadline, async {
            control.ready(&mut child).await.map_err(ExecError::Start)?;
            let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
                unreachable!("stdout and stderr are piped")
            };
            // On Linux the sandbox's PID namespace ends with the shell. macOS
            // has none, so the shell's group is killed once it exits: a
            // leftover background process cannot hold the pipes open.
            let exited = async {
                let status = child.wait().await;
                group.kill();
                status
            };
            let (status, out, err) = tokio::join!(exited, bounded(stdout), bounded(stderr));
            Ok::<_, ExecError>((status.map_err(ExecError::Output)?, out?, err?))
        })
        .await;
        drop(group);
        control.stop().await;
        let (status, (out, out_cut), (err, err_cut)) = match finished {
            Ok(result) => result?,
            Err(_) => {
                // Killed through `group` above; reap what is left.
                child.kill().await.ignore();
                return Err(ExecError::TimedOut {
                    seconds: call.timeout,
                });
            }
        };
        Ok(ToolOutput {
            text: format!(
                "{status}\n{}\n{}",
                stream_text("stdout", &out, out_cut),
                stream_text("stderr", &err, err_cut)
            ),
            is_error: !status.success(),
        })
    }
}

/// The sandbox's process group, killed when dropped.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn reject_protected_workspace(workspace: &Path, hidden: &[PathBuf]) -> Result<(), ExecError> {
    for path in hidden {
        // Missing credential stores cannot contain a canonical workspace.
        // Existing symlink aliases must be resolved before comparison.
        let protected = match std::fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(ExecError::Missing {
                    path: path.clone(),
                    source,
                });
            }
        };
        if workspace.starts_with(protected) {
            return Err(ExecError::ProtectedWorkspace);
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn stream_text(name: &str, bytes: &[u8], cut: bool) -> String {
    let decoded = String::from_utf8_lossy(bytes);
    let mut end = decoded.len().min(MAX_STREAM);
    while !decoded.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = format!("--- {name} ---\n{}", &decoded[..end]);
    if cut || end < decoded.len() {
        text.push_str(&format!("\n[{name} cut to {MAX_STREAM} bytes]"));
    }
    text
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Group(std::sync::Mutex<Option<nix::unistd::Pid>>);
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Group {
    /// Kill the group once; later calls do nothing.
    fn kill(&self) {
        use a206_core::Ignore;
        let group = self.0.lock().ok().and_then(|mut group| group.take());
        if let Some(group) = group {
            // The group may already have exited, and there is no caller to report to.
            nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL).ignore();
        }
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Read a stream to its end, keeping at most [`MAX_STREAM`] bytes.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn bounded(
    mut stream: impl tokio::io::AsyncRead + Unpin,
) -> Result<(Vec<u8>, bool), ExecError> {
    use tokio::io::AsyncReadExt;
    let mut kept = Vec::new();
    let mut cut = false;
    let mut buffer = [0u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.map_err(ExecError::Output)?;
        if read == 0 {
            return Ok((kept, cut));
        }
        let room = MAX_STREAM - kept.len();
        kept.extend_from_slice(&buffer[..read.min(room)]);
        cut |= read > room;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else {
            panic!("arguments are an object")
        };
        map
    }

    #[test]
    fn binary_output_preserves_room_for_both_streams() {
        let out = stream_text("stdout", &vec![0xff; 40_000], false);
        let err = stream_text("stderr", &vec![0xfe; 40_000], false);
        let result = format!("exit status: 255\n{out}\n{err}");
        assert!(result.len() <= 64 * 1024);
        assert!(result.contains("[stdout cut to"));
        assert!(result.contains("[stderr cut to"));
        assert!(result.contains("--- stderr ---"));
    }

    #[test]
    fn protected_state_cannot_be_a_project_root_or_ancestor() {
        let project = tempfile::tempdir().unwrap();
        let state = project.path().join("state");
        std::fs::create_dir_all(state.join("child")).unwrap();
        let project = std::fs::canonicalize(project.path()).unwrap();
        let state = std::fs::canonicalize(state).unwrap();
        let hidden = vec![state.clone()];
        assert!(reject_protected_workspace(&project, &hidden).is_ok());
        assert!(matches!(
            reject_protected_workspace(&state, &hidden),
            Err(ExecError::ProtectedWorkspace)
        ));
        assert!(matches!(
            reject_protected_workspace(&state.join("child"), &hidden),
            Err(ExecError::ProtectedWorkspace)
        ));
    }

    #[tokio::test]
    async fn direct_host_calls_cannot_skip_authorization() {
        let state = tempfile::tempdir().unwrap();
        let host = ExecHost::new(state.path());
        for (approved, capabilities) in [
            (false, [Capability::Execute].into()),
            (true, Default::default()),
        ] {
            let scope = ToolScope {
                thread: a206_core::Id::new_v4(),
                workspace: None,
                capabilities,
            };
            let request = ToolRequest {
                call: a206_core::Id::new_v4().to_string(),
                name: NAME.into(),
                arguments: arguments(json!({"command": "exit 0"})),
                approved,
            };
            assert_eq!(
                host.call(&scope, request).await.unwrap_err().kind,
                ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn calls_are_checked_before_anything_runs() {
        let command = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            parse(&arguments(
                json!({ "command": command, "working_dir": "src/./x", "timeout_secs": 5 })
            ))
            .unwrap(),
            Call {
                command: command.clone(),
                working_dir: Some("src/./x".into()),
                timeout: 5,
            }
        );
        assert_eq!(
            parse(&arguments(json!({ "command": command })))
                .unwrap()
                .timeout,
            DEFAULT_TIMEOUT
        );
        let refused = |value: Value| {
            let error = parse(&arguments(value)).unwrap_err();
            assert_eq!(
                crate::error::report(&error).kind,
                ErrorKind::InvalidRequest,
                "{error}"
            );
        };
        refused(json!({}));
        refused(json!({ "command": " " }));
        refused(json!({ "command": "a\0b" }));
        refused(json!({ "command": 1 }));
        refused(json!({ "command": "x".repeat(MAX_COMMAND + 1) }));
        refused(json!({ "command": command, "extra": true }));
        for dir in ["../x", "/tmp", "a/../../b", ""] {
            refused(json!({ "command": command, "working_dir": dir }));
        }
        for timeout in [
            json!(0),
            json!(MAX_TIMEOUT + 1),
            json!(-1),
            json!(1.5),
            json!("5"),
        ] {
            refused(json!({ "command": command, "timeout_secs": timeout }));
        }
    }

    #[tokio::test]
    async fn every_call_needs_approval_and_the_execute_grant() {
        let state = tempfile::tempdir().unwrap();
        let host = ExecHost::new(state.path());
        let scope = ToolScope {
            thread: a206_core::Id::new_v4(),
            workspace: None,
            capabilities: Default::default(),
        };
        let listing = host.list(&scope).await;
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let [spec] = listing.tools.as_slice() else {
                panic!("{listing:?}")
            };
            assert_eq!(spec.permission, ToolPermission::Ask);
            assert_eq!(spec.capabilities, [Capability::Execute].into());
        } else {
            assert!(listing.tools.is_empty());
            assert_eq!(listing.unavailable[0].error.kind, ErrorKind::Unavailable);
        }
    }
}
