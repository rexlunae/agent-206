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
//! exits; a process that leaves that group (with `setsid`), or one being
//! forked as the group is killed, can outlive the call there, though it
//! stays inside the same Seatbelt policy.
//!
//! With `background`, the command keeps running after the call returns, in
//! the same sandbox and under the same approval, and the conversation that
//! started it follows it with `process` (list, poll, kill, remove) and gives
//! it input with `process_write`, which the user approves like a command
//! (`sessions`). A session stops as soon as its conversation no longer grants
//! what its sandbox was given. With `terminal` a background command runs on
//! its own pseudo-terminal (`terminal`), and `process_write` can also send
//! it named keys such as Ctrl-C (`keys`).
//!
//! Platforms without that isolation do not offer these tools: they report
//! them unavailable rather than run commands unconfined.
// Nothing starts a session where commands are unsupported.
pub mod keys;
pub mod programs;
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
mod sessions;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod terminal;

use crate::tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec};
use a206_core::{Capability, ErrorKind, ErrorReport, ToolPermission, UnavailableTools};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// The tool's model-facing name, as RustyClaw named it.
pub const NAME: &str = "execute_command";
/// Follows background sessions.
pub const PROCESS: &str = "process";
/// Gives a background session input.
pub const PROCESS_WRITE: &str = "process_write";
pub use sessions::{MAX_INPUT, MAX_PER_THREAD, MAX_SESSIONS};
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
/// Seconds a background command may run when the call names no timeout.
pub const DEFAULT_BACKGROUND_TIMEOUT: u64 = 600;
/// Longest a background command may run.
pub const MAX_BACKGROUND_TIMEOUT: u64 = 3600;
/// Columns and rows a session's terminal reports.
pub const TERMINAL_COLUMNS: u16 = 80;
pub const TERMINAL_ROWS: u16 = 24;
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
    #[error("`{0}` must be true or false")]
    NotABool(&'static str),
    #[error("`command` is required")]
    MissingCommand,
    #[error("the command is empty, longer than {MAX_COMMAND} bytes or contains a NUL byte")]
    Command,
    #[error(
        "`timeout_secs` must be a whole number from 1 to {MAX_TIMEOUT}, or to {MAX_BACKGROUND_TIMEOUT} with `background`"
    )]
    Timeout,
    #[error(transparent)]
    Programs(#[from] programs::ProgramsError),
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
    #[error("`action` must be one of list, poll, kill or remove")]
    Action,
    #[error("`session` must name a background session")]
    Session,
    #[error("`data` is required, at most {MAX_INPUT} bytes, unless `keys` or `close` is set")]
    Data,
    #[error("`keys` must be a list of at most {} key names", keys::MAX_KEYS)]
    NotKeys,
    #[error(transparent)]
    Key(#[from] keys::UnknownKey),
    #[error("`terminal` needs `background`")]
    TerminalNeedsBackground,
    #[error("keys need a session started with `terminal`; send text with `data`")]
    KeysNeedTerminal,
    #[error("a terminal's input cannot be closed; send the key Ctrl-D to end it")]
    TerminalClose,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[error("could not give the command a terminal")]
    Terminal(#[source] terminal::TerminalError),
    #[error("no such background session in this conversation")]
    UnknownSession,
    #[error("this conversation already holds {limit} background sessions; remove a finished one")]
    TooManySessions { limit: usize },
    #[error("the session has already finished")]
    SessionFinished,
    #[error("the session no longer takes input")]
    InputClosed,
    #[error("could not give the session its input")]
    Input(#[source] std::io::Error),
    #[error(
        "the command did not take its input within {seconds} s; part of it may have reached the command"
    )]
    InputStalled { seconds: u64 },
    #[error("the conversation's permissions changed while the command started; it was stopped")]
    Revoked,
    #[error(
        "commands are disabled on this platform until equivalent OS filesystem and network isolation is implemented"
    )]
    Unsupported,
}
impl crate::error::Classified for ExecError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Denied | Self::ProtectedWorkspace | Self::Revoked => ErrorKind::PermissionDenied,
            Self::NotAString(_)
            | Self::NotABool(_)
            | Self::Action
            | Self::Session
            | Self::Data
            | Self::NotKeys
            | Self::Key(_)
            | Self::TerminalNeedsBackground
            | Self::MissingCommand
            | Self::Command
            | Self::Timeout
            | Self::WorkingDir
            | Self::NoWorkspace
            | Self::Unknown(_) => ErrorKind::InvalidRequest,
            // Absent and unreadable paths need different recovery.
            Self::Missing { .. } => return None,
            Self::Helper(_) | Self::Setup(_) | Self::Output(_) => ErrorKind::Internal,
            // These errors know their own kinds.
            Self::Sandbox(_) | Self::Start(_) => return None,
            // `transparent` hides it from the chain, so classify it here.
            Self::Programs(error) => return crate::error::classify(error),
            Self::Spawn(_) | Self::Unsupported => ErrorKind::Unavailable,
            Self::TimedOut { .. } | Self::InputStalled { .. } => ErrorKind::Timeout,
            Self::UnknownSession => ErrorKind::NotFound,
            Self::TooManySessions { .. }
            | Self::SessionFinished
            | Self::InputClosed
            | Self::KeysNeedTerminal
            | Self::TerminalClose
            | Self::Input(_) => ErrorKind::Conflict,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Terminal(_) => ErrorKind::Internal,
        })
    }
}

/// One validated call.
#[derive(Debug, PartialEq, Eq)]
struct Call {
    command: String,
    working_dir: Option<PathBuf>,
    timeout: u64,
    background: bool,
    /// Run on a pseudo-terminal; only in the background.
    terminal: bool,
}

fn text(arguments: &Map<String, Value>, key: &'static str) -> Result<Option<String>, ExecError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ExecError::NotAString(key)),
    }
}
fn flag(arguments: &Map<String, Value>, key: &'static str) -> Result<bool, ExecError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(ExecError::NotABool(key)),
    }
}
fn only(arguments: &Map<String, Value>, known: &[&str]) -> Result<(), ExecError> {
    match arguments.keys().find(|key| !known.contains(&key.as_str())) {
        Some(unknown) => Err(ExecError::Unknown(unknown.clone())),
        None => Ok(()),
    }
}

fn parse(arguments: &Map<String, Value>) -> Result<Call, ExecError> {
    only(
        arguments,
        &[
            "command",
            "working_dir",
            "timeout_secs",
            "background",
            "terminal",
        ],
    )?;
    let text = |key| text(arguments, key);
    let background = flag(arguments, "background")?;
    let terminal = flag(arguments, "terminal")?;
    if terminal && !background {
        return Err(ExecError::TerminalNeedsBackground);
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
    let (default, max) = if background {
        (DEFAULT_BACKGROUND_TIMEOUT, MAX_BACKGROUND_TIMEOUT)
    } else {
        (DEFAULT_TIMEOUT, MAX_TIMEOUT)
    };
    let timeout = match arguments.get("timeout_secs") {
        None | Some(Value::Null) => default,
        Some(value) => value
            .as_u64()
            .filter(|t| (1..=max).contains(t))
            .ok_or(ExecError::Timeout)?,
    };
    Ok(Call {
        command,
        working_dir,
        timeout,
        background,
        terminal,
    })
}

/// One validated `process` call.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    List,
    Poll(a206_core::Id),
    Kill(a206_core::Id),
    Remove(a206_core::Id),
}
fn session(arguments: &Map<String, Value>) -> Result<a206_core::Id, ExecError> {
    text(arguments, "session")?
        .and_then(|id| id.parse().ok())
        .ok_or(ExecError::Session)
}
fn parse_action(arguments: &Map<String, Value>) -> Result<Action, ExecError> {
    only(arguments, &["action", "session"])?;
    match text(arguments, "action")?.as_deref() {
        Some("list") => Ok(Action::List),
        Some("poll") => Ok(Action::Poll(session(arguments)?)),
        Some("kill") => Ok(Action::Kill(session(arguments)?)),
        Some("remove") => Ok(Action::Remove(session(arguments)?)),
        _ => Err(ExecError::Action),
    }
}

/// One validated `process_write` call.
#[derive(Debug, PartialEq, Eq)]
struct Write {
    session: a206_core::Id,
    /// The text, then the keys' sequences.
    data: Vec<u8>,
    /// Whether any keys were named, which needs a terminal.
    keys: bool,
    close: bool,
}
fn parse_write(arguments: &Map<String, Value>) -> Result<Write, ExecError> {
    only(arguments, &["session", "data", "keys", "close"])?;
    let session = session(arguments)?;
    let close = flag(arguments, "close")?;
    let mut data = text(arguments, "data")?.unwrap_or_default().into_bytes();
    if data.len() > MAX_INPUT {
        return Err(ExecError::Data);
    }
    let names = match arguments.get("keys") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(names)) if names.len() <= keys::MAX_KEYS => names
            .iter()
            .map(|name| name.as_str().ok_or(ExecError::NotKeys))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(ExecError::NotKeys),
    };
    data.extend(keys::translate(names.iter().copied())?);
    if data.is_empty() && !close {
        return Err(ExecError::Data);
    }
    Ok(Write {
        session,
        data,
        keys: !names.is_empty(),
        close,
    })
}

/// Runs commands for native runs. `state_root` is the gateway's private
/// state, which commands never see.
pub struct ExecHost {
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    state_root: PathBuf,
    sessions: sessions::Sessions,
}
impl ExecHost {
    pub fn new(state_root: &Path) -> Self {
        Self {
            state_root: state_root.to_owned(),
            sessions: Default::default(),
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
                 and the environment is empty. Every call needs the user's approval. With \
                 `background` it returns a session id at once and the command keeps running: \
                 follow it with `{PROCESS}` and give it input with `{PROCESS_WRITE}`. Add \
                 `terminal` for a program that needs a terminal: it then runs on a {TERMINAL_COLUMNS}x{TERMINAL_ROWS} \
                 pseudo-terminal with TERM=dumb, its stdout and stderr arrive as one stream, and \
                 `{PROCESS_WRITE}` can send it keys such as Ctrl-C."
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
                        "maximum": MAX_BACKGROUND_TIMEOUT,
                        "description": format!(
                            "Seconds before the command is stopped: default {DEFAULT_TIMEOUT}, at most \
                             {MAX_TIMEOUT}; in the background default {DEFAULT_BACKGROUND_TIMEOUT}, at \
                             most {MAX_BACKGROUND_TIMEOUT}."
                        )
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Return a session id at once and keep the command running."
                    },
                    "terminal": {
                        "type": "boolean",
                        "description": "With background: run the command on a pseudo-terminal."
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
    fn process_spec() -> ToolSpec {
        ToolSpec {
            name: PROCESS.into(),
            description: format!(
                "Follow this conversation's background commands from `{NAME}`. `list` shows each \
                 session's id and status; `poll` returns its status and the output \
                 written since the last poll (each stream keeps at most {MAX_STREAM} unread \
                 bytes, dropping the oldest); `kill` stops it; `remove` stops it if needed and \
                 forgets it. A conversation holds at most {MAX_PER_THREAD} sessions; starting \
                 another removes its oldest finished one."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "poll", "kill", "remove"] },
                    "session": { "type": "string", "description": "The session id, except for list." }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            capabilities: [Capability::Execute].into(),
            permission: ToolPermission::Allow,
            whole_output: false,
        }
    }
    fn write_spec() -> ToolSpec {
        ToolSpec {
            name: PROCESS_WRITE.into(),
            description:
                "Write text to the standard input of one of this conversation's background \
                 commands, then close its input if `close` is set. A command started with \
                 `terminal` takes `keys` too, sent after the text: Enter, Tab, Escape, Space, \
                 Backspace, Delete, Insert, Up, Down, Left, Right, Home, End, PageUp, PageDown, \
                 F1 to F12 and Ctrl-A to Ctrl-Z (Ctrl-C interrupts, Ctrl-D ends input); its \
                 input cannot be closed. Every call needs the user's approval, like a command. \
                 If the command does not take the input within a few seconds the call fails \
                 and the rest of the input is withdrawn."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "data": { "type": "string", "maxLength": MAX_INPUT },
                    "keys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": keys::MAX_KEYS,
                        "description": "Key names to press after the text, such as [\"Enter\"] or [\"Ctrl-C\"]."
                    },
                    "close": { "type": "boolean", "description": "Close the command's input afterwards." }
                },
                "required": ["session"],
                "additionalProperties": false
            }),
            capabilities: [Capability::Execute].into(),
            permission: ToolPermission::Ask,
            whole_output: false,
        }
    }
    /// Run one authorized call.
    async fn dispatch(
        &self,
        scope: &ToolScope,
        request: ToolRequest,
    ) -> Result<ToolOutput, ExecError> {
        if !scope.capabilities.contains(&Capability::Execute) {
            return Err(ExecError::Denied);
        }
        let done = |text: String| ToolOutput {
            text,
            is_error: false,
        };
        match request.name.as_str() {
            NAME if request.approved => {
                let call = parse(&request.arguments)?;
                if call.background {
                    self.background(scope, call).await
                } else {
                    self.run(scope, call).await
                }
            }
            PROCESS => match parse_action(&request.arguments)? {
                Action::List => Ok(done(self.sessions.list(scope.thread))),
                Action::Poll(id) => {
                    let (text, is_error) = self.sessions.poll(scope.thread, id)?;
                    Ok(ToolOutput { text, is_error })
                }
                Action::Kill(id) => {
                    self.sessions.kill(scope.thread, id)?;
                    Ok(done(format!(
                        "Stopping session {id}; poll it for its final output."
                    )))
                }
                Action::Remove(id) => {
                    self.sessions.remove(scope.thread, id)?;
                    Ok(done(format!("Removed session {id}.")))
                }
            },
            PROCESS_WRITE if request.approved => {
                let write = parse_write(&request.arguments)?;
                let bytes = write.data.len();
                self.sessions
                    .write(
                        scope.thread,
                        write.session,
                        write.data,
                        write.keys,
                        write.close,
                    )
                    .await?;
                Ok(done(format!(
                    "Wrote {bytes} bytes to session {}{}.",
                    write.session,
                    if write.close {
                        " and closed its input"
                    } else {
                        ""
                    }
                )))
            }
            _ => Err(ExecError::Denied),
        }
    }
}
impl ToolHost for ExecHost {
    fn list<'a>(&'a self, _scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            if cfg!(any(target_os = "linux", target_os = "macos")) {
                ToolListing {
                    tools: vec![Self::spec(), Self::process_spec(), Self::write_spec()],
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
            self.dispatch(scope, request)
                .await
                .map_err(|error| crate::error::report(&error))
        })
    }
    fn rescope(&self, scope: &dyn Fn(a206_core::Id) -> Option<ToolScope>) {
        self.sessions.rescope(scope);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl ExecHost {
    async fn run(&self, _scope: &ToolScope, _call: Call) -> Result<ToolOutput, ExecError> {
        Err(ExecError::Unsupported)
    }
    async fn background(&self, _scope: &ToolScope, _call: Call) -> Result<ToolOutput, ExecError> {
        Err(ExecError::Unsupported)
    }
}

/// A command whose sandbox has started.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Started {
    child: tokio::process::Child,
    /// The gateway's side of the command's terminal, if it has one.
    terminal: Option<terminal::Controller>,
    group: Group,
    control: a206_sandbox::Control,
    /// The command's private home and temporary directory, removed with it.
    scratch: tempfile::TempDir,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ExecHost {
    /// Cancel-safe: dropping the future kills the sandbox and everything in it.
    async fn run(&self, scope: &ToolScope, call: Call) -> Result<ToolOutput, ExecError> {
        use a206_core::Ignore;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(call.timeout);
        let Started {
            mut child,
            group,
            control,
            scratch,
            terminal: _,
        } = self.start(scope, &call, deadline).await?;
        let finished = tokio::time::timeout_at(deadline, async {
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
        drop(scratch);
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

    /// Start the command and hand it to a session; returns once its sandbox
    /// is up, so a sandbox failure still reaches this call.
    async fn background(&self, scope: &ToolScope, call: Call) -> Result<ToolOutput, ExecError> {
        // Refuse before starting anything; `adopt` checks again under its lock.
        let ticket = self.sessions.reserve(scope)?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(call.timeout);
        let started = self.start(scope, &call, deadline).await?;
        let id = self
            .sessions
            .adopt(ticket, started, call.timeout, deadline)?;
        Ok(ToolOutput {
            text: format!(
                "Started background session {id}{}; it is stopped after {} s. Follow it with \
                 `{PROCESS}` and give it input with `{PROCESS_WRITE}`.",
                if call.terminal { " on a terminal" } else { "" },
                call.timeout
            ),
            is_error: false,
        })
    }

    /// Prepare the sandbox and start the command in it. Cancel-safe.
    async fn start(
        &self,
        scope: &ToolScope,
        call: &Call,
        deadline: tokio::time::Instant,
    ) -> Result<Started, ExecError> {
        use std::process::Stdio;
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
        // Checked again: the directories may have changed since they were saved.
        let user_home = std::env::var_os("HOME").map(PathBuf::from);
        let programs = programs::validate(
            &scope.programs,
            &[],
            &hidden,
            user_home.as_deref(),
            &self.state_root,
        )?;
        let control = root.join("control");
        let pair = if call.terminal {
            Some(terminal::Pair::open().map_err(ExecError::Terminal)?)
        } else {
            None
        };
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
            programs: &programs,
            hidden: &hidden,
            cwd: &cwd,
            network: if holds(Capability::Network) {
                a206_sandbox::NetworkAccess::Host
            } else {
                a206_sandbox::NetworkAccess::None
            },
            stderr: true,
            terminal: pair.as_ref().map(|pair| pair.path.as_path()),
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
            .env("PATH", search_path(&programs))
            .env("LANG", LOCALE)
            // Bubblewrap reports setup failures here before the command starts.
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let controller = match pair {
            Some(pair) => {
                let device = || pair.device.try_clone().map_err(ExecError::Setup);
                command
                    .env("TERM", "dumb")
                    .stdin(device()?)
                    .stdout(device()?);
                // On Linux the helper gives the command the terminal as its
                // stderr, after bubblewrap's diagnostics; on macOS nothing
                // runs in between, and the launcher starts its own session
                // (and so its own group).
                #[cfg(target_os = "macos")]
                command.stderr(device()?);
                #[cfg(not(target_os = "macos"))]
                command.process_group(0);
                Some(pair.controller)
            }
            None => {
                command
                    // Only a background command can be given input.
                    .stdin(if call.background {
                        Stdio::piped()
                    } else {
                        Stdio::null()
                    })
                    .stdout(Stdio::piped())
                    .process_group(0);
                None
            }
        };
        let mut child = command.spawn().map_err(ExecError::Spawn)?;
        // The command holds the terminal now; the gateway's copies would keep
        // its output from ever ending.
        drop(command);
        let group = Group(std::sync::Mutex::new(
            child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .map(nix::unistd::Pid::from_raw),
        ));
        match tokio::time::timeout_at(deadline, control.ready(&mut child)).await {
            Ok(Ok(())) => Ok(Started {
                child,
                terminal: controller,
                group,
                control,
                scratch,
            }),
            Ok(Err(error)) => Err(ExecError::Start(error)),
            // Dropping the child and its group stops what started.
            Err(_) => Err(ExecError::TimedOut {
                seconds: call.timeout,
            }),
        }
    }
}

/// `PATH` for a command: each program directory's `bin`, or the directory
/// itself when it has none, ahead of the system directories.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn search_path(programs: &[PathBuf]) -> std::ffi::OsString {
    let mut path = std::ffi::OsString::new();
    for program in programs {
        let bin = program.join("bin");
        path.push(if bin.is_dir() { &bin } else { program });
        path.push(":");
    }
    path.push("/usr/bin:/bin");
    path
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
pub(crate) fn stream_text(name: &str, bytes: &[u8], cut: bool) -> String {
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
                programs: vec![],
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
                background: false,
                terminal: false,
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

    #[test]
    fn background_calls_get_their_own_timeout_bounds() {
        let command = uuid::Uuid::new_v4().to_string();
        let call = |value: Value| parse(&arguments(value));
        let background = call(json!({ "command": command, "background": true })).unwrap();
        assert!(background.background && !background.terminal);
        let terminal = json!({ "command": command, "background": true, "terminal": true });
        assert!(call(terminal).unwrap().terminal);
        assert_eq!(background.timeout, DEFAULT_BACKGROUND_TIMEOUT);
        let long = json!({ "command": command, "timeout_secs": MAX_BACKGROUND_TIMEOUT });
        assert!(
            call(long.clone()).is_err(),
            "only a background call runs that long"
        );
        let mut long_background = long;
        long_background["background"] = json!(true);
        assert_eq!(
            call(long_background).unwrap().timeout,
            MAX_BACKGROUND_TIMEOUT
        );
        for value in [
            json!({ "command": command, "background": true, "timeout_secs": MAX_BACKGROUND_TIMEOUT + 1 }),
            json!({ "command": command, "background": "yes" }),
            json!({ "command": command, "terminal": true }),
            json!({ "command": command, "background": true, "terminal": 1 }),
        ] {
            assert_eq!(
                crate::error::report(&call(value).unwrap_err()).kind,
                ErrorKind::InvalidRequest
            );
        }
    }

    #[test]
    fn session_calls_are_checked_before_anything_runs() {
        let id = a206_core::Id::new_v4();
        let action = |value: Value| parse_action(&arguments(value));
        assert_eq!(action(json!({ "action": "list" })).unwrap(), Action::List);
        assert_eq!(
            action(json!({ "action": "poll", "session": id.to_string() })).unwrap(),
            Action::Poll(id)
        );
        for value in [
            json!({}),
            json!({ "action": "poll" }),
            json!({ "action": "poll", "session": "not-an-id" }),
            json!({ "action": "signal", "session": id.to_string() }),
            json!({ "action": "list", "extra": 1 }),
        ] {
            let error = action(value.clone()).unwrap_err();
            assert_eq!(
                crate::error::report(&error).kind,
                ErrorKind::InvalidRequest,
                "{value}"
            );
        }
        let write = |value: Value| parse_write(&arguments(value));
        assert!(write(json!({ "session": id.to_string(), "close": true })).is_ok());
        let text = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            write(json!({ "session": id.to_string(), "data": text, "keys": ["Enter", "Ctrl-C"] }))
                .unwrap(),
            Write {
                session: id,
                data: [text.as_bytes(), b"\r\x03"].concat(),
                keys: true,
                close: false,
            }
        );
        assert!(
            !write(json!({ "session": id.to_string(), "data": text, "keys": [] }))
                .unwrap()
                .keys
        );
        for value in [
            json!({ "session": id.to_string() }),
            json!({ "session": id.to_string(), "keys": [] }),
            json!({ "session": id.to_string(), "data": "x".repeat(MAX_INPUT + 1) }),
            json!({ "session": id.to_string(), "data": "x", "close": "no" }),
            json!({ "session": id.to_string(), "keys": "Enter" }),
            json!({ "session": id.to_string(), "keys": [1] }),
            json!({ "session": id.to_string(), "keys": ["Hello"] }),
            json!({ "session": id.to_string(), "keys": vec!["Enter"; keys::MAX_KEYS + 1] }),
        ] {
            let error = write(value.clone()).unwrap_err();
            assert_eq!(
                crate::error::report(&error).kind,
                ErrorKind::InvalidRequest,
                "{value}"
            );
        }
    }

    #[tokio::test]
    async fn sessions_belong_to_their_conversation() {
        let state = tempfile::tempdir().unwrap();
        let host = ExecHost::new(state.path());
        let scope = ToolScope {
            thread: a206_core::Id::new_v4(),
            workspace: None,
            capabilities: [Capability::Execute].into(),
            programs: vec![],
        };
        let request = |name: &str, value: Value| ToolRequest {
            call: a206_core::Id::new_v4().to_string(),
            name: name.into(),
            arguments: arguments(value),
            approved: true,
        };
        let unknown = a206_core::Id::new_v4().to_string();
        for (name, value) in [
            (PROCESS, json!({ "action": "poll", "session": unknown })),
            (PROCESS, json!({ "action": "kill", "session": unknown })),
            (PROCESS_WRITE, json!({ "session": unknown, "data": "x" })),
        ] {
            assert_eq!(
                host.call(&scope, request(name, value))
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::NotFound
            );
        }
        // Input is a command: it needs approval of that exact call.
        let mut unapproved = request(PROCESS_WRITE, json!({ "session": unknown, "data": "x" }));
        unapproved.approved = false;
        assert_eq!(
            host.call(&scope, unapproved).await.unwrap_err().kind,
            ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn every_call_needs_approval_and_the_execute_grant() {
        let state = tempfile::tempdir().unwrap();
        let host = ExecHost::new(state.path());
        let scope = ToolScope {
            thread: a206_core::Id::new_v4(),
            workspace: None,
            capabilities: Default::default(),
            programs: vec![],
        };
        let listing = host.list(&scope).await;
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let [exec, process, write] = listing.tools.as_slice() else {
                panic!("{listing:?}")
            };
            assert_eq!(
                [exec.permission, process.permission, write.permission],
                [
                    ToolPermission::Ask,
                    ToolPermission::Allow,
                    ToolPermission::Ask
                ]
            );
            for spec in [exec, process, write] {
                assert_eq!(spec.capabilities, [Capability::Execute].into());
            }
        } else {
            assert!(listing.tools.is_empty());
            assert_eq!(listing.unavailable[0].error.kind, ErrorKind::Unavailable);
        }
    }
}
