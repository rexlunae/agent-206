#![cfg(unix)]
//! A model pushes a branch with `git_push`: only after the user approves the
//! call, through the vault broker, which alone holds the forge token. The
//! forge is a smart HTTP server on loopback that stores what it receives in
//! a bare repository with gitoxide's own pack indexer. Every identifier,
//! file and key is generated at runtime.
mod support;

use a206_core::*;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use gix::ObjectId;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use support::*;

type Gateway = a206_runtime::gateway::Gateway;
const API: ProviderApi = ProviderApi::OpenAi;
const TOOL: &str = a206_runtime::git_push::PUSH;

/// A forge holding one bare repository at `/team/repo.git`.
#[derive(Clone)]
struct Forge {
    bare: PathBuf,
    /// The `Authorization` header of each request, and the request path.
    seen: Arc<Mutex<Vec<(String, String)>>>,
    /// A reason to refuse every update with, like a protected branch.
    refuse: Arc<Mutex<Option<String>>>,
}

fn pkt(out: &mut Vec<u8>, line: &str) {
    out.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
    out.extend_from_slice(line.as_bytes());
}

async fn advertise(State(forge): State<Forge>, headers: HeaderMap) -> (HeaderMap, Vec<u8>) {
    forge.record(&headers, "info/refs");
    let repo = gix::open(&forge.bare).unwrap();
    let mut refs = vec![];
    for reference in repo.references().unwrap().all().unwrap() {
        let mut reference = reference.unwrap();
        let id = reference.peel_to_id().unwrap().detach();
        refs.push((reference.name().as_bstr().to_string(), id));
    }
    let mut body = vec![];
    pkt(&mut body, "# service=git-receive-pack\n");
    body.extend_from_slice(b"0000");
    let capabilities = "report-status delete-refs agent=git/forge";
    if refs.is_empty() {
        let null = ObjectId::null(repo.object_hash());
        pkt(
            &mut body,
            &format!("{null} capabilities^{{}}\0{capabilities}\n"),
        );
    }
    for (index, (name, id)) in refs.iter().enumerate() {
        if index == 0 {
            pkt(&mut body, &format!("{id} {name}\0{capabilities}\n"));
        } else {
            pkt(&mut body, &format!("{id} {name}\n"));
        }
    }
    body.extend_from_slice(b"0000");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/x-git-receive-pack-advertisement"
            .parse()
            .unwrap(),
    );
    (headers, body)
}

async fn receive(
    State(forge): State<Forge>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    forge.record(&headers, "git-receive-pack");
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/x-git-receive-pack-request"
    );
    // One command, a flush, then the pack.
    let length = usize::from_str_radix(std::str::from_utf8(&body[..4]).unwrap(), 16).unwrap();
    let command = std::str::from_utf8(&body[4..length]).unwrap();
    assert_eq!(&body[length..length + 4], b"0000");
    let pack = &body[length + 4..];
    let (command, capabilities) = command.split_once('\0').unwrap();
    assert!(
        capabilities
            .split_whitespace()
            .any(|c| c == "report-status")
    );
    let mut parts = command.split(' ');
    let (old, new, name) = (
        ObjectId::from_hex(parts.next().unwrap().as_bytes()).unwrap(),
        ObjectId::from_hex(parts.next().unwrap().as_bytes()).unwrap(),
        parts.next().unwrap().to_owned(),
    );
    let repo = gix::open(&forge.bare).unwrap();
    gix_pack::Bundle::write_to_directory(
        &mut std::io::BufReader::new(pack),
        Some(&repo.objects.store_ref().path().join("pack")),
        &mut gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
        None::<gix::objs::find::Never>,
        repo.object_hash(),
        Default::default(),
    )
    .unwrap();
    let repo = gix::open(&forge.bare).unwrap();
    let current = repo
        .try_find_reference(name.as_str())
        .unwrap()
        .map(|mut r| r.peel_to_id().unwrap().detach())
        .unwrap_or_else(|| ObjectId::null(repo.object_hash()));
    let refused = forge.refuse.lock().unwrap().clone();
    let status = if let Some(reason) = refused {
        format!("ng {name} {reason}\n")
    } else if current != old {
        format!("ng {name} stale info\n")
    } else {
        // Every object the new commit reaches must have arrived.
        let commit = repo.find_commit(new).unwrap();
        commit
            .tree()
            .unwrap()
            .traverse()
            .breadthfirst(&mut gix::traverse::tree::Recorder::default())
            .unwrap();
        let edit = gix::refs::transaction::RefEdit {
            change: gix::refs::transaction::Change::Update {
                log: Default::default(),
                expected: gix::refs::transaction::PreviousValue::Any,
                new: gix::refs::Target::Object(new),
            },
            name: name.as_str().try_into().unwrap(),
            deref: false,
        };
        let signature = gix::actor::Signature {
            name: "forge".into(),
            email: "forge@example.invalid".into(),
            time: gix::date::Time::new(2_000_000, 0),
        };
        let mut time = Default::default();
        repo.edit_references_as([edit], Some(signature.to_ref(&mut time)))
            .unwrap();
        format!("ok {name}\n")
    };
    let mut out = vec![];
    pkt(&mut out, "unpack ok\n");
    pkt(&mut out, &status);
    out.extend_from_slice(b"0000");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/x-git-receive-pack-result".parse().unwrap(),
    );
    (StatusCode::OK, headers, out)
}

impl Forge {
    fn record(&self, headers: &HeaderMap, path: &str) {
        let auth = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        self.seen.lock().unwrap().push((auth, path.to_owned()));
    }
    fn branch(&self, name: &str) -> Option<ObjectId> {
        gix::open(&self.bare)
            .unwrap()
            .try_find_reference(format!("refs/heads/{name}").as_str())
            .unwrap()
            .map(|mut r| r.peel_to_id().unwrap().detach())
    }
    fn posts(&self) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, path)| path == "git-receive-pack")
            .count()
    }
}

/// A forge on loopback and its origin.
async fn forge(dir: &Path) -> (String, Forge) {
    let bare = dir.join("repo.git");
    gix::init_bare(&bare).unwrap();
    let forge = Forge {
        bare,
        seen: Arc::default(),
        refuse: Arc::default(),
    };
    let router = Router::new()
        .route("/team/repo.git/info/refs", get(advertise))
        .route("/team/repo.git/git-receive-pack", post(receive))
        .with_state(forge.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (origin, forge)
}

/// An unlocked broker in a new profile holding `token` bound to `origin`;
/// the credential's name and a gateway pairing.
async fn broker(origin: &str, token: &str) -> (tempfile::TempDir, String, BrokerPairing) {
    use a206_broker::*;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let serving = root.clone();
    // Detached: the broker lives as long as the test's runtime.
    tokio::spawn(async move { serve(&serving).await.unwrap() });
    let mut user = loop {
        match Client::connect(&root, Role::User).await {
            Ok(client) => break client,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    user.call(Request::Unlock {
        password: WireSecret::new(nonce()),
        totp: None,
        create: true,
    })
    .await
    .unwrap();
    let name = nonce();
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
    let Reply::GatewayPairing { token } = user.call(Request::PairGateway).await.unwrap() else {
        panic!("expected a pairing");
    };
    (dir, name, BrokerPairing::new(token.expose().to_owned()))
}

async fn approval(gateway: &Gateway, thread: Id) -> ApprovalRequest {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let state = gateway.snapshot().unwrap();
            let thread = state.threads.iter().find(|t| t.id == thread).unwrap();
            match thread.approvals.first() {
                Some(approval) => return approval.clone(),
                None => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap_or_else(wait("the approval request"))
}

/// Commit `files` on top of the current branch; the new commit.
fn commit(root: &Path, files: &[(&str, &str)]) -> ObjectId {
    let repo = gix::open(root).unwrap();
    let parent = repo.head_id().ok().map(|id| id.detach());
    let base = match parent {
        Some(id) => repo.find_commit(id).unwrap().tree_id().unwrap().detach(),
        None => ObjectId::empty_tree(repo.object_hash()),
    };
    let mut tree = repo.edit_tree(base).unwrap();
    for (path, text) in files {
        let id = repo.write_blob(text.as_bytes()).unwrap().detach();
        tree.upsert(*path, gix::objs::tree::EntryKind::Blob, id)
            .unwrap();
    }
    let tree = tree.write().unwrap().detach();
    let signature = gix::actor::Signature {
        name: "n".into(),
        email: "n@example.invalid".into(),
        time: gix::date::Time::new(2_000_000, 0),
    };
    let (mut a, mut b) = Default::default();
    repo.commit_as(
        signature.to_ref(&mut a),
        signature.to_ref(&mut b),
        "HEAD",
        nonce(),
        tree,
        parent,
    )
    .unwrap()
    .detach()
}

struct Rig {
    gateway: Gateway,
    _state: tempfile::TempDir,
    workspace: tempfile::TempDir,
    _forge_dir: tempfile::TempDir,
    forge: Forge,
    token: String,
    username: String,
    requests: tokio::sync::mpsc::Receiver<Opened>,
    thread: Id,
    branch: String,
}
impl Rig {
    async fn ok(&self, command: Command) {
        assert_eq!(request(&self.gateway, command).await, Reply::Ok);
    }
    /// One run in which the model calls `git_push` with `arguments` and the
    /// user answers with `decision`; the call's record and result text.
    async fn push(
        &mut self,
        arguments: serde_json::Value,
        decision: ApprovalDecision,
    ) -> (ToolCall, Option<String>) {
        let task = send(&self.gateway, self.thread);
        let first = next_request(&mut self.requests).await;
        assert!(offered(API, &first.body).contains(&TOOL.to_owned()));
        let call = Call::new(TOOL, arguments);
        first
            .respond(calls_response(API, std::slice::from_ref(&call), ""))
            .await;
        let asked = approval(&self.gateway, self.thread).await;
        assert_eq!(asked.tool, TOOL);
        self.ok(Command::ResolveApproval {
            thread: self.thread,
            approval: asked.id,
            digest: asked.digest,
            decision,
        })
        .await;
        let second = next_request(&mut self.requests).await;
        let result = result_for(API, &second.body, &call.id);
        assert!(!second.body.to_string().contains(&self.token));
        second.respond(text_response(API, &nonce())).await;
        let thread = finished(task).await;
        (calls_of(&thread).last().unwrap().clone(), result)
    }
}

/// A project repository with one commit and an `origin` remote on a new
/// forge; a Code mode conversation granted git, permitted to push, with a
/// git credential set for the forge.
async fn rig() -> Rig {
    let forge_dir = tempfile::tempdir().unwrap();
    let (origin, forge) = forge(forge_dir.path()).await;
    let token = nonce();
    let (state, credential, pairing) = broker(&origin, &token).await;
    let workspace = tempfile::tempdir().unwrap();
    gix::init(workspace.path()).unwrap();
    let config = workspace.path().join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[user]\n\tname = n\n\temail = n@example.invalid\n[remote \"origin\"]\n\turl = {origin}/team/repo.git\n"
    ));
    std::fs::write(&config, text).unwrap();
    commit(
        workspace.path(),
        &[("readme", &nonce()), ("src/main", &nonce())],
    );
    let branch = gix::open(workspace.path())
        .unwrap()
        .head_name()
        .unwrap()
        .unwrap()
        .shorten()
        .to_string();

    let gateway = Gateway::open(state.path()).unwrap();
    let (model, requests) = connect(&gateway, API).await;
    let Reply::Project(project) = request(
        &gateway,
        Command::CreateProject {
            name: nonce(),
            root: workspace.path().display().to_string(),
        },
    )
    .await
    else {
        panic!("project creation failed");
    };
    let username = nonce();
    let mut settings = gateway.snapshot().unwrap().settings;
    settings.git_credentials.push(GitCredential {
        origin: origin.clone(),
        username: username.clone(),
        credential,
    });
    let rig = Rig {
        gateway,
        _state: state,
        workspace,
        _forge_dir: forge_dir,
        forge,
        token,
        username,
        requests,
        thread: project.manager,
        branch,
    };
    rig.ok(Command::SaveSettings(Box::new(settings))).await;
    rig.ok(Command::PairBroker { pairing }).await;
    let configured = request(
        &rig.gateway,
        Command::ConfigureThread {
            id: rig.thread,
            mode: Mode::Code,
            model: Some(model.clone()),
            subagent_models: [model].into(),
        },
    )
    .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    rig.ok(Command::SetBackend {
        thread: rig.thread,
        backend: CodingBackend::Native,
        capabilities: [Capability::GitRead, Capability::GitWrite].into(),
    })
    .await;
    // Allowing the tool on the conversation still leaves each call to the user.
    rig.ok(Command::SetToolPermission {
        thread: rig.thread,
        tool: TOOL.into(),
        permission: Some(ToolPermission::Allow),
    })
    .await;
    rig
}

fn tracking(root: &Path, branch: &str) -> Option<ObjectId> {
    gix::open(root)
        .unwrap()
        .try_find_reference(format!("refs/remotes/origin/{branch}").as_str())
        .unwrap()
        .map(|mut r| r.peel_to_id().unwrap().detach())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_pushes_a_branch_only_after_approval_and_never_sees_the_token() {
    let mut rig = rig().await;

    // Denied: nothing is sent.
    let (record, _) = rig.push(json!({}), ApprovalDecision::Deny).await;
    assert_eq!(record.state, ToolCallState::Denied);
    assert!(rig.forge.seen.lock().unwrap().is_empty());

    // Approved: the forge gets the branch and every object it reaches.
    let first = gix::open(rig.workspace.path())
        .unwrap()
        .head_id()
        .unwrap()
        .detach();
    let (record, result) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let result: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(result["branch"], rig.branch.as_str());
    assert_eq!(result["commit"], first.to_string());
    assert_eq!(result["from"], serde_json::Value::Null);
    assert_eq!(result["commits"], 1);
    assert_eq!(rig.forge.branch(&rig.branch), Some(first));
    assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(first));
    {
        use base64::Engine as _;
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", rig.username, rig.token))
        );
        let seen = rig.forge.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|(auth, _)| *auth == basic));
    }

    // A second commit fast-forwards the forge's branch.
    let second = commit(rig.workspace.path(), &[("src/main", &nonce())]);
    let (record, result) = rig
        .push(
            json!({"branch": rig.branch, "remote": "origin"}),
            ApprovalDecision::Approve,
        )
        .await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let result: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(result["from"], first.to_string());
    assert_eq!(result["commits"], 1);
    // The commit, the root tree, `src` and the changed file.
    assert_eq!(result["objects"], 4);
    assert_eq!(rig.forge.branch(&rig.branch), Some(second));

    // Nothing new: no pack is sent.
    let posts = rig.forge.posts();
    let (record, result) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    assert!(result.unwrap().contains("\"up_to_date\":true"));
    assert_eq!(rig.forge.posts(), posts);

    // The token never reached a profile file.
    let mut pending = vec![rig._state.path().to_owned()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else if entry.file_type().unwrap().is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                let needle = rig.token.as_bytes();
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle),
                    "{} holds the token",
                    entry.path().display()
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pushes_that_would_lose_commits_or_that_the_forge_refuses_change_nothing() {
    let mut rig = rig().await;
    let (record, _) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let pushed = rig.forge.branch(&rig.branch).unwrap();

    // Someone else moved the forge's branch to a commit this repository
    // lacks: the push is refused before any pack is sent.
    let elsewhere = {
        let repo = gix::open(&rig.forge.bare).unwrap();
        let tree = repo
            .find_commit(pushed)
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let signature = gix::actor::Signature {
            name: "o".into(),
            email: "o@example.invalid".into(),
            time: gix::date::Time::new(2_000_000, 0),
        };
        let (mut a, mut b) = Default::default();
        repo.commit_as(
            signature.to_ref(&mut a),
            signature.to_ref(&mut b),
            format!("refs/heads/{}", rig.branch),
            nonce(),
            tree,
            [pushed],
        )
        .unwrap()
        .detach()
    };
    commit(rig.workspace.path(), &[("readme", &nonce())]);
    let posts = rig.forge.posts();
    let (record, _) = rig.push(json!({}), ApprovalDecision::Approve).await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::Conflict);
    assert_eq!(rig.forge.posts(), posts);
    assert_eq!(rig.forge.branch(&rig.branch), Some(elsewhere));

    // A branch the forge protects is refused with its reason, and the
    // remote-tracking branch stays where it was.
    let reason = format!("protected {}", nonce());
    *rig.forge.refuse.lock().unwrap() = Some(reason.clone());
    let topic = format!("t-{}", nonce());
    {
        let repo = gix::open(rig.workspace.path()).unwrap();
        let head = repo.head_id().unwrap().detach();
        repo.reference(
            format!("refs/heads/{topic}"),
            head,
            gix::refs::transaction::PreviousValue::MustNotExist,
            "test",
        )
        .unwrap();
    }
    let (record, _) = rig
        .push(json!({"branch": topic}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::Conflict);
    let cause = record.error.as_ref().unwrap().source.as_ref().unwrap();
    assert!(cause.message.contains(&reason), "{:?}", record.error);
    assert_eq!(rig.forge.branch(&topic), None);
    assert_eq!(tracking(rig.workspace.path(), &topic), None);

    // A remote on a forge with no git credential set is refused before the
    // broker is asked for anything.
    let config = rig.workspace.path().join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[remote \"mirror\"]\n\turl = https://example.invalid/team/repo.git\n");
    std::fs::write(&config, text).unwrap();
    let seen = rig.forge.seen.lock().unwrap().len();
    let (record, _) = rig
        .push(json!({"remote": "mirror"}), ApprovalDecision::Approve)
        .await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::PermissionDenied);
    assert_eq!(rig.forge.seen.lock().unwrap().len(), seen);
}
