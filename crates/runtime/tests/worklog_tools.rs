//! The bundled work queue extension and the development process in native
//! runs, through the real extension worker process.
//!
//! As in `memory_tools.rs`, the gateway starts each worker as
//! `current_exe() --data-dir <dir> extension-worker`, which here is this
//! test binary (`harness = false`); on Linux it is first re-run as the
//! sandbox helper that confines the worker. Work items, results and
//! changelog entries are generated for each run.
mod support;

use a206_core::*;
use a206_runtime::{extension_tools::tool_name, gateway::Gateway};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::path::PathBuf;
use support::*;
use tokio::sync::mpsc;

const EXTENSION: &str = "agent206.worklog";
const API: ProviderApi = ProviderApi::OpenAi;
const QUEUE: &str = "docs/roadmap/active-work.md";

// ── Entry point ────────────────────────────────────────────────────────────

type Test = fn() -> BoxFuture<'static, ()>;
const TESTS: &[(&str, Test)] = &[
    ("listing_needs_only_the_read_grant", || {
        Box::pin(listing_needs_only_the_read_grant())
    }),
    (
        "an_item_is_recorded_checked_and_closed_with_evidence",
        || Box::pin(an_item_is_recorded_checked_and_closed_with_evidence()),
    ),
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
}
fn tool(name: &str) -> String {
    tool_name(EXTENSION, name)
}

/// A project conversation in Code mode with no work queue yet, granted the
/// project and the extension with `capabilities`, its tools allowed.
async fn rig(capabilities: &[Capability]) -> Rig {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
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
    let rig = Rig {
        gateway,
        _state: state,
        workspace,
        thread: project.manager,
        requests,
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
    rig.ok(Command::SetBackend {
        thread: rig.thread,
        backend: CodingBackend::Native,
        capabilities: [Capability::ReadWorkspace, Capability::WriteWorkspace].into(),
    })
    .await;
    rig.ok(Command::GrantExtension {
        extension: EXTENSION.into(),
        capabilities: capabilities.iter().copied().collect(),
    })
    .await;
    for name in rig.offers().await {
        rig.ok(Command::SetToolPermission {
            thread: rig.thread,
            tool: name,
            permission: Some(ToolPermission::Allow),
        })
        .await;
    }
    rig
}
impl Rig {
    async fn run(&self, body: Command) -> Reply {
        request(&self.gateway, body).await
    }
    async fn ok(&self, body: Command) {
        assert_eq!(self.run(body).await, Reply::Ok);
    }
    /// The work queue tools offered to this conversation, by name.
    async fn offers(&self) -> Vec<String> {
        let Reply::Tools(list) = self
            .run(Command::ListTools {
                thread: self.thread,
            })
            .await
        else {
            panic!("tool listing failed");
        };
        let prefix = tool("x");
        let prefix = prefix.trim_end_matches('x');
        list.tools
            .into_iter()
            .filter(|t| t.name.starts_with(prefix))
            .map(|t| t.name)
            .collect()
    }
    fn path(&self, relative: &str) -> PathBuf {
        self.workspace.path().join(relative)
    }
    /// One run in which the model makes `calls` and then answers with text.
    /// Returns the run's system prompt, call records and result texts.
    async fn round_trip(&mut self, calls: &[Call]) -> (String, Vec<ToolCall>, Vec<String>) {
        let task = send(&self.gateway, self.thread);
        let first = next_request(&mut self.requests).await;
        let system = system_of(API, &first.body);
        first.respond(calls_response(API, calls, "")).await;
        let second = next_request(&mut self.requests).await;
        let results = calls
            .iter()
            .map(|call| result_for(API, &second.body, &call.id).unwrap())
            .collect();
        second.respond(text_response(API, &nonce())).await;
        let thread = finished(task).await;
        assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
        let mut records = calls_of(&thread);
        let records = records.split_off(records.len() - calls.len());
        (system, records, results)
    }
}
/// A generated one-line text.
fn line(prefix: &str) -> String {
    format!("{prefix} {}", nonce())
}

// ── Tests ───────────────────────────────────────────────────────────────────

async fn listing_needs_only_the_read_grant() {
    let mut rig = rig(&[Capability::ReadWorkspace]).await;
    assert_eq!(rig.offers().await, [tool("work_list")]);
    let title = line("title");
    std::fs::create_dir_all(rig.path("docs/roadmap")).unwrap();
    std::fs::write(
        rig.path(QUEUE),
        format!("# Active work\n\n### [ ] API-004 — {title}\n\n- **Evidence:**\n  - [ ] proof\n"),
    )
    .unwrap();
    let before = std::fs::read_to_string(rig.path(QUEUE)).unwrap();
    let calls = [
        Call::new(&tool("work_list"), json!({})),
        Call::new(&tool("work_close"), json!({"id": "API-004"})),
    ];
    let (system, records, results) = rig.round_trip(&calls).await;
    assert_eq!(records[0].state, ToolCallState::Succeeded, "{}", results[0]);
    assert!(
        results[0].starts_with(&format!("[ ] API-004 — {title} (evidence 0/1)")),
        "{}",
        results[0]
    );
    // Not offered without the write grant, so not callable either.
    assert_ne!(records[1].state, ToolCallState::Succeeded, "{}", results[1]);
    assert_eq!(std::fs::read_to_string(rig.path(QUEUE)).unwrap(), before);
    // The queue makes the full process apply.
    assert!(system.contains("# Development process"), "{system}");
    assert!(system.contains("Record before implementing"), "{system}");
}

async fn an_item_is_recorded_checked_and_closed_with_evidence() {
    let mut rig = rig(&[Capability::ReadWorkspace, Capability::WriteWorkspace]).await;
    let mut offered = rig.offers().await;
    offered.sort();
    let mut expected: Vec<String> = [
        "changelog_add",
        "work_add",
        "work_check",
        "work_close",
        "work_list",
        "work_record",
    ]
    .iter()
    .map(|t| tool(t))
    .collect();
    expected.sort();
    assert_eq!(offered, expected);

    let (title, task, proof) = (line("title"), line("task"), line("proof"));
    let record = Call::new(
        &tool("work_record"),
        json!({
            "area": "CORE",
            "title": title,
            "priority": "P1",
            "owner": line("owner"),
            "direction": line("direction"),
            "conclusion": line("conclusion"),
            "implementation": task,
            "evidence": proof,
        }),
    );
    let (system, records, results) = rig.round_trip(&[record]).await;
    assert_eq!(records[0].state, ToolCallState::Succeeded, "{}", results[0]);
    assert_eq!(results[0], format!("recorded CORE-001 in {QUEUE}"));
    // Before the queue existed, the run had the lighter process.
    assert!(system.contains("# Development process"), "{system}");
    assert!(
        system.contains("Do not create those files unasked"),
        "{system}"
    );

    let result = line("passed");
    let check = |section: &str, result: Option<&str>| {
        let mut arguments = json!({"id": "CORE-001", "section": section, "number": 1});
        if let Some(result) = result {
            arguments["result"] = json!(result);
        }
        Call::new(&tool("work_check"), arguments)
    };
    let entry = line("Outcome");
    let calls = [
        Call::new(&tool("work_close"), json!({"id": "CORE-001"})),
        check("evidence", None),
        check("implementation", None),
        check("evidence", Some(&result)),
        Call::new(&tool("changelog_add"), json!({"text": entry})),
        Call::new(
            &tool("changelog_add"),
            json!({"text": nonce(), "path": "../CHANGELOG.md"}),
        ),
    ];
    let (system, records, results) = rig.round_trip(&calls).await;
    let states: Vec<_> = records.iter().map(|r| r.state.clone()).collect();
    use ToolCallState::{Failed, Succeeded};
    assert_eq!(
        states,
        [Failed, Failed, Succeeded, Succeeded, Succeeded, Failed],
        "{results:?}"
    );
    assert!(results[0].contains("unchecked"), "{}", results[0]);
    assert!(results[1].contains("result"), "{}", results[1]);
    // Every box is checked now, so the item closes.
    let close = Call::new(&tool("work_close"), json!({"id": "CORE-001"}));
    let (_, records, results) = rig.round_trip(&[close]).await;
    assert_eq!(records[0].state, ToolCallState::Succeeded, "{}", results[0]);
    let queue = std::fs::read_to_string(rig.path(QUEUE)).unwrap();
    assert!(
        queue.contains(&format!("### [x] CORE-001 — {title}\n")),
        "{queue}"
    );
    assert!(queue.contains(&format!("  - [x] {task}\n")), "{queue}");
    assert!(
        queue.contains(&format!("  - [x] {proof} — {result}\n")),
        "{queue}"
    );
    assert_eq!(
        std::fs::read_to_string(rig.path("CHANGELOG.md")).unwrap(),
        format!("# Changelog\n\n## Unreleased\n\n- {entry}\n")
    );
    assert!(system.contains("Record before implementing"), "{system}");
}
