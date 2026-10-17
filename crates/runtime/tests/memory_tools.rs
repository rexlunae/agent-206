//! The bundled memory extension in native runs, through the real extension
//! worker process.
//!
//! As in `extension_tools.rs`, the gateway starts each worker as
//! `current_exe() --data-dir <dir> extension-worker`, which here is this
//! test binary (`harness = false`); on Linux it is first re-run as the
//! sandbox helper that confines the worker. Workspace files, terms and notes
//! are generated for each run.
mod support;

use a206_core::*;
use a206_runtime::{extension_tools::tool_name, gateway::Gateway};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::path::{Path, PathBuf};
use support::*;
use tokio::sync::mpsc;

const EXTENSION: &str = "agent206.memory";
const API: ProviderApi = ProviderApi::OpenAi;

// ── Entry point ────────────────────────────────────────────────────────────

type Test = fn() -> BoxFuture<'static, ()>;
const TESTS: &[(&str, Test)] = &[
    ("memory_tools_are_inert_until_the_user_grants_them", || {
        Box::pin(memory_tools_are_inert_until_the_user_grants_them())
    }),
    ("search_and_get_reach_only_memory_files", || {
        Box::pin(search_and_get_reach_only_memory_files())
    }),
    ("saving_needs_the_separate_write_grant", || {
        Box::pin(saving_needs_the_separate_write_grant())
    }),
];

fn main() {
    if let Some(result) = a206_sandbox::dispatch() {
        result.expect("sandbox helper could not report to the gateway");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "extension-worker") {
        a206_extensions::worker::serve(std::io::stdin(), std::io::stdout())
            .expect("the worker channel failed");
        return;
    }
    if args.iter().any(|a| a == "--list") {
        for (name, _) in TESTS {
            println!("{name}: test");
        }
        return;
    }
    let filter = args.iter().skip(1).find(|a| !a.starts_with('-'));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut ran = 0;
    for (name, test) in TESTS {
        if filter.is_some_and(|f| !name.contains(f.as_str())) {
            continue;
        }
        println!("test {name} ...");
        // A failing test panics, which fails this binary.
        runtime.block_on(test());
        println!("test {name} ... ok");
        ran += 1;
    }
    println!("test result: ok. {ran} passed");
}

// ── Rig ─────────────────────────────────────────────────────────────────────

struct Rig {
    gateway: Gateway,
    _state: tempfile::TempDir,
    workspace: tempfile::TempDir,
    thread: Id,
    requests: mpsc::Receiver<Opened>,
    /// A note under `memory/`, the term planted in it and that line's number.
    note: (String, String, usize),
    /// A file outside memory and the term only it holds.
    outside: (String, String),
}
fn search() -> String {
    tool_name(EXTENSION, "memory_search")
}
fn get() -> String {
    tool_name(EXTENSION, "memory_get")
}
fn save() -> String {
    tool_name(EXTENSION, "memory_save")
}
/// Generated lines of generated words.
fn lines(count: usize) -> Vec<String> {
    (0..count)
        .map(|_| {
            (0..6)
                .map(|_| nonce()[..7].to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// A project conversation in Code mode with a generated `MEMORY.md`, a note
/// under `memory/` and a file outside it. Nothing is granted yet.
async fn rig() -> Rig {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    std::fs::write(root.join("MEMORY.md"), lines(12).join("\n")).unwrap();
    let term = format!("term{}", nonce());
    let mut body = lines(30);
    let planted = 25;
    body[planted - 1] = format!("{} {term} {}", nonce(), nonce());
    let note = format!("memory/{}/{}.md", nonce(), nonce());
    std::fs::create_dir_all(root.join(&note).parent().unwrap()).unwrap();
    std::fs::write(root.join(&note), body.join("\n")).unwrap();
    let secret = format!("secret{}", nonce());
    let outside = format!("src/{}.md", nonce());
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(&outside), format!("{secret}\n")).unwrap();

    let gateway = Gateway::open(state.path()).unwrap();
    let (model, requests) = connect(&gateway, API).await;
    let Reply::Project(project) = request(
        &gateway,
        Command::CreateProject {
            name: nonce(),
            root: root.display().to_string(),
        },
    )
    .await
    else {
        panic!("project creation failed");
    };
    let rig = Rig {
        gateway,
        _state: state,
        workspace,
        thread: project.manager,
        requests,
        note: (note, term, planted),
        outside: (outside, secret),
    };
    let configured = rig
        .run(Command::ConfigureThread {
            id: rig.thread,
            mode: Mode::Code,
            model: Some(model.clone()),
            subagent_models: [model].into(),
        })
        .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    rig
}
impl Rig {
    async fn run(&self, body: Command) -> Reply {
        request(&self.gateway, body).await
    }
    async fn ok(&self, body: Command) {
        assert_eq!(self.run(body).await, Reply::Ok);
    }
    async fn grant_thread(&self, capabilities: &[Capability]) {
        self.ok(Command::SetBackend {
            thread: self.thread,
            backend: CodingBackend::Native,
            capabilities: capabilities.iter().copied().collect(),
        })
        .await;
    }
    async fn grant_extension(&self, capabilities: &[Capability]) {
        self.ok(Command::GrantExtension {
            extension: EXTENSION.into(),
            capabilities: capabilities.iter().copied().collect(),
        })
        .await;
    }
    async fn permit(&self, tools: &[String]) {
        for tool in tools {
            self.ok(Command::SetToolPermission {
                thread: self.thread,
                tool: tool.clone(),
                permission: Some(ToolPermission::Allow),
            })
            .await;
        }
    }
    /// The memory tools listed for this conversation, by name.
    async fn offers(&self) -> Vec<ToolOffer> {
        let Reply::Tools(list) = self
            .run(Command::ListTools {
                thread: self.thread,
            })
            .await
        else {
            panic!("tool listing failed");
        };
        assert!(
            list.unavailable
                .iter()
                .all(|u| u.source == a206_runtime::exec_tool::SOURCE),
            "{:?}",
            list.unavailable
        );
        let prefix = tool_name(EXTENSION, "x");
        let prefix = prefix.trim_end_matches('x');
        list.tools
            .into_iter()
            .filter(|t| t.name.starts_with(prefix))
            .collect()
    }
    fn path(&self, relative: &str) -> PathBuf {
        self.workspace.path().join(relative)
    }
    /// One run in which the model makes `calls` and then answers with text.
    /// Returns this run's call records and the result text of each call.
    async fn round_trip(&mut self, calls: &[Call]) -> (Vec<ToolCall>, Vec<String>) {
        let task = send(&self.gateway, self.thread);
        let first = next_request(&mut self.requests).await;
        first.respond(calls_response(API, calls, "")).await;
        let second = next_request(&mut self.requests).await;
        let results = calls
            .iter()
            .map(|call| result_for(API, &second.body, &call.id).unwrap())
            .collect();
        second.respond(text_response(API, &nonce())).await;
        let thread = finished(task).await;
        assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
        // The thread keeps earlier runs' calls; this run's come last.
        let mut records = calls_of(&thread);
        let records = records.split_off(records.len() - calls.len());
        (records, results)
    }
}
fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

// ── Tests ───────────────────────────────────────────────────────────────────

async fn memory_tools_are_inert_until_the_user_grants_them() {
    let mut rig = rig().await;
    rig.grant_thread(&[Capability::ReadWorkspace, Capability::WriteWorkspace])
        .await;
    // The conversation allows the tools, but the user granted the
    // extension nothing, so none is offered or callable.
    rig.permit(&[search(), get(), save()]).await;
    assert!(rig.offers().await.is_empty());
    let summary = rig
        .gateway
        .snapshot()
        .unwrap()
        .plugins
        .into_iter()
        .find(|p| p.id == EXTENSION)
        .expect("the memory extension is bundled");
    assert!(summary.granted.is_empty());
    assert_eq!(
        summary.capabilities,
        [Capability::ReadWorkspace, Capability::WriteWorkspace].into()
    );
    let (note, term, _) = rig.note.clone();
    let created = format!("memory/{}.md", nonce());
    let calls = [
        Call::new(&search(), json!({"query": term})),
        Call::new(&get(), json!({"path": note})),
        Call::new(&save(), json!({"path": created, "text": nonce()})),
    ];
    let (records, results) = rig.round_trip(&calls).await;
    for (record, result) in records.iter().zip(&results) {
        assert_eq!(record.state, ToolCallState::Denied, "{record:?}");
        assert!(!result.contains(&term), "memory leaked: {result}");
    }
    assert!(read(&rig.path(&created)).is_none());
}

async fn search_and_get_reach_only_memory_files() {
    let mut rig = rig().await;
    rig.grant_thread(&[Capability::ReadWorkspace, Capability::WriteWorkspace])
        .await;
    // Reading only: saving stays unavailable without the write grant.
    rig.grant_extension(&[Capability::ReadWorkspace]).await;
    let offers = rig.offers().await;
    let names: Vec<&str> = offers.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&search().as_str()) && names.contains(&get().as_str()));
    for offer in &offers {
        // Denied until this conversation permits it.
        assert_eq!(offer.effective, ToolPermission::Deny);
        assert_eq!(offer.capabilities, [Capability::ReadWorkspace].into());
    }
    rig.permit(&[search(), get()]).await;

    let (note, term, planted) = rig.note.clone();
    let (outside, secret) = rig.outside.clone();
    let body = read(&rig.path(&note)).unwrap();
    let body: Vec<&str> = body.lines().collect();
    let calls = [
        Call::new(
            &search(),
            json!({"query": term.to_uppercase(), "max_results": 3}),
        ),
        Call::new(
            &get(),
            json!({"path": note, "from": planted - 1, "lines": 3}),
        ),
        Call::new(&get(), json!({"path": outside})),
        Call::new(&get(), json!({"path": format!("memory/../{outside}")})),
        Call::new(&search(), json!({"query": secret})),
    ];
    let (records, results) = rig.round_trip(&calls).await;

    // BM25 over memory files found the section holding the term (lines
    // 21-30 of the note), case-insensitively, with a snippet.
    assert_eq!(records[0].state, ToolCallState::Succeeded);
    let first = results[0].split("\n\n").next().unwrap();
    assert!(
        first.starts_with(&format!("{note}:21-30 (score ")),
        "{}",
        results[0]
    );
    assert!(first.contains(&term), "{}", results[0]);

    assert_eq!(records[1].state, ToolCallState::Succeeded);
    assert_eq!(
        results[1],
        format!(
            "{note}:{}-{} of {} lines\n{}",
            planted - 1,
            planted + 1,
            body.len(),
            body[planted - 2..planted + 1].join("\n")
        )
    );

    for (record, result) in records[2..4].iter().zip(&results[2..4]) {
        assert_eq!(record.state, ToolCallState::Failed, "{record:?}");
        assert!(
            result.contains("is not MEMORY.md or a note under memory/")
                || result.contains("component"),
            "{result}"
        );
        assert!(!result.contains(&secret), "the file leaked: {result}");
    }
    // Files outside memory are not searched.
    assert_eq!(records[4].state, ToolCallState::Succeeded);
    assert!(
        results[4].starts_with("no matches in 2 memory files"),
        "{}",
        results[4]
    );
    assert!(!results[4].contains(&secret));
}

async fn saving_needs_the_separate_write_grant() {
    let mut rig = rig().await;
    let created = format!("memory/{}/{}.md", nonce(), nonce());
    let text = format!("decided{} {}", nonce(), nonce());
    let attempt = || Call::new(&save(), json!({"path": created, "text": text}));

    // The user granted reading only: saving is neither offered nor run,
    // even though the conversation could write.
    rig.grant_thread(&[Capability::ReadWorkspace, Capability::WriteWorkspace])
        .await;
    rig.grant_extension(&[Capability::ReadWorkspace]).await;
    rig.permit(&[save()]).await;
    assert!(rig.offers().await.iter().all(|o| o.name != save()));
    let (records, _) = rig.round_trip(&[attempt()]).await;
    assert_eq!(records[0].state, ToolCallState::Denied);
    // Not offered at all, so the gateway does not know the tool.
    assert_eq!(error_kind(&records[0]), ErrorKind::NotFound);
    assert!(read(&rig.path(&created)).is_none());

    // The user granted writing, but this conversation may not write.
    rig.grant_extension(&[Capability::ReadWorkspace, Capability::WriteWorkspace])
        .await;
    rig.grant_thread(&[Capability::ReadWorkspace]).await;
    let offer = rig
        .offers()
        .await
        .into_iter()
        .find(|o| o.name == save())
        .expect("saving is offered once the user grants writing");
    assert_eq!(
        offer.capabilities,
        [Capability::ReadWorkspace, Capability::WriteWorkspace].into()
    );
    assert_eq!(offer.missing, [Capability::WriteWorkspace].into());
    assert_eq!(offer.effective, ToolPermission::Deny);
    let (records, _) = rig.round_trip(&[attempt()]).await;
    let record = &records[0];
    assert_eq!(record.state, ToolCallState::Denied);
    assert_eq!(error_kind(record), ErrorKind::PermissionDenied);
    assert!(read(&rig.path(&created)).is_none());

    // Both grants: notes are created and appended under memory/ only.
    rig.grant_thread(&[Capability::ReadWorkspace, Capability::WriteWorkspace])
        .await;
    rig.permit(&[search()]).await;
    let memory = read(&rig.path("MEMORY.md")).unwrap();
    let (outside, _) = rig.outside.clone();
    let more = nonce();
    let calls = [
        attempt(),
        Call::new(&save(), json!({"path": created, "text": more})),
        Call::new(&save(), json!({"path": "MEMORY.md", "text": nonce()})),
        Call::new(&save(), json!({"path": outside, "text": nonce()})),
        Call::new(
            &save(),
            json!({"path": format!("{}.md", nonce()), "text": nonce()}),
        ),
    ];
    let (records, results) = rig.round_trip(&calls).await;
    assert_eq!(records[0].state, ToolCallState::Succeeded, "{}", results[0]);
    assert_eq!(results[0], format!("created {created}, lines 1-1"));
    assert_eq!(results[1], format!("appended to {created}, lines 2-2"));
    assert_eq!(read(&rig.path(&created)), Some(format!("{text}\n{more}\n")));
    for (record, result) in records[2..].iter().zip(&results[2..]) {
        assert_eq!(record.state, ToolCallState::Failed, "{result}");
    }
    assert_eq!(read(&rig.path("MEMORY.md")), Some(memory));
    assert_eq!(read(&rig.path(&outside)).unwrap().lines().count(), 1);
    let stray = calls[4].arguments.clone();
    let stray: serde_json::Value = serde_json::from_str(&stray).unwrap();
    assert!(read(&rig.path(stray["path"].as_str().unwrap())).is_none());

    // The saved note is searchable.
    let word = text.split(' ').next().unwrap().to_string();
    let (_, results) = rig
        .round_trip(&[Call::new(&search(), json!({"query": word}))])
        .await;
    assert!(
        results[0].starts_with(&format!("{created}:1-2 (score ")),
        "{}",
        results[0]
    );
}
