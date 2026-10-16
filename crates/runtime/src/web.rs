//! Loopback web bridge. Origin-scoped bearer sessions avoid leaking cookies to preview servers.
use crate::client::Client;
use a206_core::{Command, Reply};
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
#[derive(Clone)]
struct Web {
    client: Client,
    token: Arc<String>,
    bootstrap: Arc<String>,
    claimed: Arc<AtomicBool>,
    origin: Arc<String>,
    host: Arc<String>,
}
pub async fn serve(root: PathBuf, static_dir: PathBuf, port: u16) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let port = listener.local_addr()?.port();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let state = Web {
        client: Client::start(root, None),
        token: Arc::new(uuid::Uuid::new_v4().simple().to_string()),
        bootstrap: Arc::new(token.clone()),
        claimed: Arc::new(AtomicBool::new(false)),
        origin: Arc::new(format!("http://127.0.0.1:{port}")),
        host: Arc::new(format!("127.0.0.1:{port}")),
    };
    let app = Router::new()
        .route("/connect/{token}", get(connect))
        .route("/api/command", post(command))
        .layer(DefaultBodyLimit::max(a206_protocol::MAX_FRAME + 10))
        .fallback_service(tower_http::services::ServeDir::new(static_dir))
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
        .with_state(state);
    println!("Open http://127.0.0.1:{port}/connect/{token} to authorize this browser session.");
    axum::serve(listener, app).await?;
    Ok(())
}
async fn connect(
    State(state): State<Web>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    if token != *state.bootstrap
        || headers.get(header::HOST).and_then(|h| h.to_str().ok()) != Some(state.host.as_str())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if state
        .claimed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
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
    // Web sessions cannot terminate the shared service.
    if matches!(request.body, Command::Shutdown) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let body = state
        .client
        .call_with_id(request.id, request.body)
        .await
        .unwrap_or_else(|e| Reply::Error {
            message: e.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auth_requires_an_explicit_origin_bound_bearer() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let token = uuid::Uuid::new_v4().simple().to_string();
        let state = Web {
            client: Client::start(root, None),
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
