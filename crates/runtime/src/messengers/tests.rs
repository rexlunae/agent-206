//! The supervisor and Telegram adapter against a live local Bot API stand-in
//! and a real vault broker, with a recording host in place of the gateway.
use super::*;
use a206_core::{Mode, ModelKey};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

fn random() -> String {
    Id::new_v4().to_string()
}

/// A filesystem socket entry does not establish bind/listen or greeting readiness.
async fn ready<T, F, Fut>(mut connect: F) -> Result<T, a206_broker::ClientError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, a206_broker::ClientError>>,
{
    loop {
        match connect().await {
            Err(a206_broker::ClientError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                tokio::task::yield_now().await
            }
            result => return result,
        }
    }
}

#[tokio::test]
async fn broker_readiness_retries_transient_startup_errors_only() {
    let mut attempts = 0;
    ready(|| {
        attempts += 1;
        std::future::ready(match attempts {
            1 => Err(a206_broker::ClientError::Io(
                std::io::ErrorKind::NotFound.into(),
            )),
            2 => Err(a206_broker::ClientError::Io(
                std::io::ErrorKind::ConnectionRefused.into(),
            )),
            _ => Ok(()),
        })
    })
    .await
    .unwrap();
    assert_eq!(attempts, 3);
    let mut attempts = 0;
    let result = ready(|| {
        attempts += 1;
        std::future::ready(Err::<(), _>(a206_broker::ClientError::Io(
            std::io::ErrorKind::PermissionDenied.into(),
        )))
    })
    .await;
    assert!(
        matches!(result, Err(a206_broker::ClientError::Io(e)) if e.kind() == std::io::ErrorKind::PermissionDenied)
    );
    assert_eq!(attempts, 1);
}

/// What the stand-in Bot API serves and records.
#[derive(Clone, Default)]
struct Bot {
    token: String,
    username: String,
    /// The bot's account id; changing it stands for another bot's token.
    id: Arc<Mutex<i64>>,
    /// Status `getMe` answers with.
    me_status: Arc<Mutex<u16>>,
    /// Batches `getUpdates` returns in order (`null` answers HTTP 500);
    /// then empty batches.
    batches: Arc<Mutex<VecDeque<Value>>>,
    polls: Arc<Mutex<Vec<HashMap<String, String>>>>,
    sent: Arc<Mutex<Vec<(String, String)>>>,
    me_calls: Arc<Mutex<u32>>,
}

fn check_path(bot: &Bot, path: &str) {
    assert_eq!(path, format!("bot{}", bot.token), "token not in the path");
}

async fn me(
    State(bot): State<Bot>,
    Path(path): Path<String>,
) -> (axum::http::StatusCode, Json<Value>) {
    check_path(&bot, &path);
    *bot.me_calls.lock().unwrap() += 1;
    let status = axum::http::StatusCode::from_u16(*bot.me_status.lock().unwrap()).unwrap();
    (
        status,
        Json(json!({"ok": true, "result": {
            "id": *bot.id.lock().unwrap(),
            "username": bot.username,
        }})),
    )
}

async fn updates(
    State(bot): State<Bot>,
    Path(path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> (axum::http::StatusCode, Json<Value>) {
    check_path(&bot, &path);
    bot.polls.lock().unwrap().push(query);
    let next = bot.batches.lock().unwrap().pop_front();
    match next {
        Some(Value::Null) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false})),
        ),
        Some(batch) => (
            axum::http::StatusCode::OK,
            Json(json!({"ok": true, "result": batch})),
        ),
        None => {
            // An idle long poll, kept short for the test.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            (
                axum::http::StatusCode::OK,
                Json(json!({"ok": true, "result": []})),
            )
        }
    }
}

async fn send(
    State(bot): State<Bot>,
    Path(path): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    check_path(&bot, &path);
    let chat = body["chat_id"].as_str().unwrap().to_owned();
    let text = body["text"].as_str().unwrap().to_owned();
    bot.sent.lock().unwrap().push((chat, text));
    Json(json!({"ok": true, "result": {}}))
}

async fn serve_bot(bot: Bot) -> String {
    let router = Router::new()
        .route("/{bot}/getMe", get(me))
        .route("/{bot}/getUpdates", get(updates))
        .route("/{bot}/sendMessage", post(send))
        .with_state(bot);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    origin
}

/// A broker with the bot token stored and bound to `origin`, and a pairing
/// anchored for the test's lifetime.
async fn broker(
    origin: &str,
    token: &str,
) -> (
    tempfile::TempDir,
    String,
    BrokerPairing,
    crate::provider::PairingAnchor,
) {
    let (dir, mut names, pairing, anchor) = broker_with(origin, &[token]).await;
    (dir, names.remove(0), pairing, anchor)
}

/// A broker with each token stored as its own credential bound to `origin`.
async fn broker_with(
    origin: &str,
    tokens: &[&str],
) -> (
    tempfile::TempDir,
    Vec<String>,
    BrokerPairing,
    crate::provider::PairingAnchor,
) {
    use a206_broker::*;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let serving = root.clone();
    let mut server = tokio::spawn(async move { serve(&serving).await });
    let startup = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            connected = ready(|| Client::connect(&root, Role::User)) => connected,
            stopped = &mut server => panic!("test broker exited before readiness: {stopped:?}"),
        }
    })
    .await;
    let mut user = match startup {
        Ok(Ok(client)) => client,
        Ok(Err(error)) => {
            server.abort();
            let stopped = server.await;
            panic!("test broker connection failed: {error:?}; server stopped: {stopped:?}");
        }
        Err(error) => {
            server.abort();
            let stopped = server.await;
            panic!("test broker readiness timed out: {error:?}; server stopped: {stopped:?}");
        }
    };
    // Deliberately detached after successful readiness: the test's Tokio
    // runtime owns the server lifetime and cancels it when the test ends.
    // Failed startup above aborts and awaits it before deleting the profile.
    drop(server);
    user.call(Request::Unlock {
        password: WireSecret::new(random()),
        totp: None,
        create: true,
    })
    .await
    .unwrap();
    let mut names = vec![];
    for token in tokens {
        let name = random();
        user.call(Request::PutCredential {
            name: name.clone(),
            meta: a206_vault::CredentialMeta {
                label: name.clone(),
                kind: a206_vault::CredentialKind::Token,
                policy: a206_vault::AccessPolicy::WithApproval,
                description: None,
                disabled: false,
                destinations: vec![origin.to_owned()],
            },
            value: WireValue::Single(WireSecret::new((*token).to_owned())),
            replace: false,
        })
        .await
        .unwrap();
        names.push(name);
    }
    let BrokerReply::GatewayPairing { token } = user.call(Request::PairGateway).await.unwrap()
    else {
        panic!("expected a pairing");
    };
    let pairing = BrokerPairing::new(token.expose().to_owned());
    let anchor = crate::provider::verify_pairing(&root, &pairing)
        .await
        .unwrap();
    (dir, names, pairing, anchor)
}

/// Records what the supervisor asks of the gateway.
#[derive(Clone)]
struct Recorder {
    root: PathBuf,
    pairing: BrokerPairing,
    pairings: Arc<watch::Sender<u64>>,
    threads: Arc<Mutex<BTreeMap<ChatBinding, Id>>>,
    turns: Arc<Mutex<Vec<(Id, String)>>>,
    statuses: Arc<Mutex<BTreeMap<String, MessengerStatus>>>,
    /// How many times each messenger started connecting.
    connects: Arc<Mutex<BTreeMap<String, u32>>>,
    changed: Arc<tokio::sync::Notify>,
    /// Sessions offered to `send_message`, by messenger.
    posters: Arc<Mutex<BTreeMap<String, Poster>>>,
    /// Saved reading positions, by messenger; shared by hosts that stand for
    /// the same gateway state across a restart.
    cursors: Arc<Mutex<BTreeMap<String, SavedCursor>>>,
}
impl Host for Recorder {
    fn post_through(&self, name: &str, poster: Option<Poster>) {
        let mut posters = self.posters.lock().unwrap();
        match poster {
            Some(poster) => posters.insert(name.to_owned(), poster),
            None => posters.remove(name),
        };
    }
    fn saved_cursor(&self, name: &str) -> Result<Option<SavedCursor>> {
        Ok(self.cursors.lock().unwrap().get(name).cloned())
    }
    fn save_cursor(&self, name: &str, cursor: &SavedCursor) -> Result<()> {
        self.cursors
            .lock()
            .unwrap()
            .insert(name.to_owned(), cursor.clone());
        Ok(())
    }
    fn root(&self) -> PathBuf {
        self.root.clone()
    }
    fn pairing(&self) -> Option<BrokerPairing> {
        Some(self.pairing.clone())
    }
    fn pairings(&self) -> watch::Receiver<u64> {
        self.pairings.subscribe()
    }
    fn thread_for(&self, _: &MessengerConfig, chat: &ChatBinding, _: &str) -> Result<Id> {
        Ok(*self
            .threads
            .lock()
            .unwrap()
            .entry(chat.clone())
            .or_insert_with(Id::new_v4))
    }
    async fn converse(&self, thread: Id, text: String) -> Result<Option<String>> {
        self.turns.lock().unwrap().push((thread, text.clone()));
        Ok(Some(format!("reply to {text}")))
    }
    fn update(&self, name: &str, change: impl FnOnce(&mut MessengerStatus) + Send) {
        let mut statuses = self.statuses.lock().unwrap();
        let status = statuses.entry(name.to_owned()).or_default();
        let before = status.state;
        change(status);
        if status.state == MessengerState::Connecting && before != MessengerState::Connecting {
            *self
                .connects
                .lock()
                .unwrap()
                .entry(name.to_owned())
                .or_default() += 1;
        }
        drop(statuses);
        self.changed.notify_waiters();
    }
}
impl Recorder {
    fn status(&self, name: &str) -> MessengerStatus {
        self.statuses
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default()
    }
    fn connects(&self, name: &str) -> u32 {
        self.connects
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(0)
    }
    /// Wait until `done` holds, re-checking after every status change.
    async fn until(&self, done: impl Fn(&Self) -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let changed = self.changed.notified();
                if done(self) {
                    return;
                }
                // Turns and sends change no status, so poll as well.
                tokio::select! {
                    () = changed => {}
                    () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
                }
            }
        })
        .await
        .expect("condition not reached");
    }
}

/// A text message; a private chat's id is its sender's.
fn message(update: i64, chat: i64, from: Option<i64>, title: &str, text: &str) -> Value {
    let kind = if from.is_some() { "group" } else { "private" };
    json!({
        "update_id": update,
        "message": {
            "message_id": update,
            "chat": {"id": chat, "type": kind, "title": title},
            "from": {"id": from.unwrap_or(chat), "first_name": random()},
            "text": text,
        }
    })
}

fn config(name: &str, origin: &str, credential: &str, allowed: i64) -> MessengerConfig {
    MessengerConfig {
        enabled: true,
        credential: Some(credential.to_owned()),
        endpoint: Some(origin.to_owned()),
        allowed_chats: BTreeSet::from([allowed.to_string()]),
        model: Some(ModelKey {
            provider: Id::new_v4(),
            model: random(),
        }),
        mode: Mode::Chat,
        ..MessengerConfig::new(name.to_owned(), MessengerKind::Telegram)
    }
}

async fn setup(
    me_status: u16,
    token: Option<String>,
) -> (
    Bot,
    String,
    tempfile::TempDir,
    String,
    Recorder,
    crate::provider::PairingAnchor,
) {
    let bot = Bot {
        token: token
            .unwrap_or_else(|| format!("{}:{}", std::process::id(), random().replace('-', "_"))),
        username: random(),
        me_status: Arc::new(Mutex::new(me_status)),
        ..Bot::default()
    };
    let origin = serve_bot(bot.clone()).await;
    let (dir, credential, pairing, anchor) = broker(&origin, &bot.token).await;
    let host = Recorder {
        root: dir.path().to_owned(),
        pairing,
        pairings: Arc::new(watch::Sender::new(0)),
        threads: Arc::default(),
        turns: Arc::default(),
        statuses: Arc::default(),
        connects: Arc::default(),
        changed: Arc::default(),
        posters: Arc::default(),
        cursors: Arc::default(),
    };
    (bot, origin, dir, credential, host, anchor)
}

const FAST: Backoff = Backoff {
    initial: std::time::Duration::from_millis(5),
    max: std::time::Duration::from_millis(20),
};

#[tokio::test(flavor = "multi_thread")]
async fn allowed_chats_get_a_thread_and_a_reply_and_others_are_refused() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(200, None).await;
    let name = "team-bot";
    let (allowed, refused) = (-1_000_000 - i64::from(std::process::id()), 42);
    let (member, stranger) = (7_000, 8_000);
    let refused_title = random();
    let text = random();
    bot.batches.lock().unwrap().push_back(json!([
        message(5, refused, None, &refused_title, &random()),
        // An allowed group, but a sender who is not listed.
        message(6, allowed, Some(stranger), &random(), &random()),
        message(8, allowed, Some(member), &random(), &text),
    ]));
    let mut entry = config(name, &origin, &credential, allowed);
    entry.allowed_users.insert(member.to_string());
    let (desired, receiver) = watch::channel(vec![Desired {
        config: entry.clone(),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));

    host.until(|h| !bot.sent.lock().unwrap().is_empty() && h.status(name).refused.len() == 2)
        .await;
    let status = host.status(name);
    assert_eq!(status.state, MessengerState::Connected);
    assert_eq!(status.account, Some(format!("@{}", bot.username)));
    assert_eq!(status.refused[0].chat, refused.to_string());
    assert_eq!(status.refused[0].sender, None);
    assert_eq!(
        status.refused[0].title.as_deref(),
        Some(refused_title.as_str())
    );
    assert_eq!(status.refused[1].chat, allowed.to_string());
    assert_eq!(status.refused[1].sender, Some(stranger.to_string()));
    // Only the allowed chat reached a thread, and its reply went back to it.
    let turns = host.turns.lock().unwrap().clone();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].1, text);
    assert_eq!(
        *bot.sent.lock().unwrap(),
        [(allowed.to_string(), format!("reply to {text}"))]
    );
    // The next poll confirms the batch.
    host.until(|_| bot.polls.lock().unwrap().len() >= 2).await;
    assert_eq!(
        bot.polls.lock().unwrap()[1]
            .get("offset")
            .map(String::as_str),
        Some("9")
    );
    assert!(!bot.polls.lock().unwrap()[0].contains_key("offset"));

    // Disabling stops it; a stopped supervisor awaits its task.
    desired.send_replace(vec![Desired {
        config: entry,
        run: false,
    }]);
    host.until(|h| h.status(name).state == MessengerState::Stopped)
        .await;
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_token_fails_once_and_waits_for_the_user() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(401, None).await;
    let name = "rejected";
    let wanted = vec![Desired {
        config: config(name, &origin, &credential, 1),
        run: true,
    }];
    let (desired, receiver) = watch::channel(wanted.clone());
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| h.status(name).state == MessengerState::Failed)
        .await;
    let failure = host.status(name).failure.unwrap();
    assert_eq!(failure.kind, ErrorKind::PermissionDenied);
    // Retrying cannot help, so it does not retry: with the fast backoff, a
    // retry would have happened many times over meanwhile.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(*bot.me_calls.lock().unwrap(), 1);
    assert!(bot.polls.lock().unwrap().is_empty());
    assert_eq!(host.status(name).state, MessengerState::Failed);

    // Once the token is fixed, connecting again (the same settings
    // republished) retries it.
    *bot.me_status.lock().unwrap() = 200;
    desired.send_replace(wanted);
    host.until(|h| h.status(name).state == MessengerState::Connected)
        .await;
    assert_eq!(host.connects(name), 2);
    assert_eq!(host.status(name).failure, None);
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_that_is_not_a_bot_token_is_not_retried() {
    let token = format!("{}/../{}", std::process::id(), random());
    let (bot, origin, _dir, credential, host, _anchor) = setup(200, Some(token)).await;
    let name = "odd-token";
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: config(name, &origin, &credential, 1),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| h.status(name).state == MessengerState::Failed)
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(host.connects(name), 1);
    assert_eq!(*bot.me_calls.lock().unwrap(), 0);
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_poll_does_not_replay_answered_messages() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(200, None).await;
    let name = "flaky";
    let chat = 77;
    let text = random();
    {
        let mut batches = bot.batches.lock().unwrap();
        batches.push_back(json!([message(3, chat, None, &random(), &text)]));
        // The poll that would confirm it fails; the session reconnects.
        batches.push_back(Value::Null);
    }
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: config(name, &origin, &credential, chat),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| h.connects(name) >= 2 && bot.polls.lock().unwrap().len() >= 3)
        .await;
    // The poll after reconnecting still confirms the answered message.
    assert_eq!(
        bot.polls.lock().unwrap()[2]
            .get("offset")
            .map(String::as_str),
        Some("4")
    );
    assert_eq!(host.turns.lock().unwrap().len(), 1);
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn another_bot_behind_the_credential_starts_from_a_fresh_offset() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(200, None).await;
    let name = "replaced";
    let chat = 12;
    bot.batches
        .lock()
        .unwrap()
        .push_back(json!([message(7, chat, None, &random(), &random())]));
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: config(name, &origin, &credential, chat),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    // The same bot is polled with the offset past the handled update.
    host.until(|_| bot.polls.lock().unwrap().len() >= 2).await;
    assert_eq!(
        bot.polls.lock().unwrap()[1]
            .get("offset")
            .map(String::as_str),
        Some("8")
    );
    // The stored token now belongs to another bot, whose update ids are its
    // own: its polls must not confirm any of them.
    let polled = bot.polls.lock().unwrap().len();
    *bot.id.lock().unwrap() += 1;
    host.until(|_| bot.polls.lock().unwrap().len() >= polled + 2)
        .await;
    let polls = bot.polls.lock().unwrap().clone();
    let after = polls
        .iter()
        .skip(polled)
        .position(|poll| !poll.contains_key("offset"))
        .expect("the new bot was polled without an offset");
    assert!(
        polls[polled + after + 1..]
            .iter()
            .all(|poll| poll.get("offset").is_none()),
        "an offset was sent to the new bot"
    );
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_without_a_model_is_told_why() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(200, None).await;
    let name = "no-model";
    let chat = 5;
    bot.batches
        .lock()
        .unwrap()
        .push_back(json!([message(1, chat, None, &random(), &random())]));
    let mut entry = config(name, &origin, &credential, chat);
    entry.model = None;
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|_| !bot.sent.lock().unwrap().is_empty()).await;
    let (to, notice) = bot.sent.lock().unwrap()[0].clone();
    assert_eq!(to, chat.to_string());
    assert!(notice.contains("choose a model"), "{notice}");
    assert!(host.turns.lock().unwrap().is_empty());
    assert_eq!(
        host.status(name).failure.map(|f| f.kind),
        Some(ErrorKind::InvalidRequest)
    );
    stop.cancel();
    supervisor.await.unwrap();
}

#[test]
fn long_replies_split_at_newlines_within_the_limit() {
    let line = "x".repeat(7);
    let text = format!("{line}\n{line}\n{line}");
    let parts = split(&text, 10);
    assert_eq!(parts.concat(), text);
    assert_eq!(
        parts,
        [format!("{line}\n"), format!("{line}\n"), line.clone()]
    );
    // No newline: a hard split that never cuts a character, counting
    // UTF-16 units (an emoji is two).
    let wide = "😀".repeat(6);
    let parts = split(&wide, 5);
    assert_eq!(parts.concat(), wide);
    assert!(parts.iter().all(|p| p.encode_utf16().count() <= 5));
    assert!(split("", 5).is_empty());
}

/// What the stand-in Matrix homeserver serves and records.
#[derive(Clone, Default)]
struct Homeserver {
    token: String,
    user: String,
    /// `rooms` objects the syncs return in order; then empty ones.
    syncs: Arc<Mutex<VecDeque<Value>>>,
    sync_queries: Arc<Mutex<Vec<HashMap<String, String>>>>,
    /// Rooms that are encrypted from their Nth encryption check on (0 for
    /// always), and how often each was checked.
    encrypted_from: Arc<Mutex<HashMap<String, u32>>>,
    checks: Arc<Mutex<HashMap<String, u32>>>,
    state_status: Arc<Mutex<HashMap<String, u16>>>,
    state_body: Arc<Mutex<HashMap<String, Value>>>,
    pages: Arc<Mutex<VecDeque<Value>>>,
    page_queries: Arc<Mutex<Vec<HashMap<String, String>>>>,
    joined: Arc<Mutex<Vec<String>>>,
    /// Room, transaction id and content of each sent event.
    sent: Arc<Mutex<Vec<(String, String, Value)>>>,
    send_statuses: Arc<Mutex<VecDeque<u16>>>,
}

fn check_bearer(server: &Homeserver, headers: &axum::http::HeaderMap) {
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {}", server.token).as_str()),
        "no access token"
    );
}

async fn serve_homeserver(server: Homeserver) -> String {
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::put;
    let whoami = |State(server): State<Homeserver>, headers: HeaderMap| async move {
        check_bearer(&server, &headers);
        Json(json!({ "user_id": server.user }))
    };
    let sync = |State(server): State<Homeserver>,
                headers: HeaderMap,
                Query(query): Query<HashMap<String, String>>| async move {
        check_bearer(&server, &headers);
        let count = {
            let mut queries = server.sync_queries.lock().unwrap();
            queries.push(query);
            queries.len()
        };
        let next = server.syncs.lock().unwrap().pop_front();
        let rooms = match next {
            Some(rooms) => rooms,
            None => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                json!({})
            }
        };
        Json(json!({ "next_batch": format!("s{count}"), "rooms": rooms }))
    };
    let encryption = |State(server): State<Homeserver>,
                      headers: HeaderMap,
                      Path(room): Path<String>| async move {
        check_bearer(&server, &headers);
        let mut checks = server.checks.lock().unwrap();
        let check = checks.entry(room.clone()).or_default();
        let encrypted = server
            .encrypted_from
            .lock()
            .unwrap()
            .get(&room)
            .is_some_and(|from| *check >= *from);
        *check += 1;
        if let Some(status) = server.state_status.lock().unwrap().get(&room) {
            return (
                StatusCode::from_u16(*status).unwrap(),
                Json(json!({"errcode": "M_NOT_FOUND"})),
            );
        }
        if let Some(body) = server.state_body.lock().unwrap().get(&room) {
            return (StatusCode::OK, Json(body.clone()));
        }
        if encrypted {
            (
                StatusCode::OK,
                Json(json!([{"type": "m.room.encryption", "content": {"algorithm": random()}}])),
            )
        } else {
            (StatusCode::OK, Json(json!([])))
        }
    };
    let messages = |State(server): State<Homeserver>,
                    headers: HeaderMap,
                    Query(query): Query<HashMap<String, String>>| async move {
        check_bearer(&server, &headers);
        server.page_queries.lock().unwrap().push(query);
        Json(
            server
                .pages
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected backfill"),
        )
    };
    let join = |State(server): State<Homeserver>, headers: HeaderMap, Path(room): Path<String>| async move {
        check_bearer(&server, &headers);
        server.joined.lock().unwrap().push(room.clone());
        Json(json!({ "room_id": room }))
    };
    let send = |State(server): State<Homeserver>,
                headers: HeaderMap,
                Path((room, transaction)): Path<(String, String)>,
                Json(content): Json<Value>| async move {
        check_bearer(&server, &headers);
        server
            .sent
            .lock()
            .unwrap()
            .push((room, transaction, content));
        let status = server
            .send_statuses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(200);
        (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({ "event_id": format!("${}", random()) })),
        )
    };
    let router = Router::new()
        .route("/_matrix/client/v3/account/whoami", get(whoami))
        .route("/_matrix/client/v3/sync", get(sync))
        .route("/_matrix/client/v3/rooms/{room}/state", get(encryption))
        .route("/_matrix/client/v3/join/{room}", post(join))
        .route("/_matrix/client/v3/rooms/{room}/messages", get(messages))
        .route(
            "/_matrix/client/v3/rooms/{room}/send/m.room.message/{transaction}",
            put(send),
        )
        .with_state(server);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    origin
}

fn matrix_id(sigil: char, host: &str) -> String {
    format!("{sigil}{}:{host}", Id::new_v4().simple())
}

fn text_event(sender: &str, msgtype: &str, body: &str) -> Value {
    json!({
        "type": "m.room.message",
        "event_id": format!("${}", random()),
        "sender": sender,
        "content": {"msgtype": msgtype, "body": body},
    })
}

fn timeline(rooms: Vec<(&str, Vec<Value>)>) -> Value {
    let join: serde_json::Map<String, Value> = rooms
        .into_iter()
        .map(|(room, events)| (room.to_owned(), json!({"timeline": {"events": events}})))
        .collect();
    json!({ "join": join })
}

async fn matrix_setup() -> (
    Homeserver,
    String,
    tempfile::TempDir,
    String,
    Recorder,
    crate::provider::PairingAnchor,
) {
    let host = format!("{}.test", Id::new_v4().simple());
    let server = Homeserver {
        token: random(),
        user: matrix_id('@', &host),
        ..Homeserver::default()
    };
    let origin = serve_homeserver(server.clone()).await;
    let (dir, credential, pairing, anchor) = broker(&origin, &server.token).await;
    let host = Recorder {
        root: dir.path().to_owned(),
        pairing,
        pairings: Arc::new(watch::Sender::new(0)),
        threads: Arc::default(),
        turns: Arc::default(),
        statuses: Arc::default(),
        connects: Arc::default(),
        changed: Arc::default(),
        posters: Arc::default(),
        cursors: Arc::default(),
    };
    (server, origin, dir, credential, host, anchor)
}

/// A new adapter given a cursor first reads the pending invitations, with
/// a sync that has no cursor, and keeps `since`.
async fn past_invitations(server: &Homeserver, adapter: &mut matrix::Matrix, since: &str) {
    server.syncs.lock().unwrap().push_back(json!({}));
    let batch = adapter.receive(Some(since)).await.unwrap();
    assert!(batch.events.is_empty() && !batch.complete);
    assert_eq!(batch.next.as_deref(), Some(since));
    let query = server.sync_queries.lock().unwrap().pop().unwrap();
    assert!(!query.contains_key("since"));
}

fn matrix_config(name: &str, origin: &str, credential: &str, rooms: &[&str]) -> MessengerConfig {
    MessengerConfig {
        kind: MessengerKind::Matrix,
        allowed_chats: rooms.iter().map(|room| (*room).to_owned()).collect(),
        ..config(name, origin, credential, 0)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_answers_listed_senders_in_allowed_rooms_and_never_history() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let name = "matrix";
    let domain = server.user.split_once(':').unwrap().1.to_owned();
    let (room, invited, other) = (
        matrix_id('!', &domain),
        matrix_id('!', &domain),
        matrix_id('!', &domain),
    );
    let (member, stranger) = (matrix_id('@', &domain), matrix_id('@', &domain));
    let (history, text, other_title) = (random(), random(), random());
    {
        let mut syncs = server.syncs.lock().unwrap();
        // The first sync: history and invitations.
        let mut first = timeline(vec![(&room, vec![text_event(&member, "m.text", &history)])]);
        first["invite"] = json!({
            &invited: {"invite_state": {"events": []}},
            &other: {"invite_state": {"events": [
                {"type": "m.room.name", "state_key": "", "sender": stranger, "content": {"name": other_title}},
            ]}},
        });
        syncs.push_back(first);
        syncs.push_back(timeline(vec![(
            &room,
            vec![
                text_event(&stranger, "m.text", &random()),
                text_event(&server.user, "m.text", &random()),
                text_event(&member, "m.notice", &random()),
                text_event(&member, "m.text", &text),
            ],
        )]));
    }
    let mut entry = matrix_config(name, &origin, &credential, &[&room, &invited]);
    entry.allowed_users.insert(member.clone());
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| !server.sent.lock().unwrap().is_empty() && h.status(name).refused.len() == 2)
        .await;

    let status = host.status(name);
    assert_eq!(status.state, MessengerState::Connected);
    assert_eq!(status.account.as_ref(), Some(&server.user));
    // Only the allowed invitation was accepted.
    assert_eq!(
        *server.joined.lock().unwrap(),
        std::slice::from_ref(&invited)
    );
    assert_eq!(status.refused[0].chat, other);
    assert_eq!(
        status.refused[0].title.as_deref(),
        Some(other_title.as_str())
    );
    assert_eq!(status.refused[1].chat, room);
    assert_eq!(status.refused[1].sender.as_ref(), Some(&stranger));
    // History, the bot's own message and a notice were not answered.
    let turns = host.turns.lock().unwrap().clone();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].1, text);
    let sent = server.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, room);
    assert_eq!(
        sent[0].2,
        json!({"msgtype": "m.notice", "body": format!("reply to {text}")})
    );
    let queries = server.sync_queries.lock().unwrap().clone();
    assert_eq!(queries[0].get("timeout").map(String::as_str), Some("0"));
    assert!(!queries[0].contains_key("since"));
    assert_eq!(queries[1].get("since").map(String::as_str), Some("s1"));
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_sent_in_plain_text_to_an_encrypted_room() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let name = "encrypted";
    let domain = server.user.split_once(':').unwrap().1.to_owned();
    let member = matrix_id('@', &domain);
    let (always, later, ciphertext) = (
        matrix_id('!', &domain),
        matrix_id('!', &domain),
        matrix_id('!', &domain),
    );
    {
        let mut from = server.encrypted_from.lock().unwrap();
        from.insert(always.clone(), 0);
        // Encrypted after the check before the turn: before its reply.
        from.insert(later.clone(), 1);
    }
    {
        let mut syncs = server.syncs.lock().unwrap();
        syncs.push_back(json!({}));
        syncs.push_back(timeline(vec![
            (&always, vec![text_event(&member, "m.text", &random())]),
            (&later, vec![text_event(&member, "m.text", &random())]),
            (
                &ciphertext,
                vec![json!({
                    "type": "m.room.encrypted",
                    "event_id": format!("${}", random()),
                    "sender": member,
                    "content": {"algorithm": random(), "ciphertext": random()},
                })],
            ),
        ]));
    }
    let mut entry = matrix_config(name, &origin, &credential, &[&always, &later, &ciphertext]);
    entry.allowed_users.insert(member.clone());
    // Without a model, a turn fails, and its notice must not go out either.
    entry.model = None;
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|_| server.sync_queries.lock().unwrap().len() >= 3)
        .await;

    assert!(server.sent.lock().unwrap().is_empty());
    assert!(host.turns.lock().unwrap().is_empty());
    let status = host.status(name);
    // Each case was reported in the status and the messenger kept running.
    assert_eq!(status.state, MessengerState::Connected);
    let failure = status.failure.unwrap();
    assert_eq!(failure.kind, ErrorKind::PermissionDenied);
    assert!(
        failure.detail().contains("encrypted"),
        "{}",
        failure.detail()
    );
    let checks = server.checks.lock().unwrap().clone();
    assert_eq!(checks.get(&always), Some(&1));
    assert_eq!(checks.get(&later), Some(&2));
    // An encrypted message is never answered, so its room is not checked.
    assert_eq!(checks.get(&ciphertext), None);
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_matrix_messenger_without_a_homeserver_waits_for_the_user() {
    let (server, _origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let name = "no-server";
    let mut entry = matrix_config(name, "", &credential, &[]);
    entry.endpoint = None;
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| h.status(name).state == MessengerState::Failed)
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(host.connects(name), 1);
    assert_eq!(
        host.status(name).failure.map(|f| f.kind),
        Some(ErrorKind::InvalidRequest)
    );
    assert!(server.sync_queries.lock().unwrap().is_empty());
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_recovers_limited_timelines_and_ignores_edits() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let config = matrix_config("backfill", &origin, &credential, &[]);
    let room = matrix_id('!', "example.test");
    let mut adapter = matrix::Matrix::new(
        Api::new(host.root.clone(), host.pairing.clone(), &config).unwrap(),
        [room.clone()].into(),
    );
    adapter.connect().await.unwrap();
    past_invitations(&server, &mut adapter, "previous-sync").await;
    let sender = matrix_id('@', "example.test");
    let old = text_event(&sender, "m.text", &random());
    let recent = text_event(&sender, "m.text", &random());
    let mut edit = text_event(&sender, "m.text", "* edited");
    edit["content"]["m.relates_to"] = json!({"rel_type": "m.replace", "event_id": old["event_id"]});
    server
        .syncs
        .lock()
        .unwrap()
        .push_back(json!({"join": {&room: {"timeline": {
            "limited": true, "prev_batch": "boundary", "events": [recent.clone(), edit],
        }}}}));
    server.pages.lock().unwrap().extend([
        json!({"chunk": [old.clone()], "end": "page2"}),
        // Deliberate overlap with both an earlier page and the timeline.
        json!({"chunk": [old, recent], "end": "boundary"}),
    ]);
    let batch = adapter.receive(Some("previous-sync")).await.unwrap();
    assert_eq!(batch.events.len(), 3);
    assert!(matches!(batch.events[0].1, Event::Message(_)));
    assert!(matches!(batch.events[1].1, Event::Message(_)));
    assert_eq!(batch.events[2].1, Event::Ignored);
    assert_eq!(batch.next.as_deref(), Some("s1"));
    let queries = server.page_queries.lock().unwrap();
    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0]["from"], "previous-sync");
    assert_eq!(queries[1]["from"], "page2");
    assert_eq!(queries[0]["to"], "boundary");
    assert_eq!(queries[0]["dir"], "f");
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_refuses_ambiguous_state_before_sending_plaintext() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let config = matrix_config("state", &origin, &credential, &[]);
    let adapter = matrix::Matrix::new(
        Api::new(host.root.clone(), host.pairing.clone(), &config).unwrap(),
        Default::default(),
    );
    let room = matrix_id('!', "example.test");
    for status in [404, 403, 401, 500] {
        server
            .state_status
            .lock()
            .unwrap()
            .insert(room.clone(), status);
        assert!(adapter.send(&room, "must stay private").await.is_err());
        assert!(server.sent.lock().unwrap().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_batch_budgets_do_not_advance_the_cursor() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let config = matrix_config("bounds", &origin, &credential, &[]);
    let room = matrix_id('!', "example.test");
    let sender = matrix_id('@', "example.test");
    let rooms: serde_json::Map<String, Value> = (0..65)
        .map(|_| (matrix_id('!', "example.test"), json!({})))
        .collect();
    let mut allowed: BTreeSet<String> = rooms.keys().cloned().collect();
    allowed.insert(room.clone());
    let mut adapter = matrix::Matrix::new(
        Api::new(host.root.clone(), host.pairing.clone(), &config).unwrap(),
        allowed,
    );
    past_invitations(&server, &mut adapter, "previous").await;
    server
        .syncs
        .lock()
        .unwrap()
        .push_back(json!({"join": rooms}));
    assert!(matches!(
        adapter.receive(Some("previous")).await,
        Err(MessengerError::SyncBudget { .. })
    ));
    let events: Vec<_> = (0..1_001)
        .map(|_| text_event(&sender, "m.text", "x"))
        .collect();
    server
        .syncs
        .lock()
        .unwrap()
        .push_back(timeline(vec![(&room, events)]));
    assert!(matches!(
        adapter.receive(Some("previous")).await,
        Err(MessengerError::SyncBudget { .. })
    ));
    server.syncs.lock().unwrap().push_back(json!({"join": {&room: {"timeline": {"limited": true, "prev_batch": "boundary", "events": []}}}}));
    server
        .pages
        .lock()
        .unwrap()
        .push_back(json!({"chunk": [], "end": "previous"}));
    assert!(matches!(
        adapter.receive(Some("previous")).await,
        Err(MessengerError::SyncGap { .. })
    ));
    server.syncs.lock().unwrap().push_back(json!({"join": {&room: {"timeline": {"limited": true, "prev_batch": "boundary", "events": []}}}}));
    server.pages.lock().unwrap().push_back(json!({"chunk": (0..1_001).map(|_| text_event(&sender, "m.text", "x")).collect::<Vec<_>>(), "end": "boundary"}));
    assert!(matches!(
        adapter.receive(Some("previous")).await,
        Err(MessengerError::SyncBudget { .. })
    ));
    server.syncs.lock().unwrap().push_back(json!({"join": {&room: {"timeline": {"limited": true, "prev_batch": "boundary", "events": []}}}}));
    server
        .pages
        .lock()
        .unwrap()
        .extend((0..32).map(|i| json!({"chunk": [], "end": format!("page-{i}")})));
    assert!(matches!(
        adapter.receive(Some("previous")).await,
        Err(MessengerError::SyncBudget { .. })
    ));
    assert!(
        server
            .sync_queries
            .lock()
            .unwrap()
            .iter()
            .all(|q| q["since"] == "previous")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unrelated_rooms_do_not_starve_allowed_messages_and_large_state_is_scanned() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let room = matrix_id('!', "example.test");
    let sender = matrix_id('@', "example.test");
    let config = matrix_config("unrelated", &origin, &credential, &[&room]);
    let mut adapter = matrix::Matrix::new(
        Api::new(host.root.clone(), host.pairing.clone(), &config).unwrap(),
        config.allowed_chats.clone(),
    );
    past_invitations(&server, &mut adapter, "previous").await;
    let mut joined: serde_json::Map<String, Value> = (0..100)
        .map(|_| {
            (
                matrix_id('!', "example.test"),
                json!({"timeline": {"limited": true}}),
            )
        })
        .collect();
    joined.insert(
        room.clone(),
        json!({"timeline": {"events": [text_event(&sender, "m.text", "deliver me")]}}),
    );
    server
        .syncs
        .lock()
        .unwrap()
        .push_back(json!({"join": joined}));
    let batch = adapter.receive(Some("previous")).await.unwrap();
    assert_eq!(batch.events.len(), 64);
    assert_eq!(batch.next.as_deref(), Some("previous"));
    assert!(
        batch
            .events
            .iter()
            .all(|(_, event)| matches!(event, Event::Discovered { .. }))
    );
    let batch = adapter.receive(Some("previous")).await.unwrap();
    assert_eq!(batch.events.len(), 37);
    assert!(matches!(&batch.events[36].1, Event::Message(message) if message.chat == room));
    assert_eq!(batch.next.as_deref(), Some("s1"));
    assert!(server.page_queries.lock().unwrap().is_empty());
    let mut state: Vec<_> = (0..2_000)
        .map(|_| json!({"type": "m.room.member", "content": {"membership": "join"}}))
        .collect();
    server
        .state_body
        .lock()
        .unwrap()
        .insert(room.clone(), json!(state));
    adapter.send(&room, "large room supported").await.unwrap();
    assert_eq!(server.sent.lock().unwrap().len(), 1);
    state.push(json!({"type": "m.room.encryption", "content": {"algorithm": random()}}));
    server
        .state_body
        .lock()
        .unwrap()
        .insert(room.clone(), json!(state));
    assert!(matches!(
        adapter.send(&room, "never plaintext").await,
        Err(MessengerError::EncryptedRoom { .. })
    ));
    assert_eq!(server.sent.lock().unwrap().len(), 1);
    server
        .state_body
        .lock()
        .unwrap()
        .insert(room.clone(), json!([{"content": {}}]));
    assert!(adapter.send(&room, "malformed state").await.is_err());
    assert_eq!(server.sent.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_drains_all_pending_invitations_before_confirming_the_sync() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let config = matrix_config("pending", &origin, &credential, &[]);
    let mut adapter = matrix::Matrix::new(
        Api::new(host.root.clone(), host.pairing.clone(), &config).unwrap(),
        Default::default(),
    );
    adapter.connect().await.unwrap();
    let invites: serde_json::Map<String, Value> = (0..130)
        .map(|_| (matrix_id('!', "example.test"), json!({})))
        .collect();
    server
        .syncs
        .lock()
        .unwrap()
        .push_back(json!({"invite": invites}));
    let mut seen = BTreeSet::new();
    for expected in [64, 64, 2] {
        // Revalidating the same account must not discard the pending queue.
        adapter.connect().await.unwrap();
        let batch = adapter.receive(None).await.unwrap();
        assert_eq!(batch.events.len(), expected);
        for (_, event) in batch.events {
            let Event::Discovered { chat, .. } = event else {
                panic!("discovery must not cause a turn or a join")
            };
            assert!(seen.insert(chat));
        }
        assert_eq!(batch.next.as_deref(), (expected == 2).then_some("s1"));
    }
    assert_eq!(seen.len(), 130);
    assert_eq!(server.sync_queries.lock().unwrap().len(), 1);
    assert!(server.joined.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_reconnect_after_a_final_chunk_failure_never_repeats_a_turn() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let room = matrix_id('!', "example.test");
    let member = matrix_id('@', "example.test");
    let mut rooms: serde_json::Map<String, Value> = (0..65)
        .map(|_| (matrix_id('!', "example.test"), json!({})))
        .collect();
    rooms.insert(
        room.clone(),
        json!({"timeline": {"events": [text_event(&member, "m.text", &random())]}}),
    );
    let replay = json!({"join": rooms});
    server
        .syncs
        .lock()
        .unwrap()
        .extend([json!({}), replay.clone(), replay]);
    server.send_statuses.lock().unwrap().push_back(500);
    let mut config = matrix_config("retry-chunks", &origin, &credential, &[&room]);
    config.allowed_users.insert(member);
    let (_desired, receiver) = watch::channel(vec![Desired { config, run: true }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|_| server.sync_queries.lock().unwrap().len() >= 4)
        .await;
    stop.cancel();
    supervisor.await.unwrap();
    assert_eq!(
        host.turns.lock().unwrap().len(),
        1,
        "a failed send must not charge a second turn"
    );
    assert_eq!(server.sent.lock().unwrap().len(), 1);
    let queries = server.sync_queries.lock().unwrap();
    assert_eq!(queries[1]["since"], "s1");
    assert_eq!(queries[2]["since"], "s1");
    assert_eq!(queries[3]["since"], "s3");
}

/// What the stand-in Discord API and Gateway serve and record.
#[derive(Clone, Default)]
struct DiscordApi {
    token: String,
    bot_id: String,
    username: String,
    origin: Arc<Mutex<String>>,
    /// Dispatches the open gateway sends, in order.
    dispatches: Arc<Mutex<VecDeque<Value>>>,
    /// Close the open gateway with this code.
    close: Arc<Mutex<Option<u16>>>,
    /// Stop acknowledging heartbeats.
    silent: Arc<Mutex<bool>>,
    identifies: Arc<Mutex<Vec<Value>>>,
    heartbeats: Arc<Mutex<u32>>,
    /// Channel and body of each sent message.
    sent: Arc<Mutex<Vec<(String, Value)>>>,
    /// Channels that are direct messages; every other channel is a server's.
    direct: Arc<Mutex<BTreeSet<String>>>,
}

fn check_bot(api: &DiscordApi, headers: &axum::http::HeaderMap) {
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some(format!("Bot {}", api.token).as_str()),
        "no bot token"
    );
}

async fn serve_discord(api: DiscordApi) -> String {
    use axum::extract::ws::{CloseFrame, Message as Ws, WebSocketUpgrade};
    use axum::http::HeaderMap;
    use axum::routing::any;
    let me = |State(api): State<DiscordApi>, headers: HeaderMap| async move {
        check_bot(&api, &headers);
        Json(json!({"id": api.bot_id, "username": api.username, "bot": true}))
    };
    let gateway = |State(api): State<DiscordApi>, headers: HeaderMap| async move {
        check_bot(&api, &headers);
        let origin = api.origin.lock().unwrap().replace("http", "ws");
        Json(json!({"url": format!("{origin}/gateway")}))
    };
    let send = |State(api): State<DiscordApi>,
                headers: HeaderMap,
                Path(channel): Path<String>,
                Json(body): Json<Value>| async move {
        check_bot(&api, &headers);
        api.sent.lock().unwrap().push((channel, body));
        Json(json!({"id": random()}))
    };
    let channel = |State(api): State<DiscordApi>,
                   headers: HeaderMap,
                   Path(channel): Path<String>| async move {
        check_bot(&api, &headers);
        let kind = if api.direct.lock().unwrap().contains(&channel) {
            1
        } else {
            0
        };
        Json(json!({"id": channel, "type": kind}))
    };
    let socket = |State(api): State<DiscordApi>, upgrade: WebSocketUpgrade| async move {
        upgrade.on_upgrade(move |mut socket| async move {
            let hello = json!({"op": 10, "d": {"heartbeat_interval": 1000}});
            socket.send(Ws::text(hello.to_string())).await.unwrap();
            let Some(Ok(Ws::Text(identify))) = socket.recv().await else {
                panic!("no identify");
            };
            api.identifies
                .lock()
                .unwrap()
                .push(serde_json::from_str(identify.as_str()).unwrap());
            let ready = json!({"op": 0, "t": "READY", "s": 1, "d": {
                "user": {"id": api.bot_id, "username": api.username, "bot": true},
            }});
            socket.send(Ws::text(ready.to_string())).await.unwrap();
            let mut seq = 1;
            loop {
                let close = api.close.lock().unwrap().take();
                if let Some(code) = close {
                    let frame = CloseFrame {
                        code,
                        reason: random().into(),
                    };
                    // The client may already be gone; either way this session ends.
                    socket.send(Ws::Close(Some(frame))).await.ok();
                    return;
                }
                let next = api.dispatches.lock().unwrap().pop_front();
                if let Some(mut dispatch) = next {
                    seq += 1;
                    dispatch["s"] = json!(seq);
                    socket.send(Ws::text(dispatch.to_string())).await.unwrap();
                    continue;
                }
                let received =
                    tokio::time::timeout(std::time::Duration::from_millis(10), socket.recv()).await;
                match received {
                    Err(_) => {}
                    Ok(Some(Ok(Ws::Text(text)))) => {
                        let beat: Value = serde_json::from_str(text.as_str()).unwrap();
                        assert_eq!(beat["op"], 1);
                        *api.heartbeats.lock().unwrap() += 1;
                        if !*api.silent.lock().unwrap() {
                            let ack = json!({"op": 11});
                            socket.send(Ws::text(ack.to_string())).await.unwrap();
                        }
                    }
                    Ok(Some(Ok(_))) => {}
                    Ok(_) => return,
                }
            }
        })
    };
    let router = Router::new()
        .route("/api/v10/users/@me", get(me))
        .route("/api/v10/gateway/bot", get(gateway))
        .route("/api/v10/channels/{channel}", get(channel))
        .route("/api/v10/channels/{channel}/messages", post(send))
        .route("/gateway", any(socket))
        .with_state(api.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    *api.origin.lock().unwrap() = origin.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    origin
}

async fn discord_setup() -> (
    DiscordApi,
    String,
    tempfile::TempDir,
    String,
    Recorder,
    crate::provider::PairingAnchor,
) {
    let api = DiscordApi {
        token: random(),
        bot_id: random(),
        username: random(),
        ..DiscordApi::default()
    };
    let origin = serve_discord(api.clone()).await;
    let (dir, credential, pairing, anchor) = broker(&origin, &api.token).await;
    let host = Recorder {
        root: dir.path().to_owned(),
        pairing,
        pairings: Arc::new(watch::Sender::new(0)),
        threads: Arc::default(),
        turns: Arc::default(),
        statuses: Arc::default(),
        connects: Arc::default(),
        changed: Arc::default(),
        posters: Arc::default(),
        cursors: Arc::default(),
    };
    (api, origin, dir, credential, host, anchor)
}

fn discord_config(
    name: &str,
    origin: &str,
    credential: &str,
    channels: &[&str],
) -> MessengerConfig {
    MessengerConfig {
        kind: MessengerKind::Discord,
        allowed_chats: channels.iter().map(|c| (*c).to_owned()).collect(),
        ..config(name, origin, credential, 0)
    }
}

fn discord_message(channel: &str, guild: Option<&str>, author: &str, text: &str) -> Value {
    let mut d = json!({
        "id": random(),
        "type": 0,
        "channel_id": channel,
        "author": {"id": author, "username": random()},
        "content": text,
    });
    if let Some(guild) = guild {
        d["guild_id"] = json!(guild);
    }
    json!({"op": 0, "t": "MESSAGE_CREATE", "d": d})
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_answers_allowed_channels_and_ignores_itself_and_webhooks() {
    let (api, origin, _dir, credential, host, _anchor) = discord_setup().await;
    let name = "discord";
    let (dm, channel, other, guild) = (random(), random(), random(), random());
    let (member, stranger, friend) = (random(), random(), random());
    api.direct.lock().unwrap().insert(dm.clone());
    let mut entry = discord_config(name, &origin, &credential, &[&dm, &channel]);
    entry.allowed_users.extend([member.clone(), friend.clone()]);
    let (dm_text, group_text) = (random(), random());
    let mut webhook = discord_message(&channel, Some(&guild), &member, &random());
    webhook["d"]["webhook_id"] = json!(random());
    let mut join = discord_message(&channel, Some(&guild), &member, &random());
    join["d"]["type"] = json!(7);
    api.dispatches.lock().unwrap().extend([
        // The bot's own message, a webhook posing as a member, a join notice.
        discord_message(&channel, Some(&guild), &api.bot_id, &random()),
        webhook,
        join,
        discord_message(&other, Some(&guild), &member, &random()),
        discord_message(&channel, Some(&guild), &stranger, &random()),
        discord_message(&dm, None, &friend, &dm_text),
        discord_message(&channel, Some(&guild), &member, &group_text),
    ]);
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| api.sent.lock().unwrap().len() == 2 && h.status(name).refused.len() == 2)
        .await;
    let status = host.status(name);
    assert_eq!(status.state, MessengerState::Connected);
    assert_eq!(status.account.as_deref(), Some(api.username.as_str()));
    assert_eq!(status.refused[0].chat, other);
    assert_eq!(status.refused[1].chat, channel);
    assert_eq!(status.refused[1].sender.as_deref(), Some(stranger.as_str()));
    let turns: Vec<String> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.1.clone())
        .collect();
    assert_eq!(turns, [dm_text.clone(), group_text.clone()]);
    let sent = api.sent.lock().unwrap().clone();
    assert_eq!(sent[0].0, dm);
    assert_eq!(sent[0].1["content"], format!("reply to {dm_text}"));
    assert_eq!(sent[1].0, channel);
    // Replies never ping anyone.
    assert_eq!(sent[1].1["allowed_mentions"], json!({"parse": []}));
    // The broker identified with the token and the message content intent.
    let identify = api.identifies.lock().unwrap()[0].clone();
    assert_eq!(identify["d"]["token"], api.token.as_str());
    let intents = identify["d"]["intents"].as_u64().unwrap();
    assert_ne!(intents & (1 << 15), 0);
    // Heartbeats keep the session alive.
    host.until(|_| *api.heartbeats.lock().unwrap() >= 2).await;
    assert_eq!(host.connects(name), 1);
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_reconnects_when_asked_or_unacknowledged_and_stops_on_intents() {
    let (api, origin, _dir, credential, host, _anchor) = discord_setup().await;
    let name = "discord";
    let (channel, server) = (random(), random());
    api.direct.lock().unwrap().insert(channel.clone());
    // No senders are listed, so only direct messages are accepted.
    let entry = discord_config(name, &origin, &credential, &[&channel, &server]);
    let (desired, receiver) = watch::channel(vec![Desired {
        config: entry.clone(),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|_| api.identifies.lock().unwrap().len() == 1)
        .await;

    // Discord asks for a reconnect: the session starts again and still answers.
    api.dispatches
        .lock()
        .unwrap()
        .push_back(json!({"op": 7, "d": null}));
    host.until(|_| api.identifies.lock().unwrap().len() == 2)
        .await;
    let text = random();
    let author = random();
    // A server channel's message without a guild id is still a group's.
    let stranger = random();
    api.dispatches.lock().unwrap().extend([
        discord_message(&server, None, &stranger, &random()),
        discord_message(&channel, None, &author, &text),
    ]);
    host.until(|_| api.sent.lock().unwrap().len() == 1).await;
    assert_eq!(api.sent.lock().unwrap()[0].0, channel);
    let refused = host.status(name).refused;
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].chat, server);
    assert_eq!(refused[0].sender.as_deref(), Some(stranger.as_str()));

    // A gateway that stops acknowledging heartbeats is left for a new one.
    *api.silent.lock().unwrap() = true;
    host.until(|_| api.identifies.lock().unwrap().len() == 3)
        .await;
    *api.silent.lock().unwrap() = false;

    // Missing intents need the user, so the messenger stops retrying.
    *api.close.lock().unwrap() = Some(4014);
    host.until(|h| h.status(name).state == MessengerState::Failed)
        .await;
    let failure = host.status(name).failure.unwrap();
    assert_eq!(failure.kind, ErrorKind::PermissionDenied);
    assert!(failure.detail().contains("Message Content"));
    let identifies = api.identifies.lock().unwrap().len();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(api.identifies.lock().unwrap().len(), identifies);
    assert_eq!(host.status(name).state, MessengerState::Failed);
    drop(desired);
    stop.cancel();
    supervisor.await.unwrap();
}

/// What the stand-in Slack Web API and Socket Mode serve and record.
#[derive(Clone, Default)]
struct SlackApi {
    bot_token: String,
    app_token: String,
    user_id: String,
    user: String,
    origin: Arc<Mutex<String>>,
    /// Envelopes the open socket sends, in order.
    envelopes: Arc<Mutex<VecDeque<Value>>>,
    acks: Arc<Mutex<Vec<String>>>,
    opens: Arc<Mutex<u32>>,
    /// Channel and body of each posted message.
    sent: Arc<Mutex<Vec<(String, Value)>>>,
}

fn bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned)
}

async fn serve_slack(api: SlackApi) -> String {
    use axum::extract::ws::{Message as Ws, WebSocketUpgrade};
    use axum::http::HeaderMap;
    use axum::routing::any;
    let auth_test = |State(api): State<SlackApi>, headers: HeaderMap| async move {
        if bearer(&headers) != Some(api.bot_token.clone()) {
            return Json(json!({"ok": false, "error": "invalid_auth"}));
        }
        Json(json!({"ok": true, "user_id": api.user_id, "user": api.user}))
    };
    let open = |State(api): State<SlackApi>, headers: HeaderMap| async move {
        // Only the app-level token opens a socket.
        if bearer(&headers) != Some(api.app_token.clone()) {
            return Json(json!({"ok": false, "error": "not_allowed_token_type"}));
        }
        *api.opens.lock().unwrap() += 1;
        let origin = api.origin.lock().unwrap().replace("http", "ws");
        Json(json!({"ok": true, "url": format!("{origin}/socket?ticket={}", random())}))
    };
    let post_message =
        |State(api): State<SlackApi>, headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(
                bearer(&headers),
                Some(api.bot_token.clone()),
                "no bot token"
            );
            let channel = body["channel"].as_str().unwrap().to_owned();
            api.sent.lock().unwrap().push((channel, body));
            Json(json!({"ok": true}))
        };
    let socket = |State(api): State<SlackApi>, upgrade: WebSocketUpgrade| async move {
        upgrade.on_upgrade(move |mut socket| async move {
            let hello = json!({"type": "hello"});
            socket.send(Ws::text(hello.to_string())).await.unwrap();
            loop {
                let next = api.envelopes.lock().unwrap().pop_front();
                if let Some(envelope) = next {
                    let disconnect = envelope["type"] == "disconnect";
                    socket.send(Ws::text(envelope.to_string())).await.unwrap();
                    if disconnect {
                        return;
                    }
                    continue;
                }
                let received =
                    tokio::time::timeout(std::time::Duration::from_millis(10), socket.recv()).await;
                match received {
                    Err(_) => {}
                    Ok(Some(Ok(Ws::Text(text)))) => {
                        let ack: Value = serde_json::from_str(text.as_str()).unwrap();
                        let id = ack["envelope_id"].as_str().unwrap().to_owned();
                        api.acks.lock().unwrap().push(id);
                    }
                    Ok(Some(Ok(_))) => {}
                    Ok(_) => return,
                }
            }
        })
    };
    let router = Router::new()
        .route("/api/auth.test", post(auth_test))
        .route("/api/apps.connections.open", post(open))
        .route("/api/chat.postMessage", post(post_message))
        .route("/socket", any(socket))
        .with_state(api.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    *api.origin.lock().unwrap() = origin.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    origin
}

fn slack_event(channel: &str, channel_type: &str, user: &str, text: &str) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": random(),
        "payload": {"event": {
            "type": "message",
            "channel": channel,
            "channel_type": channel_type,
            "user": user,
            "text": text,
            "ts": format!("{}.{}", std::process::id(), Id::new_v4().as_u128() % 1_000_000),
        }},
    })
}

/// A Slack messenger whose bot and app tokens are separate vault credentials.
async fn slack_setup(
    channels: &[&str],
) -> (
    SlackApi,
    MessengerConfig,
    tempfile::TempDir,
    Recorder,
    crate::provider::PairingAnchor,
) {
    let api = SlackApi {
        bot_token: random(),
        app_token: random(),
        user_id: random(),
        user: random(),
        ..SlackApi::default()
    };
    let origin = serve_slack(api.clone()).await;
    let (dir, names, pairing, anchor) =
        broker_with(&origin, &[&api.bot_token, &api.app_token]).await;
    let host = Recorder {
        root: dir.path().to_owned(),
        pairing,
        pairings: Arc::new(watch::Sender::new(0)),
        threads: Arc::default(),
        turns: Arc::default(),
        statuses: Arc::default(),
        connects: Arc::default(),
        changed: Arc::default(),
        posters: Arc::default(),
        cursors: Arc::default(),
    };
    let config = MessengerConfig {
        kind: MessengerKind::Slack,
        app_credential: Some(names[1].clone()),
        allowed_chats: channels.iter().map(|c| (*c).to_owned()).collect(),
        ..config("slack", &origin, &names[0], 0)
    };
    (api, config, dir, host, anchor)
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_acknowledges_everything_and_answers_allowed_people_once() {
    let (dm, channel, other) = (random(), random(), random());
    let (api, mut entry, _dir, host, _anchor) = slack_setup(&[&dm, &channel]).await;
    let (member, stranger) = (random(), random());
    entry.allowed_users.insert(member.clone());
    let name = entry.name.clone();
    let own = slack_event(&channel, "channel", &api.user_id, &random());
    let mut bot = slack_event(&channel, "channel", &member, &random());
    bot["payload"]["event"]["bot_id"] = json!(random());
    let mut edit = slack_event(&channel, "channel", &member, &random());
    edit["payload"]["event"]["subtype"] = json!("message_changed");
    let question = format!("{} &lt;tag&gt; &amp;", random());
    let group = slack_event(&channel, "channel", &member, &question);
    // Delivered again under a new envelope, as after a missed acknowledgement.
    let mut again = group.clone();
    again["envelope_id"] = json!(random());
    let dm_text = random();
    let envelopes = vec![
        own,
        bot,
        edit,
        json!({"type": "slash_commands", "envelope_id": random(), "payload": {}}),
        slack_event(&other, "channel", &member, &random()),
        slack_event(&channel, "channel", &stranger, &random()),
        group,
        again,
        slack_event(&dm, "im", &stranger, &dm_text),
    ];
    let ids: Vec<String> = envelopes
        .iter()
        .map(|e| e["envelope_id"].as_str().unwrap().to_owned())
        .collect();
    api.envelopes.lock().unwrap().extend(envelopes);
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| {
        api.sent.lock().unwrap().len() == 1
            && h.status(&name).refused.len() == 3
            && api.acks.lock().unwrap().len() == ids.len()
    })
    .await;
    assert_eq!(*api.acks.lock().unwrap(), ids);
    let status = host.status(&name);
    assert_eq!(status.state, MessengerState::Connected);
    assert_eq!(status.account, Some(format!("@{}", api.user)));
    let refused: Vec<_> = status
        .refused
        .iter()
        .map(|r| (r.chat.clone(), r.sender.clone()))
        .collect();
    // A direct message from someone unlisted is refused while senders are listed.
    assert_eq!(
        refused,
        [
            (other, None),
            (channel.clone(), Some(stranger.clone())),
            (dm, Some(stranger)),
        ]
    );
    // Answered once, with Slack's escapes undone for the model.
    let turns: Vec<String> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.1.clone())
        .collect();
    let typed = question
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    assert_eq!(turns, std::slice::from_ref(&typed));
    let sent = api.sent.lock().unwrap().clone();
    assert_eq!(sent[0].0, channel);
    // Markup in the reply is escaped, so it cannot mention or link.
    let escaped = format!("reply to {typed}")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    assert_eq!(sent[0].1["text"], escaped);
    assert_eq!(*api.opens.lock().unwrap(), 1);
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_reconnects_on_request_and_needs_both_tokens() {
    let channel = random();
    let (api, entry, _dir, host, _anchor) = slack_setup(&[&channel]).await;
    let name = entry.name.clone();
    let (desired, receiver) = watch::channel(vec![Desired {
        config: entry.clone(),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|_| *api.opens.lock().unwrap() == 1).await;
    let author = random();
    let (first, second) = (random(), random());
    let answered = slack_event(&channel, "im", &author, &first);
    api.envelopes.lock().unwrap().push_back(answered.clone());
    host.until(|_| api.sent.lock().unwrap().len() == 1).await;
    // Slack asks for a refresh: a new socket, and messages still arrive.
    api.envelopes
        .lock()
        .unwrap()
        .push_back(json!({"type": "disconnect", "reason": "refresh_requested"}));
    host.until(|_| *api.opens.lock().unwrap() == 2).await;
    // The new socket redelivers the answered message, as Slack does when the
    // acknowledgement was lost with the old one: it is not answered again.
    let mut redelivered = answered;
    redelivered["envelope_id"] = json!(random());
    api.envelopes
        .lock()
        .unwrap()
        .extend([redelivered, slack_event(&channel, "im", &author, &second)]);
    host.until(|_| api.sent.lock().unwrap().len() == 2).await;
    let turns: Vec<String> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.1.clone())
        .collect();
    assert_eq!(turns, [first, second]);

    // Without an app-level token it waits for the user instead of retrying.
    desired.send_replace(vec![Desired {
        config: MessengerConfig {
            app_credential: None,
            ..entry.clone()
        },
        run: true,
    }]);
    host.until(|h| h.status(&name).state == MessengerState::Failed)
        .await;
    assert_eq!(
        host.status(&name).failure.unwrap().kind,
        ErrorKind::InvalidRequest
    );
    // The bot token in place of the app token is refused as unauthorized.
    desired.send_replace(vec![Desired {
        config: MessengerConfig {
            app_credential: entry.credential.clone(),
            ..entry
        },
        run: true,
    }]);
    host.until(|h| {
        h.status(&name)
            .failure
            .is_some_and(|f| f.kind == ErrorKind::PermissionDenied)
    })
    .await;
    let opens = *api.opens.lock().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(*api.opens.lock().unwrap(), opens);
    stop.cancel();
    supervisor.await.unwrap();
}

/// A stand-in IRC server on loopback: it signs the bot in with SASL PLAIN
/// when the password matches, then sends `script` once the bot has joined
/// its channels, answers `WHOIS` from `accounts` (nickname to the accounts
/// it belongs to at each `WHOIS` in turn, the last one from then on) and
/// records every line it receives.
#[derive(Clone, Default)]
struct IrcServer {
    password: String,
    channels: usize,
    script: Arc<Mutex<Vec<String>>>,
    accounts: Arc<Mutex<HashMap<String, VecDeque<String>>>>,
    received: Arc<Mutex<Vec<String>>>,
}

async fn serve_irc(server: IrcServer) -> String {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("irc://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let server = server.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let (mut nick, mut joined) = (String::new(), 0);
                while let Ok(Some(line)) = lines.next_line().await {
                    server.received.lock().unwrap().push(line.clone());
                    let mut send = vec![];
                    if let Some(name) = line.strip_prefix("NICK ") {
                        nick = name.to_owned();
                    } else if line == "CAP LS 302" {
                        send.push(":irc.test CAP * LS :sasl account-tag message-tags".to_owned());
                    } else if let Some(caps) = line.strip_prefix("CAP REQ :") {
                        send.push(format!(":irc.test CAP * ACK :{caps}"));
                    } else if line == "AUTHENTICATE PLAIN" {
                        send.push("AUTHENTICATE +".to_owned());
                    } else if let Some(payload) = line.strip_prefix("AUTHENTICATE ") {
                        use base64::Engine;
                        let plain = base64::engine::general_purpose::STANDARD
                            .decode(payload)
                            .unwrap();
                        let ok = plain == format!("\0{nick}\0{}", server.password).as_bytes();
                        send.push(format!(
                            ":irc.test {} {nick} :SASL",
                            if ok { 903 } else { 904 }
                        ));
                    } else if line == "CAP END" {
                        send.push(format!(":irc.test 001 {nick} :Welcome"));
                    } else if line.starts_with("JOIN ") {
                        joined += 1;
                    } else if let Some(whois) = line.strip_prefix("WHOIS ") {
                        let mut accounts = server.accounts.lock().unwrap();
                        let account = accounts.get_mut(whois).and_then(|owners| {
                            if owners.len() > 1 {
                                owners.pop_front()
                            } else {
                                owners.front().cloned()
                            }
                        });
                        drop(accounts);
                        if let Some(account) = account {
                            send.push(format!(
                                ":irc.test 330 {nick} {whois} {account} :is logged in as"
                            ));
                        }
                        send.push(format!(":irc.test 318 {nick} {whois} :End of WHOIS"));
                    }
                    if joined == server.channels && line.starts_with("JOIN ") {
                        send.append(&mut server.script.lock().unwrap());
                    }
                    for line in send {
                        let line = line.replace("{nick}", &nick);
                        if writer
                            .write_all(format!("{line}\r\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });
        }
    });
    origin
}

#[tokio::test(flavor = "multi_thread")]
async fn irc_answers_signed_in_people_and_checks_nicknames_before_direct_replies() {
    let nickname = || format!("n{}", &random().replace('-', "")[..10]);
    let (login, member, friend, moved) = (nickname(), nickname(), nickname(), nickname());
    let (friend_nick, moved_nick, impostor) = (nickname(), nickname(), nickname());
    let (handover, handover_nick) = (nickname(), nickname());
    let channel = format!("#{}", nickname());
    let server = IrcServer {
        password: random(),
        channels: 1,
        ..IrcServer::default()
    };
    let (q1, q2, q3) = (random(), random(), random());
    // Long enough for a reply of two lines.
    let q4 = (0..16).map(|_| random()).collect::<Vec<_>>().join(" ");
    let token = nickname();
    server.script.lock().unwrap().extend([
        // Not signed in to services: no sender, so a group refuses it.
        format!(":{}!u@h PRIVMSG {channel} :{}", nickname(), random()),
        format!(
            "@account={} :m!u@h PRIVMSG {} :\u{1}VERSION\u{1}",
            member, channel
        ),
        format!(
            "@account={} :m!u@h PRIVMSG {} :{q1}",
            member.to_uppercase(),
            channel.to_uppercase()
        ),
        format!(
            "@account={} :o!u@h PRIVMSG {channel} :{}",
            nickname(),
            random()
        ),
        format!("PING :{token}"),
        format!("@account={friend} :{friend_nick}!u@h PRIVMSG {{nick}} :{q2}"),
        format!("@account={moved} :{moved_nick}!u@h PRIVMSG {{nick}} :{q3}"),
        format!("@account={handover} :{handover_nick}!u@h PRIVMSG {{nick}} :{q4}"),
    ]);
    {
        let mut accounts = server.accounts.lock().unwrap();
        accounts.insert(friend_nick.clone(), VecDeque::from([friend.clone()]));
        // Someone else has the nickname by the time the reply is ready.
        accounts.insert(moved_nick.clone(), VecDeque::from([impostor.clone()]));
        // Or takes it between the reply's first and second line.
        accounts.insert(
            handover_nick.clone(),
            VecDeque::from([handover.clone(), impostor]),
        );
    }
    let origin = serve_irc(server.clone()).await;
    let (dir, credential, pairing, _anchor) = broker(&origin, &server.password).await;
    let host = Recorder {
        root: dir.path().to_owned(),
        pairing,
        pairings: Arc::new(watch::Sender::new(0)),
        threads: Arc::default(),
        turns: Arc::default(),
        statuses: Arc::default(),
        connects: Arc::default(),
        changed: Arc::default(),
        posters: Arc::default(),
        cursors: Arc::default(),
    };
    let entry = MessengerConfig {
        kind: MessengerKind::Irc,
        login: Some(login.clone()),
        allowed_chats: BTreeSet::from([
            channel.clone(),
            friend.clone(),
            moved.clone(),
            handover.clone(),
        ]),
        allowed_users: BTreeSet::from([
            member.clone(),
            friend.clone(),
            moved.clone(),
            handover.clone(),
        ]),
        ..config("irc", &origin, &credential, 0)
    };
    let name = entry.name.clone();
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    let has = |line: &str| server.received.lock().unwrap().iter().any(|l| l == line);
    let sent_to = |nick: &str| {
        server
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(&format!("PRIVMSG {nick} ")))
            .count()
    };
    host.until(|h| {
        has(&format!("PRIVMSG {friend_nick} :reply to {q2}"))
            && h.turns.lock().unwrap().len() == 4
            && h.status(&name)
                .failure
                .is_some_and(|f| f.message.contains(&handover))
    })
    .await;
    let status = host.status(&name);
    assert_eq!(status.state, MessengerState::Connected);
    assert_eq!(status.account.as_deref(), Some(login.as_str()));
    let turns: Vec<String> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|(_, t)| t.clone())
        .collect();
    assert_eq!(turns, [q1.clone(), q2.clone(), q3.clone(), q4.clone()]);
    assert!(has(&format!("JOIN {channel}")));
    assert!(has(&format!("PONG :{token}")));
    assert!(has(&format!("PRIVMSG {channel} :reply to {q1}")));
    assert!(has(&format!("WHOIS {moved_nick}")));
    // The nickname now belongs to someone else: nothing is sent to it.
    assert_eq!(sent_to(&moved_nick), 0);
    // A nickname taken over mid-reply gets only the line checked before.
    let whois = format!("WHOIS {handover_nick}");
    let asked = server
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|l| **l == whois)
        .count();
    assert_eq!((asked, sent_to(&handover_nick)), (2, 1));
    let refused: Vec<_> = status
        .refused
        .iter()
        .map(|r| (r.chat.clone(), r.sender.clone()))
        .collect();
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert!(refused.iter().all(|(chat, _)| *chat == channel));
    assert!(refused.contains(&(channel.clone(), None)));
    // The password went only to the broker's sign-in.
    assert!(
        !server
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains(&server.password))
    );
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn irc_without_a_login_waits_for_the_user() {
    let server = IrcServer {
        password: random(),
        ..IrcServer::default()
    };
    let origin = serve_irc(server.clone()).await;
    let (dir, credential, pairing, _anchor) = broker(&origin, &server.password).await;
    let host = Recorder {
        root: dir.path().to_owned(),
        pairing,
        pairings: Arc::new(watch::Sender::new(0)),
        threads: Arc::default(),
        turns: Arc::default(),
        statuses: Arc::default(),
        connects: Arc::default(),
        changed: Arc::default(),
        posters: Arc::default(),
        cursors: Arc::default(),
    };
    let entry = MessengerConfig {
        kind: MessengerKind::Irc,
        ..config("irc", &origin, &credential, 0)
    };
    let name = entry.name.clone();
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| h.status(&name).state == MessengerState::Failed)
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(host.connects(&name), 1);
    assert!(server.received.lock().unwrap().is_empty());
    stop.cancel();
    supervisor.await.unwrap();
}

/// A stand-in signal-cli daemon on a Unix socket: it serves `accounts`,
/// sends `early` before answering `subscribeReceive` and `script` after,
/// and records every request it receives. With `hang_up_after`, the first
/// connection closes after answering that many `send`s (or, with
/// `unanswered`, on receiving the last of them, after sending `on_hang_up`),
/// and the next subscription
/// sends `replay`.
#[derive(Clone, Default)]
struct SignalDaemon {
    accounts: Vec<String>,
    early: Arc<Mutex<Vec<Value>>>,
    script: Arc<Mutex<Vec<Value>>>,
    replay: Arc<Mutex<Vec<Value>>>,
    hang_up_after: usize,
    unanswered: bool,
    on_hang_up: Arc<Mutex<Vec<Value>>>,
    sent: Arc<std::sync::atomic::AtomicUsize>,
    hung_up: Arc<std::sync::atomic::AtomicBool>,
    received: Arc<Mutex<Vec<Value>>>,
}

async fn serve_signal(dir: &std::path::Path, daemon: SignalDaemon) -> String {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let path = dir.join(format!("{}.sock", &random()[..8]));
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let daemon = daemon.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    daemon.received.lock().unwrap().push(request.clone());
                    let id = request["id"].clone();
                    let mut send = vec![];
                    match request["method"].as_str() {
                        Some("listAccounts") => {
                            let accounts: Vec<Value> = daemon
                                .accounts
                                .iter()
                                .map(|number| json!({ "number": number }))
                                .collect();
                            send.push(json!({ "jsonrpc": "2.0", "id": id, "result": accounts }));
                        }
                        Some("subscribeReceive") => {
                            send.append(&mut daemon.early.lock().unwrap());
                            send.push(json!({ "jsonrpc": "2.0", "id": id, "result": 0 }));
                            send.append(&mut daemon.script.lock().unwrap());
                            if daemon.hung_up.load(Ordering::SeqCst) {
                                send.append(&mut daemon.replay.lock().unwrap());
                            }
                        }
                        Some("send")
                            if daemon.unanswered
                                && !daemon.hung_up.load(Ordering::SeqCst)
                                && daemon.sent.load(Ordering::SeqCst) + 1
                                    == daemon.hang_up_after =>
                        {
                            daemon.sent.fetch_add(1, Ordering::SeqCst);
                            let last = std::mem::take(&mut *daemon.on_hang_up.lock().unwrap());
                            for message in last {
                                let mut line = serde_json::to_vec(&message).unwrap();
                                line.push(b'\n');
                                if writer.write_all(&line).await.is_err() {
                                    break;
                                }
                            }
                            daemon.hung_up.store(true, Ordering::SeqCst);
                            return;
                        }
                        Some("send") => {
                            send.push(
                                json!({ "jsonrpc": "2.0", "id": id, "result": { "timestamp": 1 } }),
                            );
                        }
                        _ => send.push(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": "Method not implemented" }
                        })),
                    }
                    for message in send {
                        let mut line = serde_json::to_vec(&message).unwrap();
                        line.push(b'\n');
                        if writer.write_all(&line).await.is_err() {
                            return;
                        }
                    }
                    if request["method"] == "send"
                        && daemon.sent.fetch_add(1, Ordering::SeqCst) + 1 == daemon.hang_up_after
                        && !daemon.hung_up.swap(true, Ordering::SeqCst)
                    {
                        return;
                    }
                }
            });
        }
    });
    format!("unix://{}", path.display())
}

fn signal_message(account: &str, from: &str, timestamp: u64, data: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "receive",
        "params": {
            "subscription": 0,
            "result": {
                "account": account,
                "envelope": {
                    "sourceUuid": from,
                    "sourceName": random(),
                    "timestamp": timestamp,
                    "dataMessage": data,
                }
            }
        }
    })
}

fn signal_host(dir: &tempfile::TempDir, pairing: BrokerPairing) -> Recorder {
    Recorder {
        root: dir.path().to_owned(),
        pairing,
        pairings: Arc::new(watch::Sender::new(0)),
        threads: Arc::default(),
        turns: Arc::default(),
        statuses: Arc::default(),
        connects: Arc::default(),
        changed: Arc::default(),
        posters: Arc::default(),
        cursors: Arc::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn signal_answers_listed_people_through_the_users_own_daemon() {
    use base64::Engine;
    let aci = || Id::new_v4().hyphenated().to_string();
    let number = format!("+1555{}", 1_000_000 + std::process::id() % 1_000_000);
    let (friend, member, stranger) = (aci(), aci(), aci());
    let group = base64::engine::general_purpose::STANDARD.encode(Id::new_v4().as_bytes().repeat(2));
    let (q1, q2) = (random(), random());
    // No vault credential: the broker is there only because messengers run
    // while the gateway is paired.
    let (dir, _credential, pairing, _anchor) = broker("https://unused.invalid", &random()).await;
    let daemon = SignalDaemon {
        accounts: vec![number.clone()],
        ..SignalDaemon::default()
    };
    daemon.script.lock().unwrap().extend([
        signal_message(&number, &friend, 1, json!({ "message": q1 })),
        // Delivered twice: answered once.
        signal_message(&number, &friend, 1, json!({ "message": q1 })),
        signal_message(
            &number,
            &stranger,
            2,
            json!({ "message": random(), "groupInfo": { "groupId": group } }),
        ),
        signal_message(
            &number,
            &member,
            3,
            json!({ "message": q2, "groupInfo": { "groupId": group } }),
        ),
        // Another account the daemon serves.
        signal_message("+15550000000", &friend, 4, json!({ "message": random() })),
    ]);
    let endpoint = serve_signal(dir.path(), daemon.clone()).await;
    let host = signal_host(&dir, pairing);
    let entry = MessengerConfig {
        enabled: true,
        endpoint: Some(endpoint),
        login: Some(number.clone()),
        allowed_chats: BTreeSet::from([friend.clone(), group.clone()]),
        allowed_users: BTreeSet::from([friend.clone(), member.clone()]),
        model: Some(ModelKey {
            provider: Id::new_v4(),
            model: random(),
        }),
        ..MessengerConfig::new("signal".to_owned(), MessengerKind::Signal)
    };
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    let sends = || -> Vec<Value> {
        daemon
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "send")
            .map(|r| r["params"].clone())
            .collect()
    };
    host.until(|h| sends().len() == 2 && !h.status("signal").refused.is_empty())
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let status = host.status("signal");
    assert_eq!(status.state, MessengerState::Connected);
    assert_eq!(status.account.as_deref(), Some(number.as_str()));
    let turns: Vec<String> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|(_, t)| t.clone())
        .collect();
    assert_eq!(turns, [q1.clone(), q2.clone()]);
    assert_eq!(
        sends(),
        [
            json!({ "account": number, "recipient": [friend], "message": format!("reply to {q1}") }),
            json!({ "account": number, "groupId": group, "message": format!("reply to {q2}") }),
        ]
    );
    let refused: Vec<_> = status
        .refused
        .iter()
        .map(|r| (r.chat.clone(), r.sender.clone()))
        .collect();
    assert_eq!(refused, [(group.clone(), Some(stranger))]);
    // Subscribed for this account only.
    let received = daemon.received.lock().unwrap().clone();
    assert!(
        received
            .iter()
            .any(|r| r["method"] == "subscribeReceive" && r["params"]["account"] == number)
    );
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn signal_keeps_early_messages_and_answers_none_twice_across_a_reconnect() {
    let number = format!("+1555{}", 1_000_000 + std::process::id() % 1_000_000);
    let friend = Id::new_v4().hyphenated().to_string();
    let (q1, q2, q3) = (random(), random(), random());
    let (dir, _credential, pairing, _anchor) = broker("https://unused.invalid", &random()).await;
    let daemon = SignalDaemon {
        accounts: vec![number.clone()],
        hang_up_after: 2,
        ..SignalDaemon::default()
    };
    let message = |timestamp, text: &String| {
        signal_message(&number, &friend, timestamp, json!({ "message": text }))
    };
    // Sent before the subscription is confirmed.
    daemon.early.lock().unwrap().push(message(1, &q1));
    daemon.script.lock().unwrap().push(message(2, &q2));
    // After the reconnect: an answered message again, then a new one.
    daemon
        .replay
        .lock()
        .unwrap()
        .extend([message(2, &q2), message(3, &q3)]);
    let endpoint = serve_signal(dir.path(), daemon.clone()).await;
    let host = signal_host(&dir, pairing);
    let entry = MessengerConfig {
        enabled: true,
        endpoint: Some(endpoint),
        login: Some(number.clone()),
        allowed_chats: BTreeSet::from([friend.clone()]),
        allowed_users: BTreeSet::from([friend.clone()]),
        model: Some(ModelKey {
            provider: Id::new_v4(),
            model: random(),
        }),
        ..MessengerConfig::new("signal".to_owned(), MessengerKind::Signal)
    };
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    let sends = || -> Vec<Value> {
        daemon
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "send")
            .map(|r| r["params"]["message"].clone())
            .collect()
    };
    host.until(|_| sends().len() == 3).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(daemon.hung_up.load(Ordering::SeqCst));
    let turns: Vec<String> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|(_, t)| t.clone())
        .collect();
    assert_eq!(turns, [q1.clone(), q2.clone(), q3.clone()]);
    assert_eq!(
        sends(),
        [q1, q2, q3].map(|q| json!(format!("reply to {q}")))
    );
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn signal_answers_what_a_failed_session_left_unhandled() {
    let number = format!("+1555{}", 1_000_000 + std::process::id() % 1_000_000);
    let friend = Id::new_v4().hyphenated().to_string();
    let (q1, q2) = (random(), random());
    let (dir, _credential, pairing, _anchor) = broker("https://unused.invalid", &random()).await;
    // q2 arrives while q1's reply waits, which never gets an answer: the
    // connection closes with q2 queued on it. signal-cli does not send q2
    // again.
    let daemon = SignalDaemon {
        accounts: vec![number.clone()],
        hang_up_after: 1,
        unanswered: true,
        ..SignalDaemon::default()
    };
    let message = |timestamp, text: &String| {
        signal_message(&number, &friend, timestamp, json!({ "message": text }))
    };
    daemon.script.lock().unwrap().push(message(1, &q1));
    daemon.on_hang_up.lock().unwrap().push(message(2, &q2));
    daemon.replay.lock().unwrap().push(message(1, &q1));
    let endpoint = serve_signal(dir.path(), daemon.clone()).await;
    let host = signal_host(&dir, pairing);
    let entry = MessengerConfig {
        enabled: true,
        endpoint: Some(endpoint),
        login: Some(number.clone()),
        allowed_chats: BTreeSet::from([friend.clone()]),
        allowed_users: BTreeSet::from([friend.clone()]),
        model: Some(ModelKey {
            provider: Id::new_v4(),
            model: random(),
        }),
        ..MessengerConfig::new("signal".to_owned(), MessengerKind::Signal)
    };
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    let sends = || -> Vec<Value> {
        daemon
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "send")
            .map(|r| r["params"]["message"].clone())
            .collect()
    };
    host.until(|_| sends().len() == 3).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // q2, queued by the session that failed, is answered through the new
    // one; q1, whose reply never went out, is answered when signal-cli
    // delivers it again.
    assert_eq!(
        sends(),
        [&q1, &q2, &q1].map(|q| json!(format!("reply to {q}")))
    );
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn signal_waits_for_the_user_when_the_daemon_does_not_serve_the_account() {
    let (dir, _credential, pairing, _anchor) = broker("https://unused.invalid", &random()).await;
    let daemon = SignalDaemon {
        accounts: vec!["+15550000000".to_owned()],
        ..SignalDaemon::default()
    };
    let endpoint = serve_signal(dir.path(), daemon.clone()).await;
    let host = signal_host(&dir, pairing);
    let number = format!("+1555{}", 2_000_000 + std::process::id() % 1_000_000);
    let entry = MessengerConfig {
        enabled: true,
        endpoint: Some(endpoint),
        login: Some(number.clone()),
        ..MessengerConfig::new("signal".to_owned(), MessengerKind::Signal)
    };
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| h.status("signal").state == MessengerState::Failed)
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(host.connects("signal"), 1);
    let failure = host.status("signal").failure.unwrap();
    assert!(failure.message.contains(&number), "{failure:?}");
    // Never subscribed to another account's messages.
    assert!(
        !daemon
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["method"] == "subscribeReceive")
    );
    stop.cancel();
    supervisor.await.unwrap();
}

/// A stand-in Beeper Desktop API. Every message and chat activity takes the
/// next number of one counter; cursors name such a number.
#[derive(Clone, Default)]
struct BeeperDesktop {
    token: String,
    accounts: Arc<Mutex<Value>>,
    state: Arc<Mutex<BeeperChats>>,
    /// Statuses the next sends answer with; then 200.
    send_statuses: Arc<Mutex<VecDeque<u16>>>,
    /// Chat ids and texts of accepted sends.
    sent: Arc<Mutex<Vec<(String, String)>>>,
    chat_lists: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Default)]
struct BeeperChats {
    counter: u64,
    chats: BTreeMap<String, BeeperChat>,
}

struct BeeperChat {
    info: Value,
    activity: u64,
    messages: Vec<(u64, Value)>,
}

impl BeeperChats {
    fn chat(&mut self, id: &str, kind: &str, network: &str, title: &str) {
        self.counter += 1;
        let activity = self.counter;
        self.chats.insert(
            id.to_owned(),
            BeeperChat {
                info: json!({"id": id, "accountID": random(), "network": network, "title": title, "type": kind}),
                activity,
                messages: vec![],
            },
        );
    }
    /// Add `message` (with a fresh id) to `chat`.
    fn post(&mut self, chat: &str, mut message: Value) {
        self.counter += 1;
        let at = self.counter;
        message["id"] = json!(random());
        message["chatID"] = json!(chat);
        let chat = self.chats.get_mut(chat).unwrap();
        chat.activity = at;
        chat.messages.push((at, message));
    }
}

impl BeeperDesktop {
    fn chat(&self, id: &str, kind: &str, network: &str, title: &str) {
        self.state.lock().unwrap().chat(id, kind, network, title);
    }
    fn post(&self, chat: &str, message: Value) {
        self.state.lock().unwrap().post(chat, message);
    }
    fn lists(&self) -> usize {
        self.chat_lists.load(Ordering::SeqCst)
    }
}

fn beeper_text(sender: &str, text: &str) -> Value {
    json!({"senderID": sender, "senderName": random(), "text": text, "type": "TEXT"})
}

fn cursor_after(query: &HashMap<String, String>, prefix: char) -> Option<u64> {
    (query.get("direction").map(String::as_str) == Some("after"))
        .then(|| query.get("cursor")?.strip_prefix(prefix)?.parse().ok())
        .flatten()
}

async fn serve_beeper(desktop: BeeperDesktop) -> String {
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    fn authorized(desktop: &BeeperDesktop, headers: &HeaderMap) -> bool {
        bearer(headers).as_deref() == Some(desktop.token.as_str())
    }
    let accounts = |State(desktop): State<BeeperDesktop>, headers: HeaderMap| async move {
        if !authorized(&desktop, &headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Json(desktop.accounts.lock().unwrap().clone()).into_response()
    };
    let chats = |State(desktop): State<BeeperDesktop>,
                 headers: HeaderMap,
                 Query(query): Query<HashMap<String, String>>| async move {
        assert!(authorized(&desktop, &headers), "no token");
        desktop.chat_lists.fetch_add(1, Ordering::SeqCst);
        let state = desktop.state.lock().unwrap();
        let limit: usize = query.get("limit").unwrap().parse().unwrap();
        let after = cursor_after(&query, 'c').unwrap_or(0);
        let mut chats: Vec<_> = state
            .chats
            .values()
            .filter(|c| c.activity > after)
            .collect();
        // The most recent changes first, as Beeper lists chats; a full page
        // leaves the oldest changes out.
        chats.sort_by_key(|c| std::cmp::Reverse(c.activity));
        let more = chats.len() > limit;
        chats.truncate(limit);
        let newest = chats.first().map_or(after, |c| c.activity);
        let items: Vec<_> = chats
            .iter()
            .map(|c| {
                let mut info = c.info.clone();
                if let Some((_, last)) = c.messages.last() {
                    info["preview"] = last.clone();
                }
                info
            })
            .collect();
        Json(json!({"items": items, "hasMore": more, "newestCursor": format!("c{newest}")}))
    };
    let chat = |State(desktop): State<BeeperDesktop>, Path(id): Path<String>| async move {
        let state = desktop.state.lock().unwrap();
        match state.chats.get(&id) {
            Some(chat) => Json(chat.info.clone()).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    };
    let messages = |State(desktop): State<BeeperDesktop>,
                    headers: HeaderMap,
                    Path(id): Path<String>,
                    Query(query): Query<HashMap<String, String>>| async move {
        assert!(authorized(&desktop, &headers), "no token");
        let state = desktop.state.lock().unwrap();
        let Some(chat) = state.chats.get(&id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let before = (query.get("direction").map(String::as_str) == Some("before"))
            .then(|| query.get("cursor")?.strip_prefix('m')?.parse::<u64>().ok())
            .flatten();
        let (from, items, more): (u64, Vec<_>, bool) = match cursor_after(&query, 'm') {
            Some(after) => (
                after,
                chat.messages.iter().filter(|(at, _)| *at > after).collect(),
                false,
            ),
            // The latest page, or the page before `cursor`: twenty messages,
            // and whether older ones remain.
            None => {
                let earlier: Vec<_> = chat
                    .messages
                    .iter()
                    .filter(|(at, _)| before.is_none_or(|before| *at < before))
                    .collect();
                let skip = earlier.len().saturating_sub(20);
                (0, earlier[skip..].to_vec(), skip > 0)
            }
        };
        let newest = items.last().map_or(from, |(at, _)| *at);
        let oldest = items.first().map_or(from, |(at, _)| *at);
        let items: Vec<_> = items.into_iter().map(|(_, m)| m.clone()).collect();
        Json(json!({
            "items": items,
            "hasMore": more,
            "newestCursor": format!("m{newest}"),
            "oldestCursor": format!("m{oldest}"),
        }))
        .into_response()
    };
    let send = |State(desktop): State<BeeperDesktop>,
                headers: HeaderMap,
                Path(id): Path<String>,
                Json(body): Json<Value>| async move {
        assert!(authorized(&desktop, &headers), "no token");
        let status = desktop.send_statuses.lock().unwrap().pop_front();
        if let Some(status) = status {
            return Response::builder()
                .status(status)
                .body(axum::body::Body::empty())
                .unwrap();
        }
        let text = body["text"].as_str().unwrap().to_owned();
        desktop
            .sent
            .lock()
            .unwrap()
            .push((id.clone(), text.clone()));
        // The reply shows up in the chat as the user's own message.
        desktop.post(
            &id,
            json!({"senderID": random(), "text": text, "type": "TEXT", "isSender": true}),
        );
        Json(json!({"chatID": id, "pendingMessageID": random()})).into_response()
    };
    let router = Router::new()
        .route("/v1/accounts", get(accounts))
        .route("/v1/chats", get(chats))
        .route("/v1/chats/{id}", get(chat))
        .route("/v1/chats/{id}/messages", get(messages).post(send))
        .with_state(desktop);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    origin
}

async fn beeper_setup() -> (
    BeeperDesktop,
    String,
    tempfile::TempDir,
    String,
    Recorder,
    crate::provider::PairingAnchor,
) {
    let network = random();
    let desktop = BeeperDesktop {
        token: random(),
        accounts: Arc::new(Mutex::new(
            json!([{"accountID": random(), "network": network, "status": "connected"}]),
        )),
        ..BeeperDesktop::default()
    };
    let origin = serve_beeper(desktop.clone()).await;
    let (dir, credential, pairing, anchor) = broker(&origin, &desktop.token).await;
    let host = Recorder {
        root: dir.path().to_owned(),
        pairing,
        pairings: Arc::new(watch::Sender::new(0)),
        threads: Arc::default(),
        turns: Arc::default(),
        statuses: Arc::default(),
        connects: Arc::default(),
        changed: Arc::default(),
        posters: Arc::default(),
        cursors: Arc::default(),
    };
    (desktop, origin, dir, credential, host, anchor)
}

fn beeper_config(name: &str, origin: &str, credential: &str, chats: &[&str]) -> MessengerConfig {
    MessengerConfig {
        kind: MessengerKind::Beeper,
        allowed_chats: chats.iter().map(|chat| (*chat).to_owned()).collect(),
        ..config(name, origin, credential, 0)
    }
}

/// Run `entry` until `done`, then stop it.
async fn beeper_run(host: &Recorder, entry: MessengerConfig, done: impl Fn(&Recorder) -> bool) {
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(done).await;
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn beeper_answers_allowed_chats_as_the_user_and_never_history() {
    let (desktop, origin, _dir, credential, host, _anchor) = beeper_setup().await;
    let name = "beeper";
    let (direct, group, other) = (random(), random(), random());
    let (friend, member, stranger) = (random(), random(), random());
    let (group_title, other_title) = (random(), random());
    desktop.chat(&direct, "single", "WhatsApp", &random());
    desktop.chat(&group, "group", "iMessage", &group_title);
    desktop.chat(&other, "single", "Signal", &other_title);
    desktop.post(&direct, beeper_text(&friend, &random()));
    let mut entry = beeper_config(name, &origin, &credential, &[&direct, &group]);
    entry.allowed_users.insert(member.clone());
    // Direct chats answer anyone in them only while no senders are listed.
    entry.allowed_users.insert(friend.clone());

    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    // The session marked where each chat ends and looked for changes once.
    host.until(|h| h.status(name).state == MessengerState::Connected && desktop.lists() >= 2)
        .await;
    let (hello, question) = (random(), random());
    desktop.post(&direct, beeper_text(&friend, &hello));
    desktop.post(&group, beeper_text(&stranger, &random()));
    desktop.post(
        &group,
        json!({"senderID": member, "text": random(), "type": "NOTICE"}),
    );
    desktop.post(&group, beeper_text(&member, &question));
    desktop.post(&other, beeper_text(&stranger, &random()));
    host.until(|_| desktop.sent.lock().unwrap().len() == 2)
        .await;
    host.until(|h| h.status(name).refused.len() == 2).await;
    // The replies came back as the user's own messages, which are ignored.
    let seen = desktop.lists();
    host.until(|_| desktop.lists() >= seen + 3).await;
    stop.cancel();
    supervisor.await.unwrap();

    let status = host.status(name);
    let network = desktop.accounts.lock().unwrap()[0]["network"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(status.account, Some(format!("Beeper: {network}")));
    let turns: BTreeSet<_> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.1.clone())
        .collect();
    assert_eq!(turns, BTreeSet::from([hello.clone(), question.clone()]));
    let sent: BTreeSet<_> = desktop.sent.lock().unwrap().iter().cloned().collect();
    assert_eq!(
        sent,
        BTreeSet::from([
            (direct.clone(), format!("reply to {hello}")),
            (group.clone(), format!("reply to {question}")),
        ])
    );
    let refused: BTreeSet<_> = status
        .refused
        .iter()
        .map(|r| (r.chat.clone(), r.sender.clone(), r.title.clone()))
        .collect();
    assert_eq!(
        refused,
        BTreeSet::from([
            (other, None, Some(format!("Signal: {other_title}"))),
            (
                group,
                Some(stranger),
                Some(format!("iMessage: {group_title}"))
            ),
        ])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn beeper_answers_each_message_at_most_once_when_a_reply_fails() {
    let (desktop, origin, _dir, credential, host, _anchor) = beeper_setup().await;
    let name = "beeper";
    let (chat, friend) = (random(), random());
    desktop.chat(&chat, "single", "WhatsApp", &random());
    desktop.send_statuses.lock().unwrap().push_back(500);
    let entry = beeper_config(name, &origin, &credential, &[&chat]);
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| h.status(name).state == MessengerState::Connected && desktop.lists() >= 2)
        .await;
    let (first, second) = (random(), random());
    desktop.post(&chat, beeper_text(&friend, &first));
    desktop.post(&chat, beeper_text(&friend, &second));
    // The first reply fails, which ends the session; the second message is
    // still answered after reconnecting, and the first is not repeated.
    host.until(|_| !desktop.sent.lock().unwrap().is_empty())
        .await;
    let seen = desktop.lists();
    host.until(|_| desktop.lists() >= seen + 3).await;
    stop.cancel();
    supervisor.await.unwrap();

    assert!(
        host.connects(name) >= 2,
        "the failed reply ended the session"
    );
    let turns: Vec<_> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.1.clone())
        .collect();
    assert_eq!(turns, vec![first, second.clone()]);
    assert_eq!(
        *desktop.sent.lock().unwrap(),
        vec![(chat, format!("reply to {second}"))]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_beeper_token_waits_for_the_user() {
    let (_desktop, origin, _dir, _credential, mut host, _anchor) = beeper_setup().await;
    // A credential holding another token, in a broker paired with the host.
    let (dir, wrong, pairing, _anchor) = broker(&origin, &random()).await;
    host.root = dir.path().to_owned();
    host.pairing = pairing;
    let name = "beeper";
    beeper_run(&host, beeper_config(name, &origin, &wrong, &[]), |h| {
        h.status(name).state == MessengerState::Failed
    })
    .await;
    let status = host.status(name);
    assert_eq!(
        status.failure.map(|f| f.kind),
        Some(ErrorKind::PermissionDenied)
    );
    assert_eq!(host.connects(name), 1, "not retried");
}

#[tokio::test(flavor = "multi_thread")]
async fn beeper_finds_allowed_chats_behind_busy_lists_and_new_chats_and_rechecks_accounts() {
    let (desktop, origin, _dir, credential, host, _anchor) = beeper_setup().await;
    let name = "beeper";
    let (busy, later, friend) = (random(), random(), random());
    desktop.chat(&busy, "single", "WhatsApp", &random());
    // `later` is allowed before Beeper has such a chat.
    let entry = beeper_config(name, &origin, &credential, &[&busy, &later]);
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| h.status(name).state == MessengerState::Connected && desktop.lists() >= 2)
        .await;
    // More of `later`'s messages than one page holds.
    let behind = random();
    let first: Vec<String> = (0..45).map(|_| random()).collect();
    {
        // At once, so one look sees it all: the allowed chat changes first,
        // then more chats than one page holds.
        let mut state = desktop.state.lock().unwrap();
        state.post(&busy, beeper_text(&friend, &behind));
        for _ in 0..100 {
            let other = random();
            state.chat(&other, "single", "Signal", &random());
            state.post(&other, beeper_text(&random(), &random()));
        }
        // A chat created after the session started, and its first messages.
        state.chat(&later, "single", "iMessage", &random());
        for text in &first {
            state.post(&later, beeper_text(&friend, text));
        }
    }
    host.until(|_| desktop.sent.lock().unwrap().len() == 1 + first.len())
        .await;
    // Once nothing is left to answer, another token reaching other accounts
    // is still noticed.
    let seen = desktop.lists();
    host.until(|_| desktop.lists() >= seen + 3).await;
    let network = random();
    *desktop.accounts.lock().unwrap() = json!([{"accountID": random(), "network": network}]);
    host.until(|h| h.status(name).account == Some(format!("Beeper: {network}")))
        .await;
    stop.cancel();
    supervisor.await.unwrap();

    let sent = desktop.sent.lock().unwrap().clone();
    let to = |chat: &str| -> Vec<String> {
        sent.iter()
            .filter(|(to, _)| to == chat)
            .map(|(_, text)| text.clone())
            .collect()
    };
    assert_eq!(to(&busy), [format!("reply to {behind}")]);
    // Every message the new chat held is answered once, in order.
    let replies: Vec<_> = first.iter().map(|t| format!("reply to {t}")).collect();
    assert_eq!(to(&later), replies);
}

impl Recorder {
    fn poster(&self, name: &str) -> Option<Poster> {
        self.posters.lock().unwrap().get(name).cloned()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn irc_posts_go_out_on_the_running_session_line_by_line() {
    let nickname = || format!("n{}", &random().replace('-', "")[..10]);
    let (login, friend, friend_nick, absent) = (nickname(), nickname(), nickname(), nickname());
    let channel = format!("#{}", nickname());
    let server = IrcServer {
        password: random(),
        channels: 1,
        ..IrcServer::default()
    };
    let question = random();
    server.script.lock().unwrap().push(format!(
        "@account={friend} :{friend_nick}!u@h PRIVMSG {{nick}} :{question}"
    ));
    server
        .accounts
        .lock()
        .unwrap()
        .insert(friend_nick.clone(), VecDeque::from([friend.clone()]));
    let origin = serve_irc(server.clone()).await;
    let (dir, credential, pairing, _anchor) = broker(&origin, &server.password).await;
    let host = signal_host(&dir, pairing);
    let entry = MessengerConfig {
        kind: MessengerKind::Irc,
        login: Some(login.clone()),
        allowed_chats: BTreeSet::from([channel.clone(), friend.clone(), absent.clone()]),
        allowed_users: BTreeSet::from([friend.clone(), absent.clone()]),
        ..config("irc", &origin, &credential, 0)
    };
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry.clone(),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    let privmsgs = |target: &str| -> Vec<String> {
        let prefix = format!("PRIVMSG {target} :");
        server
            .received
            .lock()
            .unwrap()
            .iter()
            .filter_map(|l| l.strip_prefix(&prefix).map(str::to_owned))
            .collect()
    };
    // The friend's message tells the session which nickname they use.
    host.until(|h| {
        h.poster("irc").is_some() && privmsgs(&friend_nick) == [format!("reply to {question}")]
    })
    .await;
    let poster = host.poster("irc").unwrap();
    assert_eq!(poster.config(), &entry);
    let (first, second) = (random(), random());
    let text = format!("{first}\n\n\u{7}{second}\n");
    assert_eq!(
        post_through(&poster, &channel, &text, || false)
            .await
            .unwrap(),
        2
    );
    // Sent lines reach the server through the broker shortly after.
    host.until(|_| privmsgs(&channel) == [first.clone(), second.clone()])
        .await;
    // A direct chat's lines each follow a check of the nickname.
    assert_eq!(
        post_through(&poster, &friend, &first, || false)
            .await
            .unwrap(),
        1
    );
    host.until(|_| privmsgs(&friend_nick)[1..] == [first.clone()])
        .await;
    let whois = format!("WHOIS {friend_nick}");
    assert!(server.received.lock().unwrap().contains(&whois));
    // An account that has not written during this session cannot be found.
    assert!(matches!(
        post_through(&poster, &absent, &first, || false).await,
        Err(MessengerError::RecipientGone { .. })
    ));
    // Too long to send whole: nothing goes out.
    let long = (0..=irc::MAX_REPLY_LINES)
        .map(|_| random())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(matches!(
        post_through(&poster, &channel, &long, || false).await,
        Err(MessengerError::TooManyLines { .. })
    ));
    assert_eq!(privmsgs(&channel).len(), 2);
    // Withdrawn after the first line: the rest stays unsent.
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let withdrawn = || asked.fetch_add(1, Ordering::SeqCst) > 0;
    assert!(matches!(
        post_through(&poster, &channel, &text, withdrawn).await,
        Err(MessengerError::PartlySent {
            sent: 1,
            total: 2,
            ..
        })
    ));
    host.until(|_| privmsgs(&channel).len() == 3).await;
    assert_eq!(privmsgs(&channel), [first.clone(), second, first]);
    // Stopping the messenger withdraws its session.
    stop.cancel();
    supervisor.await.unwrap();
    assert!(host.poster("irc").is_none());
    assert!(matches!(
        post_through(&poster, &channel, &random(), || false).await,
        Err(MessengerError::SocketEnded { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn signal_posts_go_out_through_the_daemon_session_in_parts() {
    use base64::Engine;
    let number = format!("+1555{}", 1_000_000 + std::process::id() % 1_000_000);
    let friend = Id::new_v4().hyphenated().to_string();
    let group = base64::engine::general_purpose::STANDARD.encode(Id::new_v4().as_bytes().repeat(2));
    let (dir, _credential, pairing, _anchor) = broker("https://unused.invalid", &random()).await;
    let daemon = SignalDaemon {
        accounts: vec![number.clone()],
        ..SignalDaemon::default()
    };
    let endpoint = serve_signal(dir.path(), daemon.clone()).await;
    let host = signal_host(&dir, pairing);
    let entry = MessengerConfig {
        enabled: true,
        endpoint: Some(endpoint),
        login: Some(number.clone()),
        allowed_chats: BTreeSet::from([friend.clone(), group.clone()]),
        ..MessengerConfig::new("signal".to_owned(), MessengerKind::Signal)
    };
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry.clone(),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| {
        h.poster("signal").is_some() && h.status("signal").state == MessengerState::Connected
    })
    .await;
    let sends = || -> Vec<Value> {
        daemon
            .received
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "send")
            .map(|r| r["params"].clone())
            .collect()
    };
    let poster = host.poster("signal").unwrap();
    assert_eq!(poster.config(), &entry);
    let first = "a".repeat(signal::MAX_MESSAGE);
    let rest = random();
    let sent = post_through(&poster, &group, &format!("{first}{rest}"), || false).await;
    assert_eq!(sent.unwrap(), 2);
    let direct = random();
    assert_eq!(
        post_through(&poster, &friend, &direct, || false)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sends(),
        [
            json!({ "account": number, "groupId": group, "message": first }),
            json!({ "account": number, "groupId": group, "message": rest }),
            json!({ "account": number, "recipient": [friend], "message": direct }),
        ]
    );
    let long = "a".repeat(signal::MAX_MESSAGE * MAX_SENT_PARTS + 1);
    assert!(matches!(
        post_through(&poster, &group, &long, || false).await,
        Err(MessengerError::MessageTooLong { .. })
    ));
    assert_eq!(sends().len(), 3);
    stop.cancel();
    supervisor.await.unwrap();
    assert!(host.poster("signal").is_none());
    assert!(matches!(
        post_through(&poster, &group, &random(), || false).await,
        Err(MessengerError::SocketEnded { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_resumes_its_saved_offset_after_a_restart() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(200, None).await;
    let name = "restarted";
    let chat = 31;
    let (before, during, after) = (random(), random(), random());
    bot.batches
        .lock()
        .unwrap()
        .push_back(json!([message(8, chat, None, &random(), &before)]));
    let wanted = vec![Desired {
        config: config(name, &origin, &credential, chat),
        run: true,
    }];
    let (_desired, receiver) = watch::channel(wanted.clone());
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|_| bot.polls.lock().unwrap().len() >= 2).await;
    stop.cancel();
    supervisor.await.unwrap();
    let account = bot.id.lock().unwrap().to_string();
    let saved = host.cursors.lock().unwrap().get(name).cloned().unwrap();
    assert_eq!(
        saved,
        SavedCursor {
            kind: MessengerKind::Telegram,
            account: account.clone(),
            next: Some("9".to_owned()),
            handled: BTreeSet::new(),
        }
    );
    // The gateway stopped while update 9's turn was running: its handling
    // had started, so it was saved as handled.
    host.cursors
        .lock()
        .unwrap()
        .get_mut(name)
        .unwrap()
        .handled
        .insert("9".to_owned());

    // After the restart, Telegram delivers that batch again.
    let polled = bot.polls.lock().unwrap().len();
    bot.batches.lock().unwrap().push_back(json!([
        message(9, chat, None, &random(), &during),
        message(10, chat, None, &random(), &after),
    ]));
    let (_desired, receiver) = watch::channel(wanted);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|_| bot.polls.lock().unwrap().len() >= polled + 2)
        .await;
    stop.cancel();
    supervisor.await.unwrap();
    let polls = bot.polls.lock().unwrap().clone();
    assert_eq!(polls[polled].get("offset").map(String::as_str), Some("9"));
    assert_eq!(
        polls[polled + 1].get("offset").map(String::as_str),
        Some("11")
    );
    let texts: Vec<_> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.1.clone())
        .collect();
    assert_eq!(texts, [before, after]);
    assert_eq!(
        host.cursors.lock().unwrap()[name].next.as_deref(),
        Some("11")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_position_of_another_kind_or_account_is_not_resumed() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(200, None).await;
    let (name, chat) = ("other-position", 41);
    let account = bot.id.lock().unwrap().to_string();
    for (kind, account) in [
        (MessengerKind::Matrix, account),
        (MessengerKind::Telegram, random()),
    ] {
        let saved = SavedCursor {
            kind,
            account,
            next: Some(random()),
            handled: BTreeSet::new(),
        };
        host.cursors.lock().unwrap().insert(name.to_owned(), saved);
        let polled = bot.polls.lock().unwrap().len();
        let (_desired, receiver) = watch::channel(vec![Desired {
            config: config(name, &origin, &credential, chat),
            run: true,
        }]);
        let stop = CancellationToken::new();
        let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
        host.until(|_| bot.polls.lock().unwrap().len() > polled)
            .await;
        stop.cancel();
        supervisor.await.unwrap();
        assert!(!bot.polls.lock().unwrap()[polled].contains_key("offset"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_answers_what_arrived_while_it_was_stopped() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let name = "matrix-resume";
    let domain = server.user.split_once(':').unwrap().1.to_owned();
    let (room, member) = (matrix_id('!', &domain), matrix_id('@', &domain));
    // Invited before the position was saved, and allowed since.
    let invited = matrix_id('!', &domain);
    let (since, missed) = (random(), random());
    host.cursors.lock().unwrap().insert(
        name.to_owned(),
        SavedCursor {
            kind: MessengerKind::Matrix,
            account: server.user.clone(),
            next: Some(since.clone()),
            handled: BTreeSet::new(),
        },
    );
    {
        let mut syncs = server.syncs.lock().unwrap();
        // An incremental sync would not list the invitation again; a sync
        // without a cursor does, along with history.
        let mut pending = timeline(vec![(
            &room,
            vec![text_event(&member, "m.text", &random())],
        )]);
        pending["invite"] = json!({ &invited: {"invite_state": {"events": []}} });
        syncs.push_back(pending);
        syncs.push_back(timeline(vec![(
            &room,
            vec![text_event(&member, "m.text", &missed)],
        )]));
    }
    let mut entry = matrix_config(name, &origin, &credential, &[&room, &invited]);
    entry.allowed_users.insert(member);
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|_| !server.sent.lock().unwrap().is_empty())
        .await;
    stop.cancel();
    supervisor.await.unwrap();
    assert_eq!(*server.joined.lock().unwrap(), [invited]);
    let queries = server.sync_queries.lock().unwrap().clone();
    assert!(!queries[0].contains_key("since"));
    assert_eq!(queries[1].get("since"), Some(&since));
    let texts: Vec<_> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.1.clone())
        .collect();
    assert_eq!(texts, [missed]);
    let saved = host.cursors.lock().unwrap()[name].clone();
    assert_ne!(saved.next, Some(since));
    assert!(saved.next.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_starts_over_when_too_much_arrived_while_it_was_stopped() {
    let (server, origin, _dir, credential, host, _anchor) = matrix_setup().await;
    let name = "matrix-behind";
    let domain = server.user.split_once(':').unwrap().1.to_owned();
    let (room, member) = (matrix_id('!', &domain), matrix_id('@', &domain));
    let (since, text) = (random(), random());
    host.cursors.lock().unwrap().insert(
        name.to_owned(),
        SavedCursor {
            kind: MessengerKind::Matrix,
            account: server.user.clone(),
            next: Some(since.clone()),
            handled: BTreeSet::new(),
        },
    );
    {
        let mut syncs = server.syncs.lock().unwrap();
        // No pending invitations.
        syncs.push_back(json!({}));
        // More than one batch reads.
        syncs.push_back(timeline(vec![(
            &room,
            (0..1_001)
                .map(|_| text_event(&member, "m.text", &random()))
                .collect(),
        )]));
        // Starting over: the first sync only marks where answering starts.
        syncs.push_back(timeline(vec![(
            &room,
            vec![text_event(&member, "m.text", &random())],
        )]));
        syncs.push_back(timeline(vec![(
            &room,
            vec![text_event(&member, "m.text", &text)],
        )]));
    }
    let mut entry = matrix_config(name, &origin, &credential, &[&room]);
    entry.allowed_users.insert(member);
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: entry,
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|_| !server.sent.lock().unwrap().is_empty())
        .await;
    let status = host.status(name);
    stop.cancel();
    supervisor.await.unwrap();
    // The skipped messages are reported, and the messenger kept running.
    assert_eq!(status.state, MessengerState::Connected);
    let failure = status.failure.unwrap();
    assert_eq!(failure.kind, ErrorKind::Protocol);
    assert!(failure.source.is_some());
    assert_eq!(host.connects(name), 1);
    let queries = server.sync_queries.lock().unwrap().clone();
    assert_eq!(queries[1].get("since"), Some(&since));
    assert!(!queries[2].contains_key("since"));
    let texts: Vec<_> = host
        .turns
        .lock()
        .unwrap()
        .iter()
        .map(|t| t.1.clone())
        .collect();
    assert_eq!(texts, [text]);
}
