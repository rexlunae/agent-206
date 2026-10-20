//! MCP server tools in native runs, through the gateway's MCP call path.
//!
//! The fixture server (`crates/mcp/src/bin/a206-mcp-fixture`) is served in
//! process and its session attached with the `test-support` hook
//! `Gateway::attach_mcp_session`, in place of the sandboxed launch: the
//! fixture binary belongs to `a206-mcp`, so Cargo does not build it for this
//! crate's tests, and on Linux the launching binary must also serve as the
//! sandbox helper. Launching through the real macOS and Linux sandboxes is
//! tested in `crates/mcp/tests/sandboxed.rs`. Listing, authorization,
//! approval, calls, withdrawal and cancellation are the shipped paths.
mod support;

#[path = "../../mcp/src/bin/a206-mcp-fixture/server.rs"]
mod fixture;

use a206_core::*;
use a206_runtime::{gateway::Gateway, mcp_tools::tool_name};
use serde_json::json;
use std::time::Duration;
use support::*;
use tokio::sync::mpsc;

const API: ProviderApi = ProviderApi::OpenAi;
/// The authority the fixture's entry holds, which a project conversation in
/// Code mode can be granted.
const AUTHORITY: Capability = Capability::Execute;

struct Rig {
    gateway: Gateway,
    _state: tempfile::TempDir,
    _install: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    thread: Id,
    requests: mpsc::Receiver<Opened>,
    entry: McpServerConfig,
}
/// A Code project conversation granted [`AUTHORITY`], and an enabled server
/// entry holding it, with MCP's default policy, connected to the fixture.
async fn rig() -> Rig {
    let state = tempfile::tempdir().unwrap();
    let install = tempfile::tempdir().unwrap();
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
        thread: project.manager,
        requests,
        entry: entry(install.path()),
        _state: state,
        _install: install,
        _workspace: workspace,
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
    rig.grant(&[AUTHORITY]).await;
    rig.save(vec![rig.entry.clone()]).await;
    rig.attach().await;
    rig
}
/// An enabled entry with a generated name. Its executable is never launched.
fn entry(install: &std::path::Path) -> McpServerConfig {
    let executable = install.join(nonce());
    std::fs::write(&executable, b"").unwrap();
    McpServerConfig {
        name: format!("s{}", &nonce()[..8]),
        enabled: true,
        executable: executable.to_str().unwrap().into(),
        runtime_root: install.to_str().unwrap().into(),
        args: vec![],
        env: Default::default(),
        capabilities: [AUTHORITY].into(),
        project: None,
        default_policy: ToolPolicy::default(),
        tools: Default::default(),
        timeout_seconds: None,
        program_directories: false,
    }
}
impl Rig {
    async fn run(&self, body: Command) -> Reply {
        request(&self.gateway, body).await
    }
    async fn ok(&self, body: Command) {
        assert_eq!(self.run(body).await, Reply::Ok);
    }
    async fn grant(&self, capabilities: &[Capability]) {
        self.ok(Command::SetBackend {
            thread: self.thread,
            backend: CodingBackend::Native,
            capabilities: capabilities.iter().copied().collect(),
        })
        .await;
    }
    async fn save(&self, servers: Vec<McpServerConfig>) {
        let mut settings = self.gateway.snapshot().unwrap().settings;
        settings.mcp_servers = servers;
        self.ok(Command::SaveSettings(Box::new(settings))).await;
    }
    /// Serve the fixture in process and connect the entry to it. Aborting
    /// the returned task ends the server as a crash would.
    async fn attach(&self) -> tokio::task::JoinHandle<()> {
        let (client, server) = tokio::io::duplex(1 << 16);
        // Not awaited: the fixture ends when the session closes its end, or
        // when a test aborts it.
        let served = tokio::spawn(async move {
            // The fixture's own result is not under test here.
            let _ = fixture::serve(tokio::io::split(server)).await;
        });
        let session =
            a206_mcp::Session::start(tokio::io::split(client), None, a206_mcp::Limits::default())
                .await
                .unwrap();
        self.gateway
            .attach_mcp_session(&self.entry.name, session)
            .unwrap();
        served
    }
    fn tool(&self, tool: &str) -> String {
        tool_name(&self.entry.name, tool)
    }
    async fn permit(&self, tool: &str, permission: ToolPermission) {
        self.ok(Command::SetToolPermission {
            thread: self.thread,
            tool: self.tool(tool),
            permission: Some(permission),
        })
        .await;
    }
    /// The listing, without other hosts' unavailable sources: native exec
    /// is unavailable where it has no sandbox.
    async fn tools(&self) -> ToolList {
        let Reply::Tools(mut list) = self
            .run(Command::ListTools {
                thread: self.thread,
            })
            .await
        else {
            panic!("tool listing failed");
        };
        list.unavailable
            .retain(|u| u.source.starts_with(a206_runtime::mcp_tools::SOURCE));
        list
    }
    async fn offer(&self, tool: &str) -> Option<ToolOffer> {
        let list = self.tools().await;
        assert!(list.unavailable.is_empty(), "{:?}", list.unavailable);
        list.tools.into_iter().find(|t| t.name == self.tool(tool))
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
    /// Returns what the first request offered, the finished thread and the
    /// results the model was sent.
    async fn round_trip(&mut self, calls: &[Call]) -> (Vec<String>, Thread, Vec<Option<String>>) {
        let task = send(&self.gateway, self.thread);
        let first = next_request(&mut self.requests).await;
        let offered = offered(API, &first.body);
        first.respond(calls_response(API, calls, "")).await;
        let second = next_request(&mut self.requests).await;
        let results = calls
            .iter()
            .map(|call| result_for(API, &second.body, &call.id))
            .collect();
        second.respond(text_response(API, &nonce())).await;
        (offered, finished(task).await, results)
    }
    async fn until<T>(&self, what: &'static str, found: impl Fn(&Thread) -> Option<T>) -> T {
        tokio::time::timeout(PATIENCE, async {
            loop {
                if let Some(found) = found(&self.thread()) {
                    return found;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(wait(what))
    }
}

#[tokio::test]
async fn mcp_tools_are_denied_until_the_user_permits_them() {
    let mut rig = rig().await;
    let offer = rig
        .offer("echo")
        .await
        .expect("the fixture's tools are listed");
    assert_eq!(offer.permission, None);
    assert_eq!(offer.host_permission, ToolPermission::Ask);
    assert_eq!(offer.effective, ToolPermission::Deny);
    assert_eq!(offer.capabilities, [AUTHORITY].into());
    let call = Call::new(&rig.tool("echo"), json!({"text": nonce()}));
    let (offered, thread, results) = rig.round_trip(std::slice::from_ref(&call)).await;
    assert!(offered.is_empty(), "{offered:?}");
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Denied);
    assert_eq!(error_kind(record), ErrorKind::PermissionDenied);
    assert!(results[0].as_deref().unwrap().contains("not permitted"));
}

#[tokio::test]
async fn an_ask_tool_runs_after_the_user_approves_and_its_result_reaches_the_model() {
    let mut rig = rig().await;
    // The server's Ask caps the conversation's Allow.
    rig.permit("echo", ToolPermission::Allow).await;
    let offer = rig.offer("echo").await.unwrap();
    assert_eq!(offer.effective, ToolPermission::Ask);
    let text = nonce();
    let call = Call::new(&rig.tool("echo"), json!({"text": text}));
    let task = send(&rig.gateway, rig.thread);
    let first = next_request(&mut rig.requests).await;
    assert_eq!(offered(API, &first.body), vec![rig.tool("echo")]);
    first
        .respond(calls_response(API, std::slice::from_ref(&call), ""))
        .await;
    let approval = rig
        .until("an approval request", |t| t.approvals.first().cloned())
        .await;
    assert_eq!(approval.tool, rig.tool("echo"));
    assert_eq!(approval.capabilities, [AUTHORITY].into());
    rig.ok(Command::ResolveApproval {
        thread: rig.thread,
        approval: approval.id,
        digest: approval.digest.clone(),
        decision: ApprovalDecision::Approve,
    })
    .await;
    let second = next_request(&mut rig.requests).await;
    assert_eq!(result_for(API, &second.body, &call.id), Some(text.clone()));
    second.respond(text_response(API, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Succeeded);
    assert_eq!(record.output.as_deref(), Some(text.as_str()));
}

#[tokio::test]
async fn a_tool_reported_failure_reaches_the_model_as_a_failure() {
    let mut rig = rig().await;
    rig.entry.default_policy = ToolPolicy::Allow;
    rig.save(vec![rig.entry.clone()]).await;
    rig.attach().await;
    rig.permit("read_file", ToolPermission::Allow).await;
    let missing = format!("/{}", nonce());
    let call = Call::new(&rig.tool("read_file"), json!({"path": missing}));
    let (offered, thread, results) = rig.round_trip(std::slice::from_ref(&call)).await;
    assert_eq!(offered, vec![rig.tool("read_file")]);
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Failed, "{record:?}");
    // The model receives the tool's own text, which is the report's cause.
    let result = results[0].clone().unwrap();
    let report = record.error.as_ref().unwrap();
    assert_eq!(
        report.causes().next().map(|c| c.message.clone()),
        Some(result)
    );
}

#[tokio::test]
async fn the_servers_deny_caps_the_conversations_allow() {
    let mut rig = rig().await;
    rig.entry.tools.insert("echo".into(), ToolPolicy::Deny);
    rig.save(vec![rig.entry.clone()]).await;
    rig.attach().await;
    rig.permit("echo", ToolPermission::Allow).await;
    let offer = rig.offer("echo").await.expect("listed for the user to see");
    assert_eq!(offer.permission, Some(ToolPermission::Allow));
    assert_eq!(offer.host_permission, ToolPermission::Deny);
    assert_eq!(offer.effective, ToolPermission::Deny);
    let call = Call::new(&rig.tool("echo"), json!({"text": nonce()}));
    let (offered, thread, _) = rig.round_trip(std::slice::from_ref(&call)).await;
    assert!(offered.is_empty(), "{offered:?}");
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Denied);
    assert_eq!(error_kind(record), ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn a_conversation_without_the_servers_authority_is_not_offered_its_tools() {
    let mut rig = rig().await;
    rig.permit("echo", ToolPermission::Allow).await;
    assert!(rig.offer("echo").await.is_some());
    rig.grant(&[]).await;
    assert!(rig.offer("echo").await.is_none());
    let text = nonce();
    let call = Call::new(&rig.tool("echo"), json!({"text": text}));
    let (offered, thread, results) = rig.round_trip(std::slice::from_ref(&call)).await;
    assert!(offered.is_empty(), "{offered:?}");
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Denied);
    assert_eq!(error_kind(record), ErrorKind::NotFound);
    assert!(!results[0].as_deref().unwrap().contains(&text));
    // Granted again, the same session serves it.
    rig.grant(&[AUTHORITY]).await;
    assert!(rig.offer("echo").await.is_some());
}

#[tokio::test]
async fn an_unavailable_server_is_reported_beside_the_tools_that_work() {
    let rig = rig().await;
    let install = tempfile::tempdir().unwrap();
    let broken = entry(install.path());
    rig.save(vec![rig.entry.clone(), broken.clone()]).await;
    // A missing executable fails before any sandbox starts, on every
    // platform: this test binary cannot serve as the Linux sandbox helper.
    std::fs::remove_file(&broken.executable).unwrap();
    let list = rig.tools().await;
    assert!(
        list.tools.iter().any(|t| t.name == rig.tool("echo")),
        "{:?}",
        list.tools
    );
    let [failed] = list.unavailable.as_slice() else {
        panic!("one unavailable server: {:?}", list.unavailable);
    };
    assert_eq!(failed.source, format!("mcp__{}", broken.name));
    // The cause is the server's own start failure, kept as a chain.
    assert!(failed.error.causes().next().is_some(), "{failed:?}");
    assert_eq!(failed.error.kind, ErrorKind::NotFound);
    let status = rig
        .gateway
        .snapshot()
        .unwrap()
        .mcp_servers
        .into_iter()
        .find(|s| s.name == broken.name)
        .unwrap();
    assert_eq!(status.state, McpServerState::Failed);
}

#[tokio::test]
async fn cancelling_the_run_cancels_the_call_at_the_server() {
    let mut rig = rig().await;
    rig.entry.default_policy = ToolPolicy::Allow;
    rig.save(vec![rig.entry.clone()]).await;
    rig.attach().await;
    rig.permit("wait_cancelled", ToolPermission::Allow).await;
    let scratch = tempfile::tempdir().unwrap();
    let marker = scratch.path().join(nonce());
    let call = Call::new(
        &rig.tool("wait_cancelled"),
        json!({"path": marker.to_str().unwrap()}),
    );
    let task = send(&rig.gateway, rig.thread);
    let first = next_request(&mut rig.requests).await;
    first
        .respond(calls_response(API, std::slice::from_ref(&call), ""))
        .await;
    rig.until("the call to run", |t| {
        calls_of(t)
            .first()
            .is_some_and(|c| c.state == ToolCallState::Running)
            .then_some(())
    })
    .await;
    rig.ok(Command::CancelRun { thread: rig.thread }).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Cancelled);
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Cancelled);
    assert_eq!(error_kind(record), ErrorKind::Cancelled);
    // The fixture's handler runs only once the server hears the cancellation.
    tokio::time::timeout(PATIENCE, async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(wait("the server to receive the cancellation"));
    assert!(rig.requests.try_recv().is_err(), "no further model round");
}

/// How the user withdraws a server's tools without touching the
/// conversation's own permission.
#[derive(Debug, Clone, Copy)]
enum Withdrawal {
    Disconnect,
    Disable,
}
impl Rig {
    async fn withdraw(&mut self, how: Withdrawal) {
        match how {
            Withdrawal::Disconnect => {
                self.ok(Command::DisconnectMcpServer {
                    name: self.entry.name.clone(),
                })
                .await;
            }
            Withdrawal::Disable => {
                self.entry.enabled = false;
                self.save(vec![self.entry.clone()]).await;
            }
        }
    }
    fn status(&self) -> McpServerStatus {
        self.gateway
            .snapshot()
            .unwrap()
            .mcp_servers
            .into_iter()
            .find(|s| s.name == self.entry.name)
            .unwrap()
    }
}

#[tokio::test]
async fn withdrawing_the_server_stops_its_running_call() {
    for how in [Withdrawal::Disconnect, Withdrawal::Disable] {
        let mut rig = rig().await;
        rig.entry.default_policy = ToolPolicy::Allow;
        rig.save(vec![rig.entry.clone()]).await;
        rig.attach().await;
        rig.permit("wait", ToolPermission::Allow).await;
        let call = Call::new(&rig.tool("wait"), json!({}));
        let task = send(&rig.gateway, rig.thread);
        let first = next_request(&mut rig.requests).await;
        first
            .respond(calls_response(API, std::slice::from_ref(&call), ""))
            .await;
        rig.until("the call to run", |t| {
            calls_of(t)
                .first()
                .is_some_and(|c| c.state == ToolCallState::Running)
                .then_some(())
        })
        .await;
        // The conversation still permits the tool; only the user's authority
        // over the server itself is withdrawn.
        rig.withdraw(how).await;
        let second = next_request(&mut rig.requests).await;
        let result = result_for(API, &second.body, &call.id).unwrap();
        assert!(result.contains("withdrawn"), "{how:?}: {result}");
        assert!(offered(API, &second.body).is_empty(), "{how:?}");
        second.respond(text_response(API, &nonce())).await;
        let thread = finished(task).await;
        assert_eq!(
            thread.state,
            RunState::Complete,
            "{how:?}: {:?}",
            thread.error
        );
        let record = &calls_of(&thread)[0];
        assert_eq!(record.state, ToolCallState::Denied, "{how:?}");
        assert_eq!(error_kind(record), ErrorKind::PermissionDenied, "{how:?}");
        // The session the user stopped is not reported as a server failure.
        let status = rig.status();
        assert_eq!(status.state, McpServerState::Disconnected, "{how:?}");
        assert_eq!(status.failure, None, "{how:?}");
    }
}

#[tokio::test]
async fn a_server_that_fails_during_a_call_is_reported_as_a_failure_not_a_withdrawal() {
    let mut rig = rig().await;
    rig.entry.default_policy = ToolPolicy::Allow;
    rig.save(vec![rig.entry.clone()]).await;
    let served = rig.attach().await;
    rig.permit("wait", ToolPermission::Allow).await;
    let call = Call::new(&rig.tool("wait"), json!({}));
    let task = send(&rig.gateway, rig.thread);
    let first = next_request(&mut rig.requests).await;
    first
        .respond(calls_response(API, std::slice::from_ref(&call), ""))
        .await;
    rig.until("the call to run", |t| {
        calls_of(t)
            .first()
            .is_some_and(|c| c.state == ToolCallState::Running)
            .then_some(())
    })
    .await;
    // The server goes away on its own; the user withdrew nothing, and its
    // (empty) executable cannot start again.
    served.abort();
    let second = next_request(&mut rig.requests).await;
    let result = result_for(API, &second.body, &call.id).unwrap();
    assert!(!result.contains("withdrawn"), "{result}");
    second.respond(text_response(API, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Failed);
    assert_ne!(error_kind(record), ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn disconnecting_the_server_withdraws_a_pending_approval() {
    let mut rig = rig().await;
    rig.permit("echo", ToolPermission::Allow).await;
    let call = Call::new(&rig.tool("echo"), json!({"text": nonce()}));
    let task = send(&rig.gateway, rig.thread);
    let first = next_request(&mut rig.requests).await;
    first
        .respond(calls_response(API, std::slice::from_ref(&call), ""))
        .await;
    let approval = rig
        .until("an approval request", |t| t.approvals.first().cloned())
        .await;
    rig.withdraw(Withdrawal::Disconnect).await;
    let second = next_request(&mut rig.requests).await;
    let result = result_for(API, &second.body, &call.id).unwrap();
    assert!(result.contains("withdrawn"), "{result}");
    second.respond(text_response(API, &nonce())).await;
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
    assert!(thread.approvals.is_empty());
    let record = &calls_of(&thread)[0];
    assert_eq!(record.state, ToolCallState::Denied);
    // A late approval finds nothing to approve.
    let late = rig
        .run(Command::ResolveApproval {
            thread: rig.thread,
            approval: approval.id,
            digest: approval.digest,
            decision: ApprovalDecision::Approve,
        })
        .await;
    assert!(matches!(late, Reply::Error(_)), "{late:?}");
}
