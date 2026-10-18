//! Test catalogs come exclusively from a running API, with runtime-generated facts.
use a206_core::*;
use a206_protocol::Request;
use a206_runtime::gateway::Gateway;
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use serde_json::{Value, json};
#[derive(Clone)]
struct Api {
    model: String,
    context: u64,
    effort: String,
    request: std::sync::Arc<std::sync::Mutex<Option<Value>>>,
}
async fn models(State(api): State<Api>) -> Json<Value> {
    Json(
        json!({"data":[{"id":api.model,"context_length":api.context, "supported_reasoning_efforts":[api.effort], "pricing":{"prompt": 1.0 / api.context as f64}}], "balance": api.context as f64 / 1000.0, "token_balance": api.context}),
    )
}
/// Streams the last message back as the reply, then ends the stream.
async fn chat(State(api): State<Api>, Json(request): Json<Value>) -> String {
    assert_eq!(request["model"].as_str(), Some(api.model.as_str()));
    assert_eq!(request["messages"][0]["role"], "system");
    assert_eq!(request["stream"], true);
    *api.request.lock().unwrap() = Some(request.clone());
    let content = &request["messages"].as_array().unwrap().last().unwrap()["content"];
    let delta = json!({"choices":[{"delta":{"content": content}, "finish_reason": "stop"}]});
    format!("data: {delta}\n\ndata: [DONE]\n\n")
}
async fn run(gateway: &Gateway, command: Command) -> Reply {
    gateway
        .request(Request {
            id: Id::new_v4(),
            body: command,
        })
        .await
        .body
}
#[tokio::test]
async fn shutdown_cancels_active_inference_and_rejects_new_work() {
    let identity = Id::new_v4();
    let api = Api {
        model: identity.to_string(),
        context: (identity.as_u128() % 100_000 + 1) as u64,
        effort: Id::new_v4().to_string(),
        request: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/v1/models", get(models))
        .route(
            "/v1/chat/completions",
            post(
                |State(api): State<Api>, Json(request): Json<Value>| async move {
                    *api.request.lock().unwrap() = Some(request);
                    std::future::pending::<Json<Value>>().await
                },
            ),
        )
        .with_state(api.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(directory.path()).unwrap();
    let provider = Id::new_v4();
    let mut settings = Settings::default();
    settings.providers.push(ProviderConfig {
        id: provider,
        label: Id::new_v4().to_string(),
        base_url: format!("http://{address}/v1"),
        credential: None,
        api: Default::default(),
    });
    assert_eq!(
        run(&gateway, Command::SaveSettings(Box::new(settings))).await,
        Reply::Ok
    );
    let Reply::Catalog(catalog) = run(&gateway, Command::RefreshCatalog { provider }).await else {
        panic!()
    };
    let Reply::Thread(thread) = run(
        &gateway,
        Command::CreateThread {
            title: Id::new_v4().to_string(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!()
    };
    assert!(matches!(
        run(
            &gateway,
            Command::ConfigureThread {
                id: thread.id,
                mode: Mode::Chat,
                model: Some(catalog.models[0].key.clone()),
                subagent_models: Default::default()
            }
        )
        .await,
        Reply::Thread(_)
    ));
    let worker = gateway.clone();
    let send = tokio::spawn(async move {
        run(
            &worker,
            Command::Send {
                thread: thread.id,
                text: Id::new_v4().to_string(),
                attachments: vec![],
                images: vec![],
            },
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while api.request.lock().unwrap().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(
            run(&gateway, Command::Shutdown),
            run(&gateway, Command::Shutdown)
        )
    })
    .await
    .unwrap();
    assert_eq!(first, Reply::Ok);
    assert_eq!(second, Reply::Ok);
    assert_eq!(run(&gateway, Command::Shutdown).await, Reply::Ok);
    assert!(
        matches!(send.await.unwrap(), Reply::Thread(thread) if thread.state == RunState::Cancelled)
    );
    assert!(directory.path().join("gateway.stopped").exists());
    let Reply::Gateway(info) = run(&gateway, Command::GatewayStatus).await else {
        panic!()
    };
    assert_eq!(info.active_runs, 0);
    assert_eq!(info.accepting_work, Some(false));
    assert!(matches!(
        run(
            &gateway,
            Command::CreateThread {
                title: Id::new_v4().to_string(),
                mode: Mode::Chat
            }
        )
        .await,
        Reply::Error { .. }
    ));
    server.abort();
}
#[tokio::test]
async fn discovers_models_from_api_then_sends_and_persists_a_conversation() {
    let identity = Id::new_v4();
    let api = Api {
        model: identity.to_string(),
        context: (identity.as_u128() % 100_000 + 1) as u64,
        effort: Id::new_v4().to_string(),
        request: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(api.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(dir.path()).unwrap();
    let provider = Id::new_v4();
    let mut settings = Settings::default();
    settings.providers.push(ProviderConfig {
        id: provider,
        label: Id::new_v4().to_string(),
        base_url: format!("http://{address}/v1"),
        credential: None,
        api: Default::default(),
    });
    assert_eq!(
        run(&gateway, Command::SaveSettings(Box::new(settings))).await,
        Reply::Ok
    );
    let Reply::Catalog(catalog) = run(&gateway, Command::RefreshCatalog { provider }).await else {
        panic!("expected discovered catalog");
    };
    assert_eq!(catalog.models[0].key.model, api.model);
    assert_eq!(catalog.models[0].context_limit, Some(api.context));
    assert_eq!(catalog.models[0].output_cost_per_token, None);
    let Reply::Thread(thread) = run(
        &gateway,
        Command::CreateThread {
            title: "Integration".into(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!()
    };
    let key = catalog.models[0].key.clone();
    run(
        &gateway,
        Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: Some(key),
            subagent_models: Default::default(),
        },
    )
    .await;
    assert_eq!(
        catalog.models[0].reasoning_efforts,
        vec![api.effort.clone()]
    );
    let expected_balance = api.context as f64 / 1000.0;
    assert!(
        (catalog.balance.unwrap() - expected_balance).abs()
            <= expected_balance * f64::EPSILON * 4.0
    );
    assert_eq!(catalog.token_balance, Some(api.context));
    let expected_price = 1.0 / api.context as f64;
    let actual_price = catalog.models[0].input_cost_per_token.unwrap();
    assert!((actual_price - expected_price).abs() <= expected_price * f64::EPSILON * 4.0);
    assert!(matches!(
        run(
            &gateway,
            Command::SetEffort {
                thread: thread.id,
                effort: Some(Id::new_v4().to_string())
            }
        )
        .await,
        Reply::Error { .. }
    ));
    assert_eq!(
        run(
            &gateway,
            Command::SetEffort {
                thread: thread.id,
                effort: Some(api.effort.clone())
            }
        )
        .await,
        Reply::Ok
    );
    assert!(matches!(
        run(
            &gateway,
            Command::Send {
                thread: thread.id,
                text: String::new(),
                attachments: vec![TextAttachment {
                    name: Id::new_v4().to_string(),
                    text: "x".repeat(128 * 1024 + 1)
                }],
                images: vec![],
            }
        )
        .await,
        Reply::Error { .. }
    ));
    assert!(gateway.snapshot().unwrap().threads[0].messages.is_empty());
    let attached = TextAttachment {
        name: Id::new_v4().to_string(),
        text: Id::new_v4().to_string(),
    };
    let message = Id::new_v4().to_string();
    let Reply::Thread(thread) = run(
        &gateway,
        Command::Send {
            thread: thread.id,
            text: message.clone(),
            attachments: vec![attached.clone()],
            images: vec![],
        },
    )
    .await
    else {
        panic!()
    };
    assert_eq!(thread.state, RunState::Complete);
    assert_eq!(thread.messages[0].text, message);
    assert_eq!(thread.messages[0].attachments, vec![attached.clone()]);
    let request = api.request.lock().unwrap().clone().unwrap();
    assert_eq!(request["reasoning_effort"], api.effort);
    let sent = request["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(sent.contains(&attached.name) && sent.contains(&attached.text));
    assert_eq!(thread.messages.last().unwrap().text, sent);
    assert!(thread.usage.unwrap().input_tokens.is_none());
    let Reply::Thread(unchanged) = run(
        &gateway,
        Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: Some(catalog.models[0].key.clone()),
            subagent_models: Default::default(),
        },
    )
    .await
    else {
        panic!()
    };
    assert!(unchanged.usage.is_some());
    let Reply::Thread(cleared) = run(
        &gateway,
        Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: None,
            subagent_models: Default::default(),
        },
    )
    .await
    else {
        panic!()
    };
    assert!(cleared.usage.is_none());
    assert!(cleared.effort.is_none());
    run(
        &gateway,
        Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: Some(catalog.models[0].key.clone()),
            subagent_models: Default::default(),
        },
    )
    .await;
    assert_eq!(
        run(
            &gateway,
            Command::SetSubagentModel {
                id: thread.id,
                model: catalog.models[0].key.clone(),
                allowed: true
            }
        )
        .await,
        Reply::Ok
    );
    let Reply::Thread(child) = run(
        &gateway,
        Command::CreateSubagent {
            parent: thread.id,
            title: "Delegated".into(),
            mode: Mode::Code,
            model: catalog.models[0].key.clone(),
            requested: [Capability::Execute, Capability::Network].into(),
        },
    )
    .await
    else {
        panic!("delegation failed")
    };
    assert!(child.grant.capabilities.is_empty());
    assert_eq!(
        run(
            &gateway,
            Command::SetSubagentModel {
                id: thread.id,
                model: catalog.models[0].key.clone(),
                allowed: false
            }
        )
        .await,
        Reply::Ok
    );
    assert!(matches!(
        run(
            &gateway,
            Command::Send {
                thread: child.id,
                text: Id::new_v4().to_string(),
                attachments: vec![],
                images: vec![],
            }
        )
        .await,
        Reply::Error { .. }
    ));
    assert!(
        gateway
            .snapshot()
            .unwrap()
            .threads
            .iter()
            .find(|t| t.id == child.id)
            .unwrap()
            .messages
            .is_empty()
    );
    drop(gateway);
    let restored = Gateway::open(dir.path()).unwrap().snapshot().unwrap();
    assert_eq!(restored.threads[0].messages.len(), 2);
    assert!(restored.threads[0].effort.is_none());
    assert_eq!(restored.threads[0].messages[0].attachments, vec![attached]);
    server.abort();
}
#[tokio::test]
async fn missing_catalog_and_unapproved_delegation_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(dir.path()).unwrap();
    let Reply::Thread(thread) = run(
        &gateway,
        Command::CreateThread {
            title: "Test".into(),
            mode: Mode::Code,
        },
    )
    .await
    else {
        panic!()
    };
    assert!(matches!(
        run(
            &gateway,
            Command::Send {
                thread: thread.id,
                text: "Hello".into(),
                attachments: vec![],
                images: vec![],
            }
        )
        .await,
        Reply::Error { .. }
    ));
    assert!(gateway.snapshot().unwrap().threads[0].messages.is_empty());
}

#[tokio::test]
async fn stale_settings_cannot_overwrite_newer_user_changes() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(dir.path()).unwrap();
    let old = Settings::default();
    let mut first = old.clone();
    first.prompts[0].prompt = Id::new_v4().to_string();
    assert_eq!(
        run(&gateway, Command::SaveSettings(Box::new(first.clone()))).await,
        Reply::Ok
    );
    assert!(matches!(
        run(&gateway, Command::SaveSettings(Box::new(old))).await,
        Reply::Error { .. }
    ));
    assert_eq!(gateway.snapshot().unwrap().settings.prompts, first.prompts);
}

#[cfg(unix)]
#[tokio::test]
async fn document_reads_use_the_project_capability_even_through_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("inside.md"), "Project document").unwrap();
    std::fs::write(directory.path().join("outside.md"), "Outside the grant").unwrap();
    std::os::unix::fs::symlink(directory.path().join("outside.md"), root.join("linked.md"))
        .unwrap();
    let gateway = Gateway::open(&directory.path().join("state")).unwrap();
    let Reply::Project(project) = run(
        &gateway,
        Command::CreateProject {
            name: "Workspace".into(),
            root: root.to_string_lossy().into_owned(),
        },
    )
    .await
    else {
        panic!("project creation failed")
    };
    assert_eq!(
        run(
            &gateway,
            Command::ReadDocument {
                project: project.id,
                path: "inside.md".into()
            }
        )
        .await,
        Reply::Text("Project document".into())
    );
    for path in ["linked.md", "../outside.md"] {
        assert!(matches!(
            run(
                &gateway,
                Command::ReadDocument {
                    project: project.id,
                    path: path.into()
                }
            )
            .await,
            Reply::Error { .. }
        ));
    }
}
#[derive(Clone)]
struct VisionApi {
    model: String,
    reply: String,
    /// Whether the catalog reports image input.
    images: std::sync::Arc<std::sync::atomic::AtomicBool>,
    request: std::sync::Arc<std::sync::Mutex<Option<Value>>>,
}
fn error_kind(reply: Reply) -> ErrorKind {
    match reply {
        Reply::Error(report) => report.kind,
        other => panic!("expected an error, got {other:?}"),
    }
}
#[tokio::test]
async fn images_are_staged_checked_against_the_models_api_and_kept_only_when_sent() {
    use base64::Engine;
    use std::sync::atomic::Ordering;
    let api = VisionApi {
        model: Id::new_v4().to_string(),
        reply: Id::new_v4().to_string(),
        images: Default::default(),
        request: Default::default(),
    };
    api.images.store(true, Ordering::SeqCst);
    let router = Router::new()
        .route(
            "/v1/models",
            get(|State(api): State<VisionApi>| async move {
                let modalities = if api.images.load(Ordering::SeqCst) {
                    json!(["text", "image"])
                } else {
                    json!(["text"])
                };
                Json(json!({"data": [{"id": api.model, "architecture": {"input_modalities": modalities}}]}))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(
                |State(api): State<VisionApi>, Json(request): Json<Value>| async move {
                    *api.request.lock().unwrap() = Some(request);
                    let delta = json!({"choices":[{"delta":{"content": api.reply}, "finish_reason": "stop"}]});
                    format!("data: {delta}\n\ndata: [DONE]\n\n")
                },
            ),
        )
        .with_state(api.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(dir.path()).unwrap();
    let provider = Id::new_v4();
    let mut settings = Settings::default();
    settings.providers.push(ProviderConfig {
        id: provider,
        label: Id::new_v4().to_string(),
        base_url: format!("http://{address}/v1"),
        credential: None,
        api: Default::default(),
    });
    assert_eq!(
        run(&gateway, Command::SaveSettings(Box::new(settings))).await,
        Reply::Ok
    );
    let Reply::Catalog(catalog) = run(&gateway, Command::RefreshCatalog { provider }).await else {
        panic!("expected a catalog");
    };
    assert_eq!(catalog.models[0].image_input, Some(true));
    let Reply::Thread(thread) = run(
        &gateway,
        Command::CreateThread {
            title: Id::new_v4().to_string(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!()
    };
    let configured = run(
        &gateway,
        Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: Some(catalog.models[0].key.clone()),
            subagent_models: Default::default(),
        },
    )
    .await;
    assert!(matches!(configured, Reply::Thread(_)));
    let stage = |data: Vec<u8>| Command::StageImage {
        name: Id::new_v4().to_string(),
        data,
    };
    let png = || {
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.extend_from_slice(Id::new_v4().as_bytes());
        data
    };

    // Only images, recognised by their bytes, are stored.
    let text = Id::new_v4().to_string().into_bytes();
    assert_eq!(
        error_kind(run(&gateway, stage(text)).await),
        ErrorKind::InvalidRequest
    );
    let data = png();
    let Reply::Image(image) = run(&gateway, stage(data.clone())).await else {
        panic!("expected a staged image");
    };
    assert_eq!(image.media_type, ImageType::Png);
    let read = |digest: &str| Command::ReadImage {
        digest: digest.to_owned(),
    };
    assert_eq!(
        run(&gateway, read(&image.digest)).await,
        Reply::ImageData(data.clone())
    );

    // An image the gateway does not hold is refused, and nothing is added.
    let mut unknown = image.clone();
    unknown.digest = format!("{:064x}", Id::new_v4().as_u128());
    let send = |text: String, images: Vec<ImageAttachment>| Command::Send {
        thread: thread.id,
        text,
        attachments: vec![],
        images,
    };
    assert_eq!(
        error_kind(run(&gateway, send(String::new(), vec![unknown])).await),
        ErrorKind::NotFound
    );
    assert!(gateway.snapshot().unwrap().threads[0].messages.is_empty());

    // A sent image is stored on the message and reaches the model.
    let Reply::Thread(sent) = run(&gateway, send(String::new(), vec![image.clone()])).await else {
        panic!("expected the conversation");
    };
    assert_eq!(sent.state, RunState::Complete, "{:?}", sent.error);
    assert_eq!(sent.messages[0].images, vec![image.clone()]);
    let request = api.request.lock().unwrap().take().unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    assert_eq!(
        request["messages"][1]["content"][0]["image_url"]["url"],
        format!("data:image/png;base64,{encoded}")
    );

    // Once the API reports the model takes no images, new ones are refused
    // and earlier ones are named instead of sent.
    let Reply::Image(unsent) = run(&gateway, stage(png())).await else {
        panic!("expected a staged image");
    };
    api.images.store(false, Ordering::SeqCst);
    let Reply::Catalog(catalog) = run(&gateway, Command::RefreshCatalog { provider }).await else {
        panic!("expected a catalog");
    };
    assert_eq!(catalog.models[0].image_input, Some(false));
    assert_eq!(
        error_kind(
            run(
                &gateway,
                send(Id::new_v4().to_string(), vec![unsent.clone()])
            )
            .await
        ),
        ErrorKind::InvalidRequest
    );
    assert_eq!(gateway.snapshot().unwrap().threads[0].messages.len(), 2);
    let Reply::Thread(_) = run(&gateway, send(Id::new_v4().to_string(), vec![])).await else {
        panic!("expected the conversation");
    };
    let request = api.request.lock().unwrap().take().unwrap().to_string();
    assert!(!request.contains(&encoded));
    assert!(request.contains(&image.name) && request.contains("does not accept images"));

    // Staged images no message refers to are bounded: the oldest goes first,
    // and a sent image is never counted among them.
    let mut newest = None;
    for _ in 0..a206_runtime::images::MAX_UNSENT {
        let Reply::Image(staged) = run(&gateway, stage(png())).await else {
            panic!("expected a staged image");
        };
        newest = Some(staged);
    }
    assert_eq!(
        error_kind(run(&gateway, read(&unsent.digest)).await),
        ErrorKind::NotFound
    );
    assert!(matches!(
        run(&gateway, read(&newest.unwrap().digest)).await,
        Reply::ImageData(_)
    ));
    assert_eq!(
        run(&gateway, read(&image.digest)).await,
        Reply::ImageData(data.clone())
    );

    // A restart keeps what a message refers to and drops what was never sent.
    drop(gateway);
    let gateway = Gateway::open(dir.path()).unwrap();
    assert_eq!(
        run(&gateway, read(&image.digest)).await,
        Reply::ImageData(data)
    );
    assert_eq!(
        error_kind(run(&gateway, read(&unsent.digest)).await),
        ErrorKind::NotFound
    );
    server.abort();
}
