//! Real subprocess/protocol tests; all model facts are discovered from a live API.
use a206_core::*;
use a206_protocol::Request;
use a206_runtime::gateway::Gateway;
use axum::{Json, Router, routing::get};
use serde_json::{Value, json};
use std::path::Path;
async fn command(gateway: &Gateway, body: Command) -> Reply {
    gateway
        .request(Request {
            id: Id::new_v4(),
            body,
        })
        .await
        .body
}
fn literal(value: impl AsRef<str>) -> String {
    format!("{:?}", value.as_ref())
}
fn compile_fixture(
    root: &Path,
    model: &str,
    text: &str,
    state: &Path,
    outside: &Path,
    project_root: &Path,
) -> std::path::PathBuf {
    let thread = Id::new_v4().to_string();
    let turn = Id::new_v4().to_string();
    let session = Id::new_v4().to_string();
    let item = Id::new_v4().to_string();
    let json_literal = |value: Value| literal(value.to_string());
    let blocked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let blocked_address = blocked.local_addr().unwrap().to_string();
    let source = r#"
use std::io::{self, BufRead, Read, Write};
fn receive() -> String { let mut line = String::new(); io::stdin().lock().read_line(&mut line).unwrap(); assert!(!line.is_empty()); line }
fn send(value: &str) { println!("{value}"); io::stdout().flush().unwrap(); }
fn answer(template: &str, request: &str) {
    let id = request.split("\"id\":").nth(1).unwrap().split(',').next().unwrap();
    send(&template.replace("\"REPLY_ID\"", id));
}
fn check_scope(prompt: &str) {
    if prompt.contains("fixture-read") || prompt.contains("fixture-write") {
        assert!(std::fs::read(PROJECT_FILE).is_ok());
        assert!(std::fs::read(PROJECT_LINK).is_err());
        assert_eq!(std::fs::write(PROJECT_FILE, "edited").is_ok(), prompt.contains("fixture-write"));
    }
}
fn main() {
    if std::env::args().any(|arg| arg == "--fixture-descendant") { loop { std::thread::sleep(std::time::Duration::from_secs(1)); } }
    assert!(std::env::var("HOME").unwrap().contains("a206-backend-"));
    assert!(std::fs::read(STATE_FILE).is_err());
    assert!(std::fs::metadata(STATE_FILE).is_err());
    assert!(std::fs::write(STATE_FILE, "should-not-write").is_err());
    assert!(std::fs::read(OUTSIDE_FILE).is_err());
    assert!(std::fs::metadata(OUTSIDE_FILE).is_err());
    assert!(std::fs::symlink_metadata(OUTSIDE_FILE).is_err());
    assert!(std::fs::read(format!("/System/Volumes/Data{}", OUTSIDE_FILE)).is_err());
    assert!(std::fs::read(format!("/System/Volumes/Data{}", STATE_FILE)).is_err());
    assert!(std::net::TcpStream::connect(BLOCKED_ADDRESS).is_err());
    let (base, token) = if let Ok(base) = std::env::var("ANTHROPIC_BASE_URL") { (base, std::env::var("ANTHROPIC_API_KEY").unwrap()) }
        else {
            let config = std::fs::read_to_string(format!("{}/config.toml", std::env::var("CODEX_HOME").unwrap())).unwrap();
            let base = config.lines().find(|line| line.starts_with("base_url =")).unwrap().split('"').nth(1).unwrap().to_owned();
            (base, std::env::var("A206_PROXY_TOKEN").unwrap())
        };
    let address = base.strip_prefix("http://").unwrap().split('/').next().unwrap();
    let mut socket = std::net::TcpStream::connect(address).unwrap();
    write!(socket, "GET /v1/models HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = String::new(); socket.read_to_string(&mut response).unwrap(); assert!(response.contains(MODEL));
    let mut first = receive();
    let claude = first.contains("protocolVersion");
    if claude {
        answer(CLAUDE_INIT, &first);
        first = receive();
        assert!(first.contains(MODEL)); assert!(first.contains("settingSources")); assert!(first.contains("bare"));
        answer(CLAUDE_SESSION, &first);
        let prompt = receive(); assert!(prompt.contains("session/prompt")); check_scope(&prompt);
        send(CLAUDE_PERMISSION);
        let denied = receive(); assert!(denied.contains("cancelled"));
        send(CLAUDE_FOREIGN);
        send(CLAUDE_TEXT);
        answer(CLAUDE_DONE, &prompt);
    } else {
        assert!(first.contains("initialize")); send(CODEX_INIT);
        assert!(receive().contains("initialized"));
        let start = receive(); assert!(start.contains(MODEL)); assert!(start.contains("baseInstructions")); send(CODEX_THREAD);
        let prompt = receive(); assert!(prompt.contains("turn/start")); assert!(prompt.contains(MODEL)); check_scope(&prompt); send(CODEX_TURN);
        if prompt.contains("fixture-malformed") { send("{"); std::thread::sleep(std::time::Duration::from_secs(60)); return; }
        if prompt.contains("fixture-oversized") { send(&"x".repeat(2 * 1024 * 1024 + 1)); std::thread::sleep(std::time::Duration::from_secs(60)); return; }
        if prompt.contains("fixture-wait") {
            let child = std::process::Command::new(std::env::current_exe().unwrap()).arg("--fixture-descendant").spawn().unwrap();
            let kind = format!("fixture:{}:{}", std::process::id(), child.id());
            send(&WAIT_EVENT.replace("WAIT_KIND", &kind));
            loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
        }
        send(CODEX_PERMISSION); assert!(receive().contains("decline"));
        send(CODEX_FOREIGN); send(CODEX_DELTA); send(CODEX_TEXT); send(CODEX_DONE);
    }
    // Keep the process alive to verify owned-process teardown after completion.
    std::thread::sleep(std::time::Duration::from_secs(60));
}
"#;
    let replacements = [
        (
            "PROJECT_FILE",
            literal(project_root.join("source").to_str().unwrap()),
        ),
        (
            "PROJECT_LINK",
            literal(project_root.join("link").to_str().unwrap()),
        ),
        (
            "WAIT_EVENT",
            json_literal(
                json!({"method": "item/started", "params": {"threadId": thread, "turnId": turn, "item": {"type": "WAIT_KIND"}}}),
            ),
        ),
        ("BLOCKED_ADDRESS", literal(&blocked_address)),
        (
            "STATE_FILE",
            literal(state.join("private-marker").to_str().unwrap()),
        ),
        ("OUTSIDE_FILE", literal(outside.to_str().unwrap())),
        ("MODEL", literal(model)),
        (
            "CLAUDE_INIT",
            json_literal(
                json!({"jsonrpc": "2.0", "id": "REPLY_ID", "result": {"protocolVersion": 1, "agentCapabilities": {}}}),
            ),
        ),
        (
            "CLAUDE_SESSION",
            json_literal(
                json!({"jsonrpc": "2.0", "id": "REPLY_ID", "result": {"sessionId": session}}),
            ),
        ),
        (
            "CLAUDE_PERMISSION",
            json_literal(
                json!({"jsonrpc": "2.0", "id": 99, "method": "session/request_permission", "params": {"sessionId": session, "toolCall": {"toolCallId": item}, "options": []}}),
            ),
        ),
        (
            "CLAUDE_FOREIGN",
            json_literal(
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": Id::new_v4().to_string(), "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": Id::new_v4().to_string()}}}}),
            ),
        ),
        (
            "CLAUDE_TEXT",
            json_literal(
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}}),
            ),
        ),
        (
            "CLAUDE_DONE",
            json_literal(
                json!({"jsonrpc": "2.0", "id": "REPLY_ID", "result": {"stopReason": "end_turn"}}),
            ),
        ),
        ("CODEX_INIT", json_literal(json!({"id": 1, "result": {}}))),
        (
            "CODEX_THREAD",
            json_literal(json!({"id": 2, "result": {"thread": {"id": thread}}})),
        ),
        (
            "CODEX_TURN",
            json_literal(json!({"id": 3, "result": {"turn": {"id": turn}}})),
        ),
        (
            "CODEX_PERMISSION",
            json_literal(
                json!({"id": 99, "method": "item/commandExecution/requestApproval", "params": {"threadId": thread, "turnId": turn}}),
            ),
        ),
        (
            "CODEX_FOREIGN",
            json_literal(
                json!({"method": "item/agentMessage/delta", "params": {"threadId": Id::new_v4().to_string(), "turnId": turn, "itemId": item, "delta": Id::new_v4().to_string()}}),
            ),
        ),
        (
            "CODEX_DELTA",
            json_literal(
                json!({"method": "item/agentMessage/delta", "params": {"threadId": thread, "turnId": turn, "itemId": item, "delta": Id::new_v4().to_string()}}),
            ),
        ),
        (
            "CODEX_TEXT",
            json_literal(
                json!({"method": "item/completed", "params": {"threadId": thread, "turnId": turn, "item": {"id": item, "type": "agentMessage", "text": text}}}),
            ),
        ),
        (
            "CODEX_DONE",
            json_literal(
                json!({"method": "turn/completed", "params": {"threadId": thread, "turn": {"id": turn, "status": "completed"}}}),
            ),
        ),
    ];
    let mut source = source.to_owned();
    for (from, to) in replacements {
        source = source.replace(from, &to);
    }
    let path = root.join("fixture.rs");
    std::fs::write(&path, source).unwrap();
    let executable = root.join("fixture");
    let status = std::process::Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&executable)
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    executable
}
pub async fn drives_both_agent_protocols_in_the_real_os_sandbox() {
    let directory = tempfile::tempdir().unwrap();
    let installation = directory.path().join("runtime");
    std::fs::create_dir(&installation).unwrap();
    let project_root = directory.path().join("project");
    std::fs::create_dir(&project_root).unwrap();
    let state = project_root.join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("private-marker"), Id::new_v4().to_string()).unwrap();
    let outside = directory.path().join("ungranted-file");
    std::fs::write(&outside, Id::new_v4().to_string()).unwrap();
    std::fs::write(project_root.join("source"), Id::new_v4().to_string()).unwrap();
    std::os::unix::fs::symlink(&outside, project_root.join("link")).unwrap();
    let model = Id::new_v4().to_string();
    let text = Id::new_v4().to_string();
    let executable = compile_fixture(
        &installation,
        &model,
        &text,
        &state,
        &outside,
        &project_root,
    );
    let script = installation.join("adapter.js");
    std::fs::write(&script, "").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let returned_model = model.clone();
    let router = Router::new().route(
        "/v1/models",
        get(move || {
            let model = returned_model.clone();
            async move { Json(json!({"data": [{"id": model}]})) }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let gateway = Gateway::open(&state).unwrap();
    let mut settings = Settings::default();
    for (kind, api) in [
        (CodingBackend::Codex, ProviderApi::OpenAi),
        (CodingBackend::ClaudeAgent, ProviderApi::Anthropic),
    ] {
        settings.providers.push(ProviderConfig {
            id: Id::new_v4(),
            label: Id::new_v4().to_string(),
            base_url: format!("http://{address}/v1"),
            credential: None,
            api,
        });
        settings.backends.push(BackendConfig {
            kind,
            executable: executable.to_string_lossy().into(),
            runtime_root: installation.to_string_lossy().into(),
            adapter_script: (kind == CodingBackend::ClaudeAgent)
                .then(|| script.to_string_lossy().into()),
        });
    }
    assert_eq!(
        command(&gateway, Command::SaveSettings(settings.clone())).await,
        Reply::Ok
    );
    for (kind, provider) in [CodingBackend::Codex, CodingBackend::ClaudeAgent]
        .into_iter()
        .zip(settings.providers)
    {
        let Reply::Catalog(catalog) = command(
            &gateway,
            Command::RefreshCatalog {
                provider: provider.id,
            },
        )
        .await
        else {
            panic!("expected API catalog");
        };
        let Reply::Thread(thread) = command(
            &gateway,
            Command::CreateThread {
                title: Id::new_v4().to_string(),
                mode: Mode::Code,
            },
        )
        .await
        else {
            panic!();
        };
        command(
            &gateway,
            Command::ConfigureThread {
                id: thread.id,
                mode: thread.mode,
                model: Some(catalog.models[0].key.clone()),
                subagent_models: Default::default(),
            },
        )
        .await;
        assert_eq!(
            command(
                &gateway,
                Command::SetBackend {
                    thread: thread.id,
                    backend: kind,
                    capabilities: Default::default()
                }
            )
            .await,
            Reply::Ok
        );
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            command(
                &gateway,
                Command::Send {
                    thread: thread.id,
                    text: Id::new_v4().to_string(),
                    attachments: vec![],
                },
            ),
        )
        .await
        .unwrap();
        let Reply::Thread(result) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(result.state, RunState::Complete, "{:?}", result.messages);
        let assistants: Vec<_> = result
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .collect();
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0].text, text);
        assert_eq!(result.backend, kind);
        assert!(matches!(
            command(
                &gateway,
                Command::SetBackend {
                    thread: thread.id,
                    backend: kind,
                    capabilities: [Capability::Network].into()
                }
            )
            .await,
            Reply::Error { .. }
        ));
    }
    // Stop an active turn only after its root and descendant have actually started.
    let provider = gateway.snapshot().unwrap().settings.providers[0].id;
    let key = gateway
        .snapshot()
        .unwrap()
        .catalogs
        .iter()
        .find(|c| c.provider == provider)
        .unwrap()
        .models[0]
        .key
        .clone();
    for behavior in [
        "fixture-malformed",
        "fixture-oversized",
        "fixture-wait",
        "fixture-wait-revoke",
    ] {
        let Reply::Thread(thread) = command(
            &gateway,
            Command::CreateThread {
                title: Id::new_v4().to_string(),
                mode: Mode::Code,
            },
        )
        .await
        else {
            panic!();
        };
        command(
            &gateway,
            Command::ConfigureThread {
                id: thread.id,
                mode: thread.mode,
                model: Some(key.clone()),
                subagent_models: Default::default(),
            },
        )
        .await;
        command(
            &gateway,
            Command::SetBackend {
                thread: thread.id,
                backend: CodingBackend::Codex,
                capabilities: Default::default(),
            },
        )
        .await;
        let actor = gateway.clone();
        let job = tokio::spawn(async move {
            command(
                &actor,
                Command::Send {
                    thread: thread.id,
                    text: behavior.into(),
                    attachments: vec![],
                },
            )
            .await
        });
        if behavior.starts_with("fixture-wait") {
            let pids = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let Reply::Events(events) =
                        command(&gateway, Command::Events { after: 0 }).await
                    else {
                        panic!();
                    };
                    if let Some(title) = events.iter().find_map(|event| match &event.kind {
                        EventKind::BackendActivity { thread: id, title }
                            if *id == thread.id && title.starts_with("Codex: fixture:") =>
                        {
                            Some(title.clone())
                        }
                        _ => None,
                    }) {
                        break title
                            .trim_start_matches("Codex: fixture:")
                            .split(':')
                            .map(|pid| pid.parse::<i32>().unwrap())
                            .collect::<Vec<_>>();
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let old_backends = gateway.snapshot().unwrap().settings.backends.clone();
            if behavior.ends_with("revoke") {
                let mut settings = gateway.snapshot().unwrap().settings;
                settings
                    .backends
                    .iter_mut()
                    .find(|b| b.kind == CodingBackend::Codex)
                    .unwrap()
                    .executable = installation
                    .join("changed-executable")
                    .to_string_lossy()
                    .into();
                assert_eq!(
                    command(&gateway, Command::SaveSettings(settings)).await,
                    Reply::Ok
                );
            } else {
                assert_eq!(
                    command(&gateway, Command::CancelRun { thread: thread.id }).await,
                    Reply::Ok
                );
            }
            let Reply::Thread(result) =
                tokio::time::timeout(std::time::Duration::from_secs(5), job)
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!();
            };
            assert_eq!(result.state, RunState::Cancelled);
            if behavior.ends_with("revoke") {
                let mut settings = gateway.snapshot().unwrap().settings;
                settings.backends = old_backends;
                assert_eq!(
                    command(&gateway, Command::SaveSettings(settings)).await,
                    Reply::Ok
                );
            }
            // The root is reaped; the process group is killed, including the descendant.
            #[cfg(target_os = "macos")]
            {
                assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pids[0]), None).is_err());
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pids[1]), None).is_ok()
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
            }
            // Linux runtimes report PIDs from their own namespace, so look for
            // any host process still running the fixture instead.
            #[cfg(target_os = "linux")]
            {
                assert_eq!(pids.len(), 2);
                let fixture = std::fs::canonicalize(&executable).unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while running(&fixture) {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
            }
        } else {
            let Reply::Thread(result) =
                tokio::time::timeout(std::time::Duration::from_secs(5), job)
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!();
            };
            assert_eq!(result.state, RunState::Failed);
        }
    }
    for (behavior, capabilities) in [
        (
            "fixture-read",
            [Capability::ReadWorkspace].into_iter().collect(),
        ),
        (
            "fixture-write",
            [Capability::ReadWorkspace, Capability::WriteWorkspace]
                .into_iter()
                .collect(),
        ),
    ] {
        let Reply::Project(project) = command(
            &gateway,
            Command::CreateProject {
                name: Id::new_v4().to_string(),
                root: project_root.to_string_lossy().into(),
            },
        )
        .await
        else {
            panic!();
        };
        command(
            &gateway,
            Command::ConfigureThread {
                id: project.manager,
                mode: Mode::Code,
                model: Some(key.clone()),
                subagent_models: Default::default(),
            },
        )
        .await;
        assert_eq!(
            command(
                &gateway,
                Command::SetBackend {
                    thread: project.manager,
                    backend: CodingBackend::Codex,
                    capabilities
                }
            )
            .await,
            Reply::Ok
        );
        let Reply::Thread(result) = command(
            &gateway,
            Command::Send {
                thread: project.manager,
                text: behavior.into(),
                attachments: vec![],
            },
        )
        .await
        else {
            panic!();
        };
        assert_eq!(result.state, RunState::Complete, "{:?}", result.messages);
    }
    server.abort();
}
/// Whether any host process was started from `executable`.
#[cfg(target_os = "linux")]
fn running(executable: &Path) -> bool {
    std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
        std::fs::read(entry.path().join("cmdline")).is_ok_and(|cmdline| {
            cmdline.split(|byte| *byte == 0).next()
                == Some(executable.as_os_str().as_encoded_bytes())
        })
    })
}
