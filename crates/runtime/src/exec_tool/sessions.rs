//! Background command sessions: `execute_command` with `background` starts a
//! sandboxed command and returns at once; `process` and `process_write`
//! address it afterwards.
//!
//! A session belongs to the conversation that started it. Every other
//! conversation, a subagent included, sees no such session, so a missing and
//! a foreign session are indistinguishable. Each session's output is kept in
//! a bounded buffer per stream that `poll` drains; when a command writes
//! faster than it is polled the oldest bytes are dropped and counted. A
//! session ends at its own exit, when it is killed, at its timeout, or when
//! the host is dropped with the gateway.
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::Started;
use super::{ExecError, MAX_STREAM, Size};
use crate::tools::ToolScope;
use a206_core::{Capability, Id, Ignore};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    process::ExitStatus,
    sync::{Arc, Mutex, atomic::AtomicU64},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

/// Sessions one conversation may hold, running or finished.
pub const MAX_PER_THREAD: usize = 8;
/// Sessions the host holds across all conversations.
pub const MAX_SESSIONS: usize = 64;
/// Longest input accepted in one `process_write` call.
pub const MAX_INPUT: usize = 64 * 1024;
/// How long a write waits for the command to take its input, queueing
/// included.
const INPUT_WAIT: Duration = Duration::from_secs(5);
/// How long output is still collected after a session ends, in case an
/// orphan of the command holds its pipes open.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// The gateway's side of a session's terminal.
#[cfg(any(target_os = "linux", target_os = "macos"))]
type Window = super::terminal::Controller;
/// No terminal exists where commands cannot run.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
type Window = std::convert::Infallible;
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resize(window: &Window, size: Size) -> std::io::Result<()> {
    window.resize(size)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resize(window: &Window, _size: Size) -> std::io::Result<()> {
    match *window {}
}

/// How a session ended, or that it has not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Running,
    Exited(ExitStatus),
    Killed,
    /// Stopped because its conversation no longer grants what it started
    /// with.
    Revoked,
    TimedOut {
        seconds: u64,
    },
    /// Waiting for the command failed; the kind is all that is kept, since
    /// the error itself was recorded when it happened.
    Failed(std::io::ErrorKind),
}
impl Status {
    fn finished(&self) -> bool {
        *self != Status::Running
    }
}
impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Exited(status) => status.fmt(f),
            Self::Killed => f.write_str("killed"),
            Self::Revoked => f.write_str("stopped: its permissions were withdrawn"),
            Self::TimedOut { seconds } => write!(f, "stopped at its {seconds} s timeout"),
            Self::Failed(kind) => write!(f, "lost: {kind}"),
        }
    }
}

/// Why a session is told to stop.
#[derive(Debug, Clone, Copy)]
enum Stop {
    Killed,
    Revoked,
}

/// What a session's sandbox was given: the parts of its conversation's
/// scope that shape the sandbox.
#[derive(Debug, Clone)]
pub(super) struct Authority {
    capabilities: BTreeSet<Capability>,
    workspace: Option<PathBuf>,
    programs: Vec<PathBuf>,
}
impl Authority {
    pub(super) fn of(scope: &ToolScope) -> Self {
        let capabilities: BTreeSet<Capability> = scope
            .capabilities
            .iter()
            .copied()
            .filter(|c| {
                matches!(
                    c,
                    Capability::Execute
                        | Capability::ReadWorkspace
                        | Capability::WriteWorkspace
                        | Capability::Network
                )
            })
            .collect();
        let workspace = scope
            .workspace
            .clone()
            .filter(|_| capabilities.contains(&Capability::ReadWorkspace));
        Self {
            capabilities,
            workspace,
            programs: scope.programs.clone(),
        }
    }
    /// Whether `scope` still grants everything the sandbox holds.
    fn covered_by(&self, scope: Option<&ToolScope>) -> bool {
        scope.is_some_and(|scope| {
            self.capabilities.is_subset(&scope.capabilities)
                && (self.workspace.is_none() || self.workspace == scope.workspace)
                && self
                    .programs
                    .iter()
                    .all(|program| scope.programs.contains(program))
        })
    }
}

/// Output not yet polled from one stream.
#[derive(Debug, Default)]
struct Unread {
    bytes: VecDeque<u8>,
    dropped: u64,
    /// Reading the stream failed; nothing after that was collected.
    lost: Option<std::io::ErrorKind>,
}
impl Unread {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        let over = self.bytes.len().saturating_sub(MAX_STREAM);
        self.bytes.drain(..over);
        self.dropped += over as u64;
    }
    /// The unread output as text of at most [`MAX_STREAM`] bytes. What does
    /// not fit, because invalid UTF-8 grows when replaced, waits for the
    /// next poll.
    fn take(&mut self, name: &str) -> String {
        let bytes = self.bytes.make_contiguous();
        let mut decoded = String::new();
        let mut consumed = 0;
        for chunk in bytes.utf8_chunks() {
            let valid = chunk.valid();
            let room = MAX_STREAM - decoded.len();
            if valid.len() > room {
                let end = valid.floor_char_boundary(room);
                decoded.push_str(&valid[..end]);
                consumed += end;
                break;
            }
            decoded.push_str(valid);
            consumed += valid.len();
            if !chunk.invalid().is_empty() {
                if decoded.len() + char::REPLACEMENT_CHARACTER.len_utf8() > MAX_STREAM {
                    break;
                }
                decoded.push(char::REPLACEMENT_CHARACTER);
                consumed += chunk.invalid().len();
            }
        }
        self.bytes.drain(..consumed);
        let mut text = format!("--- {name} ---\n{decoded}");
        if self.dropped > 0 {
            text.push_str(&format!(
                "\n[{} earlier {name} bytes dropped before this poll]",
                self.dropped
            ));
            self.dropped = 0;
        }
        if !self.bytes.is_empty() {
            text.push_str(&format!(
                "\n[{} more {name} bytes wait for the next poll]",
                self.bytes.len()
            ));
        }
        if let Some(kind) = self.lost {
            text.push_str(&format!(
                "\n[reading {name} failed: {kind}; later output is lost]"
            ));
        }
        text
    }
}

/// What the supervising task and the tool calls share. Locked only briefly,
/// never across an `.await`.
#[derive(Debug)]
struct Shared {
    status: Status,
    /// The command's stdout, or with a terminal everything it shows there.
    stdout: Unread,
    /// With a terminal, only what the sandbox itself reports.
    stderr: Unread,
    terminal: bool,
}
type Output = Arc<Mutex<Shared>>;

/// Input for the command, with the result of writing it. A message whose
/// result nobody waits for any more is not written, and one being written
/// stops there.
type Input = (Vec<u8>, bool, oneshot::Sender<std::io::Result<()>>);

struct Session {
    /// Start order, for eviction and listing.
    seq: u64,
    thread: Id,
    authority: Authority,
    /// Its pseudo-terminal, if it was started on one.
    terminal: Option<Window>,
    output: Output,
    input: mpsc::Sender<Input>,
    /// Sending to or dropping this stops the command.
    stop: Option<oneshot::Sender<Stop>>,
    /// Dropping this detaches the supervising task, which then ends promptly
    /// because `stop` was dropped with it.
    _task: tokio::task::JoinHandle<()>,
}
impl Session {
    fn status(&self) -> Status {
        lock(&self.output).status.clone()
    }
}

/// A command whose sandbox is being prepared.
struct Starting {
    thread: Id,
    authority: Authority,
    /// Its conversation lost a grant meanwhile.
    revoked: bool,
}

/// A place held for a command until its session is adopted; dropping it
/// gives the place up.
pub(super) struct Ticket<'a> {
    sessions: &'a Sessions,
    seq: u64,
}
impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        self.sessions.starting().remove(&self.seq);
    }
}

fn lock(output: &Output) -> std::sync::MutexGuard<'_, Shared> {
    // A poisoned buffer still holds valid bytes and status: every update is
    // a single assignment or append.
    output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The sessions of one host. Where both locks are taken, `map` is first.
#[derive(Default)]
pub(super) struct Sessions {
    map: Mutex<BTreeMap<Id, Session>>,
    starting: Mutex<BTreeMap<u64, Starting>>,
    started: AtomicU64,
}
impl Sessions {
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<Id, Session>> {
        // Each update is a single insert or removal.
        self.map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    fn starting(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Starting>> {
        // Each update is a single insert, removal or assignment.
        self.starting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The finished sessions of `thread` to remove, oldest first, so it can
    /// hold one more. Running sessions are never evicted, and neither is
    /// another conversation's output it has not collected.
    fn evictions(map: &BTreeMap<Id, Session>, thread: Id) -> Result<Vec<Id>, ExecError> {
        let held = map.values().filter(|s| s.thread == thread).count();
        let needed = (held + 1)
            .saturating_sub(MAX_PER_THREAD)
            .max((map.len() + 1).saturating_sub(MAX_SESSIONS));
        let mut finished: Vec<_> = map
            .iter()
            .filter(|(_, s)| s.thread == thread && s.status().finished())
            .map(|(id, s)| (s.seq, *id))
            .collect();
        if finished.len() < needed {
            let limit = if held >= MAX_PER_THREAD {
                MAX_PER_THREAD
            } else {
                MAX_SESSIONS
            };
            return Err(ExecError::TooManySessions { limit });
        }
        finished.sort_unstable();
        Ok(finished
            .into_iter()
            .take(needed)
            .map(|(_, id)| id)
            .collect())
    }

    /// Check that `thread` may start one more session, without removing
    /// anything, and hold a place for it while its sandbox starts.
    pub(super) fn reserve(&self, scope: &ToolScope) -> Result<Ticket<'_>, ExecError> {
        Self::evictions(&self.map(), scope.thread)?;
        let seq = self
            .started
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.starting().insert(
            seq,
            Starting {
                thread: scope.thread,
                authority: Authority::of(scope),
                revoked: false,
            },
        );
        Ok(Ticket {
            sessions: self,
            seq,
        })
    }

    /// Supervise a started command; returns its session id. Finished
    /// sessions make room only now that the replacement is running.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn adopt(
        &self,
        ticket: Ticket<'_>,
        started: Started,
        seconds: u64,
        deadline: tokio::time::Instant,
    ) -> Result<Id, ExecError> {
        let mut map = self.map();
        let Some(Starting {
            thread,
            authority,
            revoked: false,
        }) = self.starting().remove(&ticket.seq)
        else {
            // Dropping `started` stops the command.
            return Err(ExecError::Revoked);
        };
        for id in Self::evictions(&map, thread)? {
            map.remove(&id);
        }
        let terminal = started.terminal.clone();
        let output: Output = Arc::new(Mutex::new(Shared {
            status: Status::Running,
            stdout: Unread::default(),
            stderr: Unread::default(),
            terminal: terminal.is_some(),
        }));
        let (input, inputs) = mpsc::channel(1);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(supervise(
            started,
            output.clone(),
            inputs,
            stopped,
            seconds,
            deadline,
        ));
        let id = Id::new_v4();
        map.insert(
            id,
            Session {
                seq: ticket.seq,
                thread,
                authority,
                terminal,
                output,
                input,
                stop: Some(stop),
                _task: task,
            },
        );
        Ok(id)
    }

    /// Stop every session, and every command still starting, whose
    /// conversation's current scope no longer covers its sandbox. `scope`
    /// gives that scope, or `None` for a conversation that is gone.
    pub(super) fn rescope(&self, scope: &dyn Fn(Id) -> Option<ToolScope>) {
        let mut map = self.map();
        for session in map.values_mut() {
            if session.stop.is_none()
                || session.status().finished()
                || session.authority.covered_by(scope(session.thread).as_ref())
            {
                continue;
            }
            if let Some(stop) = session.stop.take() {
                // The command may have just ended on its own.
                stop.send(Stop::Revoked).ignore();
            }
        }
        for starting in self.starting().values_mut() {
            if !starting
                .authority
                .covered_by(scope(starting.thread).as_ref())
            {
                starting.revoked = true;
            }
        }
    }

    /// Each session's id and status; commands are not repeated, since they
    /// can carry what the conversation should not see again.
    pub(super) fn list(&self, thread: Id) -> String {
        let map = self.map();
        let mut owned: Vec<_> = map.iter().filter(|(_, s)| s.thread == thread).collect();
        owned.sort_by_key(|(_, s)| s.seq);
        let lines: Vec<String> = owned
            .into_iter()
            .map(|(id, s)| format!("{id}  {}", s.status()))
            .collect();
        if lines.is_empty() {
            "No background sessions.".into()
        } else {
            lines.join("\n")
        }
    }

    /// The session's status and the output written since the last poll.
    pub(super) fn poll(&self, thread: Id, id: Id) -> Result<(String, bool), ExecError> {
        let map = self.map();
        let session = owned(&map, thread, id)?;
        let mut shared = lock(&session.output);
        let failed = match &shared.status {
            Status::Running => false,
            Status::Exited(status) => !status.success(),
            _ => true,
        } || shared.stdout.lost.is_some()
            || shared.stderr.lost.is_some();
        let text = if shared.terminal {
            let shown = shared.stdout.take("terminal");
            let mut text = format!("{}\n{shown}", shared.status);
            // The sandbox reports here only when something went wrong.
            if !shared.stderr.bytes.is_empty() || shared.stderr.lost.is_some() {
                text.push('\n');
                text.push_str(&shared.stderr.take("sandbox"));
            }
            text
        } else {
            let (out, err) = (shared.stdout.take("stdout"), shared.stderr.take("stderr"));
            format!("{}\n{out}\n{err}", shared.status)
        };
        Ok((text, failed))
    }

    pub(super) fn kill(&self, thread: Id, id: Id) -> Result<(), ExecError> {
        let mut map = self.map();
        let session = owned_mut(&mut map, thread, id)?;
        if session.status().finished() {
            return Err(ExecError::SessionFinished);
        }
        match session.stop.take() {
            Some(stop) => {
                // The task may have just finished on its own; then nothing
                // is left to stop.
                stop.send(Stop::Killed).ignore();
                Ok(())
            }
            None => Err(ExecError::SessionFinished),
        }
    }

    /// Give a running session's terminal a new window size. A session
    /// being stopped is refused at once; one whose command exits on its own
    /// meanwhile may still take the size, as a terminal window would.
    pub(super) fn resize(&self, thread: Id, id: Id, size: Size) -> Result<(), ExecError> {
        let map = self.map();
        let session = owned(&map, thread, id)?;
        let window = session
            .terminal
            .as_ref()
            .ok_or(ExecError::ResizeNeedsTerminal)?;
        // `kill` takes the stop signal before the status catches up.
        if session.stop.is_none() || session.status().finished() {
            return Err(ExecError::SessionFinished);
        }
        resize(window, size).map_err(ExecError::Resize)
    }

    /// Forget a session, stopping it first if it still runs.
    pub(super) fn remove(&self, thread: Id, id: Id) -> Result<(), ExecError> {
        let mut map = self.map();
        owned(&map, thread, id)?;
        // Dropping the session drops its stop sender, which ends it.
        map.remove(&id);
        Ok(())
    }

    /// Give the command input, then close its input if `close`. One
    /// deadline covers waiting for a place in the queue and for the write;
    /// past it the input is withdrawn, though part of it may have been
    /// written by then. `keys` says the input names keys, which only a
    /// terminal takes; a terminal's input cannot be closed.
    pub(super) async fn write(
        &self,
        thread: Id,
        id: Id,
        data: Vec<u8>,
        keys: bool,
        close: bool,
    ) -> Result<(), ExecError> {
        // Taken out so no lock is held while waiting for the command.
        let input = {
            let map = self.map();
            let session = owned(&map, thread, id)?;
            match (session.terminal.is_some(), keys, close) {
                (false, true, _) => return Err(ExecError::KeysNeedTerminal),
                (true, _, true) => return Err(ExecError::TerminalClose),
                _ => session.input.clone(),
            }
        };
        let (done, written) = oneshot::channel();
        let delivered = async {
            input
                .send((data, close, done))
                .await
                .map_err(|_| ExecError::InputClosed)?;
            match written.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(source)) => Err(ExecError::Input(source)),
                Err(_) => Err(ExecError::InputClosed),
            }
        };
        // Timing out drops `written`, which withdraws the input.
        tokio::time::timeout(INPUT_WAIT, delivered)
            .await
            .unwrap_or(Err(ExecError::InputStalled {
                seconds: INPUT_WAIT.as_secs(),
            }))
    }
}

fn owned(map: &BTreeMap<Id, Session>, thread: Id, id: Id) -> Result<&Session, ExecError> {
    map.get(&id)
        .filter(|s| s.thread == thread)
        .ok_or(ExecError::UnknownSession)
}
fn owned_mut(
    map: &mut BTreeMap<Id, Session>,
    thread: Id,
    id: Id,
) -> Result<&mut Session, ExecError> {
    map.get_mut(&id)
        .filter(|s| s.thread == thread)
        .ok_or(ExecError::UnknownSession)
}

/// Run a session to its end, collecting its output.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn supervise(
    started: Started,
    output: Output,
    mut inputs: mpsc::Receiver<Input>,
    mut stop: oneshot::Receiver<Stop>,
    seconds: u64,
    deadline: tokio::time::Instant,
) {
    use tokio::io::{AsyncRead, AsyncWrite};
    let Started {
        mut child,
        terminal,
        group,
        control,
        scratch,
    } = started;
    type Reader = Box<dyn AsyncRead + Unpin + Send>;
    type Writer = Box<dyn AsyncWrite + Unpin + Send>;
    let streams: Option<(Reader, Writer)> = match terminal {
        Some(terminal) => Some((Box::new(terminal.clone()), Box::new(terminal))),
        None => child
            .stdout
            .take()
            .zip(child.stdin.take())
            .map(|(out, input)| (Box::new(out) as Reader, Box::new(input) as Writer)),
    };
    let Some((stdout, mut stdin)) = streams else {
        unreachable!("background sessions pipe or attach stdin and stdout")
    };
    // Absent only on macOS with a terminal, where stderr is the terminal.
    let stderr = child.stderr.take();
    let pumps =
        futures_util::future::join(pump(stdout, output.clone(), |s| &mut s.stdout), async {
            if let Some(stderr) = stderr {
                pump(stderr, output.clone(), |s| &mut s.stderr).await;
            }
        });
    tokio::pin!(pumps);
    let mut drained = false;
    let feed = async {
        let mut open = true;
        while let Some((data, close, mut done)) = inputs.recv().await {
            if done.is_closed() {
                // The caller gave up before its input was written.
                continue;
            }
            tokio::select! {
                result = give(&mut stdin, &mut open, &data, close) => {
                    // The caller may have stopped waiting just now.
                    done.send(result).ignore();
                }
                // The caller gave up; the rest of its input is withdrawn.
                () = done.closed() => {}
            }
        }
        // No sender is left: the session was removed, and `stop` ends it.
        std::future::pending::<()>().await
    };
    tokio::pin!(feed);
    let status = loop {
        tokio::select! {
            status = child.wait() => {
                break match status {
                    Ok(status) => Status::Exited(status),
                    Err(error) => {
                        tracing::warn!(error = &error as &dyn std::error::Error, "lost a background command");
                        Status::Failed(error.kind())
                    }
                }
            }
            // Sent by `kill` or `rescope`, or dropped with the session.
            reason = &mut stop => break match reason {
                Ok(Stop::Revoked) => Status::Revoked,
                Ok(Stop::Killed) | Err(_) => Status::Killed,
            },
            () = tokio::time::sleep_until(deadline) => break Status::TimedOut { seconds },
            () = &mut feed => {}
            _ = &mut pumps, if !drained => drained = true,
        }
    };
    // On Linux the sandbox's PID namespace ends with the shell; macOS has
    // none, so the shell's group goes too.
    group.kill();
    if !matches!(status, Status::Exited(_)) {
        // Already gone if killed through the group; reaping cannot fail
        // in a way anyone could act on.
        child.kill().await.ignore();
    }
    if !drained {
        // An orphan outside the group may hold the pipes; stop reading then.
        tokio::time::timeout(DRAIN_GRACE, &mut pumps).await.ignore();
    }
    control.stop().await;
    drop(scratch);
    lock(&output).status = status;
}

/// Write one input, then close the command's input if `close`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn give(
    stdin: &mut (impl tokio::io::AsyncWrite + Unpin),
    open: &mut bool,
    data: &[u8],
    close: bool,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    if !*open {
        return Err(std::io::ErrorKind::BrokenPipe.into());
    }
    stdin.write_all(data).await?;
    stdin.flush().await?;
    if close {
        *open = false;
        stdin.shutdown().await?;
    }
    Ok(())
}

/// Copy a stream into its unread buffer until it ends. A read failure is
/// kept with the output, so polls report it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn pump(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    output: Output,
    unread: fn(&mut Shared) -> &mut Unread,
) {
    use tokio::io::AsyncReadExt;
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => return,
            Ok(read) => unread(&mut lock(&output)).push(&buffer[..read]),
            Err(error) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "could not read a background command's output"
                );
                unread(&mut lock(&output)).lost = Some(error.kind());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_output_is_bounded_and_counts_what_it_drops() {
        let mut unread = Unread::default();
        unread.push(&vec![b'a'; MAX_STREAM]);
        unread.push(b"tail");
        let text = unread.take("stdout");
        assert!(text.ends_with("[4 earlier stdout bytes dropped before this poll]"));
        assert!(text.contains("tail"));
        // A poll drains: the next one starts empty.
        assert_eq!(unread.take("stdout"), "--- stdout ---\n");
    }

    #[test]
    fn invalid_utf8_waits_for_the_next_poll_rather_than_growing_past_the_bound() {
        let mut unread = Unread::default();
        unread.push(&vec![0xff; MAX_STREAM]);
        let mut shown = 0;
        let mut polls = 0;
        while !unread.bytes.is_empty() {
            let text = unread.take("stdout");
            let body = text
                .strip_prefix("--- stdout ---\n")
                .unwrap()
                .split("\n[")
                .next()
                .unwrap();
            assert!(body.len() <= MAX_STREAM, "{}", body.len());
            assert!(text.len() <= MAX_STREAM + 256, "{}", text.len());
            shown += body.chars().count();
            polls += 1;
            if !unread.bytes.is_empty() {
                assert!(text.contains("more stdout bytes wait for the next poll"));
            }
        }
        // Every byte is shown once, as a replacement character.
        assert_eq!(shown, MAX_STREAM);
        assert!(polls > 1);
        // A character is never split at the bound.
        unread.push(&vec![b'a'; MAX_STREAM - 1]);
        let mut wide = "é".as_bytes().to_vec();
        unread.bytes.extend(wide.drain(..));
        assert!(!unread.take("stdout").contains('\u{fffd}'));
        assert_eq!(unread.take("stdout"), "--- stdout ---\né");
    }

    #[test]
    fn lost_output_is_reported_on_every_poll() {
        let mut unread = Unread::default();
        unread.push(b"partial");
        unread.lost = Some(std::io::ErrorKind::BrokenPipe);
        for _ in 0..2 {
            assert!(unread.take("stderr").contains("reading stderr failed"));
        }
    }

    fn scope(thread: Id, capabilities: &[Capability]) -> ToolScope {
        ToolScope {
            thread,
            workspace: Some(PathBuf::from(Id::new_v4().to_string())),
            capabilities: capabilities.iter().copied().collect(),
            programs: vec![],
        }
    }

    /// Hold a session of `thread` with `status`; returns its stop signal.
    fn hold(sessions: &Sessions, scope: &ToolScope, status: Status) -> oneshot::Receiver<Stop> {
        let (input, _) = mpsc::channel(1);
        let (stop, stopped) = oneshot::channel();
        let output = Arc::new(Mutex::new(Shared {
            status,
            stdout: Unread::default(),
            stderr: Unread::default(),
            terminal: false,
        }));
        sessions.map().insert(
            Id::new_v4(),
            Session {
                seq: sessions
                    .started
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                thread: scope.thread,
                authority: Authority::of(scope),
                terminal: None,
                output,
                input,
                stop: Some(stop),
                _task: tokio::spawn(async {}),
            },
        );
        stopped
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn a_session_being_stopped_cannot_be_resized() {
        let sessions = Sessions::default();
        let scope = scope(Id::new_v4(), &[Capability::Execute]);
        let _stopped = hold(&sessions, &scope, Status::Running);
        let pair = super::super::terminal::Pair::open(Size::default()).unwrap();
        let id = {
            let mut map = sessions.map();
            let (id, session) = map.iter_mut().next().unwrap();
            session.terminal = Some(pair.controller);
            *id
        };
        let size = Size {
            columns: 100,
            rows: 30,
        };
        sessions.resize(scope.thread, id, size).unwrap();
        sessions.kill(scope.thread, id).unwrap();
        // Its status still reads running until the supervisor catches up.
        assert!(!sessions.map()[&id].status().finished());
        assert!(matches!(
            sessions.resize(scope.thread, id, size),
            Err(ExecError::SessionFinished)
        ));
    }

    #[tokio::test]
    async fn reserving_removes_nothing_until_a_session_is_adopted() {
        let sessions = Sessions::default();
        let scope = scope(Id::new_v4(), &[Capability::Execute]);
        for _ in 0..MAX_PER_THREAD {
            hold(&sessions, &scope, Status::Killed);
        }
        let ticket = sessions.reserve(&scope).unwrap();
        drop(ticket);
        assert_eq!(sessions.map().len(), MAX_PER_THREAD);
        assert!(
            sessions.starting().is_empty(),
            "a dropped ticket is given up"
        );
        assert_eq!(
            Sessions::evictions(&sessions.map(), scope.thread)
                .unwrap()
                .len(),
            1
        );
        // Running sessions are never evicted.
        let busy = Sessions::default();
        for _ in 0..MAX_PER_THREAD {
            hold(&busy, &scope, Status::Running);
        }
        assert!(matches!(
            busy.reserve(&scope),
            Err(ExecError::TooManySessions { .. })
        ));
    }

    #[tokio::test]
    async fn removing_a_program_directory_stops_sessions_that_were_given_it() {
        let sessions = Sessions::default();
        let tools = PathBuf::from(Id::new_v4().to_string());
        let other = PathBuf::from(Id::new_v4().to_string());
        let given = ToolScope {
            programs: vec![tools.clone(), other.clone()],
            ..scope(Id::new_v4(), &[Capability::Execute])
        };
        let mut stopped = hold(&sessions, &given, Status::Running);
        let plain = ToolScope {
            programs: vec![],
            ..given.clone()
        };
        let mut unaffected = hold(&sessions, &plain, Status::Running);
        // Adding a directory or reordering keeps both.
        let more = ToolScope {
            programs: vec![other.clone(), PathBuf::from("added"), tools],
            ..given.clone()
        };
        sessions.rescope(&|_| Some(more.clone()));
        assert!(stopped.try_recv().is_err());
        let fewer = ToolScope {
            programs: vec![other],
            ..given
        };
        sessions.rescope(&|_| Some(fewer.clone()));
        assert!(matches!(stopped.try_recv(), Ok(Stop::Revoked)));
        assert!(unaffected.try_recv().is_err());
    }

    #[tokio::test]
    async fn withdrawn_grants_stop_running_and_starting_commands_only() {
        let sessions = Sessions::default();
        let all = [
            Capability::Execute,
            Capability::ReadWorkspace,
            Capability::Network,
        ];
        let granted = scope(Id::new_v4(), &all);
        let mut stopped = hold(&sessions, &granted, Status::Running);
        let ticket = sessions.reserve(&granted).unwrap();
        // The same grant, or more, keeps both.
        sessions.rescope(&|_| Some(granted.clone()));
        assert!(stopped.try_recv().is_err());
        assert!(!sessions.starting()[&ticket.seq].revoked);
        // Another project is not the one the sandbox mounted.
        let moved = scope(granted.thread, &all);
        sessions.rescope(&|_| Some(moved.clone()));
        assert!(matches!(stopped.try_recv(), Ok(Stop::Revoked)));
        assert!(sessions.starting()[&ticket.seq].revoked);
        // Losing a grant the sandbox holds stops it.
        let other = Sessions::default();
        let mut revoked = hold(&other, &granted, Status::Running);
        let narrowed = scope(granted.thread, &[Capability::Execute]);
        other.rescope(&|_| Some(narrowed.clone()));
        assert!(matches!(revoked.try_recv(), Ok(Stop::Revoked)));
        // A project it never mounted can change freely.
        let unused = Sessions::default();
        let plain = scope(granted.thread, &[Capability::Execute]);
        let mut running = hold(&unused, &plain, Status::Running);
        let mut wider = plain.clone();
        wider.workspace = None;
        unused.rescope(&|_| Some(wider.clone()));
        assert!(running.try_recv().is_err(), "it never held the workspace");
    }
}
