//! Connector identities and setup defaults contain no model or account facts.
use crate::{Id, ProviderConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
/// A provider endpoint the gateway refuses to call.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("provider endpoint is not a valid URL")]
    Url(#[from] url::ParseError),
    #[error("endpoint must not contain credentials, query or fragment")]
    Shape,
    #[error("use HTTPS or an SSH-forwarded loopback endpoint")]
    Insecure,
    #[error("API path escaped the configured endpoint")]
    Escaped,
}
/// A provider account's endpoint as a directory URL, refused unless it is
/// HTTPS or loopback HTTP without credentials, query or fragment.
pub fn base_url(base_url: &str) -> Result<url::Url, EndpointError> {
    let url = url::Url::parse(&format!("{}/", base_url.trim_end_matches('/')))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(EndpointError::Shape);
    }
    let local = url
        .host_str()
        .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]" | "::1"));
    if url.scheme() == "https" || (url.scheme() == "http" && local) {
        Ok(url)
    } else {
        Err(EndpointError::Insecure)
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApi {
    #[default]
    OpenAi,
    Anthropic,
    Gemini,
}
impl ProviderApi {
    pub const ALL: [Self; 3] = [Self::OpenAi, Self::Anthropic, Self::Gemini];
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI compatible",
            Self::Anthropic => "Anthropic Messages",
            Self::Gemini => "Google Gemini",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPreset {
    pub id: String,
    pub name: String,
    #[serde(default = "custom_group")]
    pub group: String,
    #[serde(default)]
    pub api: ProviderApi,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub setup: String,
}
fn custom_group() -> String {
    "Custom".into()
}
fn default_true() -> bool {
    true
}
/// User entries override a bundled ID or add a new one. Hiding presets never changes accounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderPresetList {
    #[serde(default = "default_true")]
    pub include_builtins: bool,
    #[serde(default)]
    pub disabled: BTreeSet<String>,
    #[serde(default)]
    pub entries: Vec<ProviderPreset>,
}
impl Default for ProviderPresetList {
    fn default() -> Self {
        Self {
            include_builtins: true,
            disabled: BTreeSet::new(),
            entries: vec![],
        }
    }
}
impl ProviderPresetList {
    pub fn all(&self) -> Vec<ProviderPreset> {
        let mut presets = if self.include_builtins {
            ProviderPreset::builtins()
        } else {
            vec![]
        };
        for entry in &self.entries {
            if let Some(existing) = presets.iter_mut().find(|p| p.id == entry.id) {
                *existing = entry.clone();
            } else {
                presets.push(entry.clone());
            }
        }
        presets
    }
    pub fn effective(&self) -> Vec<ProviderPreset> {
        self.all()
            .into_iter()
            .filter(|p| !self.disabled.contains(&p.id))
            .collect()
    }
}
impl ProviderPreset {
    pub fn configure(&self, base_url: String) -> ProviderConfig {
        ProviderConfig {
            id: Id::new_v4(),
            label: self.name.clone(),
            api: self.api,
            base_url,
            credential: None,
        }
    }
    pub fn builtins() -> Vec<Self> {
        use ProviderApi::*;
        let presets = [
            (
                "alibaba",
                "Alibaba Cloud Model Studio",
                "Hosted",
                OpenAi,
                "",
                "Enter the OpenAI-compatible regional/workspace URL from the Model Studio console. Endpoints and keys are region-specific.",
            ),
            (
                "anthropic",
                "Anthropic",
                "Hosted",
                Anthropic,
                "https://api.anthropic.com/v1",
                "Use an Anthropic API key through the vault broker.",
            ),
            (
                "qianfan",
                "Baidu Qianfan",
                "Hosted",
                OpenAi,
                "https://qianfan.baidubce.com/v2",
                "Use a Qianfan API key; API model discovery must be available for selection.",
            ),
            (
                "deepseek",
                "DeepSeek",
                "Hosted",
                OpenAi,
                "https://api.deepseek.com",
                "Use a DeepSeek API key through the vault broker.",
            ),
            (
                "fireworks",
                "Fireworks AI",
                "Hosted",
                OpenAi,
                "https://api.fireworks.ai/inference/v1",
                "Use a Fireworks API key through the vault broker.",
            ),
            (
                "gemini",
                "Google Gemini",
                "Hosted",
                Gemini,
                "https://generativelanguage.googleapis.com/v1beta",
                "Use a Gemini API key through the vault broker.",
            ),
            (
                "groq",
                "Groq",
                "Hosted",
                OpenAi,
                "https://api.groq.com/openai/v1",
                "Use a Groq API key through the vault broker.",
            ),
            (
                "minimax",
                "MiniMax",
                "Hosted",
                OpenAi,
                "https://api.minimax.io/v1",
                "Use a MiniMax API key for this endpoint; review your account region.",
            ),
            (
                "mistral",
                "Mistral AI",
                "Hosted",
                OpenAi,
                "https://api.mistral.ai/v1",
                "Use a Mistral API key through the vault broker.",
            ),
            (
                "moonshot",
                "Moonshot / Kimi",
                "Hosted",
                OpenAi,
                "https://api.moonshot.ai/v1",
                "Use a Moonshot API key for this endpoint; regional accounts may require a different URL.",
            ),
            (
                "openai",
                "OpenAI",
                "Hosted",
                OpenAi,
                "https://api.openai.com/v1",
                "Use an OpenAI API key through the vault broker.",
            ),
            (
                "openrouter",
                "OpenRouter",
                "Hosted",
                OpenAi,
                "https://openrouter.ai/api/v1",
                "Public model discovery is available without an API key; inference needs the vault broker.",
            ),
            (
                "perplexity",
                "Perplexity",
                "Hosted",
                OpenAi,
                "https://api.perplexity.ai/router/v1",
                "Use the OpenAI-compatible router API and its API key through the vault broker.",
            ),
            (
                "siliconflow",
                "SiliconFlow",
                "Hosted",
                OpenAi,
                "https://api.siliconflow.com/v1",
                "Use a SiliconFlow API key for this endpoint; review your account region.",
            ),
            (
                "stepfun",
                "StepFun",
                "Hosted",
                OpenAi,
                "https://api.stepfun.ai/v1",
                "Use a StepFun API key through the vault broker.",
            ),
            (
                "hunyuan",
                "Tencent Hunyuan",
                "Hosted",
                OpenAi,
                "https://api.hunyuan.cloud.tencent.com/v1",
                "Use a Hunyuan API key; API model discovery must be available for selection.",
            ),
            (
                "together",
                "Together AI",
                "Hosted",
                OpenAi,
                "https://api.together.ai/v1",
                "Use a Together API key through the vault broker.",
            ),
            (
                "volcengine",
                "Volcengine / Doubao",
                "Hosted",
                OpenAi,
                "https://ark.cn-beijing.volces.com/api/v3",
                "Review your Ark region and endpoint. API model discovery must be available for selection.",
            ),
            (
                "x-ai",
                "xAI",
                "Hosted",
                OpenAi,
                "https://api.x.ai/v1",
                "Use an xAI API key through the vault broker.",
            ),
            (
                "z-ai",
                "Z.ai",
                "Hosted",
                OpenAi,
                "https://api.z.ai/api/paas/v4",
                "Use the standard API endpoint and its API key; coding-plan endpoints are separate.",
            ),
            (
                "joshua",
                "Joshua",
                "Local",
                OpenAi,
                "",
                "Start Joshua in its app page, or enter its local or SSH-forwarded API URL.",
            ),
            (
                "lm-studio",
                "LM Studio",
                "Local",
                OpenAi,
                "http://127.0.0.1:1234/v1",
                "Enable the LM Studio local API server, or enter its SSH-forwarded URL.",
            ),
            (
                "ollama",
                "Ollama",
                "Local",
                OpenAi,
                "http://127.0.0.1:11434/v1",
                "Start your Ollama service, or replace this with its SSH-forwarded URL.",
            ),
            (
                "custom",
                "Custom endpoint",
                "Custom",
                OpenAi,
                "",
                "Choose the API protocol and enter an HTTPS or loopback HTTP base URL.",
            ),
        ];
        presets
            .into_iter()
            .map(|(id, name, group, api, base_url, setup)| Self {
                id: id.into(),
                name: name.into(),
                group: group.into(),
                api,
                base_url: base_url.into(),
                setup: setup.into(),
            })
            .collect()
    }
}
