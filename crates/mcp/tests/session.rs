//! Sessions against the fixture server binary, as a real child process.
use a206_core::{Capability, McpServerConfig, ToolPolicy};
use a206_mcp::{Content, Limits, McpError, Session};
use std::process::Stdio;

const FIXTURE: &str = env!("CARGO_BIN_EXE_a206-mcp-fixture");

async fn direct(limits: Limits) -> Result<(Session, tokio::process::Child), McpError> {
    let mut child = tokio::process::Command::new(FIXTURE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let transport = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
    Ok((Session::start(transport, None, limits).await?, child))
}

fn arguments(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let serde_json::Value::Object(map) = value else {
        panic!("arguments are an object")
    };
    map
}

#[tokio::test]
async fn lists_and_calls_tools() {
    let (session, _child) = direct(Limits::default()).await.unwrap();
    let mut names: Vec<_> = session.tools().iter().map(|t| t.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["echo", "exit", "flood", "read_file", "wait"]);
    let echo = session.tools().iter().find(|t| t.name == "echo").unwrap();
    let schema: serde_json::Value = serde_json::from_str(&echo.input_schema).unwrap();
    assert!(schema["properties"]["text"].is_object());

    let text = uuid::Uuid::new_v4().to_string();
    let output = session
        .call("echo", arguments(serde_json::json!({ "text": text })), None)
        .await
        .unwrap();
    assert!(!output.is_error);
    assert_eq!(output.content, [Content::Text(text)]);

    let missing = uuid::Uuid::new_v4().to_string();
    let output = session
        .call(
            "read_file",
            arguments(serde_json::json!({ "path": format!("/{missing}") })),
            None,
        )
        .await
        .unwrap();
    assert!(output.is_error, "a tool-reported failure stays a failure");

    assert!(matches!(
        session.call(&missing, serde_json::Map::new(), None).await,
        Err(McpError::UnknownTool)
    ));
}

#[tokio::test]
async fn bounds_tool_output() {
    let limits = Limits {
        max_output_bytes: 4096,
        ..Limits::default()
    };
    let (session, _child) = direct(limits).await.unwrap();
    session
        .call("flood", arguments(serde_json::json!({ "bytes": 64 })), None)
        .await
        .unwrap();
    assert!(matches!(
        session
            .call(
                "flood",
                arguments(serde_json::json!({ "bytes": 8192 })),
                None
            )
            .await,
        Err(McpError::OutputTooLarge { limit: 4096 })
    ));
}

#[tokio::test]
async fn a_revoked_call_is_cancelled_and_the_session_stays_usable() {
    let (session, _child) = direct(Limits::default()).await.unwrap();
    let (revoke, revoked) = tokio::sync::oneshot::channel::<()>();
    let call = session.call_until("wait", serde_json::Map::new(), None, async {
        // The sender is dropped below, which also counts as revocation.
        let _revoked = revoked.await.is_ok();
    });
    drop(revoke);
    assert!(matches!(call.await, Err(McpError::Revoked)));
    let text = uuid::Uuid::new_v4().to_string();
    let output = session
        .call("echo", arguments(serde_json::json!({ "text": text })), None)
        .await
        .unwrap();
    assert_eq!(output.content, [Content::Text(text)]);
    assert!(!session.is_closed());
}

#[tokio::test]
async fn a_server_that_exits_closes_its_session() {
    let (session, _child) = direct(Limits::default()).await.unwrap();
    assert!(!session.is_closed());
    assert!(matches!(
        session.call("exit", serde_json::Map::new(), None).await,
        Err(McpError::Service(_))
    ));
    session.closed().await;
    assert!(session.is_closed());
}

#[tokio::test]
async fn bounds_the_tool_listing() {
    let limits = Limits {
        max_tools: 2,
        ..Limits::default()
    };
    assert!(matches!(
        direct(limits).await,
        Err(McpError::TooManyTools { limit: 2 })
    ));
}

fn fixture_entry(capabilities: &[Capability]) -> McpServerConfig {
    let executable = std::fs::canonicalize(FIXTURE).unwrap();
    McpServerConfig {
        name: "fixture".into(),
        enabled: true,
        runtime_root: executable.parent().unwrap().to_str().unwrap().into(),
        executable: executable.to_str().unwrap().into(),
        args: vec![],
        env: Default::default(),
        capabilities: capabilities.iter().copied().collect(),
        project: None,
        default_policy: ToolPolicy::Ask,
        tools: Default::default(),
        timeout_seconds: None,
    }
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn refuses_to_start_servers_without_os_isolation() {
    let state = tempfile::tempdir().unwrap();
    let result = a206_mcp::spawn(&fixture_entry(&[]), None, state.path(), Limits::default()).await;
    assert!(matches!(result, Err(McpError::Unsupported)));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandboxed_servers_see_only_their_project() {
    let state = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("file.txt");
    let text = uuid::Uuid::new_v4().to_string();
    std::fs::write(&secret, &text).unwrap();
    let read =
        |path: &std::path::Path| arguments(serde_json::json!({ "path": path.to_str().unwrap() }));

    let session = a206_mcp::spawn(&fixture_entry(&[]), None, state.path(), Limits::default())
        .await
        .unwrap();
    let echo = uuid::Uuid::new_v4().to_string();
    let output = session
        .call("echo", arguments(serde_json::json!({ "text": echo })), None)
        .await
        .unwrap();
    assert_eq!(output.content, [Content::Text(echo)]);
    let output = session
        .call("read_file", read(&secret), None)
        .await
        .unwrap();
    assert!(output.is_error, "files outside the sandbox stay unreadable");
    let output = session
        .call("read_file", read(&state.path().join("state.db")), None)
        .await
        .unwrap();
    assert!(output.is_error);

    // A symlink inside the project cannot reach outside it: Seatbelt checks
    // the resolved path, for links to files and to directories alike.
    let project = tempfile::tempdir().unwrap();
    let inside = project.path().join("own.txt");
    let own = uuid::Uuid::new_v4().to_string();
    std::fs::write(&inside, &own).unwrap();
    std::os::unix::fs::symlink(&secret, project.path().join("link.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path(), project.path().join("dir")).unwrap();
    let mut entry = fixture_entry(&[Capability::ReadWorkspace]);
    entry.project = Some(uuid::Uuid::new_v4());
    let session = a206_mcp::spawn(
        &entry,
        Some(project.path()),
        state.path(),
        Limits::default(),
    )
    .await
    .unwrap();
    let output = session
        .call("read_file", read(&inside), None)
        .await
        .unwrap();
    assert_eq!(output.content, [Content::Text(own)]);
    for link in ["link.txt", "dir/file.txt"] {
        let output = session
            .call("read_file", read(&project.path().join(link)), None)
            .await
            .unwrap();
        assert!(output.is_error, "{link} leads outside the project");
    }

    let session = a206_mcp::spawn(
        &entry,
        Some(outside.path()),
        state.path(),
        Limits::default(),
    )
    .await
    .unwrap();
    let output = session
        .call("read_file", read(&secret), None)
        .await
        .unwrap();
    assert_eq!(output.content, [Content::Text(text)]);
}
