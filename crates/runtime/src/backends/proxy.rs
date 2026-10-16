//! A backend can reach only this proxy. Its selected model is an inference ceiling.
use a206_core::*;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
/// A proxy failure, recorded where it happens; the backend sees only a status.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProxyError {
    #[error("backend credential proxy stopped")]
    Stopped(#[source] std::io::Error),
    #[error(transparent)]
    Provider(#[from] crate::provider::ProviderError),
    #[error("backend stopped")]
    Cancelled,
    #[error("provider returned an invalid HTTP status")]
    Status(#[source] axum::http::status::InvalidStatusCode),
}
impl crate::error::Classified for ProxyError {
    fn kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Stopped(_) => Some(ErrorKind::Unavailable),
            Self::Provider(error) => crate::error::classify(error),
            Self::Cancelled => Some(ErrorKind::Cancelled),
            Self::Status(_) => Some(ErrorKind::Provider),
        }
    }
}
#[derive(Clone)]
struct Scope {
    config: ProviderConfig,
    model: ModelInfo,
    token: String,
    stop: tokio_util::sync::CancellationToken,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
pub struct Proxy {
    task: tokio::task::JoinHandle<()>,
    port: u16,
    token: String,
    stop: tokio_util::sync::CancellationToken,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}
impl Proxy {
    pub async fn start(config: ProviderConfig, model: ModelInfo) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let token = Id::new_v4().to_string();
        let stop = tokio_util::sync::CancellationToken::new();
        let scope = Scope {
            config,
            model,
            token: token.clone(),
            stop: stop.clone(),
            requests: Default::default(),
        };
        let router = Router::new()
            .route("/{*path}", any(forward))
            .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024))
            .with_state(scope);
        let task = tokio::spawn(async move {
            // A failed proxy surfaces to the backend as refused requests; record why.
            if let Err(error) = axum::serve(listener, router).await {
                crate::error::handle("backend_proxy", &ProxyError::Stopped(error));
            }
        });
        Ok(Self {
            task,
            port,
            token,
            stop,
        })
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn token(&self) -> &str {
        &self.token
    }
}
async fn forward(
    State(scope): State<Scope>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    if scope.stop.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {}", scope.token))
        || headers.get("x-api-key").and_then(|v| v.to_str().ok()) == Some(scope.token.as_str());
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if scope
        .requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        >= 128
    {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let path = path.strip_prefix("v1/").unwrap_or(&path);
    if method == Method::GET && path == "models" {
        // These values derive from the live provider catalog, not a backend's bundled catalog.
        return axum::Json(json!({"data": [{"id": scope.model.key.model, "display_name": scope.model.name, "name": scope.model.name,
            "max_input_tokens": scope.model.context_limit, "max_tokens": scope.model.output_limit}], "has_more": false})).into_response();
    }
    let allowed = match scope.config.api {
        ProviderApi::OpenAi => matches!(path, "responses" | "chat/completions"),
        ProviderApi::Anthropic => matches!(path, "messages" | "messages/count_tokens"),
        ProviderApi::Gemini => false,
    };
    if method != Method::POST || !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(mut body) = serde_json::from_slice::<Value>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if body.get("model").and_then(Value::as_str) != Some(scope.model.key.model.as_str()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // The proxy is not a credential broker. Never bypass the existing credential boundary.
    if scope.config.credential.is_some() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if path != "messages/count_tokens" {
        let fields: &[&str] = match path {
            "chat/completions" => &["max_tokens", "max_completion_tokens"],
            "responses" => &["max_output_tokens"],
            "messages" => &["max_tokens"],
            _ => return StatusCode::FORBIDDEN.into_response(),
        };
        let mut budget = scope.model.output_limit.unwrap_or(8192).min(8192);
        for field in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
            if let Some(value) = body.get(field) {
                let Some(value) = value.as_u64().filter(|value| *value > 0) else {
                    return StatusCode::BAD_REQUEST.into_response();
                };
                if !fields.contains(&field) {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                budget = budget.min(value);
            }
        }
        if budget == 0 {
            return StatusCode::BAD_REQUEST.into_response();
        }
        // Use one protocol-valid field, honoring the stricter of any supplied aliases.
        let field = fields
            .iter()
            .rev()
            .find(|field| body.get(**field).is_some())
            .unwrap_or(&fields[0]);
        let field = *field;
        for alias in fields {
            body.as_object_mut()
                .expect("validated model requires an object")
                .remove(*alias);
        }
        body[field] = json!(budget);
    }
    let result = async {
        let url = crate::provider::endpoint(&scope.config, path)
            .map_err(crate::provider::ProviderError::from)?;
        let mut request = crate::provider::client()?.post(url).json(&body);
        if scope.config.api == ProviderApi::Anthropic {
            request = request.header("anthropic-version", "2023-06-01");
            if let Some(beta) = headers.get("anthropic-beta") {
                request = request.header("anthropic-beta", beta);
            }
        }
        let upstream = tokio::select! {
            result = request.send() => result.map_err(|error| crate::provider::ProviderError::Transport(error.without_url()))?,
            _ = scope.stop.cancelled() => return Err(ProxyError::Cancelled),
        };
        let status = upstream.status();
        if !status.is_success() {
            return Ok::<_, ProxyError>(
                (
                    StatusCode::from_u16(status.as_u16()).map_err(ProxyError::Status)?,
                    "Provider rejected the backend request",
                )
                    .into_response(),
            );
        }
        let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
        let mut received = 0usize;
        let stream = upstream.bytes_stream().take_until(scope.stop.clone().cancelled_owned()).map(move |chunk| {
            let chunk = chunk.map_err(std::io::Error::other)?;
            received = received.saturating_add(chunk.len());
            if received > 16 * 1024 * 1024 {
                return Err(std::io::Error::other(
                    "backend API response exceeds its limit",
                ));
            }
            Ok(chunk)
        });
        let mut response = Body::from_stream(stream).into_response();
        if let Some(content_type) = content_type {
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
        }
        Ok(response)
    }
    .await;
    result.unwrap_or_else(|error| {
        crate::error::handle("backend_proxy", &error);
        (StatusCode::BAD_GATEWAY, "Provider connection failed").into_response()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    #[tokio::test]
    async fn proxy_enforces_model_auth_paths_and_revokes_on_drop() {
        let model = Id::new_v4().to_string();
        let output_limit = (Id::new_v4().as_u128() % 16_000 + 1) as u64;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let received = seen.clone();
        let completion = post(
            move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
                let received = received.clone();
                async move {
                    assert!(headers.get(header::AUTHORIZATION).is_none());
                    assert!(headers.get("x-api-key").is_none());
                    *received.lock().unwrap() = Some(body);
                    axum::Json(json!({"ok": true}))
                }
            },
        );
        let discovered = model.clone();
        let router = Router::new()
            .route(
                "/v1/models",
                get(move || {
                    let model = discovered.clone();
                    async move {
                        axum::Json(
                            json!({"data": [{"id": model, "max_output_tokens": output_limit}]}),
                        )
                    }
                }),
            )
            .route("/v1/responses", completion.clone())
            .route("/v1/chat/completions", completion);
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config = ProviderConfig {
            id: Id::new_v4(),
            label: Id::new_v4().to_string(),
            base_url: format!("http://{address}/v1"),
            credential: None,
            api: ProviderApi::OpenAi,
        };
        let info = crate::provider::catalog(
            &config,
            crate::provider::Broker {
                root: std::path::Path::new("/nonexistent"),
                pairing: None,
            },
        )
        .await
        .unwrap()
        .models
        .remove(0);
        let proxy = Proxy::start(config, info).await.unwrap();
        let base = format!("http://127.0.0.1:{}/v1", proxy.port());
        let client = reqwest::Client::new();
        assert_eq!(
            client
                .get(format!("{base}/models"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let catalog: Value = client
            .get(format!("{base}/models"))
            .bearer_auth(proxy.token())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(catalog["data"][0]["id"], model);
        for (path, body) in [
            ("responses", json!({"model": Id::new_v4().to_string()})),
            ("files", json!({"model": model})),
        ] {
            assert_eq!(
                client
                    .post(format!("{base}/{path}"))
                    .bearer_auth(proxy.token())
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        let response = client
            .post(format!("{base}/responses"))
            .bearer_auth(proxy.token())
            .json(&json!({"model": model, "input": Id::new_v4().to_string()}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert_eq!(seen.lock().unwrap().as_ref().unwrap()["model"], model);
        let ceiling = output_limit.min(8192);
        assert_eq!(
            seen.lock().unwrap().as_ref().unwrap()["max_output_tokens"],
            ceiling
        );
        for (request, field, expected) in [
            (json!({}), "max_tokens", ceiling),
            (json!({"max_tokens": u64::MAX}), "max_tokens", ceiling),
            (
                json!({"max_completion_tokens": u64::MAX}),
                "max_completion_tokens",
                ceiling,
            ),
            (
                json!({"max_tokens": 1, "max_completion_tokens": u64::MAX}),
                "max_completion_tokens",
                1,
            ),
            (
                json!({"max_tokens": u64::MAX, "max_completion_tokens": 1}),
                "max_completion_tokens",
                1,
            ),
        ] {
            let mut request = request;
            request["model"] = json!(model);
            assert!(
                client
                    .post(format!("{base}/chat/completions"))
                    .bearer_auth(proxy.token())
                    .json(&request)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .is_success()
            );
            let sent = seen.lock().unwrap().clone().unwrap();
            assert_eq!(sent[field], expected);
            assert!(sent.get("max_output_tokens").is_none());
            let other = if field == "max_tokens" {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            assert!(sent.get(other).is_none());
        }
        for mut request in [
            json!({"max_tokens": 0}),
            json!({"max_completion_tokens": -1}),
            json!({"max_tokens": "large"}),
            json!({"max_output_tokens": u64::MAX}),
        ] {
            request["model"] = json!(model);
            assert_eq!(
                client
                    .post(format!("{base}/chat/completions"))
                    .bearer_auth(proxy.token())
                    .json(&request)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        drop(proxy);
        tokio::task::yield_now().await;
        if let Ok(response) = client.get(format!("{base}/models")).send().await {
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
        task.abort();
    }
}
