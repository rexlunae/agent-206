//! Loopback web bridge. Origin-scoped bearer sessions avoid leaking cookies to preview servers.
use crate::control::GatewayController;
use a206_core::{Command, GatewayAction, Reply};
use a206_protocol::{Decoder, Request};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};
#[derive(Clone)]
struct Web {
    client: GatewayController,
    token: Arc<String>,
    bootstrap: Arc<String>,
    claimed: Arc<AtomicBool>,
    origin: Arc<String>,
    host: Arc<String>,
}
/// Why the loopback web bridge could not run.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("could not listen on 127.0.0.1:{port}")]
    Bind {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("web bridge stopped")]
    Serve(#[source] std::io::Error),
}
/// The bridge connects to the configured gateway, local or SSH, on the browser's behalf.
pub async fn serve(
    root: PathBuf,
    remote: Option<String>,
    static_dir: PathBuf,
    port: u16,
) -> Result<(), WebError> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|source| WebError::Bind { port, source })?;
    let port = listener
        .local_addr()
        .map_err(|source| WebError::Bind { port, source })?
        .port();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let state = Web {
        client: GatewayController::new(root, remote),
        token: Arc::new(uuid::Uuid::new_v4().simple().to_string()),
        bootstrap: Arc::new(token.clone()),
        claimed: Arc::new(AtomicBool::new(false)),
        origin: Arc::new(format!("http://127.0.0.1:{port}")),
        host: Arc::new(format!("127.0.0.1:{port}")),
    };
    println!("Open http://127.0.0.1:{port}/connect/{token} to authorize this browser.");
    axum::serve(listener, app(state, static_dir))
        .await
        .map_err(WebError::Serve)?;
    Ok(())
}
fn app(state: Web, static_dir: PathBuf) -> Router {
    Router::new()
        .route("/connect/{token}", get(connect))
        .route("/api/command", post(command))
        .route("/api/gateway", post(control))
        .layer(DefaultBodyLimit::max(a206_protocol::MAX_FRAME + 10))
        .fallback_service(static_files(static_dir))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            canonical_host,
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            axum::http::HeaderValue::from_static("no-referrer"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            axum::http::HeaderValue::from_static(
                "frame-ancestors 'none'; base-uri 'self'; object-src 'none'",
            ),
        ))
        .with_state(state)
}

/// `localhost` is a different browser origin from `127.0.0.1`, so its storage
/// would never hold the bearer. Send it to the canonical origin before anything
/// else runs; the bootstrap token stays unclaimed and only travels to loopback.
async fn canonical_host(
    State(state): State<Web>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok());
    let alias = host.and_then(|h| h.strip_prefix("localhost:"));
    if alias.is_some() && alias == state.host.split_once(':').map(|(_, port)| port) {
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str());
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [
                (header::LOCATION, format!("{}{path}", state.origin)),
                (header::CACHE_CONTROL, "no-store".into()),
            ],
        )
            .into_response();
    }
    next.run(request).await
}

/// Serve built assets first, then the SPA entry point for browser page requests.
/// Reserved bridge/asset paths and non-HTML requests retain their normal 404s.
fn static_files(static_dir: PathBuf) -> Router {
    let index = ServeFile::new(static_dir.join("index.html"));
    let pages = get(move |request: axum::extract::Request| {
        let index = index.clone();
        async move {
            let path = request.uri().path();
            let reserved = matches!(
                path.split('/').nth(1),
                Some("api" | "connect" | "assets" | ".well-known")
            );
            let wants_html = request
                .headers()
                .get(header::ACCEPT)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value.split(',').any(|part| {
                        part.split(';')
                            .next()
                            .is_some_and(|mime| mime.trim() == "text/html")
                    })
                });
            if reserved || !wants_html {
                return StatusCode::NOT_FOUND.into_response();
            }
            index
                .oneshot(request)
                .await
                .unwrap_or_else(|never| match never {})
                .into_response()
        }
    });
    Router::new().fallback_service(ServeDir::new(static_dir).fallback(pages))
}
async fn connect(
    State(state): State<Web>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    if token != *state.bootstrap
        || headers.get(header::HOST).and_then(|h| h.to_str().ok()) != Some(state.host.as_str())
    {
        return (
            StatusCode::FORBIDDEN,
            "This is not the current Agent 206 authorization link. Use the newest link printed by `agent206 web`.",
        )
            .into_response();
    }
    if state
        .claimed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return (
            StatusCode::FORBIDDEN,
            format!(
                "This authorization link was already used. Open {}/ in the browser that used it, or restart `agent206 web` for a new link.",
                state.origin
            ),
        )
            .into_response();
    }
    (
        [
            // Remove the earlier preview's cross-port cookie if present.
            (
                header::SET_COOKIE,
                "a206_session=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/".into(),
            ),
            (header::LOCATION, format!("/#a206={}", state.token)),
            (header::CACHE_CONTROL, "no-store".into()),
            (header::REFERRER_POLICY, "no-referrer".into()),
        ],
        StatusCode::SEE_OTHER,
    )
        .into_response()
}
fn authorized(state: &Web, headers: &HeaderMap) -> bool {
    let bearer = format!("Bearer {}", state.token);
    headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        == Some(bearer.as_str())
        && headers.get(header::HOST).and_then(|h| h.to_str().ok()) == Some(state.host.as_str())
        && headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) == Some(state.origin.as_str())
        && headers
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            == Some("application/msgpack")
}
async fn command(State(state): State<Web>, headers: HeaderMap, bytes: Bytes) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut decoder = Decoder::default();
    let requests = decoder.feed::<Request<Command>>(&bytes);
    let Ok(mut requests) = requests else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if requests.len() != 1 || decoder.finish().is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let request = requests.remove(0);
    // Lifecycle changes must use the serialized, deduplicated user controller.
    if matches!(request.body, Command::Shutdown) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let operation = request.body.name();
    let body = state
        .client
        .call_with_id(request.id, request.body)
        .await
        .unwrap_or_else(|error| {
            Reply::Error(crate::error::correlated(operation, request.id, &error))
        });
    match a206_protocol::encode(&a206_protocol::Response {
        id: request.id,
        body,
    }) {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/msgpack"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn control(State(state): State<Web>, headers: HeaderMap, bytes: Bytes) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut decoder = Decoder::default();
    let Ok(mut requests) = decoder.feed::<Request<GatewayAction>>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if requests.len() != 1 || decoder.finish().is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let request = requests.remove(0);
    match state.client.control_with_id(request.id, request.body).await {
        Ok(report) => match a206_protocol::encode(&a206_protocol::Response {
            id: request.id,
            body: report,
        }) {
            Ok(bytes) => (
                [
                    (header::CONTENT_TYPE, "application/msgpack"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::CONFLICT.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn browser_deep_links_reload_without_masking_api_or_asset_errors() {
        let directory = tempfile::tempdir().unwrap();
        let html = "<!doctype html><title>Navigation test</title>";
        tokio::fs::write(directory.path().join("index.html"), html)
            .await
            .unwrap();
        tokio::fs::create_dir(directory.path().join("assets"))
            .await
            .unwrap();
        tokio::fs::write(directory.path().join("assets/app.css"), "body {}")
            .await
            .unwrap();
        let app = static_files(directory.path().into());
        let id = uuid::Uuid::new_v4();
        for path in [
            "/connections".into(),
            format!("/conversations/{id}?split={}", uuid::Uuid::new_v4()),
            "/unknown/page".into(),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(&path)
                        .header(header::ACCEPT, "text/html,application/xhtml+xml")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert!(
                response.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with("text/html")
            );
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap(),
                html
            );
        }
        for (method, path, accept, status) in [
            (
                "GET",
                "/assets/missing.js",
                "text/javascript",
                StatusCode::NOT_FOUND,
            ),
            (
                "GET",
                "/assets/missing.js",
                "text/html",
                StatusCode::NOT_FOUND,
            ),
            ("GET", "/api/missing", "text/html", StatusCode::NOT_FOUND),
            (
                "GET",
                "/connect/missing/path",
                "text/html",
                StatusCode::NOT_FOUND,
            ),
            (
                "GET",
                "/connections",
                "application/msgpack",
                StatusCode::NOT_FOUND,
            ),
            (
                "POST",
                "/connections",
                "text/html",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            ("GET", "/assets/app.css", "text/css", StatusCode::OK),
            ("HEAD", "/settings/appearance", "text/html", StatusCode::OK),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(path)
                        .header(header::ACCEPT, accept)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{method} {path}");
            if method == "HEAD" {
                assert!(
                    axum::body::to_bytes(response.into_body(), 1024)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
    fn bridge(port: u16) -> Web {
        Web {
            client: GatewayController::attach(
                std::env::temp_dir().join(uuid::Uuid::new_v4().to_string()),
                None,
            ),
            token: Arc::new(uuid::Uuid::new_v4().simple().to_string()),
            bootstrap: Arc::new(uuid::Uuid::new_v4().simple().to_string()),
            claimed: Arc::new(AtomicBool::new(false)),
            host: Arc::new(format!("127.0.0.1:{port}")),
            origin: Arc::new(format!("http://127.0.0.1:{port}")),
        }
    }
    async fn get_with_host(app: &Router, path: &str, host: &str) -> Response {
        app.clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .header(header::HOST, host)
                    .header(header::ACCEPT, "text/html")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn localhost_is_sent_to_the_canonical_origin_without_claiming_the_link() {
        let directory = tempfile::tempdir().unwrap();
        tokio::fs::write(directory.path().join("index.html"), "<!doctype html>")
            .await
            .unwrap();
        let port = 20_000 + (uuid::Uuid::new_v4().as_u128() % 10_000) as u16;
        let state = bridge(port);
        let link = format!("/connect/{}", state.bootstrap);
        let app = app(state.clone(), directory.path().into());
        let response =
            get_with_host(&app, &format!("{link}?x=1"), &format!("localhost:{port}")).await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response.headers()[header::LOCATION],
            format!("{}{link}?x=1", state.origin).as_str()
        );
        assert!(!state.claimed.load(Ordering::Acquire));
        // Another port's localhost is not this bridge's alias.
        let other = get_with_host(&app, "/", &format!("localhost:{}", port + 1)).await;
        assert_ne!(other.status(), StatusCode::TEMPORARY_REDIRECT);
        let claimed = get_with_host(&app, &link, &state.host).await;
        assert_eq!(claimed.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            claimed.headers()[header::LOCATION],
            format!("/#a206={}", state.token).as_str()
        );
        let reused = get_with_host(&app, &link, &state.host).await;
        assert_eq!(reused.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(reused.into_body(), 4096)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("already used"), "{body}");
        assert!(!body.contains(state.token.as_str()));
    }
    #[test]
    fn auth_requires_an_explicit_origin_bound_bearer() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let token = uuid::Uuid::new_v4().simple().to_string();
        let state = Web {
            client: GatewayController::attach(root, None),
            token: Arc::new(token.clone()),
            bootstrap: Arc::new(uuid::Uuid::new_v4().to_string()),
            claimed: Arc::new(AtomicBool::new(false)),
            host: Arc::new("127.0.0.1:8206".into()),
            origin: Arc::new("http://127.0.0.1:8206".into()),
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, state.host.parse().unwrap());
        headers.insert(header::ORIGIN, state.origin.parse().unwrap());
        headers.insert(header::CONTENT_TYPE, "application/msgpack".parse().unwrap());
        headers.insert(
            header::COOKIE,
            format!("a206_session={token}").parse().unwrap(),
        );
        assert!(!authorized(&state, &headers));
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(authorized(&state, &headers));
        headers.insert(header::ORIGIN, "http://127.0.0.1:9999".parse().unwrap());
        assert!(!authorized(&state, &headers));
    }
}
