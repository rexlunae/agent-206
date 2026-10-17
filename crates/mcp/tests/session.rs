//! Sessions against the fixture server binary, as a real child process.
use a206_mcp::{Content, Limits, McpError, Session};
use common::{FIXTURE, arguments};
use std::process::Stdio;

mod common;

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

#[tokio::test]
async fn lists_and_calls_tools() {
    let (session, _child) = direct(Limits::default()).await.unwrap();
    let mut names: Vec<_> = session.tools().iter().map(|t| t.name.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "connect",
            "echo",
            "exit",
            "flood",
            "read_file",
            "run",
            "wait"
        ]
    );
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

/// Answers initialization and the tool listing, then stops reading, as a
/// wedged server would: its input pipe fills and writes to it block.
async fn wedged() -> Session {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (client, server) = tokio::io::duplex(4096);
    // Ends when the session closes its side.
    tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server);
        let mut lines = tokio::io::BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let message: serde_json::Value = serde_json::from_str(&line).unwrap();
            let result = match message["method"].as_str() {
                Some("initialize") => serde_json::json!({
                    "protocolVersion": message["params"]["protocolVersion"],
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "wedged", "version": "1" },
                }),
                Some("tools/list") => serde_json::json!({
                    "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }],
                }),
                _ => continue,
            };
            let reply =
                serde_json::json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            if message["method"] == "tools/list" {
                // Keep the pipe open but never read again.
                std::future::pending::<()>().await;
            }
        }
    });
    let (read, write) = tokio::io::split(client);
    Session::start((read, write), None, Limits::default())
        .await
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn the_call_timeout_covers_a_send_that_blocks() {
    let session = wedged().await;
    let text = "x".repeat(64 * 1024);
    let result = session
        .call(
            "echo",
            arguments(serde_json::json!({ "text": text })),
            Some(std::time::Duration::from_secs(5)),
        )
        .await;
    assert!(matches!(result, Err(McpError::Timeout(_))), "{result:?}");
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
