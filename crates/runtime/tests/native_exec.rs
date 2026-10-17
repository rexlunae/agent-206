//! The native `execute_command` tool through the real OS sandbox (bubblewrap
//! and Landlock on Linux, Seatbelt on macOS). The harness is custom because on
//! Linux this binary is re-run as the sandbox's helper.
#[cfg(any(target_os = "linux", target_os = "macos"))]
use a206_core::{Capability, ErrorKind, Id};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use a206_runtime::{
    exec_tool::{ExecHost, MAX_INPUT, MAX_PER_THREAD, MAX_STREAM, NAME, PROCESS, PROCESS_WRITE},
    tools::{ToolHost, ToolOutput, ToolRequest, ToolScope},
};

fn main() -> std::process::ExitCode {
    if let Some(result) = a206_sandbox::dispatch() {
        return result.expect("sandbox helper could not report to the gateway");
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(commands_hold_only_the_conversations_grants());
        runtime.block_on(network_follows_the_grant());
        runtime.block_on(output_status_and_time_are_bounded());
        runtime.block_on(background_sessions_follow_their_conversation());
        runtime.block_on(background_sessions_stop_when_their_grant_is_withdrawn());
        runtime.block_on(stalled_input_fails_within_its_deadline());
        runtime.block_on(background_sessions_end_with_their_host());
        println!("native exec: ok");
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn scope(workspace: Option<&std::path::Path>, capabilities: &[Capability]) -> ToolScope {
    ToolScope {
        thread: Id::new_v4(),
        workspace: workspace.map(Into::into),
        capabilities: capabilities.iter().copied().collect(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn run(
    host: &ExecHost,
    scope: &ToolScope,
    arguments: serde_json::Value,
) -> Result<ToolOutput, a206_core::ErrorReport> {
    call(host, scope, NAME, arguments).await
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn call(
    host: &ExecHost,
    scope: &ToolScope,
    name: &str,
    arguments: serde_json::Value,
) -> Result<ToolOutput, a206_core::ErrorReport> {
    let serde_json::Value::Object(arguments) = arguments else {
        panic!("arguments are an object")
    };
    host.call(
        scope,
        ToolRequest {
            call: Id::new_v4().to_string(),
            name: name.into(),
            arguments,
            approved: true,
        },
    )
    .await
}

/// Start `command` in the background; returns its session id.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn spawn(host: &ExecHost, scope: &ToolScope, arguments: serde_json::Value) -> String {
    let mut arguments = arguments;
    arguments["background"] = true.into();
    let output = run(host, scope, arguments).await.unwrap();
    assert!(!output.is_error, "{}", output.text);
    let id = output
        .text
        .split_whitespace()
        .find_map(|word| word.trim_end_matches(';').parse::<Id>().ok())
        .unwrap_or_else(|| panic!("no session id in {}", output.text));
    id.to_string()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn process(
    host: &ExecHost,
    scope: &ToolScope,
    action: &str,
    session: &str,
) -> Result<ToolOutput, a206_core::ErrorReport> {
    call(
        host,
        scope,
        PROCESS,
        serde_json::json!({ "action": action, "session": session }),
    )
    .await
}

/// Wait, without polling its output, until the session has finished.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn idle(host: &ExecHost, scope: &ToolScope, session: &str) {
    for _ in 0..200 {
        let listed = call(
            host,
            scope,
            PROCESS,
            serde_json::json!({ "action": "list" }),
        )
        .await
        .unwrap();
        let line = listed
            .text
            .lines()
            .find(|line| line.starts_with(session))
            .unwrap_or_else(|| panic!("{session} is not listed: {}", listed.text));
        if !line.ends_with("  running") {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("session {session} did not finish")
}

/// Poll until the session has finished, collecting its output.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn finish(host: &ExecHost, scope: &ToolScope, session: &str) -> ToolOutput {
    let mut text = String::new();
    for _ in 0..200 {
        let output = process(host, scope, "poll", session).await.unwrap();
        assert!(output.text.len() < 2 * MAX_STREAM, "{}", output.text.len());
        text.push_str(&output.text);
        if !output.text.starts_with("running") {
            return ToolOutput {
                text,
                is_error: output.is_error,
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("session {session} did not finish: {text}")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn sh(host: &ExecHost, scope: &ToolScope, command: &str) -> ToolOutput {
    run(host, scope, serde_json::json!({ "command": command }))
        .await
        .unwrap()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

    // A background process left behind cannot keep the call open: it ends
    // with the shell, well before this timeout.
    let output = run(
        &host,
        &scope,
        serde_json::json!({ "command": "(sleep 60 &); echo left", "timeout_secs": 30 }),
    )
    .await
    .unwrap();
    assert!(!output.is_error, "{}", output.text);
    assert!(output.text.contains("left"));

    let error = run(
        &host,
        &scope,
        serde_json::json!({ "command": "sleep 60", "timeout_secs": 1 }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn background_sessions_follow_their_conversation() {
    let state = tempfile::tempdir().unwrap();
    let host = ExecHost::new(state.path());
    let scope = scope(None, &[Capability::Execute]);
    let marker = Id::new_v4().to_string();

    // Input reaches the command, and its output and status come back.
    let session = spawn(
        &host,
        &scope,
        serde_json::json!({ "command": "read line; echo got:$line; echo late >&2; exit 4" }),
    )
    .await;
    let other = self::scope(None, &[Capability::Execute]);
    for action in ["poll", "kill", "remove"] {
        assert_eq!(
            process(&host, &other, action, &session)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::NotFound,
            "another conversation must not reach the session"
        );
    }
    assert_eq!(
        call(
            &host,
            &other,
            PROCESS,
            serde_json::json!({ "action": "list" })
        )
        .await
        .unwrap()
        .text,
        "No background sessions."
    );
    let listed = call(
        &host,
        &scope,
        PROCESS,
        serde_json::json!({ "action": "list" }),
    )
    .await
    .unwrap();
    assert!(listed.text.contains(&session), "{}", listed.text);
    let wrote = call(
        &host,
        &scope,
        PROCESS_WRITE,
        serde_json::json!({ "session": session, "data": format!("{marker}\n") }),
    )
    .await
    .unwrap();
    assert!(!wrote.is_error, "{}", wrote.text);
    let finished = finish(&host, &scope, &session).await;
    assert!(finished.is_error);
    assert!(
        finished.text.contains("exit status: 4"),
        "{}",
        finished.text
    );
    assert!(finished.text.contains(&format!("got:{marker}")));
    assert!(finished.text.contains("late"));
    assert_eq!(
        call(
            &host,
            &scope,
            PROCESS_WRITE,
            serde_json::json!({ "session": session, "data": "more" }),
        )
        .await
        .unwrap_err()
        .kind,
        ErrorKind::Conflict,
        "a finished session takes no input"
    );

    // Output nobody polls stays bounded.
    let session = spawn(
        &host,
        &scope,
        serde_json::json!({ "command": "yes | head -c 1000000" }),
    )
    .await;
    // Listing does not drain, so the whole output waits for one poll.
    idle(&host, &scope, &session).await;
    let polled = process(&host, &scope, "poll", &session).await.unwrap();
    assert!(polled.text.contains("earlier stdout bytes dropped"));
    assert!(polled.text.len() < 2 * MAX_STREAM, "{}", polled.text.len());

    // Kill and the timeout both stop a command.
    let session = spawn(&host, &scope, serde_json::json!({ "command": "sleep 600" })).await;
    process(&host, &scope, "kill", &session).await.unwrap();
    assert!(
        finish(&host, &scope, &session)
            .await
            .text
            .contains("killed")
    );
    assert_eq!(
        process(&host, &scope, "kill", &session)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Conflict,
        "a finished session cannot be stopped again"
    );
    let session = spawn(
        &host,
        &scope,
        serde_json::json!({ "command": "sleep 600", "timeout_secs": 1 }),
    )
    .await;
    assert!(
        finish(&host, &scope, &session)
            .await
            .text
            .contains("timeout")
    );

    // A conversation's running sessions are capped; finished ones make room.
    process(&host, &scope, "remove", &session).await.unwrap();
    let listed = call(
        &host,
        &scope,
        PROCESS,
        serde_json::json!({ "action": "list" }),
    )
    .await
    .unwrap();
    let held = listed.text.lines().count();
    let mut running = Vec::new();
    for _ in held..MAX_PER_THREAD {
        running.push(spawn(&host, &scope, serde_json::json!({ "command": "sleep 600" })).await);
    }
    // Every slot holds a session; the finished ones give theirs up.
    for _ in 0..held {
        running.push(spawn(&host, &scope, serde_json::json!({ "command": "sleep 600" })).await);
    }
    let refused = run(
        &host,
        &scope,
        serde_json::json!({ "command": "sleep 600", "background": true }),
    )
    .await
    .unwrap_err();
    assert_eq!(refused.kind, ErrorKind::Conflict);
    process(&host, &scope, "remove", &running[0]).await.unwrap();
    spawn(&host, &scope, serde_json::json!({ "command": "true" })).await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn background_sessions_stop_when_their_grant_is_withdrawn() {
    let state = tempfile::tempdir().unwrap();
    let host = ExecHost::new(state.path());
    let granted = scope(None, &[Capability::Execute, Capability::Network]);
    let kept = spawn(
        &host,
        &granted,
        serde_json::json!({ "command": "sleep 600" }),
    )
    .await;
    let other = scope(None, &[Capability::Execute]);
    let unaffected = spawn(&host, &other, serde_json::json!({ "command": "sleep 600" })).await;
    // A wider grant, or another conversation's change, leaves it running.
    let mut wider = granted.clone();
    wider.capabilities.insert(Capability::ReadWorkspace);
    host.rescope(&|id| {
        (id == granted.thread)
            .then(|| wider.clone())
            .or(Some(other.clone()))
    });
    let polled = process(&host, &granted, "poll", &kept).await.unwrap();
    assert!(polled.text.starts_with("running"), "{}", polled.text);
    // Withdrawing Network stops it; the other conversation keeps its own.
    let mut narrower = granted.clone();
    narrower.capabilities.remove(&Capability::Network);
    host.rescope(&|id| {
        Some(if id == granted.thread {
            narrower.clone()
        } else {
            other.clone()
        })
    });
    let ended = finish(&host, &granted, &kept).await;
    assert!(ended.is_error);
    assert!(
        ended.text.contains("permissions were withdrawn"),
        "{}",
        ended.text
    );
    let polled = process(&host, &other, "poll", &unaffected).await.unwrap();
    assert!(polled.text.starts_with("running"), "{}", polled.text);
    // A conversation that is gone takes its sessions with it.
    host.rescope(&|_| None);
    let ended = finish(&host, &other, &unaffected).await;
    assert!(
        ended.text.contains("permissions were withdrawn"),
        "{}",
        ended.text
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn stalled_input_fails_within_its_deadline() {
    let state = tempfile::tempdir().unwrap();
    let host = ExecHost::new(state.path());
    let scope = scope(None, &[Capability::Execute]);
    // The command never reads, so its pipe fills and later writes queue.
    let session = spawn(&host, &scope, serde_json::json!({ "command": "sleep 600" })).await;
    let data = "x".repeat(MAX_INPUT);
    let writes = (0..7).map(|_| {
        call(
            &host,
            &scope,
            PROCESS_WRITE,
            serde_json::json!({ "session": session, "data": data }),
        )
    });
    // Each write has its own short deadline, queueing included; this bound
    // only catches a write that waits for a place in the queue forever.
    let results = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        futures_util::future::join_all(writes),
    )
    .await
    .expect("a write waited past its deadline");
    let stalled = results
        .iter()
        .filter(|result| matches!(result, Err(error) if error.kind == ErrorKind::Timeout))
        .count();
    assert!(stalled >= 5, "{results:?}");
    // The rest were written; nothing failed another way.
    assert_eq!(results.iter().filter(|r| r.is_err()).count(), stalled);
    process(&host, &scope, "kill", &session).await.unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn background_sessions_end_with_their_host() {
    use std::io::Read;
    let state = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let host = ExecHost::new(state.path());
    let scope = scope(
        Some(project.path()),
        &[
            Capability::Execute,
            Capability::ReadWorkspace,
            Capability::WriteWorkspace,
        ],
    );
    // The command holds the writing end of a FIFO for as long as it runs,
    // so the reader sees its end as end of file rather than after a delay.
    // Its child starts before it reports, so the host is not dropped while
    // the shell is still forking: on macOS a process forked as its group is
    // killed can escape the kill.
    let fifo = project.path().join("held");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    spawn(
        &host,
        &scope,
        serde_json::json!({ "command": "exec 3> held; sleep 600 & echo started >&3; wait" }),
    )
    .await;
    let (running, started) = tokio::sync::oneshot::channel();
    let (ended, rest) = tokio::sync::oneshot::channel();
    // A plain thread, so a failure does not wait for the command to end.
    std::thread::spawn(move || {
        let mut held = std::fs::File::open(fifo).unwrap();
        let mut first = [0u8; 8];
        held.read_exact(&mut first).unwrap();
        running.send(first).unwrap();
        let mut rest = Vec::new();
        held.read_to_end(&mut rest).unwrap();
        // The test may have given up already.
        a206_core::Ignore::ignore(ended.send(rest));
    });
    let patience = std::time::Duration::from_secs(60);
    let first = tokio::time::timeout(patience, started)
        .await
        .expect("the command did not start")
        .unwrap();
    assert_eq!(&first, b"started\n");
    drop(host);
    // The read ends only once nothing of the command is left; the bound
    // only catches a session that outlives its host.
    let Ok(rest) = tokio::time::timeout(patience, rest).await else {
        let processes = std::process::Command::new("ps")
            .args(["-axo", "pid,ppid,pgid,stat,command"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
        let left: Vec<_> = processes
            .lines()
            .filter(|l| l.contains("sleep 600"))
            .collect();
        panic!("a session must not outlive the gateway's host: {left:#?}");
    };
    let rest = rest.unwrap();
    assert!(rest.is_empty());
}
