//! MCP servers started through the real OS sandbox. The harness is custom
//! because on Linux this binary is re-run as the sandbox's in-namespace helper.
use a206_core::{Capability, McpServerConfig, ToolPolicy};
use a206_mcp::{Content, Limits, Session};
use common::{FIXTURE, arguments};
use std::path::Path;

mod common;

fn main() -> std::process::ExitCode {
    if let Some(result) = a206_sandbox::dispatch() {
        return result.expect("sandbox helper could not report to the gateway");
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        runtime.block_on(sandboxed_servers_see_only_their_project());
        runtime.block_on(network_and_programs_follow_their_grants());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    runtime.block_on(refuses_to_start_servers_without_os_isolation());
    println!("sandboxed MCP servers: ok");
    std::process::ExitCode::SUCCESS
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

async fn spawn(entry: &McpServerConfig, project: Option<&Path>, state: &Path) -> Session {
    a206_mcp::spawn(entry, project, state, Limits::default())
        .await
        .unwrap()
}

/// Call `tool` with `argument` set to `value`, returning the text or the
/// tool-reported failure.
async fn call(session: &Session, tool: &str, argument: &str, value: &str) -> Result<String, ()> {
    let output = session
        .call(
            tool,
            arguments(serde_json::json!({ argument: value })),
            None,
        )
        .await
        .unwrap();
    match (output.is_error, output.content.as_slice()) {
        (false, [Content::Text(text)]) => Ok(text.clone()),
        (true, _) => Err(()),
        (false, content) => panic!("{tool} returned {content:?}"),
    }
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn refuses_to_start_servers_without_os_isolation() {
    let state = tempfile::tempdir().unwrap();
    let result = a206_mcp::spawn(&fixture_entry(&[]), None, state.path(), Limits::default()).await;
    assert!(matches!(result, Err(a206_mcp::McpError::Unsupported)));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn sandboxed_servers_see_only_their_project() {
    let state = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("file.txt");
    let text = uuid::Uuid::new_v4().to_string();
    std::fs::write(&secret, &text).unwrap();

    let session = spawn(&fixture_entry(&[]), None, state.path()).await;
    let echo = uuid::Uuid::new_v4().to_string();
    assert_eq!(call(&session, "echo", "text", &echo).await, Ok(echo));
    assert!(
        call(&session, "read_file", "path", path(&secret))
            .await
            .is_err(),
        "files outside the sandbox stay unreadable"
    );
    let private = state.path().join("state.db");
    std::fs::write(&private, &text).unwrap();
    assert!(
        call(&session, "read_file", "path", path(&private))
            .await
            .is_err()
    );

    // A symlink inside the project cannot reach outside it, for links to
    // files and to directories alike.
    let project = tempfile::tempdir().unwrap();
    let inside = project.path().join("own.txt");
    let own = uuid::Uuid::new_v4().to_string();
    std::fs::write(&inside, &own).unwrap();
    std::os::unix::fs::symlink(&secret, project.path().join("link.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path(), project.path().join("dir")).unwrap();
    let mut entry = fixture_entry(&[Capability::ReadWorkspace]);
    entry.project = Some(uuid::Uuid::new_v4());
    let session = spawn(&entry, Some(project.path()), state.path()).await;
    assert_eq!(
        call(&session, "read_file", "path", path(&inside)).await,
        Ok(own)
    );
    for link in ["link.txt", "dir/file.txt"] {
        assert!(
            call(
                &session,
                "read_file",
                "path",
                path(&project.path().join(link))
            )
            .await
            .is_err(),
            "{link} leads outside the project"
        );
    }

    let session = spawn(&entry, Some(outside.path()), state.path()).await;
    assert_eq!(
        call(&session, "read_file", "path", path(&secret)).await,
        Ok(text)
    );

    // Private state stays hidden even inside a granted project.
    let parent = tempfile::tempdir().unwrap();
    let nested = parent.path().join("state");
    std::fs::create_dir(&nested).unwrap();
    let private = nested.join("state.db");
    std::fs::write(&private, uuid::Uuid::new_v4().to_string()).unwrap();
    let session = spawn(&entry, Some(parent.path()), &nested).await;
    assert!(
        call(&session, "read_file", "path", path(&private))
            .await
            .is_err()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn network_and_programs_follow_their_grants() {
    let state = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    // A program that exists on every supported system and needs nothing else.
    let program = "/bin/sh";

    let session = spawn(&fixture_entry(&[]), None, state.path()).await;
    assert!(
        call(&session, "connect", "address", &address)
            .await
            .is_err(),
        "a server without Network reaches nothing"
    );
    assert!(
        call(&session, "run", "program", program).await.is_err(),
        "a server without Execute runs only its own installation"
    );

    let entry = fixture_entry(&[Capability::Network, Capability::Execute]);
    let session = spawn(&entry, None, state.path()).await;
    assert_eq!(
        call(&session, "connect", "address", &address).await,
        Ok("connected".into())
    );
    // `sh` with no input exits successfully.
    assert_eq!(
        call(&session, "run", "program", program).await,
        Ok("exit status: 0".into())
    );
}
