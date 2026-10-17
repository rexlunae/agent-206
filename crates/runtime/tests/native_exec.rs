//! The native `execute_command` tool through the real OS sandbox. The harness
//! is custom because on Linux this binary is re-run as the sandbox's helper.
#[cfg(target_os = "linux")]
use a206_core::{Capability, ErrorKind, Id};
#[cfg(target_os = "linux")]
use a206_runtime::{
    exec_tool::{ExecHost, MAX_STREAM, NAME},
    tools::{ToolHost, ToolOutput, ToolRequest, ToolScope},
};

fn main() -> std::process::ExitCode {
    if let Some(result) = a206_sandbox::dispatch() {
        return result.expect("sandbox helper could not report to the gateway");
    }
    #[cfg(target_os = "linux")]
    {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(commands_hold_only_the_conversations_grants());
        runtime.block_on(network_follows_the_grant());
        runtime.block_on(output_status_and_time_are_bounded());
        println!("native exec: ok");
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(target_os = "linux")]
fn scope(workspace: Option<&std::path::Path>, capabilities: &[Capability]) -> ToolScope {
    ToolScope {
        thread: Id::new_v4(),
        workspace: workspace.map(Into::into),
        capabilities: capabilities.iter().copied().collect(),
    }
}

#[cfg(target_os = "linux")]
async fn run(
    host: &ExecHost,
    scope: &ToolScope,
    arguments: serde_json::Value,
) -> Result<ToolOutput, a206_core::ErrorReport> {
    let serde_json::Value::Object(arguments) = arguments else {
        panic!("arguments are an object")
    };
    host.call(
        scope,
        ToolRequest {
            call: Id::new_v4().to_string(),
            name: NAME.into(),
            arguments,
            approved: true,
        },
    )
    .await
}

#[cfg(target_os = "linux")]
async fn sh(host: &ExecHost, scope: &ToolScope, command: &str) -> ToolOutput {
    run(host, scope, serde_json::json!({ "command": command }))
        .await
        .unwrap()
}

#[cfg(target_os = "linux")]
async fn commands_hold_only_the_conversations_grants() {
    let state = tempfile::tempdir().unwrap();
    let host = ExecHost::new(state.path());
    let project = tempfile::tempdir().unwrap();
    let own = Id::new_v4().to_string();
    std::fs::write(project.path().join("own.txt"), &own).unwrap();
    std::fs::create_dir(project.path().join("sub")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, Id::new_v4().to_string()).unwrap();
    let private = state.path().join("state.db");
    std::fs::write(&private, Id::new_v4().to_string()).unwrap();

    // Execute alone: a private empty directory, no project.
    let bare = scope(Some(project.path()), &[Capability::Execute]);
    let output = sh(&host, &bare, "pwd; cat own.txt").await;
    assert!(output.is_error, "{}", output.text);
    assert!(!output.text.contains(&own));

    let read = scope(
        Some(project.path()),
        &[Capability::Execute, Capability::ReadWorkspace],
    );
    let output = sh(&host, &read, "cat own.txt").await;
    assert!(!output.is_error, "{}", output.text);
    assert!(output.text.contains(&own));
    for path in [&secret, &private] {
        let output = sh(&host, &read, &format!("cat '{}'", path.display())).await;
        assert!(output.is_error, "{} must stay hidden", path.display());
    }
    let output = sh(&host, &read, "echo x > new.txt").await;
    assert!(output.is_error, "read access must not write");
    assert!(!project.path().join("new.txt").exists());

    let output = run(
        &host,
        &read,
        serde_json::json!({ "command": "pwd", "working_dir": "sub" }),
    )
    .await
    .unwrap();
    assert!(output.text.contains(&format!(
        "{}\n",
        std::fs::canonicalize(project.path().join("sub"))
            .unwrap()
            .display()
    )));
    std::os::unix::fs::symlink(outside.path(), project.path().join("away")).unwrap();
    let error = run(
        &host,
        &read,
        serde_json::json!({ "command": "pwd", "working_dir": "away" }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidRequest);

    let write = scope(
        Some(project.path()),
        &[
            Capability::Execute,
            Capability::ReadWorkspace,
            Capability::WriteWorkspace,
        ],
    );
    let output = sh(&host, &write, "echo written > new.txt").await;
    assert!(!output.is_error, "{}", output.text);
    assert_eq!(
        std::fs::read_to_string(project.path().join("new.txt")).unwrap(),
        "written\n"
    );

    // State nested inside a writable project stays hidden too.
    let nested = project.path().join("state");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("state.db"), Id::new_v4().to_string()).unwrap();
    let host = ExecHost::new(&nested);
    let output = sh(&host, &write, "cat state/state.db").await;
    assert!(output.is_error, "{}", output.text);

    // Neither the state directory nor any descendant may be rebound as the
    // readable project (which would hide it from the nested-path mask).
    std::fs::create_dir(nested.join("project")).unwrap();
    for root in [&nested, &nested.join("project")] {
        let scope = scope(
            Some(root),
            &[Capability::Execute, Capability::ReadWorkspace],
        );
        let error = run(&host, &scope, serde_json::json!({"command": "pwd"}))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::PermissionDenied);
    }
}

#[cfg(target_os = "linux")]
async fn network_follows_the_grant() {
    let state = tempfile::tempdir().unwrap();
    let host = ExecHost::new(state.path());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // bash's /dev/tcp opens a TCP connection without any other program.
    let connect = format!("bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}'");
    let offline = sh(&host, &scope(None, &[Capability::Execute]), &connect).await;
    assert!(offline.is_error, "{}", offline.text);
    let online = sh(
        &host,
        &scope(None, &[Capability::Execute, Capability::Network]),
        &connect,
    )
    .await;
    assert!(!online.is_error, "{}", online.text);
}

#[cfg(target_os = "linux")]
async fn output_status_and_time_are_bounded() {
    let state = tempfile::tempdir().unwrap();
    let host = ExecHost::new(state.path());
    let scope = scope(None, &[Capability::Execute]);
    let marker = Id::new_v4().to_string();
    let output = sh(&host, &scope, &format!("echo {marker} >&2; exit 3")).await;
    assert!(output.is_error);
    assert!(output.text.contains("exit status: 3"), "{}", output.text);
    assert!(output.text.contains(&marker));

    let output = sh(&host, &scope, "yes | head -c 1000000").await;
    assert!(!output.is_error, "{}", output.text);
    assert!(output.text.contains("[stdout cut to"));
    assert!(output.text.len() < 2 * MAX_STREAM);

    let output = sh(
        &host,
        &scope,
        "head -c 40000 /dev/zero | tr '\\000' '\\377'; echo preserved-stderr >&2",
    )
    .await;
    assert!(output.text.len() <= 64 * 1024);
    assert!(output.text.contains("[stdout cut to"));
    assert!(output.text.contains("preserved-stderr"));

    let error = run(
        &host,
        &scope,
        serde_json::json!({ "command": "sleep 60", "timeout_secs": 1 }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
}
