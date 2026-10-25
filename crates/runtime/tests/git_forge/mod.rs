//! The stand-in forge, vault broker and conversation that the `git_push`
//! and `git_fetch` tests share. The forge is a smart HTTP server on
//! loopback that stores what it receives in a bare repository with
//! gitoxide's own pack indexer, and serves fetches from it.
#![allow(dead_code)]
use crate::support::*;
use a206_core::*;
use axum::{
    Router,
    body::Bytes,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use gix::ObjectId;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub type Gateway = a206_runtime::gateway::Gateway;
pub const API: ProviderApi = ProviderApi::OpenAi;
pub const TOOL: &str = a206_runtime::git_remote::PUSH;
pub const FETCH: &str = a206_runtime::git_remote::FETCH;

/// A forge holding one bare repository at `/team/repo.git`.
#[derive(Clone)]
pub struct Forge {
    pub bare: PathBuf,
    /// The `Authorization` header of each request, and the request path.
    pub seen: Arc<Mutex<Vec<(String, String)>>>,
    /// A reason to refuse every update with, like a protected branch.
    pub refuse: Arc<Mutex<Option<String>>>,
    /// Leave the last object out of every pack it serves.
    pub incomplete: Arc<std::sync::atomic::AtomicBool>,
}

/// The packets of a request body, `None` for a flush.
pub fn packets(mut body: &[u8]) -> Vec<Option<Vec<u8>>> {
    let mut out = vec![];
    while !body.is_empty() {
        let length = usize::from_str_radix(std::str::from_utf8(&body[..4]).unwrap(), 16).unwrap();
        if length == 0 {
            out.push(None);
            body = &body[4..];
        } else {
            out.push(Some(body[4..length].to_vec()));
            body = &body[length..];
        }
    }
    out
}

pub fn pkt(out: &mut Vec<u8>, line: &str) {
    out.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
    out.extend_from_slice(line.as_bytes());
}

pub async fn advertise(
    State(forge): State<Forge>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> (HeaderMap, Vec<u8>) {
    forge.record(&headers, "info/refs");
    let service = query
        .as_deref()
        .and_then(|q| q.strip_prefix("service="))
        .unwrap()
        .to_owned();
    let repo = gix::open(&forge.bare).unwrap();
    let mut refs = vec![];
    // Each reference's own object, and for a tag object the commit it
    // names, as `refs/tags/<name>^{}`, as git advertises them.
    for reference in repo.references().unwrap().all().unwrap() {
        let mut reference = reference.unwrap();
        let name = reference.name().as_bstr().to_string();
        let id = reference.follow_to_object().unwrap().detach();
        refs.push((name.clone(), id));
        let peeled = reference.peel_to_id().unwrap().detach();
        if peeled != id {
            refs.push((format!("{name}^{{}}"), peeled));
        }
    }
    let mut body = vec![];
    pkt(&mut body, &format!("# service={service}\n"));
    body.extend_from_slice(b"0000");
    let capabilities = match service.as_str() {
        "git-receive-pack" => "report-status delete-refs agent=git/forge",
        _ => "multi_ack side-band-64k ofs-delta no-progress agent=git/forge",
    };
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
        format!("application/x-{service}-advertisement")
            .parse()
            .unwrap(),
    );
    (headers, body)
}

/// Answers wants and haves with a side-band pack of what the wanted commit
/// reaches beyond the haves, as git-upload-pack does.
pub async fn upload(
    State(forge): State<Forge>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    forge.record(&headers, "git-upload-pack");
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/x-git-upload-pack-request"
    );
    let packets = packets(&body);
    let line = |packet: &Option<Vec<u8>>| String::from_utf8(packet.clone().unwrap()).unwrap();
    let first = line(&packets[0]);
    let (want, capabilities) = first
        .strip_prefix("want ")
        .unwrap()
        .trim_end()
        .split_once(' ')
        .unwrap();
    assert!(capabilities.split(' ').any(|c| c == "side-band-64k"));
    assert_eq!(packets[1], None);
    assert_eq!(line(packets.last().unwrap()), "done\n");
    let repo = gix::open(&forge.bare).unwrap();
    let haves: Vec<ObjectId> = packets[2..packets.len() - 1]
        .iter()
        .map(|packet| {
            let have = line(packet);
            ObjectId::from_hex(have.strip_prefix("have ").unwrap().trim_end().as_bytes()).unwrap()
        })
        .filter(|id| repo.has_object(id))
        .collect();
    let never = std::sync::atomic::AtomicBool::new(false);
    // A wanted tag object comes with the tag objects and commit it leads to.
    let mut want = ObjectId::from_hex(want.as_bytes()).unwrap();
    let mut tags = vec![];
    while let Ok(tag) = repo.find_tag(want) {
        tags.push(want);
        want = tag.target_id().unwrap().detach();
    }
    let mut ids =
        a206_git_push::objects(&repo, want, haves, a206_runtime::git_remote::LIMITS, &never)
            .unwrap()
            .ids;
    ids.splice(0..0, tags);
    if forge.incomplete.load(std::sync::atomic::Ordering::SeqCst) {
        ids.pop();
    }
    let pack =
        a206_git_push::write_pack(&repo, &ids, a206_runtime::git_remote::LIMITS, &never).unwrap();
    let mut out = vec![];
    pkt(&mut out, "NAK\n");
    let band = |out: &mut Vec<u8>, band: u8, data: &[u8]| {
        out.extend_from_slice(format!("{:04x}", data.len() + 5).as_bytes());
        out.push(band);
        out.extend_from_slice(data);
    };
    band(&mut out, 2, b"Counting objects\n");
    for chunk in pack.chunks(1000) {
        band(&mut out, 1, chunk);
    }
    out.extend_from_slice(b"0000");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/x-git-upload-pack-result".parse().unwrap(),
    );
    (StatusCode::OK, headers, out)
}

pub async fn receive(
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
        // Every object the new commit (or tag) reaches must have arrived.
        let commit = repo
            .find_object(new)
            .unwrap()
            .peel_to_kind(gix::object::Kind::Commit)
            .unwrap()
            .into_commit();
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
    pub fn record(&self, headers: &HeaderMap, path: &str) {
        let auth = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        self.seen.lock().unwrap().push((auth, path.to_owned()));
    }
    pub fn branch(&self, name: &str) -> Option<ObjectId> {
        gix::open(&self.bare)
            .unwrap()
            .try_find_reference(format!("refs/heads/{name}").as_str())
            .unwrap()
            .map(|mut r| r.peel_to_id().unwrap().detach())
    }
    /// The object the forge's tag `name` points at.
    pub fn tag(&self, name: &str) -> Option<ObjectId> {
        gix::open(&self.bare)
            .unwrap()
            .try_find_reference(format!("refs/tags/{name}").as_str())
            .unwrap()
            .map(|mut r| r.follow_to_object().unwrap().detach())
    }
    pub fn posts(&self) -> usize {
        self.count("git-receive-pack")
    }
    pub fn count(&self, path: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, seen)| seen == path)
            .count()
    }
    /// Commit a new file on the forge's `branch`, on top of `parents`.
    pub fn commit(&self, branch: &str, parents: &[ObjectId]) -> ObjectId {
        let repo = gix::open(&self.bare).unwrap();
        let base = match parents.first() {
            Some(parent) => repo
                .find_commit(*parent)
                .unwrap()
                .tree_id()
                .unwrap()
                .detach(),
            None => ObjectId::empty_tree(repo.object_hash()),
        };
        let blob = repo.write_blob(nonce().as_bytes()).unwrap().detach();
        let mut tree = repo.edit_tree(base).unwrap();
        tree.upsert(
            format!("elsewhere/{}", nonce()),
            gix::objs::tree::EntryKind::Blob,
            blob,
        )
        .unwrap();
        let tree = tree.write().unwrap().detach();
        let signature = gix::actor::Signature {
            name: "o".into(),
            email: "o@example.invalid".into(),
            time: gix::date::Time::new(2_000_000, 0),
        };
        let (mut a, mut b) = Default::default();
        let id = repo
            .commit_as(
                signature.to_ref(&mut a),
                signature.to_ref(&mut b),
                // A fresh scratch reference, so the commit's parents are free.
                format!("refs/scratch/{}", nonce()),
                nonce(),
                tree,
                parents.iter().copied(),
            )
            .unwrap()
            .detach();
        // Moved without checking its previous value, like a forced push.
        repo.reference(
            format!("refs/heads/{branch}"),
            id,
            gix::refs::transaction::PreviousValue::Any,
            "test",
        )
        .unwrap();
        id
    }
}

/// A forge on loopback and its origin.
pub async fn forge(dir: &Path) -> (String, Forge) {
    let bare = dir.join("repo.git");
    gix::init_bare(&bare).unwrap();
    let forge = Forge {
        bare,
        seen: Arc::default(),
        refuse: Arc::default(),
        incomplete: Arc::default(),
    };
    let router = Router::new()
        .route("/team/repo.git/info/refs", get(advertise))
        .route("/team/repo.git/git-receive-pack", post(receive))
        .route("/team/repo.git/git-upload-pack", post(upload))
        .with_state(forge.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (origin, forge)
}

/// An unlocked broker in a new profile holding `token` bound to `origin`;
/// the credential's name and a gateway pairing.
pub async fn broker(origin: &str, token: &str) -> (tempfile::TempDir, String, BrokerPairing) {
    let value = a206_broker::WireValue::Single(a206_broker::WireSecret::new(token.to_owned()));
    broker_holding(origin, a206_vault::CredentialKind::Token, value).await
}

/// The same holding `value`, a credential of `kind`.
pub async fn broker_holding(
    origin: &str,
    kind: a206_vault::CredentialKind,
    value: a206_broker::WireValue,
) -> (tempfile::TempDir, String, BrokerPairing) {
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
            kind,
            policy: a206_vault::AccessPolicy::WithApproval,
            description: None,
            disabled: false,
            destinations: vec![origin.to_owned()],
        },
        value,
        replace: false,
    })
    .await
    .unwrap();
    let Reply::GatewayPairing { token } = user.call(Request::PairGateway).await.unwrap() else {
        panic!("expected a pairing");
    };
    (dir, name, BrokerPairing::new(token.expose().to_owned()))
}

pub async fn approval(gateway: &Gateway, thread: Id) -> ApprovalRequest {
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
pub fn commit(root: &Path, files: &[(&str, &str)]) -> ObjectId {
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

pub struct Rig {
    pub gateway: Gateway,
    pub _state: tempfile::TempDir,
    pub workspace: tempfile::TempDir,
    pub _forge_dir: tempfile::TempDir,
    pub forge: Forge,
    pub token: String,
    pub username: String,
    /// The origin the git credential is set for.
    pub origin: String,
    pub requests: tokio::sync::mpsc::Receiver<Opened>,
    pub thread: Id,
    pub branch: String,
}
impl Rig {
    pub async fn ok(&self, command: Command) {
        assert_eq!(request(&self.gateway, command).await, Reply::Ok);
    }
    /// One run in which the model calls `git_push` with `arguments` and the
    /// user answers with `decision`; the call's record and result text.
    pub async fn push(
        &mut self,
        arguments: serde_json::Value,
        decision: ApprovalDecision,
    ) -> (ToolCall, Option<String>) {
        self.call(TOOL, arguments, decision).await
    }
    /// The same with `git_fetch`.
    pub async fn fetch(
        &mut self,
        arguments: serde_json::Value,
        decision: ApprovalDecision,
    ) -> (ToolCall, Option<String>) {
        self.call(FETCH, arguments, decision).await
    }
    pub async fn call(
        &mut self,
        tool: &str,
        arguments: serde_json::Value,
        decision: ApprovalDecision,
    ) -> (ToolCall, Option<String>) {
        let task = send(&self.gateway, self.thread);
        let first = next_request(&mut self.requests).await;
        assert!(offered(API, &first.body).contains(&tool.to_owned()));
        let call = Call::new(tool, arguments);
        first
            .respond(calls_response(API, std::slice::from_ref(&call), ""))
            .await;
        let asked = approval(&self.gateway, self.thread).await;
        assert_eq!(asked.tool, tool);
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
pub async fn rig() -> Rig {
    rig_over(Transport::Https).await
}

/// How the project's `origin` reaches the forge.
pub enum Transport {
    /// Its HTTP URL, with a token the broker attaches.
    Https,
    /// An ssh URL naming the forge's address, `ssh://git@host:port`, with a
    /// vault SSH key; the `ssh` on the gateway's PATH must relay to it.
    Ssh,
}

/// The same, with `origin` reached over `transport`. Over ssh, `token` is
/// the private key.
pub async fn rig_over(transport: Transport) -> Rig {
    let forge_dir = tempfile::tempdir().unwrap();
    let (http, forge) = forge(forge_dir.path()).await;
    let (origin, url, token, kind, value) = match transport {
        Transport::Https => {
            let token = nonce();
            let value = a206_broker::WireValue::Single(a206_broker::WireSecret::new(token.clone()));
            (
                http.clone(),
                format!("{http}/team/repo.git"),
                token,
                a206_vault::CredentialKind::Token,
                value,
            )
        }
        Transport::Ssh => {
            let address = http.strip_prefix("http://").unwrap();
            let (private_key, public_key) = a206_vault::generate_ssh_key(&nonce()).unwrap();
            let value = a206_broker::WireValue::SshKeyPair {
                private_key: a206_broker::WireSecret::new(private_key.to_string()),
                public_key: a206_broker::WireSecret::new(public_key),
            };
            (
                format!("ssh://{address}"),
                format!("ssh://git@{address}/team/repo.git"),
                private_key.to_string(),
                a206_vault::CredentialKind::SshKey,
                value,
            )
        }
    };
    let (state, credential, pairing) = broker_holding(&origin, kind, value).await;
    let workspace = tempfile::tempdir().unwrap();
    gix::init(workspace.path()).unwrap();
    let config = workspace.path().join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[user]\n\tname = n\n\temail = n@example.invalid\n[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
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
        forge: None,
    });
    let rig = Rig {
        gateway,
        _state: state,
        workspace,
        _forge_dir: forge_dir,
        forge,
        token,
        username,
        origin,
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
    // Allowing the tools on the conversation still leaves each call to the
    // user.
    for tool in [TOOL, FETCH] {
        rig.ok(Command::SetToolPermission {
            thread: rig.thread,
            tool: tool.into(),
            permission: Some(ToolPermission::Allow),
        })
        .await;
    }
    rig
}

pub fn tracking(root: &Path, branch: &str) -> Option<ObjectId> {
    gix::open(root)
        .unwrap()
        .try_find_reference(format!("refs/remotes/origin/{branch}").as_str())
        .unwrap()
        .map(|mut r| r.peel_to_id().unwrap().detach())
}
