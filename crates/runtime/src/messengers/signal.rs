//! Signal adapter: talks to a signal-cli daemon the user runs, over its
//! JSON-RPC socket (`signal-cli daemon --socket PATH --receive-mode=manual`,
//! or `--tcp` on loopback). signal-cli holds the account's keys and does all
//! the encryption; Agent 206 never sees or stores them, so no vault
//! credential is involved, and the gateway never starts, stops or updates
//! the daemon.
//!
//! People are identified by their ACI, which signal-cli reports for every
//! sender once it has decrypted and authenticated the message, so a
//! sender list names exactly who wrote. A direct message's chat is the
//! sender's ACI; a group's is its id.
//!
//! The session runs in its own task, subscribed to the account's messages,
//! and answers replies on the same connection. Messages that arrive while
//! nothing is subscribed stay with the Signal service until the next
//! subscription, when the daemon runs with `--receive-mode=manual`.
use super::listener::{Listener, Queue};
use super::{Account, Batch, Event, Inbound, MessengerError, Result};
use a206_core::messengers::{
    SignalDaemon, valid_signal_account, valid_signal_aci, valid_signal_group,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const SERVICE: &str = "Signal";
/// Longest line read from the daemon. Messages with long text or many group
/// members stay well under this; anything longer ends the session.
const MAX_LINE: usize = 1 << 20;
/// Characters per message, as Signal's own clients send them.
const MAX_MESSAGE: usize = 2000;
/// Most parts one reply is sent as.
const MAX_PARTS: usize = 8;
/// Ends a reply that did not fit in [`MAX_PARTS`] messages.
const CUT_SHORT: &str = "\n(reply cut short; read the rest in Agent 206)";
/// How long the daemon may take to answer a request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Replies waiting for the session.
const OUTGOING: usize = 16;
/// Characters of a signal-cli refusal kept for display.
const MAX_REFUSAL: usize = 300;
/// Recent message keys remembered, so a message delivered twice (for
/// example by a second subscription) is answered once.
const RECENT: usize = 1024;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(crate) struct Signal {
    daemon: std::result::Result<SignalDaemon, a206_core::messengers::SignalDaemonError>,
    login: Option<String>,
    session: Option<Session>,
    /// Messages handled lately, kept across subscriptions so one signal-cli
    /// delivers again after a reconnect is not answered twice. The login
    /// cannot change without a new adapter, so these are all its account's.
    recent: Arc<Mutex<Recent>>,
    /// Keys of the batch last returned. The gateway asks for the next batch
    /// once it handled every event of this one, so they count as handled
    /// then, unless a reply failed meanwhile (`failed`): a failed reply ends
    /// the gateway's session with the rest of the batch unhandled, and a
    /// redelivery is answered at least once rather than never.
    returned: Vec<String>,
    failed: std::sync::atomic::AtomicBool,
    /// The listener of a session that ended, still holding messages it
    /// queued before its connection closed.
    ended: Option<Listener>,
}

struct Session {
    listener: Listener,
    outgoing: mpsc::Sender<Reply>,
    account: Account,
}

/// A reply for the session to send, answered once signal-cli has.
struct Reply {
    chat: String,
    text: String,
    done: oneshot::Sender<Result<()>>,
}

impl Signal {
    pub(crate) fn new(endpoint: Option<&str>, login: Option<String>) -> Self {
        let daemon = endpoint
            .ok_or(a206_core::messengers::SignalDaemonError::Shape)
            .and_then(SignalDaemon::parse);
        Self {
            daemon,
            login,
            session: None,
            recent: Arc::default(),
            returned: Vec::new(),
            failed: std::sync::atomic::AtomicBool::new(false),
            ended: None,
        }
    }

    /// Connect to the daemon, check that it serves the account and subscribe
    /// to its messages, unless the current session is still running.
    pub(crate) async fn connect(&mut self) -> Result<Account> {
        // A session whose task ended can send nothing more, but what it
        // queued was delivered by signal-cli and may not come again: its
        // listener is drained first, with replies going through the new
        // session.
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.outgoing.is_closed())
            && let Some(ended) = self.session.take()
        {
            self.ended = Some(ended.listener);
        }
        if let Some(session) = &self.session {
            return Ok(session.account.clone());
        }
        let daemon = self.daemon.clone().map_err(MessengerError::SignalDaemon)?;
        let login = self
            .login
            .clone()
            .filter(|login| valid_signal_account(login))
            .ok_or(MessengerError::NoLogin)?;
        let io = open(&daemon).await?;
        let (reader, writer) = tokio::io::split(io);
        let mut rpc = Rpc {
            reader: BufReader::new(reader),
            writer: Writer { writer, next: 0 },
            early: Vec::new(),
        };
        let accounts = rpc.call_now("listAccounts", json!({})).await?;
        let served = accounts.as_array().is_some_and(|accounts| {
            accounts.iter().any(|entry| {
                ["number", "aci", "uuid"]
                    .iter()
                    .any(|key| entry.get(key).and_then(Value::as_str) == Some(login.as_str()))
            })
        });
        if !served {
            return Err(MessengerError::SignalAccount { account: login });
        }
        rpc.call_now("subscribeReceive", json!({ "account": login }))
            .await?;
        let account = Account {
            id: login.clone(),
            name: Some(login.clone()),
        };
        let (outgoing, requests) = mpsc::channel(OUTGOING);
        let recent = Arc::clone(&self.recent);
        let listener = Listener::start(SERVICE, account.id.clone(), move |queue| {
            serve(rpc, login, requests, queue, recent)
        });
        self.session = Some(Session {
            listener,
            outgoing,
            account: account.clone(),
        });
        Ok(account)
    }

    /// The messages the session queued, waiting for at least one. A session
    /// failure ends it; the next connect subscribes again.
    pub(crate) async fn receive(&mut self, _cursor: Option<&str>) -> Result<Batch> {
        let failed = self.failed.swap(false, std::sync::atomic::Ordering::SeqCst);
        if failed {
            self.returned.clear();
        } else {
            let mut recent = lock(&self.recent);
            for key in self.returned.drain(..) {
                recent.insert(&key);
            }
        }
        if let Some(ended) = self.ended.as_mut() {
            match ended.receive().await {
                Ok(batch) => return Ok(self.unhandled(batch)),
                // Drained; its failure was the reason for reconnecting.
                Err(_) => self.ended = None,
            }
        }
        let Some(session) = self.session.as_mut() else {
            return Err(MessengerError::SocketEnded { service: SERVICE });
        };
        match session.listener.receive().await {
            Ok(batch) => Ok(self.unhandled(batch)),
            Err(error) => {
                self.session = None;
                Err(error)
            }
        }
    }

    /// `batch` without what was handled since it was queued (a message the
    /// ended session and the new subscription both delivered), remembering
    /// the rest as returned.
    fn unhandled(&mut self, mut batch: Batch) -> Batch {
        {
            let recent = lock(&self.recent);
            batch.events.retain(|(key, _)| !recent.contains(key));
        }
        self.returned = batch.events.iter().map(|(key, _)| key.clone()).collect();
        batch
    }

    /// Send `text` to a group or, for a direct chat, to the ACI it names.
    pub(crate) async fn send(&self, chat: &str, text: &str) -> Result<()> {
        let sent = self.reply(chat, text).await;
        if sent.is_err() {
            self.failed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        sent
    }

    async fn reply(&self, chat: &str, text: &str) -> Result<()> {
        let ended = || MessengerError::SocketEnded { service: SERVICE };
        let session = self.session.as_ref().ok_or_else(ended)?;
        let (done, answer) = oneshot::channel();
        session
            .outgoing
            .send(Reply {
                chat: chat.to_owned(),
                text: text.to_owned(),
                done,
            })
            .await
            .map_err(|_| ended())?;
        answer.await.map_err(|_| ended())?
    }
}

/// Open the daemon's socket. A path is a Unix socket, so its permissions
/// decide who else can use the account.
async fn open(daemon: &SignalDaemon) -> Result<Box<dyn Io>> {
    let unreachable = |source| MessengerError::SignalUnreachable { source };
    match daemon {
        #[cfg(unix)]
        SignalDaemon::Socket(path) => Ok(Box::new(
            tokio::net::UnixStream::connect(path)
                .await
                .map_err(unreachable)?,
        )),
        #[cfg(not(unix))]
        SignalDaemon::Socket(_) => Err(MessengerError::SignalDaemon(
            a206_core::messengers::SignalDaemonError::Shape,
        )),
        SignalDaemon::Tcp(address) => {
            let stream = tokio::net::TcpStream::connect(address)
                .await
                .map_err(unreachable)?;
            // Asked for loopback; refuse a connection that came from elsewhere.
            let peer = stream.peer_addr().map_err(unreachable)?;
            if !peer.ip().is_loopback() {
                return Err(MessengerError::SignalDaemon(
                    a206_core::messengers::SignalDaemonError::NotLoopback,
                ));
            }
            Ok(Box::new(stream))
        }
    }
}

/// A JSON-RPC connection: one JSON object per line each way.
struct Rpc {
    reader: BufReader<tokio::io::ReadHalf<Box<dyn Io>>>,
    writer: Writer,
    /// Notifications that arrived while `call_now` waited, in order, for
    /// the session to handle first.
    early: Vec<Incoming>,
}

/// What the daemon sent: an answer to a request, or a notification.
#[derive(Deserialize)]
struct Incoming {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    #[serde(default)]
    message: String,
}

impl Rpc {
    /// Call `method` before the session starts, when nothing else is
    /// waiting: the result. Notifications sent meanwhile are kept for the
    /// session, since a message signal-cli delivered is not delivered again.
    async fn call_now(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.writer.request(method, params).await?;
        let answer = async {
            loop {
                let incoming = read(&mut self.reader).await?;
                if incoming.id.as_ref().and_then(Value::as_str) == Some(id.as_str()) {
                    return answered(incoming);
                }
                if incoming.method.is_some() {
                    self.early.push(incoming);
                }
            }
        };
        tokio::time::timeout(REQUEST_TIMEOUT, answer)
            .await
            .map_err(|_| MessengerError::SignalTimeout)?
    }
}

/// The next message from the daemon, of at most [`MAX_LINE`] bytes. Not
/// cancel-safe: a partly read line is lost, so the session reads in a task
/// of its own that is never interrupted mid-line.
async fn read(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Incoming> {
    let mut line = Vec::new();
    let read = tokio::io::AsyncReadExt::take(&mut *reader, MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|source| MessengerError::SignalUnreachable { source })?;
    if read == 0 {
        return Err(MessengerError::SocketEnded { service: SERVICE });
    }
    if line.len() > MAX_LINE {
        return Err(MessengerError::TooLarge { service: SERVICE });
    }
    serde_json::from_slice(&line).map_err(|source| MessengerError::Malformed {
        service: SERVICE,
        source,
    })
}

/// A response's result, or the daemon's refusal.
fn answered(incoming: Incoming) -> Result<Value> {
    match incoming.error {
        Some(error) => Err(MessengerError::SignalRefused {
            code: error.code,
            message: refusal_text(&error.message),
        }),
        None => Ok(incoming.result.unwrap_or(Value::Null)),
    }
}

/// signal-cli's explanation as it may be shown and logged: one line, of
/// bounded length, with anything shaped like a credential removed.
fn refusal_text(message: &str) -> String {
    let line: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    // Redacted whole, before cutting, so no credential is cut to a prefix
    // the scanner no longer recognizes.
    let line = match a206_leak_scan::redact(&line) {
        Some(redacted) => redacted.text,
        None => line,
    };
    line.chars().take(MAX_REFUSAL).collect()
}

/// Queue messages and send replies until something fails.
async fn serve(
    rpc: Rpc,
    login: String,
    requests: mpsc::Receiver<Reply>,
    queue: Queue,
    recent: Arc<Mutex<Recent>>,
) -> MessengerError {
    match session(rpc, login, requests, queue, recent).await {
        Ok(never) => match never {},
        Err(error) => error,
    }
}

/// Lines are read in a task of their own, which forwards whole messages, so
/// waiting on replies or deadlines never drops half a line.
async fn session(
    rpc: Rpc,
    login: String,
    mut requests: mpsc::Receiver<Reply>,
    queue: Queue,
    recent: Arc<Mutex<Recent>>,
) -> Result<std::convert::Infallible> {
    let Rpc {
        mut reader,
        writer: mut rpc,
        early,
    } = rpc;
    // Queued in this subscription; the shared `recent` holds only what the
    // gateway handled, so a message queued but lost with a failed session
    // can still be answered when signal-cli delivers it again.
    let mut queued = Recent::default();
    for message in early {
        notified(&message, &login, &recent, &mut queued, &queue)?;
    }
    let (lines, mut incoming) = mpsc::channel::<Result<Incoming>>(64);
    // Aborted when the session ends (`_reading`), so it never outlives the
    // connection it reads; it holds nothing that needs awaiting.
    let reading = tokio::spawn(async move {
        loop {
            let message = read(&mut reader).await;
            let failed = message.is_err();
            if lines.send(message).await.is_err() || failed {
                return;
            }
        }
    });
    let _reading = AbortOnDrop(reading);
    // Replies waiting for signal-cli, by request id.
    let mut waiting: HashMap<String, (oneshot::Sender<Result<()>>, VecDeque<Value>)> =
        HashMap::new();
    let mut deadlines: VecDeque<(tokio::time::Instant, String)> = VecDeque::new();
    loop {
        let deadline = deadlines.front().map(|(at, _)| *at);
        tokio::select! {
            message = incoming.recv() => {
                let message = message.ok_or(MessengerError::SocketEnded { service: SERVICE })??;
                if let Some(id) = message.id.as_ref().and_then(Value::as_str).map(str::to_owned) {
                    let Some((done, mut rest)) = waiting.remove(&id) else { continue };
                    deadlines.retain(|(_, waiting)| *waiting != id);
                    match answered(message) {
                        Err(error) => answer(done, Err(error)),
                        Ok(_) => match rest.pop_front() {
                            None => answer(done, Ok(())),
                            Some(params) => {
                                let id = rpc.request("send", params).await?;
                                deadlines.push_back((tokio::time::Instant::now() + REQUEST_TIMEOUT, id.clone()));
                                waiting.insert(id, (done, rest));
                            }
                        },
                    }
                    continue;
                }
                notified(&message, &login, &recent, &mut queued, &queue)?;
            }
            reply = requests.recv() => {
                // The adapter went away with its listener.
                let Some(Reply { chat, text, done }) = reply else {
                    return Err(MessengerError::SocketEnded { service: SERVICE });
                };
                let mut parts: VecDeque<Value> = send_params(&login, &chat, &text).into();
                let Some(first) = parts.pop_front() else {
                    answer(done, Ok(()));
                    continue;
                };
                let id = rpc.request("send", first).await?;
                deadlines.push_back((tokio::time::Instant::now() + REQUEST_TIMEOUT, id.clone()));
                waiting.insert(id, (done, parts));
            }
            () = sleep_until(deadline) => {
                if let Some((_, id)) = deadlines.pop_front()
                    && let Some((done, _)) = waiting.remove(&id)
                {
                    answer(done, Err(MessengerError::SignalTimeout));
                }
            }
        }
    }
}

/// Queue a `receive` notification's message unless it was seen lately,
/// in this subscription or an earlier one.
fn notified(
    message: &Incoming,
    login: &str,
    recent: &Mutex<Recent>,
    queued: &mut Recent,
    queue: &Queue,
) -> Result<()> {
    if message.method.as_deref() != Some("receive") {
        return Ok(());
    }
    let Some((key, event)) = message.params.as_ref().and_then(|p| received(p, login)) else {
        return Ok(());
    };
    if lock(recent).contains(&key) || queued.contains(&key) {
        return Ok(());
    }
    queue.push(key.clone(), event)?;
    // Only once it is queued: a message refused for a full queue is not
    // remembered, so its redelivery is not mistaken for a duplicate.
    queued.insert(&key);
    Ok(())
}

/// The shared cache, held only for a check or an insert and never across
/// an await. A poisoned lock only means a session panicked mid-insert; the
/// keys it holds are still sound.
fn lock(recent: &Mutex<Recent>) -> std::sync::MutexGuard<'_, Recent> {
    recent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Aborts the reading task when the session ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The write half of a session's connection.
struct Writer {
    writer: tokio::io::WriteHalf<Box<dyn Io>>,
    next: u64,
}
impl Writer {
    async fn request(&mut self, method: &str, params: Value) -> Result<String> {
        self.next += 1;
        let id = self.next.to_string();
        let mut line = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|source| MessengerError::Malformed {
            service: SERVICE,
            source,
        })?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .await
            .map_err(|source| MessengerError::SignalUnreachable { source })?;
        Ok(id)
    }
}

/// Message keys seen lately, oldest dropped first.
#[derive(Default)]
struct Recent {
    keys: HashSet<String>,
    order: VecDeque<String>,
}
impl Recent {
    fn contains(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    /// Whether `key` is new, remembering it.
    fn insert(&mut self, key: &str) -> bool {
        if !self.keys.insert(key.to_owned()) {
            return false;
        }
        self.order.push_back(key.to_owned());
        if self.order.len() > RECENT
            && let Some(old) = self.order.pop_front()
        {
            self.keys.remove(&old);
        }
        true
    }
}

/// Answer whoever asked for `done`. A turn cancelled meanwhile no longer
/// listens, and then nobody needs the answer.
fn answer(done: oneshot::Sender<Result<()>>, result: Result<()>) {
    if done.send(result).is_err() {
        tracing::debug!("a Signal reply finished after its turn stopped waiting");
    }
}

/// Sleep until `at`, or forever without one.
async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// A person's text message for `login`, from a `receive` notification's
/// params (a subscription wraps the message in `result`), and its key.
fn received(params: &Value, login: &str) -> Option<(String, Event)> {
    let message = params.get("result").unwrap_or(params);
    if message.get("account").and_then(Value::as_str) != Some(login) {
        return None;
    }
    let envelope = message.get("envelope")?;
    let sender = envelope
        .get("sourceUuid")
        .and_then(Value::as_str)
        .filter(|aci| valid_signal_aci(aci))?;
    let timestamp = envelope.get("timestamp").and_then(Value::as_u64)?;
    let key = format!("{sender}:{timestamp}");
    // Edits, reactions, receipts, typing, stories and messages synced from
    // the account's other devices carry no `dataMessage` text.
    let Some(data) = envelope.get("dataMessage") else {
        return Some((key, Event::Ignored));
    };
    let text = data.get("message").and_then(Value::as_str).unwrap_or("");
    let reaction = data.get("reaction").is_some() || data.get("remoteDelete").is_some();
    if text.trim().is_empty() || reaction {
        return Some((key, Event::Ignored));
    }
    let title = envelope
        .get("sourceName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    // A message with any group information belongs to that group, never
    // to the sender's direct chat, whether or not its id can be read.
    let group = data
        .get("groupInfo")
        .map(|info| info.get("groupId").and_then(Value::as_str).unwrap_or(""));
    let inbound = match group {
        Some(group) if valid_signal_group(group) => Inbound {
            chat: group.to_owned(),
            sender: Some(sender.to_owned()),
            group: true,
            title: None,
            text: text.to_owned(),
            encrypted: false,
        },
        // A group id signal-cli wrote some other way: nobody's chat.
        Some(_) => return Some((key, Event::Ignored)),
        None => Inbound {
            chat: sender.to_owned(),
            sender: Some(sender.to_owned()),
            group: false,
            title,
            text: text.to_owned(),
            encrypted: false,
        },
    };
    Some((key, Event::Message(inbound)))
}

/// `send` params for each part of `text` to `chat`: a group by id, or the
/// ACI a direct chat names.
fn send_params(login: &str, chat: &str, text: &str) -> Vec<Value> {
    let mut parts: Vec<String> = super::split(text, MAX_MESSAGE)
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if parts.len() > MAX_PARTS {
        // The last part makes room to say the reply was cut short.
        parts.truncate(MAX_PARTS);
        let last = parts.pop().unwrap_or_default();
        let kept = super::split(&last, MAX_MESSAGE - CUT_SHORT.len())
            .into_iter()
            .next()
            .unwrap_or_default();
        parts.push(format!("{kept}{CUT_SHORT}"));
    }
    parts
        .into_iter()
        .map(|part| {
            if valid_signal_group(chat) {
                json!({ "account": login, "groupId": chat, "message": part })
            } else {
                json!({ "account": login, "recipient": [chat], "message": part })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aci() -> String {
        uuid::Uuid::new_v4().hyphenated().to_string()
    }

    fn group() -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(uuid::Uuid::new_v4().as_bytes().repeat(2))
    }

    #[test]
    fn notifications_name_the_sender_by_aci_and_groups_by_id() {
        let (login, from, group, text) = ("+15550000001", aci(), group(), aci());
        let envelope = |data: Value| {
            json!({
                "account": login,
                "envelope": {
                    "source": "+15550000002",
                    "sourceUuid": from,
                    "sourceName": "Someone",
                    "timestamp": 7,
                    "dataMessage": data,
                }
            })
        };
        let (key, event) = received(
            &json!({ "subscription": 0, "result": envelope(json!({ "message": text })) }),
            login,
        )
        .unwrap();
        assert_eq!(key, format!("{from}:7"));
        let Event::Message(direct) = event else {
            panic!("{event:?}")
        };
        assert_eq!(
            (
                direct.chat.as_str(),
                direct.sender.as_deref(),
                direct.group,
                direct.title.as_deref()
            ),
            (from.as_str(), Some(from.as_str()), false, Some("Someone"))
        );
        let Some((_, Event::Message(in_group))) = received(
            &envelope(
                json!({ "message": text, "groupInfo": { "groupId": group, "type": "DELIVER" } }),
            ),
            login,
        ) else {
            panic!("expected a group message");
        };
        assert_eq!(
            (in_group.chat.as_str(), in_group.group, in_group.title),
            (group.as_str(), true, None)
        );
        // Another account's messages, reactions and empty messages.
        assert!(received(&envelope(json!({ "message": text })), "+15550000009").is_none());
        for data in [
            json!({ "message": "", }),
            json!({ "reaction": { "emoji": "x" }, "message": text }),
        ] {
            assert!(matches!(
                received(&envelope(data), login),
                Some((_, Event::Ignored))
            ));
        }
        // Group information without a readable id is still not a direct chat.
        for info in [
            json!({}),
            json!({ "groupId": 5 }),
            json!({ "groupId": aci() }),
        ] {
            assert!(matches!(
                received(
                    &envelope(json!({ "message": text, "groupInfo": info })),
                    login
                ),
                Some((_, Event::Ignored))
            ));
        }
        // Without an authenticated sender there is nobody to answer.
        let mut anonymous = envelope(json!({ "message": text }));
        anonymous["envelope"]["sourceUuid"] = json!(null);
        assert!(received(&anonymous, login).is_none());
    }

    #[test]
    fn replies_go_to_the_group_or_the_aci_in_bounded_parts() {
        let (login, to, group) = ("+15550000001", aci(), group());
        let long = "word ".repeat(MAX_MESSAGE);
        let direct = send_params(login, &to, &long);
        assert!(direct.len() > 1 && direct.len() <= MAX_PARTS);
        assert!(
            direct
                .iter()
                .all(|p| p["recipient"] == json!([to]) && p["account"] == login)
        );
        assert!(
            direct
                .iter()
                .all(|p| p["message"].as_str().unwrap().encode_utf16().count() <= MAX_MESSAGE)
        );
        let grouped = send_params(login, &group, "hi");
        assert_eq!(
            grouped,
            [json!({ "account": login, "groupId": group, "message": "hi" })]
        );
        assert!(send_params(login, &to, " \n ").is_empty());
        // Too long for every part: the last says the reply was cut short.
        let longer = "word ".repeat(MAX_MESSAGE * MAX_PARTS);
        let cut = send_params(login, &to, &longer);
        assert_eq!(cut.len(), MAX_PARTS);
        assert!(
            cut.iter()
                .all(|p| p["message"].as_str().unwrap().encode_utf16().count() <= MAX_MESSAGE)
        );
        assert!(
            cut[MAX_PARTS - 1]["message"]
                .as_str()
                .unwrap()
                .ends_with(CUT_SHORT)
        );
        assert!(
            !direct.last().unwrap()["message"]
                .as_str()
                .unwrap()
                .ends_with(CUT_SHORT)
        );
    }

    #[test]
    fn daemon_refusals_are_shown_as_one_bounded_line_without_credentials() {
        let token = format!(
            "ghp_{}",
            uuid::Uuid::new_v4().simple().to_string().repeat(2)
        );
        let text = refusal_text(&format!("bad\nrequest {token}"));
        assert!(!text.contains(&token) && !text.contains('\n'), "{text}");
        assert!(text.starts_with("bad request "));
        let long = refusal_text(&"x".repeat(MAX_REFUSAL * 2));
        assert_eq!(long.chars().count(), MAX_REFUSAL);
        // A credential across the cut is removed whole, not cut to a prefix.
        let straddling = refusal_text(&format!("{} {token}", "x".repeat(MAX_REFUSAL - 10)));
        assert!(!straddling.contains("ghp_"), "{straddling}");
    }
}
