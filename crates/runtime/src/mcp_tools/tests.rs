//! Naming, specs and results, and calls through the gateway's MCP call path
//! against the fixture server served in process. Native runs are in
//! `tests/mcp_tools.rs`.
use super::*;
use crate::gateway::Gateway;
use a206_core::{
    Capability, CodingBackend, Command, Id, McpServerConfig, McpToolInfo, Mode, Reply,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};

#[path = "../../../mcp/src/bin/a206-mcp-fixture/server.rs"]
mod fixture;

fn nonce() -> String {
    Id::new_v4().simple().to_string()
}
/// A valid, generated MCP server name.
fn server_name() -> String {
    format!("s{}", &nonce()[..8])
}
fn info(name: &str) -> McpToolInfo {
    McpToolInfo {
        name: name.into(),
        title: None,
        description: None,
        input_schema: json!({"type": "object"}).to_string(),
    }
}
fn offer(server: &str, tool: McpToolInfo, policy: ToolPolicy) -> McpOffer {
    McpOffer {
        server: server.into(),
        tool,
        policy,
        capabilities: BTreeSet::new(),
    }
}
fn kinds(unavailable: &[UnavailableTools]) -> Vec<ErrorKind> {
    unavailable.iter().map(|u| u.error.kind).collect()
}

#[test]
fn names_are_namespaced_and_never_truncated_or_shared() {
    let server = format!("a-{}", &nonce()[..6]);
    let other = server_name();
    let own = nonce();
    let dotted = format!("{}.{}", &nonce()[..6], &nonce()[..6]);
    let underscored = dotted.replace('.', "_");
    let long = "x".repeat(tools::MAX_NAME);
    let failure = crate::error::report(&GatewayError::from(McpError::Unsupported));
    let listed = offers(McpOffers {
        tools: [own.as_str(), &dotted, &underscored, "", &long]
            .into_iter()
            .map(|name| offer(&server, info(name), ToolPolicy::Allow))
            .collect(),
        unavailable: vec![(other.clone(), failure.clone())],
    });
    let name = tool_name(&server, &own);
    assert_eq!(name, format!("mcp__{server}__{own}"));
    assert_eq!(listed.tools.keys().collect::<Vec<_>>(), [&name]);
    assert_eq!(server_of(&name), Some(server.as_str()));
    assert_eq!(server_of(&own), None);
    // A failed server keeps its own report as the cause; both colliding
    // tools are refused; the empty and the overlong names are refused.
    let mut found = kinds(&listed.unavailable);
    found.sort_by_key(|kind| format!("{kind:?}"));
    let mut expected = vec![
        ErrorKind::Unavailable,
        ErrorKind::Conflict,
        ErrorKind::Conflict,
        ErrorKind::InvalidRequest,
        ErrorKind::InvalidRequest,
    ];
    expected.sort_by_key(|kind| format!("{kind:?}"));
    assert_eq!(found, expected, "{:?}", listed.unavailable);
    let failed = listed
        .unavailable
        .iter()
        .find(|u| u.source == format!("mcp__{other}"))
        .unwrap();
    let cause = failed.error.causes().next().unwrap();
    assert_eq!(
        (cause.kind, &cause.message),
        (failure.kind, &failure.message)
    );
    let collided: Vec<_> = listed
        .unavailable
        .iter()
        .filter(|u| u.error.kind == ErrorKind::Conflict)
        .map(|u| u.error.message.clone())
        .collect();
    assert!(collided.iter().any(|m| m.contains(&format!("`{dotted}`"))));
    assert!(
        collided
            .iter()
            .any(|m| m.contains(&format!("`{underscored}`")))
    );
}

#[test]
fn the_server_policy_is_the_host_policy_and_mcp_defaults_to_ask() {
    assert_eq!(ToolPolicy::default(), ToolPolicy::Ask);
    let server = server_name();
    for (policy, expected) in [
        (ToolPolicy::Allow, ToolPermission::Allow),
        (ToolPolicy::Ask, ToolPermission::Ask),
        (ToolPolicy::Deny, ToolPermission::Deny),
    ] {
        let mut tool = info(&nonce());
        let description = nonce();
        tool.description = Some(description.clone());
        let mut offered = offer(&server, tool, policy);
        offered.capabilities = [Capability::Execute].into();
        let spec = spec(nonce(), offered).unwrap();
        assert_eq!(spec.permission, expected);
        assert_eq!(spec.capabilities, [Capability::Execute].into());
        assert_eq!(spec.description, description);
        assert_eq!(spec.parameters, json!({"type": "object"}));
    }
    let mut broken = info(&nonce());
    broken.input_schema = nonce();
    let error = spec(nonce(), offer(&server, broken, ToolPolicy::Allow)).unwrap_err();
    assert_eq!(crate::error::report(&error).kind, ErrorKind::Protocol);
}

#[test]
fn results_keep_tool_errors_and_describe_binary_content() {
    let text = nonce();
    let output = output_text(a206_mcp::ToolOutput {
        is_error: true,
        content: vec![
            Content::Text(text.clone()),
            Content::Image {
                mime_type: "image/png".into(),
                data: nonce(),
            },
        ],
        structured: None,
    });
    assert!(output.is_error);
    let (first, second) = output.text.split_once('\n').unwrap();
    assert_eq!(first, text);
    assert!(second.contains("image/png") && second.contains("not shown"));
    let value = json!({ nonce(): nonce() });
    let structured = output_text(a206_mcp::ToolOutput {
        is_error: false,
        content: vec![],
        structured: Some(value.clone()),
    });
    assert!(!structured.is_error);
    assert_eq!(
        serde_json::from_str::<Value>(&structured.text).unwrap(),
        value
    );
}

// ── Through the gateway ─────────────────────────────────────────────────────

struct Rig {
    _state: tempfile::TempDir,
    _install: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    gateway: Gateway,
    host: McpToolHost,
    entry: McpServerConfig,
    thread: Id,
}
async fn request(gateway: &Gateway, body: Command) -> Reply {
    gateway
        .request(a206_protocol::Request {
            id: Id::new_v4(),
            body,
        })
        .await
        .body
}
/// A Code project conversation granted `execute`, and an enabled server holding
/// `execute` whose session is the fixture.
async fn rig(policy: ToolPolicy) -> Rig {
    let state = tempfile::tempdir().unwrap();
    let install = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = install.path().join("server");
    std::fs::write(&executable, b"").unwrap();
    let gateway = Gateway::open(state.path()).unwrap();
    let entry = McpServerConfig {
        name: server_name(),
        enabled: true,
        executable: executable.to_str().unwrap().into(),
        runtime_root: install.path().to_str().unwrap().into(),
        args: vec![],
        env: Default::default(),
        capabilities: [Capability::Execute].into(),
        project: None,
        default_policy: policy,
        tools: Default::default(),
        timeout_seconds: None,
        program_directories: false,
    };
    // Only a project conversation can be granted `execute`.
    let Reply::Project(project) = request(
        &gateway,
        Command::CreateProject {
            name: nonce(),
            root: workspace.path().display().to_string(),
        },
    )
    .await
    else {
        panic!("project created");
    };
    let rig = Rig {
        _state: state,
        _install: install,
        _workspace: workspace,
        host: McpToolHost::new(gateway.mcp_client()),
        gateway,
        entry,
        thread: project.manager,
    };
    let configured = request(
        &rig.gateway,
        Command::ConfigureThread {
            id: rig.thread,
            mode: Mode::Code,
            model: None,
            subagent_models: Default::default(),
        },
    )
    .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    let granted = request(
        &rig.gateway,
        Command::SetBackend {
            thread: rig.thread,
            backend: CodingBackend::Native,
            capabilities: [Capability::Execute].into(),
        },
    )
    .await;
    assert_eq!(granted, Reply::Ok);
    rig.save().await;
    rig
}
impl Rig {
    /// Save the entry and connect a fresh fixture session to it.
    async fn save(&self) {
        let mut settings = self.gateway.snapshot().unwrap().settings;
        settings.mcp_servers = vec![self.entry.clone()];
        assert_eq!(
            request(&self.gateway, Command::SaveSettings(Box::new(settings))).await,
            Reply::Ok
        );
        let (client, server) = tokio::io::duplex(1 << 16);
        // Detached: the fixture ends when the session closes its end.
        tokio::spawn(fixture::serve(tokio::io::split(server)));
        let session =
            a206_mcp::Session::start(tokio::io::split(client), None, a206_mcp::Limits::default())
                .await
                .unwrap();
        self.gateway
            .attach_mcp_session(&self.entry.name, session)
            .unwrap();
    }
    fn scope(&self, capabilities: &[Capability]) -> ToolScope {
        ToolScope {
            thread: self.thread,
            workspace: None,
            capabilities: capabilities.iter().copied().collect(),
            programs: vec![],
        }
    }
    fn name(&self, tool: &str) -> String {
        tool_name(&self.entry.name, tool)
    }
    async fn call(
        &self,
        capabilities: &[Capability],
        tool: &str,
        arguments: Value,
        approved: bool,
    ) -> Result<ToolOutput, ErrorReport> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        let request = ToolRequest {
            call: nonce(),
            name: self.name(tool),
            arguments,
            approved,
        };
        self.host.call(&self.scope(capabilities), request).await
    }
}
const EXECUTE: &[Capability] = &[Capability::Execute];

#[tokio::test]
async fn listing_follows_the_scope_and_the_server_entry() {
    let rig = rig(ToolPolicy::Allow).await;
    let listing = rig.host.list(&rig.scope(EXECUTE)).await;
    assert!(listing.unavailable.is_empty(), "{:?}", listing.unavailable);
    let echo = listing
        .tools
        .iter()
        .find(|t| t.name == rig.name("echo"))
        .expect("the fixture's tools are listed");
    assert_eq!(echo.permission, ToolPermission::Allow);
    assert_eq!(echo.capabilities, EXECUTE.iter().copied().collect());
    assert_eq!(echo.parameters["properties"]["text"]["type"], "string");
    // A scope without the server's authority lists nothing from it.
    let listing = rig.host.list(&rig.scope(&[])).await;
    assert!(listing.tools.is_empty() && listing.unavailable.is_empty());
}

#[tokio::test]
async fn calls_reach_the_server_only_within_the_scope_and_the_entry() {
    let mut rig = rig(ToolPolicy::Allow).await;
    let text = nonce();
    let output = rig
        .call(EXECUTE, "echo", json!({ "text": text }), false)
        .await
        .unwrap();
    assert_eq!(
        output,
        ToolOutput {
            text,
            is_error: false
        }
    );
    // The scope is a ceiling even when the thread's own grant covers the server.
    let denied = rig
        .call(&[], "echo", json!({ "text": nonce() }), false)
        .await
        .unwrap_err();
    assert_eq!(denied.kind, ErrorKind::PermissionDenied);
    // A tool's own failure is a result, not a host failure.
    let missing = format!("/{}", nonce());
    let failed = rig
        .call(EXECUTE, "read_file", json!({ "path": missing }), false)
        .await
        .unwrap();
    assert!(failed.is_error);
    // Unknown names, and servers without a live session, are not reached.
    let unknown = rig
        .call(EXECUTE, &nonce(), json!({}), false)
        .await
        .unwrap_err();
    assert_eq!(unknown.kind, ErrorKind::NotFound);
    let request = ToolRequest {
        call: nonce(),
        name: tool_name(&server_name(), "echo"),
        arguments: Default::default(),
        approved: true,
    };
    let unconfigured = rig.host.call(&rig.scope(EXECUTE), request).await;
    assert_eq!(unconfigured.unwrap_err().kind, ErrorKind::NotFound);

    // The gateway's call path enforces the entry's current policy.
    rig.entry.default_policy = ToolPolicy::Ask;
    rig.save().await;
    let ask = rig
        .call(EXECUTE, "echo", json!({ "text": nonce() }), false)
        .await
        .unwrap_err();
    assert_eq!(ask.kind, ErrorKind::PermissionDenied);
    assert!(ask.detail().contains("approval"), "{}", ask.detail());
    let text = nonce();
    let approved = rig
        .call(EXECUTE, "echo", json!({ "text": text }), true)
        .await
        .unwrap();
    assert_eq!(approved.text, text);
    rig.entry.tools.insert("echo".into(), ToolPolicy::Deny);
    rig.save().await;
    let listing = rig.host.list(&rig.scope(EXECUTE)).await;
    let echo = listing.tools.iter().find(|t| t.name == rig.name("echo"));
    assert_eq!(echo.unwrap().permission, ToolPermission::Deny);
    let denied = rig
        .call(EXECUTE, "echo", json!({ "text": nonce() }), true)
        .await
        .unwrap_err();
    assert_eq!(denied.kind, ErrorKind::PermissionDenied);
    assert!(
        denied.detail().contains("denied by policy"),
        "{}",
        denied.detail()
    );
}

#[tokio::test]
async fn a_server_that_cannot_start_is_reported_with_its_cause() {
    let mut rig = rig(ToolPolicy::Allow).await;
    // Saving the changed entry stops its session, and without its executable
    // the next start fails before any sandbox starts, on every platform (this
    // test binary cannot serve as the Linux sandbox helper).
    std::fs::remove_file(&rig.entry.executable).unwrap();
    rig.entry.timeout_seconds = Some(1);
    let mut settings = rig.gateway.snapshot().unwrap().settings;
    settings.mcp_servers = vec![rig.entry.clone()];
    assert_eq!(
        request(&rig.gateway, Command::SaveSettings(Box::new(settings))).await,
        Reply::Ok
    );
    let listing = rig.host.list(&rig.scope(EXECUTE)).await;
    assert!(listing.tools.is_empty());
    let [failed] = listing.unavailable.as_slice() else {
        panic!("one failure: {:?}", listing.unavailable);
    };
    assert_eq!(failed.source, format!("mcp__{}", rig.entry.name));
    assert!(failed.error.causes().next().is_some());
    assert_eq!(failed.error.kind, ErrorKind::NotFound);
    let call = rig
        .call(EXECUTE, "echo", json!({ "text": nonce() }), false)
        .await
        .unwrap_err();
    assert_eq!(call.kind, ErrorKind::Unavailable);
}

#[tokio::test]
async fn dropping_a_call_cancels_it_at_the_server() {
    let rig = rig(ToolPolicy::Allow).await;
    let scratch = tempfile::tempdir().unwrap();
    let marker = scratch.path().join(nonce());
    let call = rig.call(
        EXECUTE,
        "wait_cancelled",
        json!({ "path": marker.to_str().unwrap() }),
        false,
    );
    let mut call = Box::pin(call);
    // One session sends in order, so once a later call is answered the
    // server has received this one.
    let text = nonce();
    tokio::select! {
        biased;
        _ = &mut call => panic!("the call waits for cancellation"),
        echo = rig.call(EXECUTE, "echo", json!({ "text": text }), false) => {
            assert_eq!(echo.unwrap().text, text);
        }
    }
    drop(call);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server is told the call was cancelled");
}
