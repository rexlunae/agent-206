//! Bundled WASM extension tools in native runs, through the real extension
//! worker process.
//!
//! The gateway starts each extension worker as `current_exe() --data-dir
//! <dir> extension-worker`. Here that executable is this test binary, so it
//! has its own `main` (`harness = false`): with those arguments it serves the
//! invocation exactly as the `agent206` binary does, otherwise it runs the
//! tests below. On Linux the gateway confines the worker by re-running this
//! binary as its sandbox helper first, so `main` dispatches that too.
//!
//! A tool call whose arguments carry the `STALL` marker makes the worker
//! accept its invocation and then keep computing, standing in for a guest
//! that has not finished, so cancellation can be observed. The worker
//! announces that in its own data directory, which on Linux is the
//! sandbox's scratch directory rather than the gateway's.
mod support;

use a206_core::*;
use a206_runtime::{extension_tools::tool_name, gateway::Gateway};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::{path::Path, time::Duration};
use support::*;
use tokio::sync::mpsc;

const EXTENSION: &str = "agent206.workspace";
const READ: &str = "read_text_file";
const STALL: &str = "stall-extension-worker";
const STALLED: &str = "stalled-extension-worker.pid";

// ── Entry point ────────────────────────────────────────────────────────────

type Test = fn() -> BoxFuture<'static, ()>;
const TESTS: &[(&str, Test)] = &[
    ("a_permitted_extension_tool_runs_in_a_native_run", || {
        Box::pin(a_permitted_extension_tool_runs_in_a_native_run())
    }),
    (
        "argument_errors_return_to_the_model_as_typed_failures",
        || Box::pin(argument_errors_return_to_the_model_as_typed_failures()),
    ),
    ("the_conversation_grant_caps_the_extension", || {
        Box::pin(the_conversation_grant_caps_the_extension())
    }),
    ("disabled_or_ungranted_extensions_are_not_offered", || {
        Box::pin(disabled_or_ungranted_extensions_are_not_offered())
    }),
    ("withdrawing_the_extension_stops_its_running_call", || {
        Box::pin(withdrawing_the_extension_stops_its_running_call())
    }),
    (
        "cancelling_during_an_extension_call_kills_the_worker",
        || Box::pin(cancelling_during_an_extension_call_kills_the_worker()),
    ),
];

fn main() {
    if let Some(result) = a206_sandbox::dispatch() {
        result.expect("sandbox helper could not report to the gateway");
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "extension-worker") {
        return worker(&args);
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

fn worker(args: &[String]) {
    let data_dir = args
        .iter()
        .position(|a| a == "--data-dir")
        .and_then(|i| args.get(i + 1))
        .map(Path::new)
        .expect("the gateway passes its data directory");
    let mut input = std::io::stdin();
    let start: a206_extensions::worker::Start =
        a206_extensions::worker::read_frame(&mut input).unwrap();
    if format!("{:?}", start.invocation).contains(STALL) {
        let pending = data_dir.join(format!("{STALLED}.tmp"));
        std::fs::write(&pending, std::process::id().to_string()).unwrap();
        std::fs::rename(pending, data_dir.join(STALLED)).unwrap();
        // A guest still computing: only being killed ends this early.
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    // Serve the invocation already read, as if it had not been.
    let frame = a206_protocol::encode(&start).unwrap();
    let replayed = std::io::Read::chain(std::io::Cursor::new(frame), input);
    a206_extensions::worker::serve(replayed, std::io::stdout()).unwrap();
}

// ── Rig ─────────────────────────────────────────────────────────────────────

const API: ProviderApi = ProviderApi::OpenAi;
struct Rig {
    gateway: Gateway,
    state: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    thread: Id,
    requests: mpsc::Receiver<Opened>,
    /// The name of a file in the project and its generated contents.
    file: (String, String),
    tool: String,
}
/// A project conversation in Plan mode granted `read_workspace`, with the
/// workspace extension granted `read_workspace` by the user.
async fn rig() -> Rig {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let file = (format!("{}.txt", nonce()), nonce());
    std::fs::write(workspace.path().join(&file.0), &file.1).unwrap();
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
        state,
        _workspace: workspace,
        thread: project.manager,
        requests,
        file,
        tool: tool_name(EXTENSION, READ),
    };
    let configured = rig
        .run(Command::ConfigureThread {
            id: rig.thread,
            mode: Mode::Plan,
            model: Some(model.clone()),
            subagent_models: [model].into(),
        })
        .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    rig.grant_thread(&[Capability::ReadWorkspace]).await;
    rig.grant_extension(&[Capability::ReadWorkspace]).await;
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
    async fn permit(&self, permission: ToolPermission) {
        self.ok(Command::SetToolPermission {
            thread: self.thread,
            tool: self.tool.clone(),
            permission: Some(permission),
        })
        .await;
    }
    async fn offer(&self) -> Option<ToolOffer> {
        let Reply::Tools(list) = self
            .run(Command::ListTools {
                thread: self.thread,
            })
            .await
        else {
            panic!("tool listing failed");
        };
        // Native exec is registered alongside extensions, but deliberately
        // unavailable until equivalent isolation exists on this platform.
        // Still reject any unavailable extension or unexpected host failure.
        if cfg!(target_os = "linux") {
            assert!(list.unavailable.is_empty(), "{:?}", list.unavailable);
        } else {
            assert_eq!(list.unavailable.len(), 1, "{:?}", list.unavailable);
            assert_eq!(list.unavailable[0].source, a206_runtime::exec_tool::SOURCE);
            assert_eq!(
                list.unavailable[0].error.kind,
                a206_core::ErrorKind::Unavailable
            );
        }
        list.tools.into_iter().find(|t| t.name == self.tool)
    }
    fn send(&self) -> tokio::task::JoinHandle<Reply> {
        send(&self.gateway, self.thread)
    }
    async fn next_request(&mut self) -> Opened {
        next_request(&mut self.requests).await
    }
    fn thread(&self) -> Thread {
        self.gateway
            .snapshot()
            .unwrap()
            .threads
            .into_iter()
            .find(|t| t.id == self.thread)
            .unwrap()
    }
    /// One run in which the model makes `calls` and then answers with text.
    /// Returns the finished thread and the results the model was sent.
    async fn round_trip(&mut self, calls: &[Call]) -> (Thread, Vec<Option<String>>) {
        let task = self.send();
        let first = self.next_request().await;
        first.respond(calls_response(API, calls, "")).await;
        let second = self.next_request().await;
        let results = calls
            .iter()
            .map(|call| result_for(API, &second.body, &call.id))
            .collect();
        second.respond(text_response(API, &nonce())).await;
        (finished(task).await, results)
    }
}
fn tool_schema(body: &serde_json::Value, name: &str) -> serde_json::Value {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == name)
        .map(|t| t["function"]["parameters"].clone())
        .unwrap()
}

// ── Tests ───────────────────────────────────────────────────────────────────

async fn a_permitted_extension_tool_runs_in_a_native_run() {
    let mut rig = rig().await;
    // Available, but denied until the user permits it in this conversation.
    let offer = rig.offer().await.expect("the extension tool is listed");
    assert_eq!(offer.permission, None);
    assert_eq!(offer.effective, ToolPermission::Deny);
    assert_eq!(offer.capabilities, [Capability::ReadWorkspace].into());
    assert!(offer.missing.is_empty());
    let task = rig.send();
    let first = rig.next_request().await;
    assert!(offered(API, &first.body).is_empty());
    first.respond(text_response(API, &nonce())).await;
    finished(task).await;

    rig.permit(ToolPermission::Ask).await;
    let task = rig.send();
    let first = rig.next_request().await;
    assert_eq!(offered(API, &first.body), vec![rig.tool.clone()]);
    let schema = tool_schema(&first.body, &rig.tool);
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["path"]["type"], "string");
    assert_eq!(schema["required"], json!(["path"]));
    let call = Call::new(&rig.tool, json!({"path": rig.file.0}));
    first
        .respond(calls_response(API, std::slice::from_ref(&call), ""))
        .await;
    let approval = tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(request) = rig.thread().approvals.first() {
                return request.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("an approval request"));
    assert_eq!(approval.tool, rig.tool);
    assert_eq!(approval.capabilities, [Capability::ReadWorkspace].into());
    rig.ok(Command::ResolveApproval {
        thread: rig.thread,
        approval: approval.id,
        digest: approval.digest.clone(),
        decision: ApprovalDecision::Approve,
    })
    .await;
    let second = rig.next_request().await;
    // The guest read the file through a gateway-authorized host call.
    assert_eq!(
        result_for(API, &second.body, &call.id),
        Some(rig.file.1.clone())
    );
    second.respond(text_response(API, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    let calls = calls_of(&thread);
    let last = calls.last().unwrap();
    assert_eq!(last.state, ToolCallState::Succeeded);
    assert_eq!(last.output.as_deref(), Some(rig.file.1.as_str()));
    assert!(last.error.is_none());
}

async fn argument_errors_return_to_the_model_as_typed_failures() {
    let mut rig = rig().await;
    rig.permit(ToolPermission::Allow).await;
    let unknown = nonce();
    let calls = [
        Call::new(&rig.tool, json!({"path": 7})),
        Call::new(
            &rig.tool,
            json!({"path": rig.file.0, unknown.clone(): true}),
        ),
        Call::new(&rig.tool, json!({})),
        Call::new(&rig.tool, json!({"path": u64::MAX})),
    ];
    let (thread, results) = rig.round_trip(&calls).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    let expected = [
        "parameter `path` must be string".to_string(),
        format!("tool has no parameter `{unknown}`"),
        "parameter `path` is required".into(),
        "outside the range".into(),
    ];
    let records = calls_of(&thread);
    for ((record, result), expected) in records.iter().zip(&results).zip(&expected) {
        assert_eq!(record.state, ToolCallState::Failed, "{record:?}");
        assert_eq!(error_kind(record), ErrorKind::InvalidRequest);
        let result = result.as_deref().unwrap();
        assert!(result.contains(expected), "{result}");
        assert!(result.contains(&rig.tool), "{result}");
    }
}

async fn the_conversation_grant_caps_the_extension() {
    let mut rig = rig().await;
    rig.permit(ToolPermission::Allow).await;
    // The user granted the extension `read_workspace`; this conversation
    // no longer has it.
    rig.grant_thread(&[]).await;
    let offer = rig.offer().await.expect("still listed");
    assert_eq!(offer.permission, Some(ToolPermission::Allow));
    assert_eq!(offer.effective, ToolPermission::Deny);
    assert_eq!(offer.missing, [Capability::ReadWorkspace].into());
    let task = rig.send();
    let first = rig.next_request().await;
    assert!(offered(API, &first.body).is_empty());
    let call = Call::new(&rig.tool, json!({"path": rig.file.0}));
    first
        .respond(calls_response(API, std::slice::from_ref(&call), ""))
        .await;
    let second = rig.next_request().await;
    let result = result_for(API, &second.body, &call.id).unwrap();
    assert!(!result.contains(&rig.file.1), "the file leaked: {result}");
    second.respond(text_response(API, &nonce())).await;
    let thread = finished(task).await;
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Denied);
    assert_eq!(error_kind(record), ErrorKind::PermissionDenied);
}

async fn disabled_or_ungranted_extensions_are_not_offered() {
    let mut rig = rig().await;
    rig.permit(ToolPermission::Allow).await;
    assert!(rig.offer().await.is_some());
    rig.ok(Command::EnableExtension {
        extension: EXTENSION.into(),
        enabled: false,
    })
    .await;
    assert!(rig.offer().await.is_none());
    let call = Call::new(&rig.tool, json!({"path": rig.file.0}));
    let task = rig.send();
    let first = rig.next_request().await;
    assert!(offered(API, &first.body).is_empty());
    first
        .respond(calls_response(API, std::slice::from_ref(&call), ""))
        .await;
    let second = rig.next_request().await;
    let result = result_for(API, &second.body, &call.id).unwrap();
    assert!(!result.contains(&rig.file.1), "the file leaked: {result}");
    second.respond(text_response(API, &nonce())).await;
    let thread = finished(task).await;
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Denied);
    assert_eq!(error_kind(record), ErrorKind::NotFound);
    // Running the conversation never re-enabled the extension.
    assert!(
        rig.gateway
            .snapshot()
            .unwrap()
            .plugins
            .iter()
            .all(|p| p.id != EXTENSION || !p.enabled)
    );

    // Enabled again by the user, but with the grant withdrawn.
    rig.ok(Command::EnableExtension {
        extension: EXTENSION.into(),
        enabled: true,
    })
    .await;
    assert!(rig.offer().await.is_some());
    rig.grant_extension(&[]).await;
    assert!(rig.offer().await.is_none());
}

async fn cancelling_during_an_extension_call_kills_the_worker() {
    let mut rig = rig().await;
    rig.permit(ToolPermission::Allow).await;
    let task = rig.send();
    let first = rig.next_request().await;
    let call = Call::new(&rig.tool, json!({"path": format!("{STALL}-{}", nonce())}));
    first
        .respond(calls_response(API, std::slice::from_ref(&call), ""))
        .await;
    let pid: u32 = tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(pid) = stalled_worker(rig.state.path()) {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the extension worker to start"));
    assert!(alive(pid), "the worker is running the call");
    assert_eq!(calls_of(&rig.thread())[0].state, ToolCallState::Running);
    rig.ok(Command::CancelRun { thread: rig.thread }).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Cancelled);
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Cancelled);
    assert_eq!(error_kind(record), ErrorKind::Cancelled);
    tokio::time::timeout(PATIENCE, async {
        while alive(pid) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the extension worker to be killed"));
    // No further model round was requested.
    assert!(rig.requests.try_recv().is_err());
}

async fn withdrawing_the_extension_stops_its_running_call() {
    for disable in [false, true] {
        let mut rig = rig().await;
        rig.permit(ToolPermission::Allow).await;
        let task = rig.send();
        let first = rig.next_request().await;
        let call = Call::new(&rig.tool, json!({"path": format!("{STALL}-{}", nonce())}));
        first
            .respond(calls_response(API, std::slice::from_ref(&call), ""))
            .await;
        let pid = stalled(&rig).await;
        // The conversation still permits the tool; only the user's authority
        // over the extension itself is withdrawn.
        if disable {
            rig.ok(Command::EnableExtension {
                extension: EXTENSION.into(),
                enabled: false,
            })
            .await;
        } else {
            rig.grant_extension(&[]).await;
        }
        let second = rig.next_request().await;
        let result = result_for(API, &second.body, &call.id).unwrap();
        assert!(result.contains("withdrawn"), "{result}");
        second.respond(text_response(API, &nonce())).await;
        let thread = finished(task).await;
        assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
        let record = &calls_of(&thread)[0];
        assert_eq!(record.state, ToolCallState::Denied);
        assert_eq!(error_kind(record), ErrorKind::PermissionDenied);
        gone(pid).await;
    }
}
/// Wait for the worker of a stalled call; its host process ID.
async fn stalled(rig: &Rig) -> u32 {
    tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(pid) = stalled_worker(rig.state.path()) {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the extension worker to start"))
}
/// Wait for `pid` to exit.
async fn gone(pid: u32) {
    tokio::time::timeout(PATIENCE, async {
        while alive(pid) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the extension worker to be killed"));
}

/// The host process ID of a worker that announced it stalled. Linux workers
/// run confined, with the sandbox's scratch directory as their data directory
/// and their own PID namespace, so the process found is this process's child
/// whose arguments name that directory; elsewhere the worker shares the
/// gateway's data directory and writes its own ID.
#[cfg(target_os = "linux")]
fn stalled_worker(_state: &Path) -> Option<u32> {
    let tasks = std::fs::read_dir("/proc/self/task").ok()?;
    let children = tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|list| {
            list.split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok())
                .collect::<Vec<_>>()
        });
    for pid in children {
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        let Some(data_dir) = args
            .iter()
            .position(|a| *a == b"--data-dir")
            .and_then(|i| args.get(i + 1))
        else {
            continue;
        };
        let data_dir = Path::new(std::str::from_utf8(data_dir).ok()?);
        if args.iter().any(|a| *a == b"extension-worker") && data_dir.join(STALLED).exists() {
            return Some(pid);
        }
    }
    None
}
#[cfg(not(target_os = "linux"))]
fn stalled_worker(state: &Path) -> Option<u32> {
    std::fs::read_to_string(state.join(STALLED))
        .ok()?
        .parse()
        .ok()
}

/// Whether `pid` is still running; an exited, unreaped child is not.
#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // The state follows the parenthesized command name.
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with(['Z', 'X']))
    })
}
#[cfg(all(unix, not(target_os = "linux")))]
fn alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    // A zombie still answers; tokio reaps killed children promptly.
    kill(Pid::from_raw(pid as i32), None).is_ok()
}
#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    // No portable liveness check: the run's cancellation is still verified.
    false
}
