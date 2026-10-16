//! Real gateway lifecycle tests: no mocked process state or launcher.
#![cfg(unix)]
#![forbid(unsafe_code)]
mod support;
use a206_core::{Command, GatewayAction, GatewayPhase, GatewayReport, Reply};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, process::Stdio};
use support::{App, TIMEOUT, capture};
use tokio::io::{AsyncBufReadExt, BufReader};

struct TestServer(tokio::task::JoinHandle<()>);
impl Drop for TestServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn command_reply(bytes: &[u8]) -> Result<Reply> {
    let mut decoder = a206_protocol::Decoder::default();
    let mut messages = decoder.feed::<a206_protocol::Response<Reply>>(bytes)?;
    decoder.finish()?;
    ensure!(messages.len() == 1);
    Ok(messages.remove(0).body)
}

async fn report(app: &App, action: &str) -> Result<GatewayReport> {
    Ok(toml::from_str(&app.cli(&["gateway", action]).await?)?)
}

async fn socket_request(app: &App, id: uuid::Uuid, body: Command) -> Result<Reply> {
    let mut socket = tokio::net::UnixStream::connect(app.root.join("gateway.sock")).await?;
    a206_runtime::transport::write_frame(&mut socket, &a206_protocol::Request { id, body }).await?;
    let response = tokio::time::timeout(
        TIMEOUT,
        a206_runtime::transport::read_frame::<a206_protocol::Response<Reply>>(&mut socket),
    )
    .await??
    .context("missing socket reply")?;
    ensure!(response.id == id);
    Ok(response.body)
}

#[tokio::test]
async fn stop_persists_for_a_fresh_offline_profile() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("fresh");
    let command = |args: &[&str]| {
        let mut command = tokio::process::Command::new(support::binary().unwrap());
        command.arg("--data-dir").arg(&root).args(args);
        command
    };
    let stopped: GatewayReport = toml::from_str(&support::checked(
        capture(command(&["gateway", "stop"])).await?,
    )?)?;
    ensure!(stopped.phase == GatewayPhase::Stopped && stopped.error.is_none());
    ensure!(root.join("gateway.stopped").exists());
    for args in [vec!["snapshot"], vec!["gateway", "--daemon"]] {
        ensure!(!capture(command(&args)).await?.status.success());
        ensure!(!root.join("gateway.sock").exists());
    }
    let status: GatewayReport = toml::from_str(&support::checked(
        capture(command(&["gateway", "status"])).await?,
    )?)?;
    ensure!(status.phase == GatewayPhase::Stopped && status.error.is_none());
    Ok(())
}

#[tokio::test]
async fn replayed_shutdown_does_not_stop_a_restarted_process() -> Result<()> {
    let mut app = App::start(tempfile::tempdir()?, BTreeMap::new()).await?;
    let original = report(&app, "status").await?.info.unwrap().pid;
    let id = uuid::Uuid::new_v4();
    ensure!(socket_request(&app, id, Command::Shutdown).await? == Reply::Ok);
    // Idempotent stop also waits for the previous process to release its lock.
    ensure!(report(&app, "stop").await?.phase == GatewayPhase::Stopped);
    let restarted = report(&app, "start").await?.info.unwrap().pid;
    ensure!(restarted != original);
    ensure!(!app.root.join("gateway.stopped").exists());
    ensure!(socket_request(&app, id, Command::Shutdown).await? == Reply::Ok);
    for _ in 0..3 {
        ensure!(report(&app, "status").await?.info.unwrap().pid == restarted);
        ensure!(!app.root.join("gateway.stopped").exists());
        app.snapshot().await?;
    }
    app.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stop_stays_stopped_and_restart_preserves_profile() -> Result<()> {
    let mut app = App::start(tempfile::tempdir()?, BTreeMap::new()).await?;
    app.cli(&["new", "Lifecycle persistence"]).await?;
    use std::os::unix::fs::PermissionsExt;
    ensure!(std::fs::metadata(&app.root)?.permissions().mode() & 0o777 == 0o700);
    ensure!(
        std::fs::metadata(app.root.join("gateway.sock"))?
            .permissions()
            .mode()
            & 0o777
            == 0o600
    );
    let original = report(&app, "status").await?;
    let pid = original.info.context("missing process info")?.pid;
    let status: toml::Value = toml::from_str(&app.cli(&["status"]).await?)?;
    ensure!(status["workspace"]["conversations"].as_integer() == Some(1));
    ensure!(
        report(&app, "start").await?.info.unwrap().pid == pid,
        "start replaced an existing service"
    );
    ensure!(report(&app, "reconnect").await?.info.unwrap().pid == pid);

    // Hold another client's connection open across stop.
    let mut peer = app
        .command(&["gateway", "--stdio", "--no-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = peer.stdin.take().unwrap();
    let mut output = peer.stdout.take().unwrap();
    a206_runtime::transport::write_frame(
        &mut input,
        &a206_protocol::Request {
            id: uuid::Uuid::new_v4(),
            body: a206_core::Command::Snapshot,
        },
    )
    .await?;
    let first = tokio::time::timeout(
        TIMEOUT,
        a206_runtime::transport::read_frame::<a206_protocol::Response<a206_core::Reply>>(
            &mut output,
        ),
    )
    .await??;
    ensure!(first.is_some());
    ensure!(report(&app, "stop").await?.phase == GatewayPhase::Stopped);
    let status: toml::Value = toml::from_str(&app.cli(&["status"]).await?)?;
    ensure!(status["gateway"]["phase"].as_str() == Some("Stopped"));
    ensure!(
        status.get("workspace").is_none(),
        "offline counts were fabricated"
    );
    tokio::time::timeout(TIMEOUT, peer.wait()).await??;
    for _ in 0..3 {
        ensure!(report(&app, "status").await?.phase == GatewayPhase::Stopped);
        ensure!(
            !capture(app.command(&["snapshot"])).await?.status.success(),
            "ordinary request restarted the service"
        );
        ensure!(!app.root.join("gateway.sock").exists());
    }
    ensure!(
        !capture(app.command(&["gateway", "--daemon"]))
            .await?
            .status
            .success(),
        "an automatic daemon starter overrode deliberate stop"
    );
    ensure!(!app.root.join("gateway.sock").exists());
    let restarted = report(&app, "restart").await?;
    ensure!(restarted.phase == GatewayPhase::Running);
    ensure!(restarted.info.unwrap().pid != pid);
    ensure!(
        app.snapshot()
            .await?
            .threads
            .iter()
            .any(|thread| thread.title == "Lifecycle persistence")
    );
    ensure!(report(&app, "restart").await?.phase == GatewayPhase::Running);
    app.shutdown().await?;
    // An absent service with an existing profile (for example after a crash)
    // must gain the same durable stop intent as a fresh profile.
    std::fs::remove_file(app.root.join("gateway.stopped"))?;
    ensure!(report(&app, "stop").await?.phase == GatewayPhase::Stopped);
    ensure!(app.root.join("gateway.stopped").exists());
    ensure!(!capture(app.command(&["snapshot"])).await?.status.success());
    Ok(())
}

#[tokio::test]
async fn web_controls_require_authority_and_deduplicate_restart() -> Result<()> {
    let mut app = App::start(tempfile::tempdir()?, BTreeMap::new()).await?;
    let static_dir = tempfile::tempdir()?;
    std::fs::write(
        static_dir.path().join("index.html"),
        "<!doctype html><title>Gateway test</title>",
    )?;
    let mut server = app
        .command(&[
            "web",
            "--static-dir",
            static_dir.path().to_str().unwrap(),
            "--port",
            "0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut line = String::new();
    tokio::time::timeout(
        TIMEOUT,
        BufReader::new(server.stdout.take().unwrap()).read_line(&mut line),
    )
    .await??;
    let bootstrap = line
        .strip_prefix("Open ")
        .and_then(|line| line.split_whitespace().next())
        .context("missing bootstrap URL")?;
    let url = reqwest::Url::parse(bootstrap)?;
    let origin = url.origin().ascii_serialization();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = http.get(bootstrap).send().await?;
    let token = response
        .headers()
        .get("location")
        .context("missing authorization redirect")?
        .to_str()?
        .strip_prefix("/#a206=")
        .context("missing session")?
        .to_owned();
    let request = a206_protocol::Request {
        id: uuid::Uuid::new_v4(),
        body: GatewayAction::Restart,
    };
    let bytes = a206_protocol::encode(&request)?;
    for bad_origin in [None, Some("http://127.0.0.1:1")] {
        let mut post = http
            .post(format!("{origin}/api/gateway"))
            .bearer_auth(&token)
            .header("Content-Type", "application/msgpack")
            .body(bytes.clone());
        if let Some(value) = bad_origin {
            post = post.header("Origin", value);
        }
        ensure!(post.send().await?.status() == reqwest::StatusCode::FORBIDDEN);
    }
    let send = |id, action| {
        http.post(format!("{origin}/api/gateway"))
            .bearer_auth(&token)
            .header("Origin", &origin)
            .header("Content-Type", "application/msgpack")
            .body(a206_protocol::encode(&a206_protocol::Request { id, body: action }).unwrap())
    };
    let decode = |bytes: &[u8]| -> Result<a206_protocol::Response<GatewayReport>> {
        let mut decoder = a206_protocol::Decoder::default();
        let mut messages = decoder.feed(bytes)?;
        decoder.finish()?;
        ensure!(messages.len() == 1);
        Ok(messages.remove(0))
    };
    let first = decode(
        &send(request.id, request.body)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?,
    )?;
    ensure!(first.id == request.id);
    ensure!(
        first.body.phase == GatewayPhase::Running,
        "{:?}",
        first.body.error
    );
    let duplicate = decode(
        &send(request.id, request.body)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?,
    )?;
    ensure!(
        duplicate.body == first.body,
        "duplicate restart executed again"
    );
    ensure!(
        send(request.id, GatewayAction::Stop).send().await?.status()
            == reqwest::StatusCode::CONFLICT
    );

    // Hold an actual API catalog request across repeated Start and a failed
    // shutdown deadline. Its pending reply must survive, and failed shutdown
    // must restore the running process's ability to accept ordinary commands.
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let api_model = uuid::Uuid::new_v4().to_string();
    let api_context = (uuid::Uuid::new_v4().as_u128() % 100_000 + 1) as u64;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let api = axum::Router::new().route(
        "/v1/models",
        axum::routing::get({
            let entered = entered.clone();
            let release = release.clone();
            let model = api_model.clone();
            move || {
                let entered = entered.clone();
                let release = release.clone();
                let model = model.clone();
                async move {
                    entered.notify_one();
                    let _permit = release.acquire().await.unwrap();
                    axum::Json(serde_json::json!({"data":[{
                        "id": model, "context_length": api_context
                    }]}))
                }
            }
        }),
    );
    let _api = TestServer(tokio::spawn(async move {
        axum::serve(listener, api).await.unwrap();
    }));
    let mut settings = app.snapshot().await?.settings;
    let provider = uuid::Uuid::new_v4();
    settings.providers.push(a206_core::ProviderConfig {
        id: provider,
        label: uuid::Uuid::new_v4().to_string(),
        base_url: format!("http://{address}/v1"),
        credential: None,
        api: Default::default(),
    });
    ensure!(
        socket_request(&app, uuid::Uuid::new_v4(), Command::SaveSettings(settings)).await?
            == Reply::Ok
    );
    let refresh = http
        .post(format!("{origin}/api/command"))
        .bearer_auth(&token)
        .header("Origin", &origin)
        .header("Content-Type", "application/msgpack")
        .body(a206_protocol::encode(&a206_protocol::Request {
            id: uuid::Uuid::new_v4(),
            body: Command::RefreshCatalog { provider },
        })?);
    let pending = tokio::spawn(async move {
        command_reply(&refresh.send().await?.error_for_status()?.bytes().await?)
    });
    tokio::time::timeout(TIMEOUT, entered.notified()).await?;
    let started = decode(
        &send(uuid::Uuid::new_v4(), GatewayAction::Start)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?,
    )?;
    ensure!(started.body.info.unwrap().pid == first.body.info.as_ref().unwrap().pid);
    ensure!(
        !pending.is_finished(),
        "Start discarded an accepted command's reply"
    );
    let failed_stop = decode(
        &tokio::time::timeout(
            std::time::Duration::from_secs(20),
            send(uuid::Uuid::new_v4(), GatewayAction::Stop).send(),
        )
        .await??
        .error_for_status()?
        .bytes()
        .await?,
    )?;
    ensure!(
        failed_stop
            .body
            .error
            .as_deref()
            .is_some_and(|error| error.contains("cleanup timed out"))
    );
    ensure!(!app.root.join("gateway.stopped").exists());
    let status = report(&app, "status")
        .await?
        .info
        .context("gateway went offline after failed stop")?;
    ensure!(status.pid == first.body.info.as_ref().unwrap().pid);
    ensure!(status.accepting_work == Some(true));
    ensure!(matches!(
        socket_request(
            &app,
            uuid::Uuid::new_v4(),
            Command::CreateThread {
                title: uuid::Uuid::new_v4().to_string(),
                mode: a206_core::Mode::Chat,
            }
        )
        .await?,
        Reply::Thread(_)
    ));
    release.add_permits(1);
    let Reply::Catalog(catalog) = tokio::time::timeout(TIMEOUT, pending).await??? else {
        anyhow::bail!("pending catalog reply was lost");
    };
    ensure!(catalog.models[0].key.model == api_model);
    ensure!(catalog.models[0].context_limit == Some(api_context));

    let raw_stop = http
        .post(format!("{origin}/api/command"))
        .bearer_auth(&token)
        .header("Origin", &origin)
        .header("Content-Type", "application/msgpack")
        .body(a206_protocol::encode(&a206_protocol::Request {
            id: uuid::Uuid::new_v4(),
            body: a206_core::Command::Shutdown,
        })?)
        .send()
        .await?;
    ensure!(raw_stop.status() == reqwest::StatusCode::FORBIDDEN);
    let stopped = decode(
        &send(uuid::Uuid::new_v4(), GatewayAction::Stop)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?,
    )?;
    ensure!(stopped.body.phase == GatewayPhase::Stopped);
    for _ in 0..3 {
        let status = decode(
            &send(uuid::Uuid::new_v4(), GatewayAction::Status)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?,
        )?;
        ensure!(status.body.phase == GatewayPhase::Stopped);
        ensure!(!app.root.join("gateway.sock").exists());
    }
    let start = decode(
        &send(uuid::Uuid::new_v4(), GatewayAction::Start)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?,
    )?;
    ensure!(start.body.phase == GatewayPhase::Running);
    server.kill().await?;
    app.shutdown().await?;
    Ok(())
}
