//! Real gateway lifecycle tests: no mocked process state or launcher.
#![cfg(unix)]
#![forbid(unsafe_code)]
mod support;
use a206_core::{GatewayAction, GatewayPhase, GatewayReport};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, process::Stdio};
use support::{App, TIMEOUT, capture};
use tokio::io::{AsyncBufReadExt, BufReader};

async fn report(app: &App, action: &str) -> Result<GatewayReport> {
    Ok(toml::from_str(&app.cli(&["gateway", action]).await?)?)
}

#[tokio::test]
async fn stop_stays_stopped_and_restart_preserves_profile() -> Result<()> {
    let mut app = App::start(tempfile::tempdir()?, BTreeMap::new()).await?;
    app.cli(&["new", "Lifecycle persistence"]).await?;
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
