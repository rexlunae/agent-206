//! Web bridge. By default it listens on loopback, and a one-time link hands the
//! browser an origin-scoped bearer (cookies would leak to preview servers).
//! Opened to other devices it serves HTTPS only and browsers sign in with the
//! vault's authenticator; see [`access`].
mod access;
pub mod tls;
mod vault;
pub use access::SignInError;
pub use tls::{Certificate, TlsError};

use crate::{
    connection::{self, ConnectionError},
    control::GatewayController,
};
use a206_core::{
    Command, ErrorReport, GatewayAction, GatewayConnection, GatewayTarget, Reply, SetupReply,
    SetupRequest, SetupStatus, SignInReply, SignInRequest,
};
use a206_protocol::{Decoder, Request};
use access::{Access, Loopback, Network};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError, RwLock, atomic::AtomicBool},
    time::Instant,
};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};
#[derive(Clone)]
struct Web {
    vault: Arc<vault::Relay>,
    gateway: Arc<RwLock<Gateway>>,
    root: Arc<PathBuf>,
    access: Arc<Access>,
}
/// Where the bridge listens and who may use it.
#[derive(Clone, Debug)]
pub enum Exposure {
    /// This computer only, authorized by a one-time link.
    Loopback { port: u16 },
    /// Other devices too, over HTTPS with authenticator sign-in.
    Network(NetworkOptions),
}
#[derive(Clone, Debug)]
pub struct NetworkOptions {
    pub listen: SocketAddr,
    /// Host names or IP addresses browsers use to reach this server. Requests
    /// for any other host are refused, which defeats DNS rebinding.
    pub names: Vec<String>,
    pub certificate: Certificate,
}
/// Why the web bridge could not run.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("could not listen on {addr}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("web bridge stopped")]
    Serve(#[source] std::io::Error),
    #[error("cannot use the gateway given with --remote")]
    Remote(#[source] ConnectionError),
    #[error(
        "name at least one host name or IP address that other devices use to reach this server"
    )]
    NoNames,
    #[error("{name:?} is not a host name or IP address")]
    Name { name: String },
    #[error("cannot serve HTTPS")]
    Tls(#[source] TlsError),
    #[error("browsers on other devices could not sign in")]
    SignIn(#[source] SignInError),
}
impl crate::error::Classified for WebError {
    fn kind(&self) -> Option<a206_core::ErrorKind> {
        use a206_core::ErrorKind;
        Some(match self {
            Self::Bind { .. } | Self::Serve(_) => ErrorKind::Unavailable,
            Self::NoNames | Self::Name { .. } => ErrorKind::InvalidRequest,
            Self::Remote(_) | Self::Tls(_) | Self::SignIn(_) => return None,
        })
    }
}
/// The gateway the bridge fronts. Until one is saved (or a saved one is
/// repaired) the browser is offered setup and nothing connects or launches.
struct Gateway {
    client: Option<GatewayController>,
    /// The target `client` talks to; status reports this, never a file that
    /// another client may have rewritten since.
    target: Option<GatewayTarget>,
    pinned: bool,
    problem: Option<ErrorReport>,
}
impl Gateway {
    fn resolve(root: &std::path::Path, remote: Option<String>) -> Result<Self, WebError> {
        Ok(match connection::resolve(root, remote.clone()) {
            Ok(target) => Gateway {
                client: target.as_ref().map(|target| {
                    GatewayController::new(root.into(), target.remote().map(str::to_owned))
                }),
                target,
                pinned: remote.is_some(),
                problem: None,
            },
            Err(error) if remote.is_some() => return Err(WebError::Remote(error)),
            // A damaged saved setup is shown to the browser, which can replace it.
            Err(error) => Gateway {
                client: None,
                target: None,
                pinned: false,
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
    fn status(&self) -> SetupStatus {
        let gateway = self.gateway.read().unwrap_or_else(PoisonError::into_inner);
        SetupStatus {
            active: gateway.target.clone(),
            pinned: gateway.pinned,
            problem: gateway.problem.clone(),
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
            target: Some(target),
            pinned: false,
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
    exposure: Exposure,
) -> Result<(), WebError> {
    match exposure {
        Exposure::Loopback { port } => serve_loopback(root, remote, static_dir, port).await,
        Exposure::Network(options) => serve_network(root, remote, static_dir, options).await,
    }
}

async fn serve_loopback(
    root: PathBuf,
    remote: Option<String>,
    static_dir: PathBuf,
    port: u16,
) -> Result<(), WebError> {
    let addr = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| WebError::Bind { addr, source })?;
    let port = listener
        .local_addr()
        .map_err(|source| WebError::Bind { addr, source })?
        .port();
    let gateway = Gateway::resolve(&root, remote)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let state = Web {
        vault: Arc::default(),
        gateway: Arc::new(RwLock::new(gateway)),
        root: Arc::new(root),
        access: Arc::new(Access::Loopback(Loopback {
            token: uuid::Uuid::new_v4().simple().to_string(),
            bootstrap: token.clone(),
            claimed: AtomicBool::new(false),
            origin: format!("http://127.0.0.1:{port}"),
            host: format!("127.0.0.1:{port}"),
        })),
    };
    println!("Open http://127.0.0.1:{port}/connect/{token} to authorize this browser.");
    axum::serve(listener, app(state, static_dir))
        .await
        .map_err(WebError::Serve)?;
    Ok(())
}

/// HTTPS for other devices. Nothing is served over plain HTTP, and the vault
/// must be able to check authenticator codes before the server starts.
async fn serve_network(
    root: PathBuf,
    remote: Option<String>,
    static_dir: PathBuf,
    options: NetworkOptions,
) -> Result<(), WebError> {
    let addr = options.listen;
    let listener =
        std::net::TcpListener::bind(addr).map_err(|source| WebError::Bind { addr, source })?;
    let bound = listener
        .set_nonblocking(true)
        .and_then(|()| listener.local_addr())
        .map_err(|source| WebError::Bind { addr, source })?;
    // Port 0 asks the system for one; browsers send the one it assigned.
    let (names, hosts) = host_names(&options.names, bound.port())?;
    access::ready(&root).await.map_err(WebError::SignIn)?;
    let tls = tls::load(
        &root,
        &names,
        &options.certificate,
        std::time::SystemTime::now(),
    )
    .map_err(WebError::Tls)?;
    let gateway = Gateway::resolve(&root, remote)?;
    let urls: Vec<_> = hosts
        .iter()
        .map(|host| format!("https://{host}/"))
        .collect();
    let state = Web {
        vault: Arc::default(),
        gateway: Arc::new(RwLock::new(gateway)),
        root: Arc::new(root.clone()),
        access: Arc::new(Access::Network(Network {
            hosts,
            root,
            sessions: Mutex::default(),
        })),
    };
    println!(
        "Serving Agent 206 to other devices at {}. Sign in with a code from the vault's authenticator app.",
        urls.join(" and ")
    );
    println!("Certificate SHA-256 fingerprint: {}", tls.fingerprint);
    if matches!(options.certificate, Certificate::Generated) {
        println!(
            "The certificate is self-signed: continue past the browser's warning only if it shows this fingerprint."
        );
    }
    axum_server::from_tcp_rustls(
        listener,
        axum_server::tls_rustls::RustlsConfig::from_config(tls.config),
    )
    .map_err(|source| WebError::Bind { addr, source })?
    .serve(app(state, static_dir).into_make_service())
    .await
    .map_err(WebError::Serve)
}

/// Certificate names and the `Host` values browsers send for them. Port 443
/// is left out of `Host` by browsers, so it is left out here too.
fn host_names(names: &[String], port: u16) -> Result<(Vec<String>, Vec<String>), WebError> {
    if names.is_empty() {
        return Err(WebError::NoNames);
    }
    let mut certified = Vec::new();
    let mut hosts = Vec::new();
    for name in names {
        let trimmed = name.trim_start_matches('[').trim_end_matches(']');
        let host = match trimmed.parse::<IpAddr>() {
            Ok(IpAddr::V6(ip)) => (ip.to_string(), format!("[{ip}]")),
            Ok(IpAddr::V4(ip)) => (ip.to_string(), ip.to_string()),
            Err(_) => match url::Host::parse(name) {
                Ok(url::Host::Domain(domain))
                    if !domain.is_empty()
                        && domain
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.') =>
                {
                    (domain.clone(), domain)
                }
                _ => return Err(WebError::Name { name: name.clone() }),
            },
        };
        hosts.push(if port == 443 {
            host.1
        } else {
            format!("{}:{port}", host.1)
        });
        certified.push(host.0);
    }
    Ok((certified, hosts))
}

fn app(state: Web, static_dir: PathBuf) -> Router {
    Router::new()
        .route("/connect/{token}", get(connect))
        .route("/api/command", post(command))
        .route("/api/gateway", post(control))
        .route("/api/setup", post(setup))
        .route("/api/vault", post(vault::handle))
        .route("/api/sign-in", post(sign_in))
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
    let loopback = match state.access.as_ref() {
        Access::Loopback(loopback) => loopback,
        // Opened to other devices, nothing (not even the sign-in page) is
        // served for a name that was not listed.
        Access::Network(network) => {
            return if network.serves(request.headers()) {
                next.run(request).await
            } else {
                StatusCode::MISDIRECTED_REQUEST.into_response()
            };
        }
    };
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok());
    let alias = host.and_then(|h| h.strip_prefix("localhost:"));
    if alias.is_some() && alias == loopback.host.split_once(':').map(|(_, port)| port) {
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str());
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [
                (header::LOCATION, format!("{}{path}", loopback.origin)),
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
    // Opened to other devices, browsers sign in instead.
    let Some(loopback) = state.access.loopback() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if token != loopback.bootstrap
        || headers.get(header::HOST).and_then(|h| h.to_str().ok()) != Some(loopback.host.as_str())
    {
        return (
            StatusCode::FORBIDDEN,
            "This is not the current Agent 206 authorization link. Use the newest link printed by `agent206 web`.",
        )
            .into_response();
    }
    if !loopback.claim() {
        return (
            StatusCode::FORBIDDEN,
            format!(
                "This authorization link was already used. Open {}/ in the browser that used it, or restart `agent206 web` for a new link.",
                loopback.origin
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
            (header::LOCATION, format!("/#a206={}", loopback.token)),
            (header::CACHE_CONTROL, "no-store".into()),
            (header::REFERRER_POLICY, "no-referrer".into()),
        ],
        StatusCode::SEE_OTHER,
    )
        .into_response()
}
fn authorized(state: &Web, headers: &HeaderMap) -> bool {
    state.access.authorized(headers, Instant::now())
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

/// Sign-in for a server opened to other devices. Only a same-origin request
/// is answered; signing out also needs the session it ends.
async fn sign_in(State(state): State<Web>, headers: HeaderMap, bytes: Bytes) -> Response {
    if !state.access.same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut decoder = Decoder::default();
    let Ok(mut requests) = decoder.feed::<Request<SignInRequest>>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if requests.len() != 1 || decoder.finish().is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let request = requests.remove(0);
    let network = match state.access.as_ref() {
        Access::Network(network) => Some(network),
        Access::Loopback(_) => None,
    };
    let outcome = match (request.body, network) {
        (SignInRequest::Methods, network) => Ok(SignInReply::Methods {
            totp: network.is_some(),
        }),
        (SignInRequest::Totp { code }, Some(network)) => network
            .sign_in(&code)
            .await
            .map(|session| SignInReply::SignedIn { session }),
        (SignInRequest::SignOut, Some(network)) => {
            if !authorized(&state, &headers) {
                return StatusCode::FORBIDDEN.into_response();
            }
            if let Some(token) = access::bearer(&headers) {
                network.sign_out(token);
            }
            Ok(SignInReply::SignedOut)
        }
        (SignInRequest::Totp { .. } | SignInRequest::SignOut, None) => Err(SignInError::NotOffered),
    };
    let reply = outcome.unwrap_or_else(|error| {
        SignInReply::Error(crate::error::correlated("sign in", request.id, &error))
    });
    respond(StatusCode::OK, request.id, &reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
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
    impl Web {
        fn lo(&self) -> &Loopback {
            self.access.loopback().unwrap()
        }
    }
    fn bridge(port: u16) -> Web {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        Web {
            vault: Arc::default(),
            gateway: Arc::new(RwLock::new(Gateway {
                client: Some(GatewayController::attach(root.clone(), None)),
                target: Some(GatewayTarget::Local),
                pinned: false,
                problem: None,
            })),
            root: Arc::new(root),
            access: Arc::new(Access::Loopback(Loopback {
                token: uuid::Uuid::new_v4().simple().to_string(),
                bootstrap: uuid::Uuid::new_v4().simple().to_string(),
                claimed: AtomicBool::new(false),
                host: format!("127.0.0.1:{port}"),
                origin: format!("http://127.0.0.1:{port}"),
            })),
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
        let link = format!("/connect/{}", state.lo().bootstrap);
        let app = app(state.clone(), directory.path().into());
        let response =
            get_with_host(&app, &format!("{link}?x=1"), &format!("localhost:{port}")).await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response.headers()[header::LOCATION],
            format!("{}{link}?x=1", state.lo().origin).as_str()
        );
        assert!(!state.lo().claimed.load(Ordering::Acquire));
        // Another port's localhost is not this bridge's alias.
        let other = get_with_host(&app, "/", &format!("localhost:{}", port + 1)).await;
        assert_ne!(other.status(), StatusCode::TEMPORARY_REDIRECT);
        let claimed = get_with_host(&app, &link, &state.lo().host).await;
        assert_eq!(claimed.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            claimed.headers()[header::LOCATION],
            format!("/#a206={}", state.lo().token).as_str()
        );
        let reused = get_with_host(&app, &link, &state.lo().host).await;
        assert_eq!(reused.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(reused.into_body(), 4096)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("already used"), "{body}");
        assert!(!body.contains(state.lo().token.as_str()));
    }
    #[test]
    fn auth_requires_an_explicit_origin_bound_bearer() {
        let state = bridge(8206);
        let token = state.lo().token.to_string();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, state.lo().host.parse().unwrap());
        headers.insert(header::ORIGIN, state.lo().origin.parse().unwrap());
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
        let lo = state.lo();
        let (status, reply) =
            post_as(app, (&lo.host, &lo.origin), Some(&lo.token), path, body).await;
        (status, reply.expect("a reply frame"))
    }
    /// Post as a page at `origin` reaching `host`; `None` when no frame came back.
    async fn post_as<T: serde::Serialize, R: serde::de::DeserializeOwned>(
        app: &Router,
        (host, origin): (&str, &str),
        bearer: Option<&str>,
        path: &str,
        body: T,
    ) -> (StatusCode, Option<R>) {
        let id = uuid::Uuid::new_v4();
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, host)
            .header(header::ORIGIN, origin)
            .header(header::CONTENT_TYPE, "application/msgpack");
        if let Some(bearer) = bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        let response = app
            .clone()
            .oneshot(
                request
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
        if bytes.is_empty() {
            return (status, None);
        }
        let mut decoder = Decoder::default();
        let mut frames = decoder.feed::<a206_protocol::Response<R>>(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        let frame = frames.remove(0);
        assert_eq!(frame.id, id);
        (status, Some(frame.body))
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
        assert_eq!(status.active, None);
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
                    .header(header::HOST, state.lo().host.as_str())
                    .header(header::ORIGIN, state.lo().origin.as_str())
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
    #[tokio::test]
    async fn status_reports_the_gateway_in_use_not_a_rewritten_file() {
        let root = tempfile::tempdir().unwrap();
        let (state, app) = unconfigured(root.path());
        let pinned = GatewayTarget::Ssh {
            endpoint: format!("ssh://u{}@gateway.test", uuid::Uuid::new_v4().simple()),
        };
        // A pinned gateway is in use; the damaged default file is not consulted.
        *state.gateway.write().unwrap() = Gateway {
            client: None,
            target: Some(pinned.clone()),
            pinned: true,
            problem: None,
        };
        std::fs::write(root.path().join("connection.toml"), "target = 7").unwrap();
        let (_, reply) =
            post::<_, SetupReply>(&app, &state, "/api/setup", SetupRequest::Status).await;
        let SetupReply::Status(status) = reply else {
            panic!("unexpected {reply:?}");
        };
        assert!(!status.required());
        assert_eq!(status.active, Some(pinned.clone()));
        assert!(status.pinned && status.problem.is_none());
        // Another client saving a different gateway does not change what this
        // bridge reports, because it does not change what this bridge uses.
        connection::save(
            root.path(),
            &GatewayConnection {
                target: GatewayTarget::Local,
            },
        )
        .unwrap();
        let (_, reply) =
            post::<_, SetupReply>(&app, &state, "/api/setup", SetupRequest::Status).await;
        let SetupReply::Status(status) = reply else {
            panic!("unexpected {reply:?}");
        };
        assert_eq!(status.active, Some(pinned));
    }

    #[test]
    fn network_names_become_certificate_names_and_host_values() {
        let domain = format!("h{}.test", uuid::Uuid::new_v4().simple());
        let (names, hosts) = host_names(
            &[domain.clone(), "192.0.2.7".into(), "[2001:db8::7]".into()],
            8443,
        )
        .unwrap();
        assert_eq!(names, [domain.as_str(), "192.0.2.7", "2001:db8::7"]);
        assert_eq!(
            hosts,
            [
                format!("{domain}:8443"),
                "192.0.2.7:8443".into(),
                "[2001:db8::7]:8443".into()
            ]
        );
        // Browsers leave the default HTTPS port out of `Host`.
        let (_, hosts) = host_names(std::slice::from_ref(&domain), 443).unwrap();
        assert_eq!(hosts, [domain]);
        assert!(matches!(host_names(&[], 8443), Err(WebError::NoNames)));
        for bad in ["", "two words", "host:8443", "a/b", "user@host"] {
            assert!(
                matches!(host_names(&[bad.into()], 8443), Err(WebError::Name { .. })),
                "{bad:?}"
            );
        }
    }

    const HOST: &str = "gateway.test:8443";
    const ORIGIN: &str = "https://gateway.test:8443";
    /// A bridge opened to other devices over `root`'s vault.
    fn network_bridge(root: &std::path::Path) -> Router {
        let state = Web {
            vault: Arc::default(),
            gateway: Arc::new(RwLock::new(Gateway::resolve(root, None).unwrap())),
            root: Arc::new(root.into()),
            access: Arc::new(Access::Network(Network {
                hosts: vec![HOST.into()],
                root: root.into(),
                sessions: Mutex::default(),
            })),
        };
        app(state, root.join("static"))
    }
    async fn sign_in_with(app: &Router, code: &str) -> SignInReply {
        let (status, reply) = post_as(
            app,
            (HOST, ORIGIN),
            None,
            "/api/sign-in",
            SignInRequest::Totp { code: code.into() },
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        reply.unwrap()
    }
    fn refused(reply: SignInReply) -> ErrorReport {
        match reply {
            SignInReply::Error(report) => report,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_loopback_bridge_offers_no_code_sign_in() {
        let root = tempfile::tempdir().unwrap();
        let (state, app) = unconfigured(root.path());
        let (_, methods) =
            post::<_, SignInReply>(&app, &state, "/api/sign-in", SignInRequest::Methods).await;
        assert_eq!(methods, SignInReply::Methods { totp: false });
        let (_, reply) = post::<_, SignInReply>(
            &app,
            &state,
            "/api/sign-in",
            SignInRequest::Totp {
                code: "123456".into(),
            },
        )
        .await;
        assert_eq!(refused(reply).kind, a206_core::ErrorKind::InvalidRequest);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn other_devices_sign_in_with_the_vaults_authenticator() {
        use a206_broker::{
            Client, Reply as BrokerReply, Request as BrokerRequest, Role, WireSecret,
        };
        let root = tempfile::tempdir().unwrap();
        let app = network_bridge(root.path());
        // Without a broker nothing signs in, and nothing is started for the request.
        let report = refused(sign_in_with(&app, "123456").await);
        assert_eq!(report.kind, a206_core::ErrorKind::Unavailable);
        assert!(report.correlation.is_some());
        assert!(!a206_broker::socket_path(root.path()).exists());

        let broker_root = root.path().to_owned();
        // Deliberately detached: the broker serves until the test runtime ends.
        tokio::spawn(async move { a206_broker::serve(&broker_root).await });
        let socket = a206_broker::socket_path(root.path());
        for _ in 0..500 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut user = Client::connect(root.path(), Role::User).await.unwrap();
        let password = uuid::Uuid::new_v4().to_string();
        user.call(BrokerRequest::Unlock {
            password: WireSecret::new(password.clone()),
            totp: None,
            create: true,
        })
        .await
        .unwrap();
        // An unlocked vault without an authenticator cannot sign anyone in.
        let report = refused(sign_in_with(&app, "123456").await);
        assert_eq!(report.kind, a206_core::ErrorKind::PermissionDenied);
        let BrokerReply::TotpEnrollment { manual_key, .. } = user
            .call(BrokerRequest::BeginTotp {
                account: uuid::Uuid::new_v4().simple().to_string(),
            })
            .await
            .unwrap()
        else {
            panic!("no enrollment");
        };
        let generator = totp_rs::TOTP::new_unchecked(
            totp_rs::Algorithm::SHA1,
            6,
            1,
            30,
            totp_rs::Secret::Encoded(manual_key.expose().to_owned())
                .to_bytes()
                .unwrap(),
            None,
            String::new(),
        );
        let now = a206_vault::unix_now();
        user.call(BrokerRequest::ConfirmTotp {
            code: generator.generate(now),
        })
        .await
        .unwrap();
        // The next step's code is unused and inside the accepted window.
        let code = generator.generate(now + 30);
        let SignInReply::SignedIn { session } = sign_in_with(&app, &code).await else {
            panic!("not signed in");
        };
        assert_eq!(
            format!(
                "{:?}",
                SignInReply::SignedIn {
                    session: session.clone()
                }
            ),
            "SignedIn { session: [REDACTED] }"
        );
        let (status, reply) = post_as::<_, SetupReply>(
            &app,
            (HOST, ORIGIN),
            Some(&session),
            "/api/setup",
            SetupRequest::Status,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(matches!(reply, Some(SetupReply::Status(_))));
        // The same session from a page on another origin is refused.
        let (status, _) = post_as::<_, SetupReply>(
            &app,
            (HOST, "https://elsewhere.test"),
            Some(&session),
            "/api/setup",
            SetupRequest::Status,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // A used code cannot sign in again, and the refusal stays typed.
        let report = refused(sign_in_with(&app, &code).await);
        assert_eq!(report.kind, a206_core::ErrorKind::PermissionDenied);
        assert!(report.source.is_some(), "{report:?}");
        // Signing out ends the session.
        let (_, reply) = post_as::<_, SignInReply>(
            &app,
            (HOST, ORIGIN),
            Some(&session),
            "/api/sign-in",
            SignInRequest::SignOut,
        )
        .await;
        assert_eq!(reply, Some(SignInReply::SignedOut));
        let (status, _) = post_as::<_, SetupReply>(
            &app,
            (HOST, ORIGIN),
            Some(&session),
            "/api/setup",
            SetupRequest::Status,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // A locked vault signs nobody in.
        user.call(BrokerRequest::Lock).await.unwrap();
        let report = refused(sign_in_with(&app, &generator.generate(now + 30)).await);
        assert_eq!(report.kind, a206_core::ErrorKind::Unavailable);
        // There is no one-time link to claim.
        let link = get_with_host(&app, "/connect/anything", HOST).await;
        assert_eq!(link.status(), StatusCode::NOT_FOUND);
        // An unlisted name gets nothing, not even the page that asks for codes.
        for path in ["/", "/connect/anything"] {
            let other = get_with_host(&app, path, "rebound.test:8443").await;
            assert_eq!(other.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn browser_vault_setup_is_ciphertext_only_and_sessions_are_owner_bound() {
        use a206_broker::{Client, Reply as BR, Request as B, Role};
        use a206_vault_channel::{
            Handshake, RelayReply as RR, RelayRequest as RQ, Reply as R, Request as Q, Secret,
        };
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let serving = root.clone();
        let broker = tokio::spawn(async move { a206_broker::serve(&serving).await.unwrap() });
        for _ in 0..200 {
            if a206_broker::socket_path(&root).exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut user = Client::connect(&root, Role::User).await.unwrap();
        let BR::ChannelKey(public) = user.call(B::ChannelKey).await.unwrap() else {
            panic!();
        };
        let mut state = bridge(8206);
        state.root = Arc::new(root.clone());
        let router = app(state.clone(), root.join("static"));
        let (status, _) = post_as::<_, RR>(
            &router,
            (&state.lo().host, &state.lo().origin),
            None,
            "/api/vault",
            RQ::Key,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (handshake, first) = Handshake::start(&public).unwrap();
        let (_, reply) = post::<_, RR>(&router, &state, "/api/vault", RQ::Open { first }).await;
        let RR::Opened { session, response } = reply else {
            panic!("{reply:?}");
        };
        let mut channel = handshake.finish(&response).unwrap();
        let password = uuid::Uuid::new_v4().to_string();
        let bytes = channel
            .seal(&Q::Unlock {
                password: Secret::new(password.clone()),
                totp: None,
                create: true,
            })
            .unwrap();
        assert!(
            !bytes
                .windows(password.len())
                .any(|w| w == password.as_bytes())
        );
        // A second, independently authorized browser cannot spend this session.
        let mut other = bridge(8206);
        other.root = state.root.clone();
        other.vault = state.vault.clone();
        let other_router = app(other.clone(), root.join("static"));
        let (_, stolen) = post::<_, RR>(
            &other_router,
            &other,
            "/api/vault",
            RQ::Exchange {
                session: session.clone(),
                bytes: bytes.clone(),
            },
        )
        .await;
        assert!(matches!(stolen, RR::Error(_)));
        let (_, reply) = post::<_, RR>(
            &router,
            &state,
            "/api/vault",
            RQ::Exchange {
                session: session.clone(),
                bytes,
            },
        )
        .await;
        let RR::Message(ciphertext) = reply else {
            panic!("{reply:?}");
        };
        assert!(matches!(channel.open::<R>(&ciphertext).unwrap(), R::Ok));
        // The relay must return an encrypted seed, never an ordinary broker reply.
        let bytes = channel
            .seal(&Q::BeginTotp {
                account: uuid::Uuid::new_v4().to_string(),
            })
            .unwrap();
        let (_, reply) = post::<_, RR>(
            &router,
            &state,
            "/api/vault",
            RQ::Exchange {
                session: session.clone(),
                bytes,
            },
        )
        .await;
        let RR::Message(ciphertext) = reply else {
            panic!();
        };
        let R::TotpEnrollment { manual_key, .. } = channel.open::<R>(&ciphertext).unwrap() else {
            panic!();
        };
        assert!(
            !ciphertext
                .windows(manual_key.expose().len())
                .any(|w| w == manual_key.expose().as_bytes())
        );
        let (_, reply) = post::<_, RR>(
            &router,
            &state,
            "/api/vault",
            RQ::Close {
                session: session.clone(),
            },
        )
        .await;
        assert!(matches!(reply, RR::Closed));
        let bytes = channel.seal(&Q::Status).unwrap();
        let (_, reply) = post::<_, RR>(
            &router,
            &state,
            "/api/vault",
            RQ::Exchange { session, bytes },
        )
        .await;
        assert!(matches!(reply, RR::Error(_)));
        user.call(B::Shutdown).await.unwrap();
        broker.await.unwrap();
    }
}
