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
    (
        "approved_skills_reach_the_model_through_the_skill_tools",
        || Box::pin(approved_skills_reach_the_model_through_the_skill_tools()),
    ),
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
    ("an_installed_extension_runs_only_as_reviewed", || {
        Box::pin(an_installed_extension_runs_only_as_reviewed())
    }),
    ("invalid_packages_are_refused_before_storing", || {
        Box::pin(invalid_packages_are_refused_before_storing())
    }),
    ("a_retried_install_replays_its_receipt", || {
        Box::pin(a_retried_install_replays_its_receipt())
    }),
    ("an_agent_prepared_package_waits_for_the_user", || {
        Box::pin(an_agent_prepared_package_waits_for_the_user())
    }),
    ("agents_cannot_leave_unbounded_proposals", || {
        Box::pin(agents_cannot_leave_unbounded_proposals())
    }),
    ("git_tools_run_once_the_conversation_is_granted_git", || {
        Box::pin(git_tools_run_once_the_conversation_is_granted_git())
    }),
    (
        "the_agent_learns_what_it_can_call_and_what_to_ask_for",
        || Box::pin(the_agent_learns_what_it_can_call_and_what_to_ask_for()),
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
        // The ID, then the invocation, so a test finds only its own call's worker.
        let record = format!("{}\n{:?}", std::process::id(), start.invocation);
        std::fs::write(&pending, record).unwrap();
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
    workspace: tempfile::TempDir,
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
        workspace,
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
        // unavailable where no equivalent isolation exists (Windows).
        // Still reject any unavailable extension or unexpected host failure.
        if cfg!(any(target_os = "linux", target_os = "macos")) {
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
        // A number where a string is declared would be converted.
        Call::new(&rig.tool, json!({"path": [7]})),
        Call::new(
            &rig.tool,
            json!({"path": rig.file.0, unknown.clone(): true}),
        ),
        Call::new(&rig.tool, json!({})),
        // Nested, so it is not converted to the declared string first.
        Call::new(&rig.tool, json!({"path": [u64::MAX]})),
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
    let marker = format!("{STALL}-{}", nonce());
    let call = Call::new(&rig.tool, json!({"path": &marker}));
    first
        .respond(calls_response(API, std::slice::from_ref(&call), ""))
        .await;
    let pid = stalled(&rig, &marker).await;
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
        let marker = format!("{STALL}-{}", nonce());
        let call = Call::new(&rig.tool, json!({"path": &marker}));
        first
            .respond(calls_response(API, std::slice::from_ref(&call), ""))
            .await;
        let pid = stalled(&rig, &marker).await;
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
/// Wait for the worker of the stalled call carrying `marker`; its host
/// process ID.
async fn stalled(rig: &Rig, marker: &str) -> u32 {
    tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(pid) = stalled_worker(rig.state.path(), marker) {
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
/// whose arguments name that directory. macOS workers are confined too but
/// share the host's PIDs, so the ID they write in their scratch directory is
/// the host's. Elsewhere the worker shares the gateway's data directory. Only
/// a record naming `marker`, unique to the test's call, counts.
#[cfg(target_os = "linux")]
fn stalled_worker(_state: &Path, marker: &str) -> Option<u32> {
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
        if args.iter().any(|a| *a == b"extension-worker")
            && stalled_record(&data_dir.join(STALLED), marker).is_some()
        {
            return Some(pid);
        }
    }
    None
}
#[cfg(target_os = "macos")]
fn stalled_worker(_state: &Path, marker: &str) -> Option<u32> {
    // Each confined worker's scratch directory is a fresh `a206-confined-`
    // directory in the temporary directory; only this binary stalls workers.
    std::fs::read_dir(std::env::temp_dir())
        .ok()?
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("a206-confined-")
        })
        .find_map(|entry| stalled_record(&entry.path().join(STALLED), marker))
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn stalled_worker(state: &Path, marker: &str) -> Option<u32> {
    stalled_record(&state.join(STALLED), marker)
}
/// The process ID in the stall record at `path`, if it is `marker`'s call.
fn stalled_record(path: &Path, marker: &str) -> Option<u32> {
    let record = std::fs::read_to_string(path).ok()?;
    let (pid, invocation) = record.split_once('\n')?;
    invocation
        .contains(marker)
        .then(|| pid.parse().ok())
        .flatten()
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

// ── Installed packages ──────────────────────────────────────────────────────

/// The bundled workspace extension's manifest under a new id and version,
/// with its compiled module: a stand-in for a third-party package.
fn package(id: &str, version: &str) -> (String, Vec<u8>) {
    let builtin = a206_extensions::builtin(EXTENSION).unwrap().unwrap();
    let text = include_str!("../../../plugins/workspace/plugin.toml")
        .replacen(
            &format!("id = \"{EXTENSION}\""),
            &format!("id = \"{id}\""),
            1,
        )
        .replacen(
            &format!("version = \"{}\"", builtin.manifest.version),
            &format!("version = \"{version}\""),
            1,
        );
    (text, builtin.wasm.to_vec())
}
async fn install(gateway: &Gateway, (manifest, module): (String, Vec<u8>)) -> Reply {
    request(gateway, Command::InstallExtension { manifest, module }).await
}
fn failure(reply: Reply) -> ErrorReport {
    match reply {
        Reply::Error(report) => report,
        other => panic!("expected a failure, got {other:?}"),
    }
}
fn installed_digest(summary: &PluginSummary) -> String {
    match &summary.source {
        PluginSource::Installed { digest } => digest.clone(),
        PluginSource::Bundled => panic!("expected an installed extension"),
    }
}

async fn an_installed_extension_runs_only_as_reviewed() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let file = format!("{}.txt", nonce());
    std::fs::write(workspace.path().join(&file), nonce()).unwrap();
    let gateway = Gateway::open(state.path()).unwrap();
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
    let id = format!("test.{}", nonce());
    let list = || Command::CallTool {
        extension: id.clone(),
        tool: "list_directory".into(),
        args: Default::default(),
        project: Some(project.id),
    };
    let ok = |command| {
        let gateway = gateway.clone();
        async move { assert_eq!(request(&gateway, command).await, Reply::Ok) }
    };
    let enable = || Command::EnableExtension {
        extension: id.clone(),
        enabled: true,
    };

    // It arrives disabled, with nothing granted.
    let Reply::Extension(summary) = install(&gateway, package(&id, "1.0.0")).await else {
        panic!("install failed");
    };
    assert!(!summary.enabled && summary.granted.is_empty());
    let digest = installed_digest(&summary);
    assert!(gateway.snapshot().unwrap().plugins.contains(&summary));
    let denied = failure(request(&gateway, list()).await);
    assert_eq!(denied.kind, ErrorKind::PermissionDenied);
    ok(enable()).await;
    let denied = failure(request(&gateway, list()).await);
    assert_eq!(denied.kind, ErrorKind::PermissionDenied);
    ok(Command::GrantExtension {
        extension: id.clone(),
        capabilities: [Capability::ReadWorkspace].into(),
    })
    .await;
    let Reply::Text(listing) = request(&gateway, list()).await else {
        panic!("the installed tool failed");
    };
    assert!(listing.contains(&file), "{listing}");

    // A stored package that no longer matches its digest never runs.
    let stored = state.path().join("extensions").join(&digest);
    std::fs::write(stored.join("module.wasm"), nonce()).unwrap();
    let changed = failure(request(&gateway, list()).await);
    assert_eq!(changed.kind, ErrorKind::Conflict, "{changed:?}");
    // Installing the same package restores it without asking again.
    let Reply::Extension(same) = install(&gateway, package(&id, "1.0.0")).await else {
        panic!("reinstall failed");
    };
    assert!(same.enabled && installed_digest(&same) == digest);
    assert!(matches!(request(&gateway, list()).await, Reply::Text(_)));

    // An update is disabled until the user enables it again, keeps the
    // grants it still requests and replaces the earlier package.
    let Reply::Extension(update) = install(&gateway, package(&id, "2.0.0")).await else {
        panic!("update failed");
    };
    assert!(!update.enabled);
    assert_eq!(update.granted, [Capability::ReadWorkspace].into());
    assert_ne!(installed_digest(&update), digest);
    assert!(!stored.exists());
    let denied = failure(request(&gateway, list()).await);
    assert_eq!(denied.kind, ErrorKind::PermissionDenied);
    ok(enable()).await;
    assert!(matches!(request(&gateway, list()).await, Reply::Text(_)));

    // Uninstalling removes the package; bundled extensions stay.
    ok(Command::UninstallExtension {
        extension: id.clone(),
    })
    .await;
    assert!(
        !gateway
            .snapshot()
            .unwrap()
            .plugins
            .iter()
            .any(|p| p.id == id)
    );
    assert!(
        !state
            .path()
            .join("extensions")
            .join(installed_digest(&update))
            .exists()
    );
    let gone = failure(request(&gateway, list()).await);
    assert_eq!(gone.kind, ErrorKind::NotFound);
    let bundled = failure(
        request(
            &gateway,
            Command::UninstallExtension {
                extension: EXTENSION.into(),
            },
        )
        .await,
    );
    assert_eq!(bundled.kind, ErrorKind::InvalidRequest);
}

async fn invalid_packages_are_refused_before_storing() {
    let state = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(state.path()).unwrap();
    let id = format!("test.{}", nonce());
    let (manifest, module) = package(&id, "1.0.0");
    let refused = [
        // The bundled namespace, including a bundled extension's own id.
        package(&format!("agent206.{}", nonce()), "1.0.0"),
        package(EXTENSION, "1.0.0"),
        (manifest.clone(), nonce().into_bytes()),
        (format!("{manifest}\n{} = 1", nonce()), module.clone()),
        (manifest.clone(), {
            let mut large = module.clone();
            large.resize(4 * 1024 * 1024, 0);
            large
        }),
    ];
    for package in refused {
        let report = failure(install(&gateway, package).await);
        assert_eq!(report.kind, ErrorKind::InvalidRequest, "{report:?}");
    }
    assert!(!state.path().join("extensions").exists());
    let plugins = gateway.snapshot().unwrap().plugins;
    assert!(plugins.iter().all(|p| p.source == PluginSource::Bundled));
}

async fn a_retried_install_replays_its_receipt() {
    let state = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(state.path()).unwrap();
    let id = format!("test.{}", nonce());
    let send = |request: Id, (manifest, module): (String, Vec<u8>)| {
        let gateway = gateway.clone();
        async move {
            gateway
                .request(a206_protocol::Request {
                    id: request,
                    body: Command::InstallExtension { manifest, module },
                })
                .await
                .body
        }
    };
    let first = Id::new_v4();
    let receipt = send(first, package(&id, "1.0.0")).await;
    assert!(matches!(receipt, Reply::Extension(_)), "{receipt:?}");
    let Reply::Extension(newer) = send(Id::new_v4(), package(&id, "2.0.0")).await else {
        panic!("update failed");
    };
    // A late retry returns its receipt instead of reinstalling the old package.
    assert_eq!(send(first, package(&id, "1.0.0")).await, receipt);
    let plugins = gateway.snapshot().unwrap().plugins;
    assert!(plugins.contains(&newer), "{plugins:?}");
    // The same id with a different package is refused, not installed.
    let reused = failure(send(first, package(&id, "3.0.0")).await);
    assert_eq!(reused.kind, ErrorKind::Conflict, "{reused:?}");
    assert!(gateway.snapshot().unwrap().plugins.contains(&newer));
}

async fn approved_skills_reach_the_model_through_the_skill_tools() {
    let api = API;
    let mut rig = rig().await;
    rig.tool = a206_runtime::skill_tool::INFO.into();
    let body = nonce();
    let dir = rig.state.path().join("skills/notes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: notes\ndescription: Take notes.\n---\n{body}"),
    )
    .unwrap();
    let Reply::Skill(reviewed) = rig
        .run(Command::ReadSkill {
            name: "notes".into(),
        })
        .await
    else {
        panic!("skill read for review");
    };
    let approved = rig
        .run(Command::SetSkillEnabled {
            name: "notes".into(),
            enabled: true,
            digest: Some(reviewed.digest),
        })
        .await;
    assert_eq!(approved, Reply::Ok);
    // The skill tools stay denied until this conversation permits them.
    let task = rig.send();
    let first = rig.next_request().await;
    assert!(offered(api, &first.body).is_empty());
    first.respond(text_response(api, &nonce())).await;
    finished(task).await;

    rig.permit(ToolPermission::Allow).await;
    let task = rig.send();
    let first = rig.next_request().await;
    assert_eq!(
        offered(api, &first.body),
        vec![a206_runtime::skill_tool::INFO.to_string()]
    );
    let call = Call::new(a206_runtime::skill_tool::INFO, json!({"name": "notes"}));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
        .await;
    let second = rig.next_request().await;
    assert_eq!(result_for(api, &second.body, &call.id), Some(body));
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
}

// ── Agent-prepared packages ─────────────────────────────────────────────────

impl Rig {
    /// Write `package` into the project and return the call proposing it.
    fn prepared(&self, (manifest, module): (String, Vec<u8>)) -> Call {
        let dir = nonce();
        let root = self.workspace.path().join(&dir);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("plugin.toml"), manifest).unwrap();
        std::fs::write(root.join("module.wasm"), module).unwrap();
        Call::new(
            a206_runtime::propose_tool::NAME,
            json!({"manifest": format!("{dir}/plugin.toml"), "module": format!("{dir}/module.wasm")}),
        )
    }
    fn proposals(&self) -> Vec<ExtensionProposal> {
        self.gateway.snapshot().unwrap().extension_proposals
    }
    fn stored(&self, digest: &str) -> bool {
        self.state.path().join("extensions").join(digest).exists()
    }
}

async fn an_agent_prepared_package_waits_for_the_user() {
    let mut rig = rig().await;
    rig.tool = a206_runtime::propose_tool::NAME.into();
    let id = format!("test.{}", nonce());
    // The user installed and granted the first version themselves.
    let Reply::Extension(first) = install(&rig.gateway, package(&id, "1.0.0")).await else {
        panic!("install failed");
    };
    let grant = Command::GrantExtension {
        extension: id.clone(),
        capabilities: [Capability::ReadWorkspace].into(),
    };
    rig.ok(grant).await;

    // Denied until this conversation permits the tool.
    let update = rig.prepared(package(&id, "2.0.0"));
    let (thread, _) = rig.round_trip(std::slice::from_ref(&update)).await;
    assert_eq!(
        calls_of(&thread).last().unwrap().state,
        ToolCallState::Denied
    );
    assert!(rig.proposals().is_empty());

    rig.permit(ToolPermission::Allow).await;
    let escape = Call::new(
        &rig.tool,
        json!({"manifest": "../plugin.toml", "module": "module.wasm"}),
    );
    // Each call gets its own id, so its result is found unambiguously.
    let update = rig.prepared(package(&id, "2.0.0"));
    let (thread, results) = rig.round_trip(&[update, escape]).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    let calls = calls_of(&thread);
    let (proposed, escaped) = (&calls[calls.len() - 2], &calls[calls.len() - 1]);
    assert_eq!(
        proposed.state,
        ToolCallState::Succeeded,
        "{:?}",
        proposed.error
    );
    assert!(results[0].as_deref().unwrap().contains(&id));
    assert_eq!(escaped.state, ToolCallState::Failed);
    assert_eq!(error_kind(escaped), ErrorKind::InvalidRequest);

    // Proposing changed nothing the user had installed or granted.
    let proposals = rig.proposals();
    assert_eq!(proposals.len(), 1, "{proposals:?}");
    let proposal = proposals[0].clone();
    assert_eq!(proposal.thread, rig.thread);
    assert_eq!(proposal.summary.id, id);
    assert!(!proposal.summary.enabled && proposal.summary.granted.is_empty());
    assert!(
        rig.gateway
            .snapshot()
            .unwrap()
            .plugins
            .contains(&PluginSummary {
                granted: [Capability::ReadWorkspace].into(),
                enabled: false,
                ..*first.clone()
            })
    );
    assert!(rig.stored(&proposal.digest));

    // Installing it is the user's step; the update keeps no earlier grants.
    let Reply::Extension(installed) = rig
        .run(Command::InstallProposal {
            digest: proposal.digest.clone(),
        })
        .await
    else {
        panic!("installing the proposal failed");
    };
    assert_eq!(installed.version, "2.0.0");
    assert!(!installed.enabled && installed.granted.is_empty());
    assert_eq!(installed_digest(&installed), proposal.digest);
    assert!(rig.proposals().is_empty());
    assert!(!rig.stored(&installed_digest(&first)));
    let again = failure(
        rig.run(Command::InstallProposal {
            digest: proposal.digest.clone(),
        })
        .await,
    );
    assert_eq!(again.kind, ErrorKind::NotFound);

    // The installed package itself cannot be proposed again.
    let update = rig.prepared(package(&id, "2.0.0"));
    let (thread, _) = rig.round_trip(std::slice::from_ref(&update)).await;
    let last = calls_of(&thread).last().unwrap().clone();
    assert_eq!(error_kind(&last), ErrorKind::Conflict, "{:?}", last.error);

    // A dismissed proposal leaves nothing behind.
    let newer = rig.prepared(package(&id, "3.0.0"));
    rig.round_trip(std::slice::from_ref(&newer)).await;
    let [proposal] = rig.proposals().try_into().unwrap();
    let dismiss = || Command::DismissProposal {
        digest: proposal.digest.clone(),
    };
    rig.ok(dismiss()).await;
    assert!(rig.proposals().is_empty());
    assert!(!rig.stored(&proposal.digest));
    assert_eq!(failure(rig.run(dismiss()).await).kind, ErrorKind::NotFound);
    assert_eq!(
        rig.gateway
            .snapshot()
            .unwrap()
            .plugins
            .iter()
            .find(|p| p.id == id),
        Some(&*installed)
    );
}

async fn agents_cannot_leave_unbounded_proposals() {
    let mut rig = rig().await;
    rig.tool = a206_runtime::propose_tool::NAME.into();
    rig.permit(ToolPermission::Allow).await;
    let calls: Vec<Call> = (0..=MAX_EXTENSION_PROPOSALS)
        .map(|_| rig.prepared(package(&format!("test.{}", nonce()), "1.0.0")))
        .collect();
    let (thread, _) = rig.round_trip(&calls).await;
    let results = calls_of(&thread);
    let (last, rest) = results.split_last().unwrap();
    assert!(
        rest.iter()
            .all(|call| call.state == ToolCallState::Succeeded)
    );
    assert_eq!(error_kind(last), ErrorKind::Conflict, "{:?}", last.error);
    let proposals = rig.proposals();
    assert_eq!(proposals.len(), MAX_EXTENSION_PROPOSALS);
    // A newer version of a waiting proposal replaces it rather than adding one.
    let first = &proposals[0];
    let replacement = rig.prepared(package(&first.summary.id, "2.0.0"));
    let (thread, _) = rig.round_trip(std::slice::from_ref(&replacement)).await;
    let last = calls_of(&thread).last().unwrap().clone();
    assert_eq!(last.state, ToolCallState::Succeeded, "{:?}", last.error);
    let proposals = rig.proposals();
    assert_eq!(proposals.len(), MAX_EXTENSION_PROPOSALS);
    assert!(!proposals.iter().any(|p| p.digest == first.digest));
    assert!(!rig.stored(&first.digest));
}

async fn git_tools_run_once_the_conversation_is_granted_git() {
    let mut rig = rig().await;
    gix::init(rig.workspace.path()).unwrap();
    const GIT: &str = "agent206.git";
    rig.ok(Command::GrantExtension {
        extension: GIT.into(),
        capabilities: [Capability::GitRead].into(),
    })
    .await;
    rig.tool = tool_name(GIT, "git_status");
    rig.permit(ToolPermission::Allow).await;
    let name = rig.tool.clone();
    let status = || Call::new(&name, json!({}));

    // Granted to the extension and permitted, but not to this conversation.
    let (thread, _) = rig.round_trip(&[status()]).await;
    let record = calls_of(&thread).last().unwrap().clone();
    assert_eq!(record.state, ToolCallState::Denied);
    assert_eq!(error_kind(&record), ErrorKind::PermissionDenied);

    // An external runtime is never offered a git grant it could not be held to.
    let id = rig.thread;
    let refused = move |backend, capabilities: &[Capability]| Command::SetBackend {
        thread: id,
        backend,
        capabilities: capabilities.iter().copied().collect(),
    };
    let reply = rig
        .run(refused(CodingBackend::Codex, &[Capability::GitRead]))
        .await;
    assert_eq!(failure(reply).kind, ErrorKind::PermissionDenied);
    // Plan mode's ceiling holds git reading, not committing or the network.
    for capability in [Capability::GitWrite, Capability::Network] {
        let reply = rig.run(refused(CodingBackend::Native, &[capability])).await;
        assert_eq!(failure(reply).kind, ErrorKind::PermissionDenied);
    }

    rig.grant_thread(&[Capability::ReadWorkspace, Capability::GitRead])
        .await;
    let (thread, results) = rig.round_trip(&[status()]).await;
    let record = calls_of(&thread).last().unwrap().clone();
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let text = results[0].as_deref().unwrap();
    assert!(text.contains(&rig.file.0), "{text}");

    // In Code mode committing is grantable, but only with reading.
    let model = rig.thread().model.unwrap();
    let configured = rig
        .run(Command::ConfigureThread {
            id: rig.thread,
            mode: Mode::Code,
            model: Some(model.clone()),
            subagent_models: [model].into(),
        })
        .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    let reply = rig
        .run(refused(CodingBackend::Native, &[Capability::GitWrite]))
        .await;
    assert_eq!(failure(reply).kind, ErrorKind::InvalidRequest);
    rig.grant_thread(&[Capability::GitRead, Capability::GitWrite])
        .await;

    // With both grants, the agent commits the file it was asked to.
    let config = rig.workspace.path().join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(
        "[user]\n\tname = Agent Test\n\temail = test@example.invalid\n[commit]\n\tgpgSign = false\n",
    );
    std::fs::write(&config, text).unwrap();
    rig.ok(Command::GrantExtension {
        extension: GIT.into(),
        capabilities: [Capability::GitRead, Capability::GitWrite].into(),
    })
    .await;
    rig.tool = tool_name(GIT, "git_commit");
    rig.permit(ToolPermission::Allow).await;
    let message = nonce();
    let commit = Call::new(
        &rig.tool,
        json!({ "message": message, "paths": rig.file.0 }),
    );
    let (thread, _) = rig.round_trip(&[commit]).await;
    let record = calls_of(&thread).last().unwrap().clone();
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let committed = {
        let repo = gix::open(rig.workspace.path()).unwrap();
        let head = repo.head_commit().unwrap();
        head.message_raw().unwrap().to_string()
    };
    assert_eq!(committed.trim(), message);

    // The same grant stages a change without committing it.
    let changed = nonce();
    std::fs::write(rig.workspace.path().join(&rig.file.0), &changed).unwrap();
    rig.tool = tool_name(GIT, "git_stage");
    rig.permit(ToolPermission::Allow).await;
    let stage = Call::new(&rig.tool, json!({ "paths": rig.file.0 }));
    let (thread, _) = rig.round_trip(&[stage]).await;
    let record = calls_of(&thread).last().unwrap().clone();
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    {
        let repo = gix::open(rig.workspace.path()).unwrap();
        let index = repo.open_index().unwrap();
        let entry = index.entry_by_path(rig.file.0.as_str().into()).unwrap();
        assert_eq!(repo.find_object(entry.id).unwrap().data, changed.as_bytes());
        // Staging committed nothing.
        let head = repo.head_commit().unwrap();
        assert_eq!(head.message_raw().unwrap().to_string().trim(), message);
    }

    // And commits what is staged.
    rig.tool = tool_name(GIT, "git_commit");
    let message = nonce();
    let commit = Call::new(&rig.tool, json!({ "message": message, "staged": true }));
    let (thread, _) = rig.round_trip(&[commit]).await;
    let record = calls_of(&thread).last().unwrap().clone();
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    {
        let repo = gix::open(rig.workspace.path()).unwrap();
        let head = repo.head_commit().unwrap();
        assert_eq!(head.message_raw().unwrap().to_string().trim(), message);
        let tree = head.tree().unwrap();
        let entry = tree.lookup_entry_by_path(&rig.file.0).unwrap().unwrap();
        assert_eq!(
            repo.find_object(entry.object_id()).unwrap().data,
            changed.as_bytes()
        );
    }

    // And creates a branch without switching to it.
    rig.tool = tool_name(GIT, "git_branch_create");
    rig.permit(ToolPermission::Allow).await;
    let branch = nonce();
    let create = Call::new(&rig.tool, json!({ "name": branch }));
    let (thread, _) = rig.round_trip(&[create]).await;
    let record = calls_of(&thread).last().unwrap().clone();
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    {
        let repo = gix::open(rig.workspace.path()).unwrap();
        let head = repo.head_id().unwrap().detach();
        let mut made = repo
            .find_reference(format!("refs/heads/{branch}").as_str())
            .unwrap();
        assert_eq!(made.peel_to_id().unwrap().detach(), head);
        let on = repo.head_name().unwrap().unwrap();
        assert_ne!(on.as_bstr(), format!("refs/heads/{branch}").as_str());
    }

    // Switching to it also edits project files, so it needs that grant too.
    rig.grant_thread(&[
        Capability::ReadWorkspace,
        Capability::WriteWorkspace,
        Capability::GitRead,
        Capability::GitWrite,
    ])
    .await;
    rig.ok(Command::GrantExtension {
        extension: GIT.into(),
        capabilities: [
            Capability::GitRead,
            Capability::GitWrite,
            Capability::WriteWorkspace,
        ]
        .into(),
    })
    .await;
    rig.tool = tool_name(GIT, "git_switch");
    rig.permit(ToolPermission::Allow).await;
    let switch = Call::new(&rig.tool, json!({ "name": branch }));
    let (thread, _) = rig.round_trip(&[switch]).await;
    let record = calls_of(&thread).last().unwrap().clone();
    assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
    let on = gix::open(rig.workspace.path())
        .unwrap()
        .head_name()
        .unwrap()
        .unwrap();
    assert_eq!(on.as_bstr(), format!("refs/heads/{branch}").as_str());

    // The network alone needs no project, unlike everything else.
    let Reply::Thread(research) = rig
        .run(Command::CreateThread {
            title: nonce(),
            mode: Mode::Research,
        })
        .await
    else {
        panic!("research thread created");
    };
    let grant = |capabilities: &[Capability]| Command::SetBackend {
        thread: research.id,
        backend: CodingBackend::Native,
        capabilities: capabilities.iter().copied().collect(),
    };
    rig.ok(grant(&[Capability::Network])).await;
    let reply = rig.run(grant(&[Capability::ReadWorkspace])).await;
    assert_eq!(failure(reply).kind, ErrorKind::InvalidRequest);
}

async fn the_agent_learns_what_it_can_call_and_what_to_ask_for() {
    let mut rig = rig().await;
    rig.tool = a206_runtime::extension_list_tool::NAME.into();
    rig.permit(ToolPermission::Allow).await;
    let list = || Call::new(a206_runtime::extension_list_tool::NAME, json!({}));
    let read = |listing: &serde_json::Value| {
        listing["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == EXTENSION)
            .and_then(|e| e["tools"].as_array())
            .and_then(|tools| tools.iter().find(|t| t["tool"] == READ))
            .cloned()
            .unwrap()
    };
    let (thread, results) = rig.round_trip(&[list()]).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    let listing: serde_json::Value = serde_json::from_str(results[0].as_deref().unwrap()).unwrap();
    let entry = read(&listing);
    assert_eq!(entry["name"], tool_name(EXTENSION, READ));
    assert_eq!(entry["blocked_by"], json!([]));

    // Once the user withdraws the grant, the agent is told what to ask for.
    rig.grant_extension(&[]).await;
    let (_, results) = rig.round_trip(&[list()]).await;
    let listing: serde_json::Value = serde_json::from_str(results[0].as_deref().unwrap()).unwrap();
    let entry = read(&listing);
    assert!(entry["name"].is_null(), "{entry}");
    assert_eq!(
        entry["blocked_by"],
        json!([format!(
            "The user has not granted this extension: {}.",
            Capability::ReadWorkspace.label()
        )])
    );
    // Listing changed nothing the user decides.
    let plugin = rig
        .gateway
        .snapshot()
        .unwrap()
        .plugins
        .into_iter()
        .find(|p| p.id == EXTENSION)
        .unwrap();
    assert!(plugin.granted.is_empty());
}
