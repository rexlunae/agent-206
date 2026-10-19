#![cfg(unix)]
//! A model reads pull requests with `pull_requests` and `pull_request`
//! through the vault broker, which alone holds the forge token. The forge is
//! a stand-in REST API on loopback that answers as GitHub or GitLab does;
//! every identifier and answer is generated at runtime.
mod support;

use a206_core::*;
use axum::{
    Router,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode, header},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use support::*;

type Gateway = a206_runtime::gateway::Gateway;
const API: ProviderApi = ProviderApi::OpenAi;
const LIST: &str = a206_runtime::forge::LIST;
const READ: &str = a206_runtime::forge::READ;

/// A forge API that answers each request path (with its query) from
/// `answers`, and 404 otherwise.
#[derive(Clone, Default)]
struct Api {
    answers: Arc<Mutex<HashMap<String, (StatusCode, Value)>>>,
    /// The `Authorization` header and path of each request.
    seen: Arc<Mutex<Vec<(String, String)>>>,
}
impl Api {
    fn answer(&self, path: &str, status: StatusCode, body: Value) {
        self.answers
            .lock()
            .unwrap()
            .insert(path.to_owned(), (status, body));
    }
    fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path)| path.clone())
            .collect()
    }
}

async fn answer_request(
    State(api): State<Api>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> (StatusCode, HeaderMap, String) {
    let path = uri.path_and_query().unwrap().to_string();
    let auth = headers
        .get(header::AUTHORIZATION)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    // Forge APIs refuse clients that do not name themselves.
    assert!(headers.contains_key(header::USER_AGENT));
    api.seen.lock().unwrap().push((auth, path.clone()));
    let mut out = HeaderMap::new();
    out.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    match api.answers.lock().unwrap().get(&path) {
        Some((status, body)) => (*status, out, body.to_string()),
        None => (StatusCode::NOT_FOUND, out, "{}".into()),
    }
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

struct Rig {
    gateway: Gateway,
    _state: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    api: Api,
    origin: String,
    token: String,
    credential: GitCredential,
    requests: tokio::sync::mpsc::Receiver<Opened>,
    thread: Id,
    /// The repository's path on the forge.
    owner: String,
    name: String,
}
impl Rig {
    async fn ok(&self, command: Command) {
        assert_eq!(request(&self.gateway, command).await, Reply::Ok);
    }
    /// Point the git credential at an API of `kind` under `path`.
    async fn forge(&mut self, forge: Option<(ForgeKind, &str)>) {
        self.credential.forge = forge.map(|(kind, path)| ForgeApi {
            kind,
            api: format!("{}{path}", self.origin),
        });
        let mut settings = self.gateway.snapshot().unwrap().settings;
        settings.git_credentials = vec![self.credential.clone()];
        self.ok(Command::SaveSettings(Box::new(settings))).await;
    }
    /// One run in which the model calls `tool` with `arguments`; the call's
    /// record and result.
    async fn call(&mut self, tool: &str, arguments: Value) -> (ToolCall, Option<String>) {
        let task = send(&self.gateway, self.thread);
        let first = next_request(&mut self.requests).await;
        assert!(offered(API, &first.body).contains(&tool.to_owned()));
        let call = Call::new(tool, arguments);
        first
            .respond(calls_response(API, std::slice::from_ref(&call), ""))
            .await;
        let second = next_request(&mut self.requests).await;
        let result = result_for(API, &second.body, &call.id);
        assert!(!second.body.to_string().contains(&self.token));
        second.respond(text_response(API, &nonce())).await;
        let thread = finished(task).await;
        (calls_of(&thread).last().unwrap().clone(), result)
    }
}

/// A project whose `origin` remote is a repository on a new stand-in forge,
/// with a git credential for it; a conversation granted git reading and
/// permitted the pull request tools.
async fn rig() -> Rig {
    let api = Api::default();
    let router = Router::new()
        .fallback(answer_request)
        .with_state(api.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    // Detached: the server lives as long as the test's runtime.
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let token = nonce();
    let (state, credential, pairing) = broker(&origin, &token).await;
    let (owner, name) = (nonce(), nonce());
    let workspace = tempfile::tempdir().unwrap();
    gix::init(workspace.path()).unwrap();
    let config = workspace.path().join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[remote \"origin\"]\n\turl = {origin}/{owner}/{name}.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
    ));
    std::fs::write(&config, text).unwrap();

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
    let credential = GitCredential {
        origin: origin.clone(),
        username: nonce(),
        credential,
        forge: None,
    };
    let mut rig = Rig {
        gateway,
        _state: state,
        _workspace: workspace,
        api,
        origin,
        token,
        credential,
        requests,
        thread: project.manager,
        owner,
        name,
    };
    rig.forge(Some((ForgeKind::GitHub, "/api/v3"))).await;
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
        capabilities: [Capability::GitRead].into(),
    })
    .await;
    for tool in [LIST, READ] {
        rig.ok(Command::SetToolPermission {
            thread: rig.thread,
            tool: tool.into(),
            permission: Some(ToolPermission::Allow),
        })
        .await;
    }
    rig
}

fn sha() -> String {
    format!("{}{}", nonce(), &nonce()[..8])
        .chars()
        .take(40)
        .collect()
}

fn result(text: Option<String>) -> Value {
    serde_json::from_str(&text.unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_reads_github_pull_requests_with_their_checks_and_reviews() {
    let mut rig = rig().await;
    let repo = format!("/api/v3/repos/{}/{}", rig.owner, rig.name);
    let branch = format!("topic/{}", nonce());
    let (head, title, reviewer) = (sha(), nonce(), nonce());
    let pull = json!({
        "number": 7,
        "title": title,
        "state": "open",
        "draft": false,
        "merged_at": null,
        "user": {"login": nonce()},
        "head": {"ref": branch, "sha": head},
        "base": {"ref": nonce(), "sha": sha()},
        "html_url": format!("{}/{}", rig.origin, nonce()),
        "body": nonce(),
        "created_at": nonce(),
        "updated_at": nonce(),
    });
    let listed = format!(
        "{repo}/pulls?state=open&per_page={}&head={}%3A{}",
        a206_forge::PAGE,
        rig.owner,
        branch.replace('/', "%2F")
    );
    rig.api.answer(&listed, StatusCode::OK, json!([pull]));

    let (record, text) = rig.call(LIST, json!({"branch": branch})).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let answer = result(text);
    assert_eq!(answer["forge"], "GitHub");
    assert_eq!(answer["repository"], format!("{}/{}", rig.owner, rig.name));
    assert_eq!(answer["complete"], true);
    assert_eq!(answer["pull_requests"][0]["number"], 7);
    assert_eq!(answer["pull_requests"][0]["title"], title);
    assert_eq!(answer["pull_requests"][0]["source"], branch);
    // Lists leave descriptions out.
    assert!(answer["pull_requests"][0].get("body").is_none());
    // The broker sent the token as a bearer token; the model never saw it.
    {
        let seen = rig.api.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, format!("Bearer {}", rig.token));
    }

    // One pull request, its check runs, commit statuses and reviews.
    let per = a206_forge::DETAIL_PAGE;
    rig.api
        .answer(&format!("{repo}/pulls/7"), StatusCode::OK, pull.clone());
    rig.api.answer(
        &format!("{repo}/commits/{head}/check-runs?per_page={per}"),
        StatusCode::OK,
        json!({"total_count": 1, "check_runs": [
            {"name": "build", "status": "completed", "conclusion": "failure", "html_url": null}
        ]}),
    );
    rig.api.answer(
        &format!("{repo}/commits/{head}/status?per_page={per}"),
        StatusCode::OK,
        json!({"state": "pending", "statuses": [{"context": "ci/legacy", "state": "pending"}]}),
    );
    rig.api.answer(
        &format!("{repo}/pulls/7/reviews?per_page={per}"),
        StatusCode::OK,
        json!([{"user": {"login": reviewer}, "state": "CHANGES_REQUESTED", "commit_id": head}]),
    );
    let (record, text) = rig.call(READ, json!({"number": 7})).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let answer = result(text);
    assert_eq!(answer["pull_request"]["body"], pull["body"]);
    assert_eq!(answer["pull_request"]["head"], head);
    assert_eq!(answer["checks"][0]["name"], "build");
    assert_eq!(answer["checks"][0]["conclusion"], "failure");
    assert_eq!(answer["checks"][1]["status"], "running");
    assert_eq!(answer["reviews"][0]["author"], reviewer);
    assert_eq!(answer["reviews"][0]["state"], "changes_requested");
    assert_eq!(answer["checks_complete"], true);
    assert_eq!(answer["reviews_complete"], true);

    // A pull request the forge does not have, and a refused credential.
    let (record, _) = rig.call(READ, json!({"number": 8})).await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::NotFound);
    rig.api.answer(
        &format!("{repo}/pulls/9"),
        StatusCode::UNAUTHORIZED,
        json!({}),
    );
    let (record, _) = rig.call(READ, json!({"number": 9})).await;
    assert_eq!(error_kind(&record), ErrorKind::PermissionDenied);
    // An answer not shaped like the API's is an error, not an empty result.
    rig.api.answer(
        &format!("{repo}/pulls/10"),
        StatusCode::OK,
        json!({"id": 1}),
    );
    let (record, _) = rig.call(READ, json!({"number": 10})).await;
    assert_eq!(error_kind(&record), ErrorKind::Protocol);
    // Bad arguments are refused before the forge is asked.
    let asked = rig.api.paths().len();
    for arguments in [
        json!({"number": 0}),
        json!({"number": -3}),
        json!({"number": 7, "other": 1}),
    ] {
        let (record, _) = rig.call(READ, arguments).await;
        assert_eq!(error_kind(&record), ErrorKind::InvalidRequest);
    }
    let (record, _) = rig.call(LIST, json!({"state": nonce()})).await;
    assert_eq!(error_kind(&record), ErrorKind::InvalidRequest);
    assert_eq!(rig.api.paths().len(), asked);
}

#[tokio::test(flavor = "multi_thread")]
async fn gitlab_merge_requests_read_the_same_way_and_unknown_forges_are_refused() {
    let mut rig = rig().await;
    rig.forge(Some((ForgeKind::GitLab, "/api/v4"))).await;
    let project = format!("/api/v4/projects/{}%2F{}", rig.owner, rig.name);
    let (head, approver) = (sha(), nonce());
    let pipeline = 100 + u64::from(nonce().as_bytes()[0]);
    rig.api.answer(
        &format!("{project}/merge_requests/3"),
        StatusCode::OK,
        json!({
            "iid": 3,
            "title": nonce(),
            "state": "merged",
            "draft": true,
            "author": {"username": nonce()},
            "source_branch": nonce(),
            "target_branch": nonce(),
            "sha": head,
            "web_url": nonce(),
            "description": nonce(),
            "head_pipeline": {"id": pipeline, "status": "success"},
        }),
    );
    rig.api.answer(
        &format!(
            "{project}/pipelines/{pipeline}/jobs?per_page={}",
            a206_forge::DETAIL_PAGE
        ),
        StatusCode::OK,
        json!([{"name": "test", "status": "success"}, {"name": "lint", "status": "failed", "allow_failure": true}]),
    );
    rig.api.answer(
        &format!("{project}/merge_requests/3/approvals"),
        StatusCode::OK,
        json!({"approved_by": [{"user": {"username": approver}}]}),
    );
    let (record, text) = rig.call(READ, json!({"number": 3})).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let answer = result(text);
    assert_eq!(answer["forge"], "GitLab");
    assert_eq!(answer["pull_request"]["state"], "merged");
    assert_eq!(answer["pull_request"]["draft"], true);
    assert_eq!(answer["checks"][0]["conclusion"], "success");
    assert_eq!(answer["checks"][1]["conclusion"], "neutral");
    assert_eq!(answer["reviews"][0]["author"], approver);
    assert_eq!(answer["reviews"][0]["state"], "approved");

    // A forge whose API is not set (and no preset covers loopback) is
    // refused without asking anything.
    rig.forge(None).await;
    let asked = rig.api.paths().len();
    let (record, _) = rig.call(LIST, json!({})).await;
    assert_eq!(record.state, ToolCallState::Failed);
    assert_eq!(error_kind(&record), ErrorKind::InvalidRequest);
    assert_eq!(rig.api.paths().len(), asked);
}

/// Text longer than the forge crate keeps of a `max`-character field, in
/// characters that take the most room in a JSON result (escaped quotes and
/// three-byte characters).
fn widest(max: usize) -> String {
    format!("{}{}", nonce(), "\"\u{20ac}".repeat(max))
}
fn wide() -> String {
    widest(a206_forge::MAX_LABEL)
}

/// Full pages of the longest fields would exceed the room for one tool
/// result. The answer stays whole JSON within it, keeps the first entries,
/// and says which lists were cut.
#[tokio::test(flavor = "multi_thread")]
async fn the_largest_answers_are_cut_by_entry_and_say_so() {
    let mut rig = rig().await;
    let repo = format!("/api/v3/repos/{}/{}", rig.owner, rig.name);
    let per = a206_forge::DETAIL_PAGE;
    let head = sha();
    let pull = |number: usize| {
        json!({
            "number": number,
            "title": wide(),
            "state": "open",
            "user": {"login": wide()},
            "head": {"ref": wide(), "sha": head},
            "base": {"ref": wide(), "sha": wide()},
            "html_url": wide(),
            "body": widest(a206_forge::MAX_BODY),
            "created_at": wide(),
            "updated_at": wide(),
        })
    };
    rig.api.answer(
        &format!("{repo}/pulls?state=open&per_page={}", a206_forge::PAGE),
        StatusCode::OK,
        Value::Array((1..=a206_forge::PAGE).map(pull).collect()),
    );
    rig.api
        .answer(&format!("{repo}/pulls/1"), StatusCode::OK, pull(1));
    let runs: Vec<Value> = (0..per)
        .map(|_| json!({"name": wide(), "status": "completed", "conclusion": wide(), "html_url": wide()}))
        .collect();
    rig.api.answer(
        &format!("{repo}/commits/{head}/check-runs?per_page={per}"),
        StatusCode::OK,
        json!({"total_count": per, "check_runs": runs}),
    );
    rig.api.answer(
        &format!("{repo}/commits/{head}/status?per_page={per}"),
        StatusCode::OK,
        json!({"state": "pending", "statuses": []}),
    );
    rig.api.answer(
        &format!("{repo}/pulls/1/reviews?per_page={per}"),
        StatusCode::OK,
        Value::Array(
            (0..per)
                .map(|_| json!({"user": {"login": wide()}, "state": wide(), "submitted_at": wide(), "commit_id": wide()}))
                .collect(),
        ),
    );

    let limit = a206_runtime::tools::MAX_OUTPUT;
    let (record, text) = rig.call(LIST, json!({})).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let text = text.unwrap();
    assert!(text.len() <= limit);
    let answer: Value = serde_json::from_str(&text).unwrap();
    let listed = answer["pull_requests"].as_array().unwrap();
    assert!(!listed.is_empty() && listed.len() < a206_forge::PAGE);
    assert_eq!(listed[0]["number"], 1);
    assert_eq!(answer["complete"], false);

    let (record, text) = rig.call(READ, json!({"number": 1})).await;
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let text = text.unwrap();
    assert!(text.len() <= limit);
    let answer: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(answer["pull_request"]["body_shortened"], true);
    let checks = answer["checks"].as_array().unwrap();
    assert!(!checks.is_empty() && checks.len() < per);
    assert_eq!(answer["checks_complete"], false);
    // Reviews share what room is left.
    assert!(answer["reviews"].as_array().unwrap().len() < per);
    assert_eq!(answer["reviews_complete"], false);
}
