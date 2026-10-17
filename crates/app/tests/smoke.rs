//! Real executable tests: shared gateway, WASM worker, ACP/MCP and managed Joshua.
//! Unix-only until the application's Windows gateway transport is implemented.
#![cfg(unix)]
#![forbid(unsafe_code)]
mod support;
// Include the standalone fixture for Cargo's formatting, lint and compile checks.
#[path = "fixtures/joshua.rs"]
#[allow(dead_code)]
mod joshua_fixture;
use a206_core::Reply;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, ffi::OsString, path::Path, process::Stdio, time::Duration};
use support::{App, TIMEOUT, capture, checked};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

struct Peer {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}
impl Peer {
    fn start(app: &App, args: &[&str]) -> Result<Self> {
        let mut child = app
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().context("protocol stdin missing")?;
        let output = BufReader::new(child.stdout.take().context("protocol stdout missing")?);
        Ok(Self {
            child,
            input,
            output,
            next_id: 0,
        })
    }
    async fn send(&mut self, message: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        Ok(())
    }
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        tokio::time::timeout(TIMEOUT, async {
            self.send(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
                .await?;
            let mut total = 0;
            for _ in 0..256 {
                let mut bytes = Vec::new();
                let count = (&mut self.output)
                    .take(2 * 1024 * 1024 + 1)
                    .read_until(b'\n', &mut bytes)
                    .await?;
                ensure!(count > 0, "protocol peer closed its output");
                total += count;
                ensure!(
                    count <= 2 * 1024 * 1024 && total <= 4 * 1024 * 1024,
                    "protocol response exceeds its limit"
                );
                let reply: Value = serde_json::from_slice(&bytes)?;
                if reply.get("id") == Some(&json!(id)) {
                    return Ok(reply);
                }
            }
            anyhow::bail!("too many unrelated protocol messages")
        })
        .await
        .context("protocol response timed out")?
    }
    async fn stop(&mut self) -> Result<()> {
        self.child.start_kill()?;
        tokio::time::timeout(TIMEOUT, self.child.wait()).await??;
        Ok(())
    }
}
fn result(reply: Value) -> Result<Value> {
    ensure!(reply.get("error").is_none(), "protocol error: {reply}");
    reply
        .get("result")
        .cloned()
        .context("protocol result missing")
}

/// The bundled WASM extension through the gateway and its worker process:
/// denied until granted, then file tools, a panel and persisted panel state.
fn refs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

async fn extension_smoke(app: &App) -> Result<()> {
    let workspace = tempfile::Builder::new().prefix("a206-ws-").tempdir()?;
    let body = uuid::Uuid::new_v4().to_string();
    std::fs::write(workspace.path().join("notes.txt"), &body)?;
    std::fs::write(workspace.path().join(".hidden"), "")?;
    let root = workspace.path().to_str().context("workspace path")?;
    let Reply::Project(project) =
        toml::from_str(&app.cli(&["project", "Extension smoke", root]).await?)?
    else {
        anyhow::bail!("expected project");
    };
    let project = project.id.to_string();
    let id = "agent206.workspace";
    let read = [
        "extension",
        "tool",
        id,
        "read_text_file",
        "--project",
        &project,
        "--args",
        "path = \"notes.txt\"",
    ];
    let denied = capture(app.command(&read)).await?;
    ensure!(
        !denied.status.success(),
        "an ungranted extension read a file"
    );
    ensure!(
        String::from_utf8_lossy(&denied.stderr).contains("permission"),
        "denial did not reach the client: {}",
        String::from_utf8_lossy(&denied.stderr)
    );
    app.cli(&["extension", "grant", id, "--capability", "read_workspace"])
        .await?;
    ensure!(
        app.cli(&read).await?.trim() == body,
        "tool output did not round-trip"
    );
    let escape = [
        "extension",
        "tool",
        id,
        "read_text_file",
        "--project",
        &project,
        "--args",
        "path = \"../escape\"",
    ];
    ensure!(
        !capture(app.command(&escape)).await?.status.success(),
        "a path escaped the workspace"
    );
    let panel = ["extension", "panel", id, "overview", "--project", &project];
    let first = app.cli(&panel).await?;
    ensure!(
        first.contains("notes.txt") && !first.contains(".hidden"),
        "unexpected panel: {first}"
    );
    let pressed = app
        .cli(&[&panel[..], &["--press", "toggle-hidden"]].concat())
        .await?;
    ensure!(
        pressed.contains(".hidden"),
        "panel event was ignored: {pressed}"
    );
    // The preference persisted in the extension's own state.
    ensure!(
        app.cli(&panel).await?.contains(".hidden"),
        "panel state was not kept"
    );
    let tool = |name: &str, args: String| -> Vec<String> {
        [
            "extension",
            "tool",
            id,
            name,
            "--project",
            &project,
            "--args",
            &args,
        ]
        .map(String::from)
        .to_vec()
    };
    let changed = uuid::Uuid::new_v4().to_string();
    let write = tool(
        "write_text_file",
        format!("path = \"made/new.txt\"\ntext = \"{body}\""),
    );
    ensure!(
        !capture(app.command(&refs(&write))).await?.status.success(),
        "a read-only grant wrote a file"
    );
    app.cli(&[
        "extension",
        "grant",
        id,
        "--capability",
        "read_workspace",
        "--capability",
        "write_workspace",
    ])
    .await?;
    app.cli(&refs(&write)).await?;
    let edit = tool(
        "edit_text_file",
        format!("path = \"made/new.txt\"\nold_text = \"{body}\"\nnew_text = \"{changed}\""),
    );
    app.cli(&refs(&edit)).await?;
    ensure!(
        std::fs::read_to_string(workspace.path().join("made/new.txt"))? == changed,
        "the write and edit did not reach the workspace"
    );
    let found = app
        .cli(&refs(&tool(
            "search_text",
            format!("query = \"{changed}\""),
        )))
        .await?;
    ensure!(
        found.contains("made/new.txt:1:"),
        "search missed the edit: {found}"
    );
    // The git extension reaches the gateway's git host calls; this
    // workspace has no repository, and the client hears why.
    app.cli(&[
        "extension",
        "grant",
        "agent206.git",
        "--capability",
        "git_read",
    ])
    .await?;
    let status = capture(app.command(&[
        "extension",
        "tool",
        "agent206.git",
        "git_status",
        "--project",
        &project,
    ]))
    .await?;
    ensure!(
        !status.status.success()
            && String::from_utf8_lossy(&status.stderr).contains("not a git repository"),
        "git status outside a repository: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    // The web extension's fetch goes through the gateway's SSRF guard: a
    // loopback address is refused before any connection, with or without
    // the grant, and the client hears the reason.
    let fetch = [
        "extension",
        "tool",
        "agent206.web",
        "fetch_url",
        "--args",
        "url = \"https://127.0.0.1/\"",
    ];
    let ungranted = capture(app.command(&fetch)).await?;
    ensure!(
        !ungranted.status.success()
            && String::from_utf8_lossy(&ungranted.stderr).contains("Use the network"),
        "fetch without the network grant: {}",
        String::from_utf8_lossy(&ungranted.stderr)
    );
    app.cli(&[
        "extension",
        "grant",
        "agent206.web",
        "--capability",
        "network",
    ])
    .await?;
    let private = capture(app.command(&fetch)).await?;
    ensure!(
        !private.status.success()
            && String::from_utf8_lossy(&private.stderr).contains("private, reserved or local"),
        "fetch of a loopback address: {}",
        String::from_utf8_lossy(&private.stderr)
    );
    app.cli(&["extension", "grant", id]).await?;
    ensure!(
        !capture(app.command(&panel)).await?.status.success(),
        "revoking a grant had no effect"
    );
    installed_extension_smoke(app, &project, &body).await
}

/// A third-party package over the wire: it arrives disabled, runs once the
/// user enables and grants it, and is gone after uninstalling.
async fn installed_extension_smoke(app: &App, project: &str, body: &str) -> Result<()> {
    let bundled = "agent206.workspace";
    let id = format!("test.{}", uuid::Uuid::new_v4().simple());
    let package = tempfile::tempdir()?;
    let (manifest, module) = (
        package.path().join("plugin.toml"),
        package.path().join("module.wasm"),
    );
    std::fs::write(
        &manifest,
        include_str!("../../../plugins/workspace/plugin.toml").replacen(
            &format!("id = \"{bundled}\""),
            &format!("id = \"{id}\""),
            1,
        ),
    )?;
    let builtin = a206_extensions::builtin(bundled)?.context("bundled workspace extension")?;
    std::fs::write(&module, builtin.wasm)?;
    let install = [
        "extension",
        "install",
        manifest.to_str().context("manifest path")?,
        module.to_str().context("module path")?,
    ];
    let Reply::Extension(summary) = toml::from_str(&app.cli(&install).await?)? else {
        anyhow::bail!("expected the installed extension");
    };
    ensure!(
        summary.id == id && !summary.enabled && summary.granted.is_empty(),
        "an installed extension arrived usable: {summary:?}"
    );
    let read = [
        "extension",
        "tool",
        &id,
        "read_text_file",
        "--project",
        project,
        "--args",
        "path = \"notes.txt\"",
    ];
    ensure!(
        !capture(app.command(&read)).await?.status.success(),
        "a disabled installed extension ran"
    );
    app.cli(&["extension", "enable", &id]).await?;
    app.cli(&["extension", "grant", &id, "--capability", "read_workspace"])
        .await?;
    ensure!(
        app.cli(&read).await?.trim() == body,
        "the installed extension's output did not round-trip"
    );
    app.cli(&["extension", "uninstall", &id]).await?;
    let gone = capture(app.command(&read)).await?;
    ensure!(
        !gone.status.success() && String::from_utf8_lossy(&gone.stderr).contains("not installed"),
        "an uninstalled extension still ran: {}",
        String::from_utf8_lossy(&gone.stderr)
    );
    Ok(())
}

#[tokio::test]
async fn protocol_smoke() -> Result<()> {
    let directory = tempfile::Builder::new().prefix("a206-").tempdir()?;
    let mut app = App::start(directory, BTreeMap::new()).await?;
    let Reply::Thread(thread) = toml::from_str(&app.cli(&["new", "Protocol verification"]).await?)?
    else {
        anyhow::bail!("expected thread");
    };
    let thread_id = thread.id.to_string();
    extension_smoke(&app).await?;
    let mut acp = Peer::start(&app, &["acp", &thread_id])?;
    let init = result(
        acp.request(
            "initialize",
            json!({"protocolVersion":1, "clientCapabilities":{}}),
        )
        .await?,
    )?;
    assert_eq!(init["protocolVersion"], 1);
    let session = result(
        acp.request(
            "session/new",
            json!({"cwd":std::env::current_dir()?, "mcpServers":[]}),
        )
        .await?,
    )?;
    assert_eq!(session["sessionId"], thread_id);
    acp.stop().await?;
    for allow_title_edit in [false, true] {
        let mut args = vec!["mcp", &thread_id];
        if allow_title_edit {
            args.push("--allow-title-edit");
        }
        let mut mcp = Peer::start(&app, &args)?;
        result(mcp.request("initialize", json!({"protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"Agent 206 Rust smoke", "version":"0.1"}})).await?)?;
        mcp.send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
            .await?;
        let tools = result(mcp.request("tools/list", json!({})).await?)?;
        let names: std::collections::BTreeSet<_> = tools["tools"]
            .as_array()
            .context("missing tools")?
            .iter()
            .map(|tool| tool["name"].as_str().context("tool name missing"))
            .collect::<Result<_>>()?;
        let mut expected = std::collections::BTreeSet::from(["thread_state", "harness_contract"]);
        if allow_title_edit {
            expected.insert("rename_thread");
        }
        assert_eq!(names, expected);
        let state = result(
            mcp.request("tools/call", json!({"name":"thread_state", "arguments":{}}))
                .await?,
        )?;
        ensure!(
            state["content"][0]["text"]
                .as_str()
                .context("missing thread state")?
                .contains(&thread_id),
            "MCP inspected the wrong conversation"
        );
        let renamed = mcp
            .request(
                "tools/call",
                json!({"name":"rename_thread", "arguments":{"title":"Verified by MCP"}}),
            )
            .await?;
        let denied = renamed.get("error").is_some()
            || renamed
                .pointer("/result/isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        assert_ne!(denied, allow_title_edit, "{renamed}");
        if allow_title_edit {
            let _result = result(renamed)?;
        }
        let snapshot = app.snapshot().await?;
        assert_eq!(
            snapshot.threads[0].title,
            if allow_title_edit {
                "Verified by MCP"
            } else {
                "Protocol verification"
            }
        );
        mcp.stop().await?;
    }
    app.shutdown().await
}

async fn compile_joshua(directory: &Path) -> Result<()> {
    let executable = directory.join("joshua");
    let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    command
        .args(["--edition=2024", "-D", "warnings", "-o"])
        .arg(&executable)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/joshua.rs"));
    checked(capture(command).await?).context("compile independent Rust Joshua fixture")?;
    Ok(())
}
fn available_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

#[tokio::test]
async fn joshua_lifecycle_smoke() -> Result<()> {
    let directory = tempfile::Builder::new().prefix("a206-").tempdir()?;
    let home = directory.path().join("home");
    let models = home.join("models");
    std::fs::create_dir_all(&models)?;
    let filename = uuid::Uuid::new_v4().to_string();
    std::fs::write(models.join(&filename), [])?;
    let runtime = directory.path().join("bin");
    std::fs::create_dir(&runtime)?;
    compile_joshua(&runtime).await?;
    // The fixture also exits if an assertion unwinds and drops this private profile.
    let lifetime = directory.path().join("test-running");
    std::fs::write(&lifetime, [])?;
    let mut path = vec![runtime];
    if let Some(current) = std::env::var_os("PATH") {
        path.extend(std::env::split_paths(&current));
    }
    let environment = BTreeMap::from([
        (OsString::from("HOME"), home.into_os_string()),
        (OsString::from("PATH"), std::env::join_paths(path)?),
        (
            OsString::from("A206_SMOKE_LIFETIME"),
            lifetime.into_os_string(),
        ),
    ]);
    let mut app = App::start(directory, environment).await?;
    // Keep this endpoint reserved so the managed listener cannot reuse its port.
    let independent_endpoint = std::net::TcpListener::bind("127.0.0.1:0")?;
    let independent_url = format!("http://{}/v1", independent_endpoint.local_addr()?);
    app.cli(&[
        "providers",
        "add",
        "custom",
        "--label",
        "User configured",
        "--endpoint",
        &independent_url,
    ])
    .await?;
    let user = app.snapshot().await?.settings.providers.remove(0);
    let mut previous_model = None;
    for _ in 0..2 {
        app.cli(&[
            "joshua",
            "start",
            &uuid::Uuid::new_v4().to_string(),
            &filename,
            "--port",
            &available_port()?.to_string(),
        ])
        .await?;
        let current = app.snapshot().await?;
        let managed = current
            .managed_joshua_provider
            .context("owned provider identity missing")?;
        assert_eq!(current.settings.providers.len(), 2);
        let catalog = tokio::time::timeout(TIMEOUT, async {
            loop {
                let output =
                    capture(app.command(&["refresh-models", &managed.to_string()])).await?;
                if output.status.success() {
                    let Reply::Catalog(catalog) = toml::from_str(&checked(output)?)? else {
                        anyhow::bail!("expected discovered catalog");
                    };
                    break Ok::<_, anyhow::Error>(catalog);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("mock Joshua API did not become ready")??;
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.provider, managed);
        let model = &catalog.models[0];
        ensure!(
            model.context_limit.is_some_and(|limit| limit > 0),
            "API context was not discovered"
        );
        assert_ne!(previous_model.as_ref(), Some(&model.key.model));
        previous_model = Some(model.key.model.clone());
        let mut edited = current.settings.clone();
        edited
            .providers
            .iter_mut()
            .find(|provider| provider.id == managed)
            .context("managed connection missing")?
            .base_url
            .clone_from(&independent_url);
        let settings_file = app.root.join("edited.toml");
        std::fs::write(&settings_file, toml::to_string_pretty(&edited)?)?;
        let mut command = app.command(&["settings", "--apply"]);
        command.arg(&settings_file);
        let output = capture(command).await?;
        ensure!(
            !output.status.success(),
            "managed connection was repointed while its process was running"
        );
        ensure!(
            String::from_utf8_lossy(&output.stderr).contains("stop managed Joshua"),
            "unexpected settings rejection"
        );
        assert_eq!(
            app.snapshot().await?.settings.providers,
            current.settings.providers
        );
        app.cli(&["joshua", "stop"]).await?;
        let stopped = app.snapshot().await?;
        assert_eq!(stopped.settings.providers, vec![user.clone()]);
        ensure!(
            !stopped
                .catalogs
                .iter()
                .any(|catalog| catalog.provider == managed),
            "managed catalog survived stop"
        );
        assert!(stopped.managed_joshua_provider.is_none());
    }
    app.shutdown().await
}
