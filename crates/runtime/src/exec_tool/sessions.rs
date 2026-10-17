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
use super::{ExecError, MAX_STREAM};
use a206_core::{Id, Ignore};
use std::{
    collections::{BTreeMap, VecDeque},
    process::ExitStatus,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

/// Sessions one conversation may hold, running or finished.
pub const MAX_PER_THREAD: usize = 8;
/// Sessions the host holds across all conversations.
pub const MAX_SESSIONS: usize = 64;
/// Longest input accepted in one `process_write` call.
pub const MAX_INPUT: usize = 64 * 1024;
/// How long a write waits for the command to take its input.
const INPUT_WAIT: Duration = Duration::from_secs(5);
/// How long output is still collected after a session ends, in case an
/// orphan of the command holds its pipes open.
const DRAIN_GRACE: Duration = Duration::from_secs(1);
/// Characters of a command shown by `list`.
const SHOWN_COMMAND: usize = 200;

/// How a session ended, or that it has not.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Running,
    Exited(ExitStatus),
    Killed,
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
            Self::TimedOut { seconds } => write!(f, "stopped at its {seconds} s timeout"),
            Self::Failed(kind) => write!(f, "lost: {kind}"),
        }
    }
}

/// Output not yet polled from one stream.
#[derive(Debug, Default)]
struct Unread {
    bytes: VecDeque<u8>,
    dropped: u64,
}
impl Unread {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        let over = self.bytes.len().saturating_sub(MAX_STREAM);
        self.bytes.drain(..over);
        self.dropped += over as u64;
    }
    fn take(&mut self, name: &str) -> String {
        let bytes: Vec<u8> = self.bytes.drain(..).collect();
        let mut text = format!("--- {name} ---\n{}", String::from_utf8_lossy(&bytes));
        if self.dropped > 0 {
            text.push_str(&format!(
                "\n[{} earlier {name} bytes dropped before this poll]",
                self.dropped
            ));
            self.dropped = 0;
        }
        text
    }
}

/// What the supervising task and the tool calls share. Locked only briefly,
/// never across an `.await`.
#[derive(Debug)]
struct Shared {
    status: Status,
    stdout: Unread,
    stderr: Unread,
}
type Output = Arc<Mutex<Shared>>;

/// Input for the command, with the result of writing it.
type Input = (Vec<u8>, bool, oneshot::Sender<std::io::Result<()>>);

struct Session {
    /// Start order, for eviction and listing.
    seq: u64,
    thread: Id,
    command: String,
    output: Output,
    input: mpsc::Sender<Input>,
    /// Sending or dropping this stops the command.
    stop: Option<oneshot::Sender<()>>,
    /// Dropping this detaches the supervising task, which then ends promptly
    /// because `stop` was dropped with it.
    _task: tokio::task::JoinHandle<()>,
}
impl Session {
    fn status(&self) -> Status {
        lock(&self.output).status.clone()
    }
}

fn lock(output: &Output) -> std::sync::MutexGuard<'_, Shared> {
    // A poisoned buffer still holds valid bytes and status: every update is
    // a single assignment or append.
    output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The sessions of one host.
#[derive(Default)]
pub(super) struct Sessions {
    map: Mutex<BTreeMap<Id, Session>>,
    started: std::sync::atomic::AtomicU64,
}
impl Sessions {
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<Id, Session>> {
        // Each update is a single insert or removal.
        self.map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Make room for one more session of `thread` by removing its oldest
    /// finished sessions. Running sessions are never evicted, and neither is
    /// another conversation's output it has not collected.
    fn admit(map: &mut BTreeMap<Id, Session>, thread: Id) -> Result<(), ExecError> {
        loop {
            let limit = if map.values().filter(|s| s.thread == thread).count() >= MAX_PER_THREAD {
                MAX_PER_THREAD
            } else if map.len() >= MAX_SESSIONS {
                MAX_SESSIONS
            } else {
                return Ok(());
            };
            let oldest = map
                .iter()
                .filter(|(_, s)| s.thread == thread && s.status().finished())
                .min_by_key(|(_, s)| s.seq)
                .map(|(id, _)| *id)
                .ok_or(ExecError::TooManySessions { limit })?;
            map.remove(&oldest);
        }
    }

    /// Check that `thread` may start one more session.
    pub(super) fn reserve(&self, thread: Id) -> Result<(), ExecError> {
        Self::admit(&mut self.map(), thread)
    }

    /// Supervise a started command; returns its session id.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn adopt(
        &self,
        thread: Id,
        command: String,
        started: Started,
        seconds: u64,
        deadline: tokio::time::Instant,
    ) -> Result<Id, ExecError> {
        let output: Output = Arc::new(Mutex::new(Shared {
            status: Status::Running,
            stdout: Unread::default(),
            stderr: Unread::default(),
        }));
        let (input, inputs) = mpsc::channel(4);
        let (stop, stopped) = oneshot::channel();
        let mut map = self.map();
        Self::admit(&mut map, thread)?;
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
                seq: self
                    .started
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                thread,
                command,
                output,
                input,
                stop: Some(stop),
                _task: task,
            },
        );
        Ok(id)
    }

    pub(super) fn list(&self, thread: Id) -> String {
        let map = self.map();
        let mut owned: Vec<_> = map.iter().filter(|(_, s)| s.thread == thread).collect();
        owned.sort_by_key(|(_, s)| s.seq);
        let lines: Vec<String> = owned
            .into_iter()
            .map(|(id, s)| {
                let mut command: String = s.command.chars().take(SHOWN_COMMAND).collect();
                if command.len() < s.command.len() {
                    command.push('…');
                }
                format!("{id}  {}  {command}", s.status())
            })
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
        };
        let (out, err) = (shared.stdout.take("stdout"), shared.stderr.take("stderr"));
        let text = format!("{}\n{out}\n{err}", shared.status);
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
                stop.send(()).ignore();
                Ok(())
            }
            None => Err(ExecError::SessionFinished),
        }
    }

    /// Forget a session, stopping it first if it still runs.
    pub(super) fn remove(&self, thread: Id, id: Id) -> Result<(), ExecError> {
        let mut map = self.map();
        owned(&map, thread, id)?;
        // Dropping the session drops its stop sender, which ends it.
        map.remove(&id);
        Ok(())
    }

    /// Give the command input, then close its input if `close`.
    pub(super) async fn write(
        &self,
        thread: Id,
        id: Id,
        data: Vec<u8>,
        close: bool,
    ) -> Result<(), ExecError> {
        // Taken out so no lock is held while waiting for the command.
        let input = owned(&self.map(), thread, id)?.input.clone();
        let (done, written) = oneshot::channel();
        input
            .send((data, close, done))
            .await
            .map_err(|_| ExecError::InputClosed)?;
        match tokio::time::timeout(INPUT_WAIT, written).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(source))) => Err(ExecError::Input(source)),
            Ok(Err(_)) => Err(ExecError::InputClosed),
            Err(_) => Err(ExecError::InputStalled {
                seconds: INPUT_WAIT.as_secs(),
            }),
        }
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
    mut stop: oneshot::Receiver<()>,
    seconds: u64,
    deadline: tokio::time::Instant,
) {
    let Started {
        mut child,
        group,
        control,
        scratch,
    } = started;
    let (Some(stdout), Some(stderr), Some(mut stdin)) =
        (child.stdout.take(), child.stderr.take(), child.stdin.take())
    else {
        unreachable!("background sessions pipe all three streams")
    };
    let pumps = futures_util::future::join(
        pump(stdout, output.clone(), |s| &mut s.stdout),
        pump(stderr, output.clone(), |s| &mut s.stderr),
    );
    tokio::pin!(pumps);
    let mut drained = false;
    let feed = async {
        use tokio::io::AsyncWriteExt;
        let mut open = true;
        while let Some((data, close, done)) = inputs.recv().await {
            let result = if open {
                let mut written = stdin.write_all(&data).await;
                if written.is_ok() {
                    written = stdin.flush().await;
                }
                if close && written.is_ok() {
                    written = stdin.shutdown().await;
                    open = false;
                }
                written
            } else {
                Err(std::io::ErrorKind::BrokenPipe.into())
            };
            // The caller may have stopped waiting.
            done.send(result).ignore();
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
            // Sent by `kill`, or dropped with the session.
            _ = &mut stop => break Status::Killed,
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

/// Copy a stream into its unread buffer until it ends.
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
}
