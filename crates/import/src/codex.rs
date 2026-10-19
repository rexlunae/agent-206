//! Explicit Codex `config.toml` scope. Schema inspected in openai/codex at
//! 8c076942cb70fba8e5c4732c23f1faabc4aed484 (`codex-rs/config/src/config_toml.rs`,
//! `codex-rs/model-provider-info/src/lib.rs`, Apache-2.0). Only
//! `[model_providers.*]` endpoints are reviewed; nothing is run, no profile or
//! project layer is merged and no credential, header or command is read.
use crate::{
    ImportError, Inactive, InactiveCategory as Category, MAX_CONFIG_BYTES, PREVIEW_VERSION,
    Preview, ProviderAction, ProviderImport, Result, Tool, Unsupported, UnsupportedProvider,
};
use a206_core::{ProviderApi, Settings};
use serde::{Deserialize, de::IgnoredAny};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

/// Codex refuses configured providers under these built-in ids, except the
/// Bedrock ones, which only override AWS signing (`validate_model_providers`).
const RESERVED: [&str; 4] = ["openai", "ollama", "lmstudio", "ollama-chat"];
const BEDROCK: [&str; 2] = ["amazon-bedrock", "amazon-bedrock-runtime"];

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    model_providers: BTreeMap<String, Provider>,
    model: Option<IgnoredAny>,
    review_model: Option<IgnoredAny>,
    model_provider: Option<IgnoredAny>,
    oss_provider: Option<IgnoredAny>,
    profile: Option<IgnoredAny>,
    #[serde(default)]
    profiles: HashMap<String, IgnoredAny>,
    approval_policy: Option<IgnoredAny>,
    approvals_reviewer: Option<IgnoredAny>,
    auto_review: Option<IgnoredAny>,
    #[serde(default)]
    projects: HashMap<String, IgnoredAny>,
    sandbox_mode: Option<IgnoredAny>,
    sandbox_workspace_write: Option<IgnoredAny>,
    permissions: Option<IgnoredAny>,
    default_permissions: Option<IgnoredAny>,
    shell_environment_policy: Option<IgnoredAny>,
    notify: Option<IgnoredAny>,
    hooks: Option<IgnoredAny>,
    #[serde(default)]
    mcp_servers: HashMap<String, IgnoredAny>,
    plugins: Option<IgnoredAny>,
    #[serde(default)]
    marketplaces: HashMap<String, IgnoredAny>,
    skills: Option<IgnoredAny>,
    instructions: Option<IgnoredAny>,
    developer_instructions: Option<IgnoredAny>,
    model_instructions_file: Option<IgnoredAny>,
}

/// `ModelProviderInfo`, reduced to what decides whether its endpoint can be
/// carried over. Values that can hold secrets are never deserialized.
#[derive(Deserialize)]
struct Provider {
    base_url: Option<String>,
    wire_api: Option<String>,
    #[serde(default)]
    requires_openai_auth: bool,
    gateway_oauth: Option<IgnoredAny>,
    aws: Option<IgnoredAny>,
    query_params: Option<IgnoredAny>,
    env_key: Option<IgnoredAny>,
    experimental_bearer_token: Option<IgnoredAny>,
    auth: Option<IgnoredAny>,
    http_headers: Option<IgnoredAny>,
    env_http_headers: Option<IgnoredAny>,
}
impl Provider {
    /// The provider's key, token, auth command or headers stay behind.
    fn credentials(&self) -> bool {
        self.env_key.is_some()
            || self.experimental_bearer_token.is_some()
            || self.auth.is_some()
            || self.http_headers.is_some()
            || self.env_http_headers.is_some()
    }
    fn refusal(&self, id: &str) -> Option<UnsupportedProvider> {
        // Codex only speaks the Responses API and refuses other wire values;
        // reserved ids and AWS signing have no Agent 206 equivalent.
        if RESERVED.contains(&id)
            || BEDROCK.contains(&id)
            || self.aws.is_some()
            || self
                .wire_api
                .as_deref()
                .is_some_and(|wire| wire != "responses")
        {
            Some(UnsupportedProvider::UnknownProvider)
        } else if self.requires_openai_auth || self.gateway_oauth.is_some() {
            Some(UnsupportedProvider::DeviceFlow)
        } else if self.query_params.is_some() {
            // The parameters may be keys; without them the endpoint differs.
            Some(UnsupportedProvider::InvalidEndpoint)
        } else {
            None
        }
    }
}

pub fn preview(root: &Path, settings: &Settings) -> Result<Preview> {
    let metadata = std::fs::symlink_metadata(root).map_err(|source| ImportError::Read {
        path: root.into(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(ImportError::NotDirectory { path: root.into() });
    }
    let path = root.join("config.toml");
    let bytes = crate::read_bounded(&path, MAX_CONFIG_BYTES)?
        .ok_or_else(|| ImportError::NotFound { path: path.clone() })?;
    let config: Config = toml::from_slice(&bytes).map_err(|source| ImportError::Config {
        path,
        source: source.into(),
    })?;
    let presets = settings.provider_presets.all();
    let mut credentials = 0;
    let mut providers = vec![];
    for (id, provider) in &config.model_providers {
        credentials += usize::from(provider.credentials());
        let base_url = provider
            .base_url
            .as_deref()
            .filter(|url| !url.trim().is_empty());
        let valid = base_url.is_some_and(|url| a206_core::providers::base_url(url).is_ok());
        let refusal = provider.refusal(id);
        // Codex appends /responses and /models to this base; Agent 206 calls
        // the OpenAI-compatible routes under the same prefix.
        let base_url = match (valid, refusal) {
            (true, None) => base_url.unwrap_or_default().to_owned(),
            _ => String::new(),
        };
        let key = base_url.trim_end_matches('/');
        let preset = presets.iter().find(|preset| {
            !base_url.is_empty()
                && preset.api == ProviderApi::OpenAi
                && preset.base_url.trim_end_matches('/') == key
        });
        let action = if let Some(reason) = refusal {
            ProviderAction::Unsupported { reason }
        } else if base_url.is_empty() {
            ProviderAction::Unsupported {
                reason: if provider.base_url.is_some() {
                    UnsupportedProvider::InvalidEndpoint
                } else {
                    UnsupportedProvider::NoEndpoint
                },
            }
        } else if let Some(existing) = crate::existing(settings, ProviderApi::OpenAi, &base_url) {
            ProviderAction::AlreadyConfigured { existing }
        } else {
            ProviderAction::Add
        };
        providers.push(ProviderImport {
            source_id: id.clone(),
            preset: preset.map(|preset| preset.id.clone()),
            label: preset.map_or_else(
                || "Imported Codex connection".into(),
                |preset| preset.name.clone(),
            ),
            base_url,
            api: ProviderApi::OpenAi,
            credential: None,
            model_reference: None,
            action,
        });
    }
    let some = |value: &Option<IgnoredAny>| usize::from(value.is_some());
    let mut inactive = vec![];
    let mut leave = |category, count| {
        if count > 0 {
            inactive.push(Inactive { category, count });
        }
    };
    leave(
        Category::ModelSelection,
        some(&config.model)
            + some(&config.review_model)
            + some(&config.model_provider)
            + some(&config.oss_provider)
            + some(&config.profile)
            + config.profiles.len(),
    );
    leave(Category::Credentials, credentials);
    leave(
        Category::ToolPermissions,
        some(&config.approval_policy)
            + some(&config.approvals_reviewer)
            + some(&config.auto_review)
            + config.projects.len(),
    );
    leave(
        Category::Sandbox,
        some(&config.sandbox_mode)
            + some(&config.sandbox_workspace_write)
            + some(&config.permissions)
            + some(&config.default_permissions),
    );
    leave(
        Category::Environment,
        some(&config.shell_environment_policy),
    );
    leave(Category::Hooks, some(&config.notify) + some(&config.hooks));
    leave(Category::McpServers, config.mcp_servers.len());
    leave(
        Category::Plugins,
        some(&config.plugins) + config.marketplaces.len(),
    );
    leave(Category::Skills, some(&config.skills));
    let instructions = config.instructions.is_some()
        || config.developer_instructions.is_some()
        || config.model_instructions_file.is_some();
    Ok(Preview {
        version: PREVIEW_VERSION,
        tool: Tool::Codex,
        source: root.into(),
        config_sha256: Some(crate::sha256(&bytes)),
        vault: None,
        providers,
        messengers: vec![],
        inactive,
        unsupported: if instructions {
            vec![Unsupported::WorkspaceInstructions]
        } else {
            vec![]
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn random() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }
    fn source(text: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config.toml"), text).unwrap();
        root
    }
    fn count(preview: &Preview, category: Category) -> usize {
        preview
            .inactive
            .iter()
            .find(|item| item.category == category)
            .map_or(0, |item| item.count)
    }
    #[test]
    fn only_provider_endpoints_are_carried_over_without_authority_or_model_facts() {
        let secret = random();
        let id = random();
        let base = format!("https://{}.example.test/v1", random());
        let root = source(&format!(
            r#"
model = "{secret}"
model_provider = "{id}"
approval_policy = "never"
sandbox_mode = "danger-full-access"
notify = ["{secret}"]
developer_instructions = "{secret}"

[model_providers.{id}]
name = "{secret}"
base_url = "{base}"
env_key = "{secret}"
experimental_bearer_token = "{secret}"
http_headers = {{ "X-Key" = "{secret}" }}

[profiles.{secret}]
model = "{secret}"

[mcp_servers.{secret}]
command = "{secret}"
args = ["{secret}"]

[projects."/{secret}"]
trust_level = "trusted"
"#
        ));
        let mut settings = Settings::default();
        let result = preview(root.path(), &settings).unwrap();
        let [provider] = result.providers.as_slice() else {
            panic!("one provider expected: {:?}", result.providers);
        };
        assert_eq!(provider.base_url, base);
        assert_eq!(provider.api, ProviderApi::OpenAi);
        assert_eq!(provider.action, ProviderAction::Add);
        assert!(provider.credential.is_none() && provider.model_reference.is_none());
        assert_eq!(count(&result, Category::ModelSelection), 3);
        assert_eq!(count(&result, Category::Credentials), 1);
        assert_eq!(count(&result, Category::McpServers), 1);
        assert_eq!(count(&result, Category::ToolPermissions), 2);
        assert_eq!(count(&result, Category::Sandbox), 1);
        assert_eq!(count(&result, Category::Hooks), 1);
        assert_eq!(result.unsupported, [Unsupported::WorkspaceInstructions]);
        let text = toml::to_string(&result).unwrap();
        assert!(!text.contains(&secret), "{text}");
        assert_eq!(result.apply_providers(&mut settings).len(), 1);
        assert!(settings.providers[0].credential.is_none());
        assert!(matches!(
            preview(root.path(), &settings).unwrap().providers[0].action,
            ProviderAction::AlreadyConfigured { .. }
        ));
    }
    #[test]
    fn endpoints_codex_or_agent_206_cannot_reproduce_are_refused_without_echoing_them() {
        let secret = random();
        let base = format!("https://{}.example.test/v1", random());
        for (table, reason) in [
            (
                format!("[model_providers.openai]\nbase_url = \"{base}\""),
                UnsupportedProvider::UnknownProvider,
            ),
            (
                format!("[model_providers.amazon-bedrock]\nbase_url = \"{base}\""),
                UnsupportedProvider::UnknownProvider,
            ),
            (
                format!(
                    "[model_providers.x]\nbase_url = \"{base}\"\naws = {{ profile = \"{secret}\" }}"
                ),
                UnsupportedProvider::UnknownProvider,
            ),
            (
                format!("[model_providers.x]\nbase_url = \"{base}\"\nwire_api = \"chat\""),
                UnsupportedProvider::UnknownProvider,
            ),
            (
                format!("[model_providers.x]\nbase_url = \"{base}\"\nrequires_openai_auth = true"),
                UnsupportedProvider::DeviceFlow,
            ),
            (
                format!(
                    "[model_providers.x]\nbase_url = \"{base}\"\ngateway_oauth = {{ client_id = \"{secret}\" }}"
                ),
                UnsupportedProvider::DeviceFlow,
            ),
            (
                format!(
                    "[model_providers.x]\nbase_url = \"{base}\"\nquery_params = {{ key = \"{secret}\" }}"
                ),
                UnsupportedProvider::InvalidEndpoint,
            ),
            (
                format!(
                    "[model_providers.x]\nbase_url = \"https://user:{secret}@example.test/v1\""
                ),
                UnsupportedProvider::InvalidEndpoint,
            ),
            (
                format!("[model_providers.x]\nbase_url = \"https://example.test/?key={secret}\""),
                UnsupportedProvider::InvalidEndpoint,
            ),
            (
                "[model_providers.x]\nname = \"x\"".to_owned(),
                UnsupportedProvider::NoEndpoint,
            ),
        ] {
            let result = preview(source(&table).path(), &Settings::default()).unwrap();
            assert_eq!(
                result.providers[0].action,
                ProviderAction::Unsupported { reason },
                "{table}"
            );
            assert!(result.providers[0].base_url.is_empty(), "{table}");
            assert!(!toml::to_string(&result).unwrap().contains(&secret));
        }
        let explicit =
            format!("[model_providers.x]\nbase_url = \"{base}\"\nwire_api = \"responses\"");
        let result = preview(source(&explicit).path(), &Settings::default()).unwrap();
        assert_eq!(result.providers[0].action, ProviderAction::Add);
    }
    #[test]
    fn a_matching_openai_compatible_preset_names_the_connection() {
        let base = format!("https://{}.example.test/v1", random());
        let mut settings = Settings::default();
        settings
            .provider_presets
            .entries
            .push(a206_core::ProviderPreset {
                id: random(),
                name: random(),
                group: random(),
                api: ProviderApi::OpenAi,
                base_url: format!("{base}/"),
                setup: String::new(),
            });
        let preset = settings.provider_presets.entries[0].clone();
        let other = format!("https://{}.example.test/v1", random());
        let root = source(&format!(
            "[model_providers.a]\nbase_url = \"{base}\"\n[model_providers.b]\nbase_url = \"{other}\""
        ));
        let result = preview(root.path(), &settings).unwrap();
        assert_eq!(result.providers[0].preset.as_ref(), Some(&preset.id));
        assert_eq!(result.providers[0].label, preset.name);
        assert_eq!(result.providers[1].preset, None);
        assert_eq!(result.providers[1].base_url, other);
    }
    #[test]
    fn malformed_toml_retains_only_location_and_regular_file_bounds_apply() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        assert!(matches!(
            preview(root.path(), &Settings::default()),
            Err(ImportError::NotFound { .. })
        ));
        let secret = random();
        std::fs::write(
            &path,
            format!("[model_providers.x]\nbase_url = [\"{secret}\"]"),
        )
        .unwrap();
        let error = preview(root.path(), &Settings::default()).unwrap_err();
        assert!(matches!(
            error,
            ImportError::Config {
                source: crate::ConfigFailure { span: Some(_) },
                ..
            }
        ));
        assert!(!format!("{error:?}").contains(&secret));
        std::fs::write(&path, vec![b' '; MAX_CONFIG_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            preview(root.path(), &Settings::default()),
            Err(ImportError::TooLarge { .. })
        ));
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink("unread", &path).unwrap();
            assert!(matches!(
                preview(root.path(), &Settings::default()),
                Err(ImportError::NotRegular { .. })
            ));
        }
    }
}
