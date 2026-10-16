//! JSON exists here only because external provider protocols require it.
use a206_core::*;
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use url::Url;
const MAX_MODELS: usize = 4096;
const MAX_PAGES: usize = 64;
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
pub fn endpoint(config: &ProviderConfig, relative: &str) -> Result<Url> {
    let url = Url::parse(&format!("{}/", config.base_url.trim_end_matches('/')))?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "endpoint must not contain credentials, query or fragment"
    );
    let local = url
        .host_str()
        .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]" | "::1"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && local),
        "use HTTPS or an SSH-forwarded loopback endpoint"
    );
    let result = url.join(relative)?;
    ensure!(
        result.origin() == url.origin() && result.path().starts_with(url.path()),
        "API path escaped the configured endpoint"
    );
    Ok(result)
}
pub fn validate_presets(library: &ProviderPresetList) -> Result<()> {
    ensure!(
        library.entries.len() <= 256 && library.disabled.len() <= 256,
        "provider preset library is too large"
    );
    let valid_id = |id: &str| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    ensure!(
        library.disabled.iter().all(|id| valid_id(id)),
        "invalid hidden provider preset ID"
    );
    let mut ids = BTreeSet::new();
    for preset in &library.entries {
        ensure!(
            valid_id(&preset.id) && ids.insert(&preset.id),
            "invalid or duplicate provider preset ID"
        );
        for label in [&preset.name, &preset.group] {
            ensure!(
                !label.trim().is_empty()
                    && label.len() <= 128
                    && !label.chars().any(char::is_control),
                "provider preset names/groups must be 1–128 bytes without control characters"
            );
        }
        ensure!(
            preset.setup.len() <= 4096 && preset.base_url.len() <= 4096,
            "provider preset instructions or endpoint are too long"
        );
        if !preset.base_url.is_empty() {
            endpoint(&preset.configure(preset.base_url.clone()), "models")?;
        }
    }
    Ok(())
}
async fn response_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    // Do not surface upstream error bodies: they can echo credentials or prompts.
    ensure!(status.is_success(), "provider returned HTTP {status}");
    let mut stream = response.bytes_stream();
    let mut bytes = vec![];
    while let Some(part) = stream.next().await {
        let part = part?;
        ensure!(
            bytes.len() + part.len() <= MAX_RESPONSE,
            "provider response is too large"
        );
        bytes.extend_from_slice(&part);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
/// Where credentialed requests go: the profile whose vault broker holds the
/// key. The gateway never reads a credential value; the broker attaches it
/// for the exact destination the user bound it to.
#[derive(Clone, Copy)]
pub struct Broker<'a>(pub &'a std::path::Path);
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
        return response_json(request.send().await?).await;
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
    use a206_broker::{AuthScheme, HttpMethod, ProviderCall, Reply, Request, Role};
    let method = match method {
        reqwest::Method::GET => HttpMethod::Get,
        reqwest::Method::POST => HttpMethod::Post,
        other => bail!("unsupported provider method {other}"),
    };
    let auth = match config.api {
        ProviderApi::OpenAi => AuthScheme::Bearer,
        ProviderApi::Anthropic => AuthScheme::ApiKeyHeader,
        ProviderApi::Gemini => AuthScheme::GoogleApiKey,
    };
    let mut client = a206_broker::Client::connect(broker.0, Role::Gateway)
        .await
        .context("unlock the vault to use this provider's credential")?;
    let reply = client
        .call(Request::Provider(ProviderCall {
            credential: credential.to_owned(),
            method,
            url: url.to_string(),
            auth,
            anthropic_version: (config.api == ProviderApi::Anthropic)
                .then(|| ANTHROPIC_VERSION.to_owned()),
            body: body.map(serde_json::to_vec).transpose()?,
        }))
        .await?;
    let Reply::Provider { status, body } = reply else {
        bail!("vault broker returned an unexpected reply");
    };
    ensure!(
        (200..300).contains(&status),
        "provider returned HTTP {status}"
    );
    ensure!(body.len() <= MAX_RESPONSE, "provider response is too large");
    Ok(serde_json::from_slice(&body)?)
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
    bail!("the vault broker is not available on this platform yet")
}
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Follow provider cursors, never provider-supplied URLs; publication is all-or-nothing.
pub async fn catalog(config: &ProviderConfig, broker: Broker<'_>) -> Result<Catalog> {
    tokio::time::timeout(Duration::from_secs(180), catalog_pages(config, broker))
        .await
        .context("provider catalog refresh timed out")?
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
        bytes += serde_json::to_vec(&value)?.len();
        ensure!(bytes <= MAX_RESPONSE, "model catalog is too large");
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
            .context("models API omitted its model array")?;
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
            ensure!(
                seen_models.insert(model.key.clone()),
                "models API repeated a model identifier"
            );
            models.push(model);
            ensure!(models.len() <= MAX_MODELS, "model catalog is too large");
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
                    .context("models API omitted its pagination cursor")?,
            ),
            _ => None,
        };
        let Some(cursor) = cursor else {
            let at = now();
            let account = account.context("models API returned no page")?;
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
        ensure!(
            cursor.len() <= 4096 && seen_cursors.insert(cursor.to_owned()),
            "models API returned an invalid or repeated cursor"
        );
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
    bail!("models API exceeded the pagination limit")
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
        .context("models API returned an item without an identifier")?;
    ensure!(
        !model.is_empty() && model.len() <= 512 && !model.chars().any(char::is_control),
        "invalid model identifier"
    );
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
    let model = thread
        .model
        .as_ref()
        .context("select a model returned by the provider API")?;
    ensure!(
        model == &info.key,
        "model metadata belongs to another model"
    );
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
            ensure!(
                thread.effort.is_none(),
                "Gemini effort mapping is unavailable; clear the effort selection"
            );
            let resource = model
                .model
                .strip_prefix("models/")
                .context("Gemini API returned an invalid model resource")?;
            ensure!(
                !resource.is_empty()
                    && resource != "."
                    && resource != ".."
                    && !resource.contains('/'),
                "invalid Gemini model resource"
            );
            let mut url = endpoint(config, "models/")?;
            url.path_segments_mut()
                .map_err(|_| anyhow::anyhow!("invalid Gemini endpoint"))?
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
    let text = text.context("provider returned no text response")?;
    ensure!(
        text.len() <= 128 * 1024,
        "provider message exceeds the conversation limit"
    );
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
