//! JSON exists here only because external provider protocols require it.
use a206_core::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use url::Url;
const MAX_MODELS: usize = 4096;
const MAX_PAGES: usize = 64;
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

pub use a206_core::providers::EndpointError;
/// A provider preset library that cannot be saved.
#[derive(Debug, thiserror::Error)]
pub enum PresetError {
    #[error("provider preset library is too large")]
    TooMany,
    #[error("invalid hidden provider preset ID")]
    HiddenId,
    #[error("invalid or duplicate provider preset ID")]
    Id,
    #[error("provider preset names/groups must be 1–128 bytes without control characters")]
    Label,
    #[error("provider preset instructions or endpoint are too long")]
    TooLong,
    #[error("provider preset endpoint is invalid")]
    Endpoint(#[source] EndpointError),
}
/// A failed provider call. Upstream bodies are never included: they can echo
/// credentials or prompts.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error("could not build the HTTP client")]
    Client(#[source] reqwest::Error),
    #[error("provider request failed")]
    Transport(#[source] reqwest::Error),
    #[error("provider returned HTTP {status}")]
    Status { status: u16 },
    #[error("provider response is too large")]
    ResponseTooLarge,
    #[error("provider response is not valid JSON")]
    Json(#[source] serde_json::Error),
    #[error("unsupported provider method {0}")]
    Method(reqwest::Method),
    #[error("unlock the vault to use this provider's credential")]
    VaultUnavailable(#[source] a206_broker::ClientError),
    #[error("the vault broker refused the provider call")]
    Broker(#[source] a206_broker::ClientError),
    #[error("the gateway is not paired with the unlocked vault; run `agent206 vault pair`")]
    NotPaired,
    #[error("vault broker returned an unexpected reply")]
    UnexpectedReply,
    #[error(
        "the provider request exceeds the vault broker's {limit} byte limit; shorten the conversation"
    )]
    RequestTooLarge { limit: usize },
    #[error("the vault broker is not available on this platform yet")]
    NoBroker,
    #[error("provider catalog refresh timed out")]
    Timeout(#[source] tokio::time::error::Elapsed),
    #[error("{0}")]
    Malformed(&'static str),
    #[error("select a model returned by the provider API")]
    NoModel,
    #[error("model metadata belongs to another model")]
    ModelMismatch,
    #[error("Gemini effort mapping is unavailable; clear the effort selection")]
    GeminiEffort,
    #[error("invalid Gemini model resource")]
    GeminiResource,
}
impl crate::error::Classified for ProviderError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Endpoint(_)
            | Self::RequestTooLarge { .. }
            | Self::NoModel
            | Self::ModelMismatch
            | Self::GeminiEffort
            | Self::GeminiResource => ErrorKind::InvalidRequest,
            Self::Client(_) | Self::Method(_) => ErrorKind::Internal,
            Self::Transport(error) if error.is_timeout() => ErrorKind::Timeout,
            Self::Transport(error) if error.is_connect() => ErrorKind::Unavailable,
            Self::Timeout(_) => ErrorKind::Timeout,
            Self::VaultUnavailable(_) | Self::NoBroker | Self::NotPaired => ErrorKind::Unavailable,
            // The broker's own reply decides (denied, locked, upstream timeout).
            Self::Broker(_) => return None,
            Self::Transport(_)
            | Self::Status { .. }
            | Self::ResponseTooLarge
            | Self::Json(_)
            | Self::UnexpectedReply
            | Self::Malformed(_) => ErrorKind::Provider,
        })
    }
}
impl crate::error::Classified for EndpointError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(ErrorKind::InvalidRequest)
    }
}
impl crate::error::Classified for PresetError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(ErrorKind::InvalidRequest)
    }
}
type Result<T, E = ProviderError> = std::result::Result<T, E>;
use crate::error::check;
/// The transport error without its URL, which can carry signed tokens.
fn transport(error: reqwest::Error) -> ProviderError {
    ProviderError::Transport(error.without_url())
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(ProviderError::Client)
}
pub fn endpoint(config: &ProviderConfig, relative: &str) -> Result<Url, EndpointError> {
    let url = a206_core::providers::base_url(&config.base_url)?;
    let result = url.join(relative)?;
    check(
        result.origin() == url.origin() && result.path().starts_with(url.path()),
        EndpointError::Escaped,
    )?;
    Ok(result)
}
pub fn validate_presets(library: &ProviderPresetList) -> Result<(), PresetError> {
    check(
        library.entries.len() <= 256 && library.disabled.len() <= 256,
        PresetError::TooMany,
    )?;
    let valid_id = |id: &str| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    check(
        library.disabled.iter().all(|id| valid_id(id)),
        PresetError::HiddenId,
    )?;
    let mut ids = BTreeSet::new();
    for preset in &library.entries {
        check(
            valid_id(&preset.id) && ids.insert(&preset.id),
            PresetError::Id,
        )?;
        for label in [&preset.name, &preset.group] {
            check(
                !label.trim().is_empty()
                    && label.len() <= 128
                    && !label.chars().any(char::is_control),
                PresetError::Label,
            )?;
        }
        check(
            preset.setup.len() <= 4096 && preset.base_url.len() <= 4096,
            PresetError::TooLong,
        )?;
        if !preset.base_url.is_empty() {
            endpoint(&preset.configure(preset.base_url.clone()), "models")
                .map_err(PresetError::Endpoint)?;
        }
    }
    Ok(())
}
async fn response_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    // Do not surface upstream error bodies: they can echo credentials or prompts.
    check(
        status.is_success(),
        ProviderError::Status {
            status: status.as_u16(),
        },
    )?;
    let mut stream = response.bytes_stream();
    let mut bytes = vec![];
    while let Some(part) = stream.next().await {
        let part = part.map_err(transport)?;
        check(
            bytes.len() + part.len() <= MAX_RESPONSE,
            ProviderError::ResponseTooLarge,
        )?;
        bytes.extend_from_slice(&part);
    }
    serde_json::from_slice(&bytes).map_err(ProviderError::Json)
}
/// Where credentialed requests go: the profile whose vault broker holds the
/// key, and the pairing a trusted client gave the gateway for this unlock.
/// The gateway never reads a credential value; the broker attaches it for
/// the exact destination the user bound it to.
#[derive(Clone, Copy)]
pub struct Broker<'a> {
    pub root: &'a std::path::Path,
    pub pairing: Option<&'a a206_core::BrokerPairing>,
}
async fn call(
    config: &ProviderConfig,
    broker: Broker<'_>,
    method: reqwest::Method,
    url: Url,
    body: Option<&Value>,
) -> Result<Value> {
    let Some(credential) = &config.credential else {
        let mut request = client()?.request(method, url);
        if config.api == ProviderApi::Anthropic {
            request = request.header("anthropic-version", ANTHROPIC_VERSION);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        return response_json(request.send().await.map_err(transport)?).await;
    };
    brokered(config, broker, credential, method, url, body).await
}
#[cfg(unix)]
async fn brokered(
    config: &ProviderConfig,
    broker: Broker<'_>,
    credential: &str,
    method: reqwest::Method,
    url: Url,
    body: Option<&Value>,
) -> Result<Value> {
    use a206_broker::{AuthScheme, HttpMethod, ProviderCall, Reply, Request};
    let method = match method {
        reqwest::Method::GET => HttpMethod::Get,
        reqwest::Method::POST => HttpMethod::Post,
        other => return Err(ProviderError::Method(other)),
    };
    let auth = match config.api {
        ProviderApi::OpenAi => AuthScheme::Bearer,
        ProviderApi::Anthropic => AuthScheme::ApiKeyHeader,
        ProviderApi::Gemini => AuthScheme::GoogleApiKey,
    };
    let mut client = paired_client(broker.root, broker.pairing).await?;
    let reply = client
        .call(Request::Provider(ProviderCall {
            credential: credential.to_owned(),
            method,
            url: url.to_string(),
            auth,
            anthropic_version: (config.api == ProviderApi::Anthropic)
                .then(|| ANTHROPIC_VERSION.to_owned()),
            body: body.map(bounded_json).transpose()?,
        }))
        .await
        .map_err(ProviderError::Broker)?;
    let Reply::Provider { status, body } = reply else {
        return Err(ProviderError::UnexpectedReply);
    };
    check(
        (200..300).contains(&status),
        ProviderError::Status { status },
    )?;
    check(body.len() <= MAX_RESPONSE, ProviderError::ResponseTooLarge)?;
    serde_json::from_slice(&body).map_err(ProviderError::Json)
}
/// Open a gateway-role broker connection and prove this unlock's pairing.
#[cfg(unix)]
async fn paired_client(
    root: &std::path::Path,
    pairing: Option<&a206_core::BrokerPairing>,
) -> Result<a206_broker::Client, ProviderError> {
    use a206_broker::{Request, Role, WireSecret};
    let mut client = a206_broker::Client::connect(root, Role::Gateway)
        .await
        .map_err(ProviderError::VaultUnavailable)?;
    let pairing = pairing.ok_or(ProviderError::NotPaired)?;
    client
        .call(Request::GatewayAuth {
            token: WireSecret::new(pairing.expose().to_owned()),
        })
        .await
        .map_err(ProviderError::Broker)?;
    Ok(client)
}
/// The connection that first proved a pairing. The broker voids the pairing
/// when it closes, so the gateway holds it for as long as it keeps the pairing.
pub struct PairingAnchor {
    #[cfg(unix)]
    _connection: a206_broker::Client,
}
/// Check a pairing against the broker before the gateway keeps it.
#[cfg(unix)]
pub async fn verify_pairing(
    root: &std::path::Path,
    pairing: &a206_core::BrokerPairing,
) -> Result<PairingAnchor, ProviderError> {
    paired_client(root, Some(pairing))
        .await
        .map(|connection| PairingAnchor {
            _connection: connection,
        })
}
#[cfg(not(unix))]
pub async fn verify_pairing(
    _: &std::path::Path,
    _: &a206_core::BrokerPairing,
) -> Result<PairingAnchor, ProviderError> {
    Err(ProviderError::NoBroker)
}
/// Serialize a request body, stopping as soon as it exceeds what one broker
/// frame can carry instead of building an oversized copy first.
#[cfg(unix)]
fn bounded_json(body: &Value) -> Result<Vec<u8>> {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len() + bytes.len() > a206_broker::MAX_PROVIDER_BODY {
                return Err(std::io::Error::other("provider request too large"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Bounded(vec![]);
    // Serializing a `Value` fails only when the bounded writer refuses more bytes.
    if serde_json::to_writer(&mut out, body).is_err() {
        return Err(ProviderError::RequestTooLarge {
            limit: a206_broker::MAX_PROVIDER_BODY,
        });
    }
    Ok(out.0)
}
#[cfg(not(unix))]
async fn brokered(
    _: &ProviderConfig,
    _: Broker<'_>,
    _: &str,
    _: reqwest::Method,
    _: Url,
    _: Option<&Value>,
) -> Result<Value> {
    Err(ProviderError::NoBroker)
}
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Follow provider cursors, never provider-supplied URLs; publication is all-or-nothing.
pub async fn catalog(config: &ProviderConfig, broker: Broker<'_>) -> Result<Catalog> {
    tokio::time::timeout(Duration::from_secs(180), catalog_pages(config, broker))
        .await
        .map_err(ProviderError::Timeout)?
}
async fn catalog_pages(config: &ProviderConfig, broker: Broker<'_>) -> Result<Catalog> {
    let source = endpoint(config, "models")?;
    let mut url = source.clone();
    let mut seen_cursors = BTreeSet::new();
    let mut seen_models = BTreeSet::new();
    let mut models = vec![];
    let mut bytes = 0;
    let mut account = None;
    for page in 0..MAX_PAGES {
        let value = call(config, broker, reqwest::Method::GET, url, None).await?;
        bytes += serde_json::to_vec(&value)
            .map_err(ProviderError::Json)?
            .len();
        check(
            bytes <= MAX_RESPONSE,
            ProviderError::Malformed("model catalog is too large"),
        )?;
        if page == 0 {
            account = Some(value.clone());
        }
        let array = if config.api == ProviderApi::Gemini {
            "models"
        } else {
            "data"
        };
        let items = value
            .get(array)
            .and_then(Value::as_array)
            .ok_or(ProviderError::Malformed(
                "models API omitted its model array",
            ))?;
        for item in items {
            if config.api == ProviderApi::Gemini
                && !item
                    .get("supportedGenerationMethods")
                    .and_then(Value::as_array)
                    .is_some_and(|methods| methods.iter().any(|method| method == "generateContent"))
            {
                continue;
            }
            let model = parse_model(config, item)?;
            check(
                seen_models.insert(model.key.clone()),
                ProviderError::Malformed("models API repeated a model identifier"),
            )?;
            models.push(model);
            check(
                models.len() <= MAX_MODELS,
                ProviderError::Malformed("model catalog is too large"),
            )?;
        }
        let cursor = match config.api {
            ProviderApi::Gemini => value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty()),
            _ if value.get("has_more").and_then(Value::as_bool) == Some(true) => Some(
                value
                    .get("last_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or(ProviderError::Malformed(
                        "models API omitted its pagination cursor",
                    ))?,
            ),
            _ => None,
        };
        let Some(cursor) = cursor else {
            let at = now();
            let account = account.ok_or(ProviderError::Malformed("models API returned no page"))?;
            return Ok(Catalog {
                provider: config.id,
                source: source.to_string(),
                fetched_at: at,
                expires_at: at + 900,
                models,
                balance: price(account.get("balance")),
                balance_currency: account
                    .get("balance_currency")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                token_balance: account.get("token_balance").and_then(Value::as_u64),
            });
        };
        check(
            cursor.len() <= 4096 && seen_cursors.insert(cursor.to_owned()),
            ProviderError::Malformed("models API returned an invalid or repeated cursor"),
        )?;
        url = source.clone();
        url.query_pairs_mut().append_pair(
            if config.api == ProviderApi::Gemini {
                "pageToken"
            } else {
                "after_id"
            },
            cursor,
        );
    }
    Err(ProviderError::Malformed(
        "models API exceeded the pagination limit",
    ))
}
fn parse_model(config: &ProviderConfig, item: &Value) -> Result<ModelInfo> {
    let id_field = if config.api == ProviderApi::Gemini {
        "name"
    } else {
        "id"
    };
    let model = item
        .get(id_field)
        .and_then(Value::as_str)
        .ok_or(ProviderError::Malformed(
            "models API returned an item without an identifier",
        ))?;
    check(
        !model.is_empty() && model.len() <= 512 && !model.chars().any(char::is_control),
        ProviderError::Malformed("invalid model identifier"),
    )?;
    let (name, context, output, reasoning) = match config.api {
        ProviderApi::OpenAi => (
            "name",
            "context_length",
            "max_output_tokens",
            item.get("supports_reasoning").and_then(Value::as_bool),
        ),
        ProviderApi::Anthropic => (
            "display_name",
            "max_input_tokens",
            "max_tokens",
            item.pointer("/capabilities/thinking/supported")
                .and_then(Value::as_bool),
        ),
        ProviderApi::Gemini => (
            "displayName",
            "inputTokenLimit",
            "outputTokenLimit",
            item.get("thinking").and_then(Value::as_bool),
        ),
    };
    let mut efforts: Vec<String> = item
        .get("supported_reasoning_efforts")
        .or_else(|| item.pointer("/effort/supported_levels"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|v| valid_effort(v))
        .take(32)
        .map(ToOwned::to_owned)
        .collect();
    let mut seen_efforts = BTreeSet::new();
    efforts.retain(|effort| seen_efforts.insert(effort.clone()));
    if config.api == ProviderApi::Gemini {
        efforts.clear();
    }
    if config.api == ProviderApi::Anthropic
        && let Some(advertised) = item
            .pointer("/capabilities/effort")
            .and_then(Value::as_object)
    {
        efforts.extend(
            advertised
                .iter()
                .filter(|(name, spec)| {
                    valid_effort(name)
                        && spec.get("supported").and_then(Value::as_bool) == Some(true)
                })
                .take(32)
                .map(|(name, _)| name.clone())
                .filter(|name| seen_efforts.insert(name.clone())),
        );
    }
    Ok(ModelInfo {
        key: ModelKey {
            provider: config.id,
            model: model.into(),
        },
        name: item
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or(model)
            .into(),
        context_limit: item
            .get(context)
            .or_else(|| {
                if config.api == ProviderApi::OpenAi {
                    item.get("context_window")
                } else {
                    None
                }
            })
            .and_then(Value::as_u64)
            .filter(|n| *n > 0),
        output_limit: item.get(output).and_then(Value::as_u64).filter(|n| *n > 0),
        input_cost_per_token: price(item.pointer("/pricing/prompt")),
        output_cost_per_token: price(item.pointer("/pricing/completion")),
        reasoning,
        reasoning_efforts: efforts,
    })
}
fn valid_effort(value: &str) -> bool {
    !value.is_empty() && value.len() <= 64 && !value.chars().any(char::is_control)
}
fn price(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|v| v.is_finite() && *v >= 0.0)
}
pub async fn complete(
    config: &ProviderConfig,
    broker: Broker<'_>,
    thread: &Thread,
    prompt: &str,
    info: &ModelInfo,
) -> Result<(String, Usage)> {
    let model = thread.model.as_ref().ok_or(ProviderError::NoModel)?;
    check(model == &info.key, ProviderError::ModelMismatch)?;
    let (url, body) = match config.api {
        ProviderApi::OpenAi => {
            let mut messages = vec![json!({"role": "system", "content": prompt})];
            messages.extend(thread.messages.iter().map(|m| json!({"role": match m.role { Role::User => "user", Role::Assistant => "assistant", Role::System => "system", Role::Tool => "tool" }, "content": message_content(m)})));
            let mut body = json!({"model": model.model, "messages": messages, "stream": false});
            if let Some(effort) = &thread.effort {
                body["reasoning_effort"] = json!(effort);
            }
            (endpoint(config, "chat/completions")?, body)
        }
        ProviderApi::Anthropic => {
            // This is a request budget, not a claim about a model's context or output capacity.
            let budget = info.output_limit.unwrap_or(8192).min(8192);
            let mut body = json!({"model": model.model, "system": prompt, "max_tokens": budget,
                "messages": thread.messages.iter().filter(|m| matches!(m.role, Role::User | Role::Assistant)).map(|m| json!({"role": if m.role == Role::Assistant { "assistant" } else { "user" }, "content": message_content(m)})).collect::<Vec<_>>() });
            if let Some(effort) = &thread.effort {
                body["output_config"] = json!({"effort": effort});
            }
            (endpoint(config, "messages")?, body)
        }
        ProviderApi::Gemini => {
            check(thread.effort.is_none(), ProviderError::GeminiEffort)?;
            let resource = model
                .model
                .strip_prefix("models/")
                .ok_or(ProviderError::GeminiResource)?;
            check(
                !resource.is_empty()
                    && resource != "."
                    && resource != ".."
                    && !resource.contains('/'),
                ProviderError::GeminiResource,
            )?;
            let mut url = endpoint(config, "models/")?;
            // An http(s) endpoint always has path segments.
            url.path_segments_mut()
                .map_err(|()| ProviderError::Endpoint(EndpointError::Shape))?
                .pop_if_empty()
                .push(&format!("{resource}:generateContent"));
            let body = json!({"systemInstruction": {"parts": [{"text": prompt}]},
                "contents": thread.messages.iter().filter(|m| matches!(m.role, Role::User | Role::Assistant)).map(|m| json!({"role": if m.role == Role::Assistant { "model" } else { "user" }, "parts": [{"text": message_content(m)}]})).collect::<Vec<_>>() });
            (url, body)
        }
    };
    let result = call(config, broker, reqwest::Method::POST, url, Some(&body)).await?;
    let (text, input, output) = match config.api {
        ProviderApi::OpenAi => (
            result
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            "/usage/prompt_tokens",
            "/usage/completion_tokens",
        ),
        ProviderApi::Anthropic => (
            text_parts(result.get("content"), "text", false),
            "/usage/input_tokens",
            "/usage/output_tokens",
        ),
        ProviderApi::Gemini => (
            text_parts(result.pointer("/candidates/0/content/parts"), "text", true),
            "/usageMetadata/promptTokenCount",
            "/usageMetadata/candidatesTokenCount",
        ),
    };
    let text = text.ok_or(ProviderError::Malformed(
        "provider returned no text response",
    ))?;
    check(
        text.len() <= 128 * 1024,
        ProviderError::Malformed("provider message exceeds the conversation limit"),
    )?;
    let usage = Usage {
        input_tokens: result.pointer(input).and_then(Value::as_u64),
        output_tokens: result.pointer(output).and_then(Value::as_u64),
        cost: price(result.pointer("/usage/cost")),
    };
    Ok((text, usage))
}
fn text_parts(parts: Option<&Value>, key: &str, skip_thought: bool) -> Option<String> {
    let parts = parts?.as_array()?;
    let texts: Vec<_> = parts
        .iter()
        .filter(|part| !skip_thought || part.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|part| part.get(key).and_then(Value::as_str))
        .collect();
    (!texts.is_empty()).then(|| texts.join(""))
}
pub(crate) fn message_content(message: &Message) -> String {
    let mut content = message.text.clone();
    for attachment in &message.attachments {
        content.push_str(&format!(
            "\n\nAttached text document ({:?}):\n{}",
            attachment.name, attachment.text
        ));
    }
    content
}
