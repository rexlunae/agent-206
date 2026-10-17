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
use std::sync::{Arc, Mutex};

fn random() -> String {
    Id::new_v4().to_string()
}

/// What the stand-in Bot API serves and records.
#[derive(Clone, Default)]
struct Bot {
    token: String,
    username: String,
    /// Status `getMe` answers with.
    me_status: u16,
    /// Batches `getUpdates` returns in order; then empty batches.
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
    let status = axum::http::StatusCode::from_u16(bot.me_status).unwrap();
    (
        status,
        Json(json!({"ok": true, "result": {"username": bot.username}})),
    )
}

async fn updates(
    State(bot): State<Bot>,
    Path(path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    check_path(&bot, &path);
    bot.polls.lock().unwrap().push(query);
    let next = bot.batches.lock().unwrap().pop_front();
    match next {
        Some(batch) => Json(json!({"ok": true, "result": batch})),
        None => {
            // An idle long poll, kept short for the test.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Json(json!({"ok": true, "result": []}))
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
    use a206_broker::*;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let serving = root.clone();
    tokio::spawn(async move { serve(&serving).await.unwrap() });
    while !socket_path(&root).exists() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut user = Client::connect(&root, Role::User).await.unwrap();
    user.call(Request::Unlock {
        password: WireSecret::new(random()),
        totp: None,
        create: true,
    })
    .await
    .unwrap();
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
        value: WireValue::Single(WireSecret::new(token.to_owned())),
        replace: false,
    })
    .await
    .unwrap();
    let BrokerReply::GatewayPairing { token } = user.call(Request::PairGateway).await.unwrap()
    else {
        panic!("expected a pairing");
    };
    let pairing = BrokerPairing::new(token.expose().to_owned());
    let anchor = crate::provider::verify_pairing(&root, &pairing)
        .await
        .unwrap();
    (dir, name, pairing, anchor)
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
    changed: Arc<tokio::sync::Notify>,
}
impl Host for Recorder {
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
        change(
            self.statuses
                .lock()
                .unwrap()
                .entry(name.to_owned())
                .or_default(),
        );
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

fn message(update: i64, chat: i64, title: &str, text: &str) -> Value {
    json!({
        "update_id": update,
        "message": {
            "message_id": update,
            "chat": {"id": chat, "type": "group", "title": title},
            "from": {"id": chat, "first_name": random()},
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
) -> (
    Bot,
    String,
    tempfile::TempDir,
    String,
    Recorder,
    crate::provider::PairingAnchor,
) {
    let bot = Bot {
        token: format!("{}:{}", std::process::id(), random().replace('-', "_")),
        username: random(),
        me_status,
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
        changed: Arc::default(),
    };
    (bot, origin, dir, credential, host, anchor)
}

const FAST: Backoff = Backoff {
    initial: std::time::Duration::from_millis(5),
    max: std::time::Duration::from_millis(20),
};

#[tokio::test(flavor = "multi_thread")]
async fn allowed_chats_get_a_thread_and_a_reply_and_others_are_refused() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(200).await;
    let name = "team-bot";
    let (allowed, refused) = (-1_000_000 - i64::from(std::process::id()), 42);
    let refused_title = random();
    let text = random();
    bot.batches.lock().unwrap().push_back(json!([
        message(7, refused, &refused_title, &random()),
        message(8, allowed, &random(), &text),
    ]));
    let (desired, receiver) = watch::channel(vec![Desired {
        config: config(name, &origin, &credential, allowed),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));

    host.until(|h| !bot.sent.lock().unwrap().is_empty() && h.status(name).refused.len() == 1)
        .await;
    let status = host.status(name);
    assert_eq!(status.state, MessengerState::Connected);
    assert_eq!(status.account, Some(format!("@{}", bot.username)));
    assert_eq!(status.refused[0].chat, refused.to_string());
    assert_eq!(
        status.refused[0].title.as_deref(),
        Some(refused_title.as_str())
    );
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
        config: config(name, &origin, &credential, allowed),
        run: false,
    }]);
    host.until(|h| h.status(name).state == MessengerState::Stopped)
        .await;
    stop.cancel();
    supervisor.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_token_fails_once_and_waits_for_the_user() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(401).await;
    let name = "rejected";
    let (_desired, receiver) = watch::channel(vec![Desired {
        config: config(name, &origin, &credential, 1),
        run: true,
    }]);
    let stop = CancellationToken::new();
    let supervisor = tokio::spawn(supervise(host.clone(), receiver, FAST, stop.clone()));
    host.until(|h| h.status(name).state == MessengerState::Failed)
        .await;
    let failure = host.status(name).failure.unwrap();
    assert_eq!(failure.kind, ErrorKind::PermissionDenied);
    // Retrying cannot help, so it does not retry: with the fast backoff, a
    // retry would have happened many times over by the time a later message
    // could be delivered.
    stop.cancel();
    supervisor.await.unwrap();
    assert_eq!(*bot.me_calls.lock().unwrap(), 1);
    assert!(bot.polls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_without_a_model_is_told_why() {
    let (bot, origin, _dir, credential, host, _anchor) = setup(200).await;
    let name = "no-model";
    let chat = 5;
    bot.batches
        .lock()
        .unwrap()
        .push_back(json!([message(1, chat, &random(), &random())]));
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
