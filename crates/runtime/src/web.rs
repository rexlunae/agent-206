//! Loopback web bridge. Origin-scoped bearer sessions avoid leaking cookies to preview servers.
use crate::{
    connection::{self, ConnectionError},
    control::GatewayController,
};
use a206_core::{
    Command, ErrorReport, GatewayAction, GatewayConnection, GatewayTarget, Reply, SetupReply,
    SetupRequest, SetupStatus,
};
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
        Arc, PoisonError, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};
#[derive(Clone)]
struct Web {
    gateway: Arc<RwLock<Gateway>>,
    root: Arc<PathBuf>,
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
    #[error("cannot use the gateway given with --remote")]
    Remote(#[source] ConnectionError),
}
/// The gateway the bridge fronts. Until one is saved (or a saved one is
/// repaired) the browser is offered setup and nothing connects or launches.
struct Gateway {
    client: Option<GatewayController>,
    pinned: Option<String>,
    problem: Option<ErrorReport>,
}
impl Gateway {
    fn resolve(root: &std::path::Path, remote: Option<String>) -> Result<Self, WebError> {
        Ok(match connection::resolve(root, remote.clone()) {
            Ok(target) => Gateway {
                client: target.map(|target| {
                    GatewayController::new(root.into(), target.remote().map(str::to_owned))
                }),
                pinned: remote,
                problem: None,
            },
            Err(error) if remote.is_some() => return Err(WebError::Remote(error)),
            // A damaged saved setup is shown to the browser, which can replace it.
            Err(error) => Gateway {
                client: None,
                pinned: None,
                problem: Some(crate::error::handle("load gateway setup", &error)),
            },
        })
    }
}
impl Web {
    /// The current controller. The lock guards a plain swap, so a poisoned
    /// value is still consistent; it is never held across an await.
    fn client(&self) -> Option<GatewayController> {
        self.gateway
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .client
            .clone()
    }
    /// A saved setup that cannot be read is reported as the problem to fix.
    fn status(&self) -> SetupStatus {
        let gateway = self.gateway.read().unwrap_or_else(PoisonError::into_inner);
        let (configured, problem) = match connection::load(&self.root) {
            Ok(saved) => (saved.map(|saved| saved.target), gateway.problem.clone()),
            Err(error) => (
                None,
                Some(crate::error::handle("load gateway setup", &error)),
            ),
        };
        SetupStatus {
            configured,
            pinned: gateway.pinned.clone(),
            problem,
        }
    }
    /// Save a verified target and switch to it in one step, so the file and the
    /// controller cannot disagree when two tabs save at once.
    fn install(&self, target: GatewayTarget) -> Result<(), ConnectionError> {
        let mut gateway = self.gateway.write().unwrap_or_else(PoisonError::into_inner);
        connection::save(
            &self.root,
            &GatewayConnection {
                target: target.clone(),
            },
        )?;
        *gateway = Gateway {
            client: Some(GatewayController::new(
                self.root.to_path_buf(),
                target.remote().map(str::to_owned),
            )),
            pinned: None,
            problem: None,
        };
        Ok(())
    }
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
    let gateway = Gateway::resolve(&root, remote)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let state = Web {
        gateway: Arc::new(RwLock::new(gateway)),
        root: Arc::new(root),
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
        .route("/api/setup", post(setup))
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
    let body = match state.client() {
        Some(client) => client
            .call_with_id(request.id, request.body)
            .await
            .unwrap_or_else(|error| {
                Reply::Error(crate::error::correlated(operation, request.id, &error))
            }),
        None => Reply::Error(crate::error::correlated(
            operation,
            request.id,
            &ConnectionError::NotConfigured,
        )),
    };
    respond(StatusCode::OK, request.id, &body)
}
/// A MessagePack response frame for request `id`.
fn respond<T: serde::Serialize>(status: StatusCode, id: uuid::Uuid, body: &T) -> Response {
    match a206_protocol::encode(&a206_protocol::Response { id, body }) {
        Ok(bytes) => (
            status,
            [
                (header::CONTENT_TYPE, "application/msgpack"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => {
            crate::error::correlated("encode web response", id, &error);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
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
    let operation = request.body.name();
    // A failed control answers 409 with the structured report as its body.
    let failed = |error: &(dyn std::error::Error + 'static)| {
        let report = crate::error::correlated(operation, request.id, error);
        respond(StatusCode::CONFLICT, request.id, &report)
    };
    match state.client() {
        Some(client) => match client.control_with_id(request.id, request.body).await {
            Ok(report) => respond(StatusCode::OK, request.id, &report),
            Err(error) => failed(&error),
        },
        None => failed(&ConnectionError::NotConfigured),
    }
}

/// Gateway setup for the browser. SSH runs here, with the bridge's OpenSSH
/// configuration and the profile's known_hosts; the browser sees only
/// fingerprints and chooses which one it verified.
async fn setup(State(state): State<Web>, headers: HeaderMap, bytes: Bytes) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut decoder = Decoder::default();
    let Ok(mut requests) = decoder.feed::<Request<SetupRequest>>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if requests.len() != 1 || decoder.finish().is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let request = requests.remove(0);
    let root = state.root.as_path();
    let outcome = match request.body {
        SetupRequest::Status => Ok(SetupReply::Status(state.status())),
        SetupRequest::Probe { endpoint } => connection::probe(root, &endpoint)
            .await
            .map(SetupReply::Probe),
        SetupRequest::Trust {
            endpoint,
            fingerprint,
        } => connection::trust_fingerprints(root, &endpoint, &[fingerprint])
            .await
            .map(SetupReply::Probe),
        SetupRequest::Save { target } => match connection::verify(root, &target).await {
            Ok(()) => state.install(target).map(|()| SetupReply::Saved),
            Err(error) => Err(error),
        },
    };
    let reply = outcome.unwrap_or_else(|error| {
        SetupReply::Error(crate::error::correlated(
            "gateway setup",
            request.id,
            &error,
        ))
    });
    respond(StatusCode::OK, request.id, &reply)
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
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        Web {
            gateway: Arc::new(RwLock::new(Gateway {
                client: Some(GatewayController::attach(root.clone(), None)),
                pinned: None,
                problem: None,
            })),
            root: Arc::new(root),
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
        let state = bridge(8206);
        let token = state.token.to_string();
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
    /// An unconfigured bridge over a fresh profile that has never connected.
    fn unconfigured(root: &std::path::Path) -> (Web, Router) {
        let mut state = bridge(8206);
        state.root = Arc::new(root.into());
        state.gateway = Arc::new(RwLock::new(Gateway::resolve(root, None).unwrap()));
        let directory = root.join("static");
        let app = app(state.clone(), directory);
        (state, app)
    }
    async fn post<T: serde::Serialize, R: serde::de::DeserializeOwned>(
        app: &Router,
        state: &Web,
        path: &str,
        body: T,
    ) -> (StatusCode, R) {
        let id = uuid::Uuid::new_v4();
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::HOST, state.host.as_str())
                    .header(header::ORIGIN, state.origin.as_str())
                    .header(header::CONTENT_TYPE, "application/msgpack")
                    .header(header::AUTHORIZATION, format!("Bearer {}", state.token))
                    .body(axum::body::Body::from(
                        a206_protocol::encode(&Request { id, body }).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), a206_protocol::MAX_FRAME)
            .await
            .unwrap();
        let mut decoder = Decoder::default();
        let mut frames = decoder.feed::<a206_protocol::Response<R>>(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        let frame = frames.remove(0);
        assert_eq!(frame.id, id);
        (status, frame.body)
    }
    #[tokio::test]
    async fn an_unconfigured_bridge_offers_setup_and_reports_why_it_cannot_serve() {
        let root = tempfile::tempdir().unwrap();
        let (state, app) = unconfigured(root.path());
        let (_, status) =
            post::<_, SetupReply>(&app, &state, "/api/setup", SetupRequest::Status).await;
        let SetupReply::Status(status) = status else {
            panic!("unexpected {status:?}");
        };
        assert!(status.required());
        assert_eq!(status.configured, None);
        let (_, reply) = post::<_, Reply>(&app, &state, "/api/command", Command::Snapshot).await;
        let Reply::Error(report) = reply else {
            panic!("unexpected {reply:?}");
        };
        assert_eq!(report.kind, a206_core::ErrorKind::Unavailable);
        assert!(
            report.message.contains("no gateway is configured"),
            "{report:?}"
        );
        assert!(report.correlation.is_some());
        let (code, report) =
            post::<_, ErrorReport>(&app, &state, "/api/gateway", GatewayAction::Start).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(report.kind, a206_core::ErrorKind::Unavailable);
        // Nothing was saved, launched or created on the browser's behalf.
        assert!(!root.path().join("connection.toml").exists());
        assert!(state.client().is_none());
    }
    #[tokio::test]
    async fn setup_rejects_unsafe_targets_without_saving_them() {
        let root = tempfile::tempdir().unwrap();
        let (state, app) = unconfigured(root.path());
        let endpoint = format!("ssh://-oProxyCommand={}", uuid::Uuid::new_v4().simple());
        for request in [
            SetupRequest::Probe {
                endpoint: endpoint.clone(),
            },
            SetupRequest::Trust {
                endpoint: endpoint.clone(),
                fingerprint: format!("SHA256:{}", uuid::Uuid::new_v4().simple()),
            },
            SetupRequest::Save {
                target: GatewayTarget::Ssh {
                    endpoint: endpoint.clone(),
                },
            },
        ] {
            let (_, reply) = post::<_, SetupReply>(&app, &state, "/api/setup", request).await;
            let SetupReply::Error(report) = reply else {
                panic!("unexpected {reply:?}");
            };
            assert_eq!(
                report.kind,
                a206_core::ErrorKind::InvalidRequest,
                "{report:?}"
            );
            // The cause chain reaches the browser intact.
            assert!(report.source.is_some(), "{report:?}");
        }
        assert!(!root.path().join("connection.toml").exists());
        assert!(!connection::known_hosts(root.path()).exists());
        assert!(state.client().is_none());
    }
    #[tokio::test]
    async fn a_damaged_saved_setup_is_shown_to_the_browser_for_repair() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("connection.toml"), "target = 7").unwrap();
        let (state, app) = unconfigured(root.path());
        let (_, reply) =
            post::<_, SetupReply>(&app, &state, "/api/setup", SetupRequest::Status).await;
        let SetupReply::Status(status) = reply else {
            panic!("unexpected {reply:?}");
        };
        assert!(status.required());
        let report = status.problem.unwrap();
        assert_eq!(report.kind, a206_core::ErrorKind::InvalidRequest);
        assert!(report.source.is_some(), "{report:?}");
        let problem = state.gateway.read().unwrap().problem.clone().unwrap();
        assert_eq!(problem.kind, a206_core::ErrorKind::InvalidRequest);
        assert!(state.client().is_none());
    }
    #[tokio::test]
    async fn setup_requires_the_bridge_bearer() {
        let root = tempfile::tempdir().unwrap();
        let (state, app) = unconfigured(root.path());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/setup")
                    .header(header::HOST, state.host.as_str())
                    .header(header::ORIGIN, state.origin.as_str())
                    .header(header::CONTENT_TYPE, "application/msgpack")
                    .body(axum::body::Body::from(
                        a206_protocol::encode(&Request {
                            id: uuid::Uuid::new_v4(),
                            body: SetupRequest::Save {
                                target: GatewayTarget::Local,
                            },
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!root.path().join("connection.toml").exists());
    }
}
