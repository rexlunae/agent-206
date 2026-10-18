//! Explicit Claude Code settings.json scope, documented at
//! code.claude.com/docs/en/settings and /env-vars (inspected 2026-10-18).
//! No scope merging, authentication discovery or foreign code execution.
use crate::{
    ImportError, Inactive, InactiveCategory as Category, MAX_CONFIG_BYTES, PREVIEW_VERSION,
    Preview, ProviderAction, ProviderImport, Result, Tool, UnsupportedProvider,
};
use a206_core::{ProviderApi, Settings};
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, Visitor},
};
use std::{fmt, path::Path};

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    env: Environment,
    permissions: Option<IgnoredAny>,
    hooks: Option<IgnoredAny>,
    #[serde(rename = "enabledPlugins")]
    plugins: Option<IgnoredAny>,
    #[serde(rename = "extraKnownMarketplaces")]
    marketplaces: Option<IgnoredAny>,
    #[serde(rename = "apiKeyHelper")]
    credential_helper: Option<IgnoredAny>,
    model: Option<IgnoredAny>,
    #[serde(rename = "mcpServers")]
    mcp: Option<IgnoredAny>,
    #[serde(rename = "enableAllProjectMcpServers")]
    mcp_auto: Option<IgnoredAny>,
    #[serde(rename = "enabledMcpjsonServers")]
    mcp_enabled: Option<IgnoredAny>,
    sandbox: Option<IgnoredAny>,
}

#[derive(Default)]
struct Environment {
    endpoint: Option<String>,
    routing: bool,
    credentials: usize,
    other: usize,
}
impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Environment;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an environment object")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Environment, M::Error> {
                let mut result = Environment::default();
                let mut endpoint_seen = false;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "ANTHROPIC_BASE_URL" => {
                            if endpoint_seen {
                                return Err(serde::de::Error::duplicate_field(
                                    "ANTHROPIC_BASE_URL",
                                ));
                            }
                            endpoint_seen = true;
                            result.endpoint = Some(map.next_value()?);
                        }
                        "CLAUDE_CODE_USE_BEDROCK"
                        | "CLAUDE_CODE_USE_VERTEX"
                        | "CLAUDE_CODE_USE_FOUNDRY"
                        | "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST" => {
                            // Even a false/empty switch remains unbound: this
                            // reader does not reconstruct shell/host precedence.
                            map.next_value::<IgnoredAny>()?;
                            result.routing = true;
                            result.other += 1;
                        }
                        "ANTHROPIC_API_KEY"
                        | "ANTHROPIC_AUTH_TOKEN"
                        | "CLAUDE_CODE_OAUTH_TOKEN" => {
                            map.next_value::<IgnoredAny>()?;
                            result.credentials += 1;
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                            result.other += 1;
                        }
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(Fields)
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
    let path = root.join("settings.json");
    let bytes = crate::read_bounded(&path, MAX_CONFIG_BYTES)?
        .ok_or_else(|| ImportError::NotFound { path: path.clone() })?;
    let config: Config =
        serde_json::from_slice(&bytes).map_err(|source| ImportError::JsonConfig {
            path,
            source: source.into(),
        })?;
    let mut providers = vec![];
    if let Some(endpoint) = config.env.endpoint {
        // The Anthropic SDK appends /v1/messages and /v1/models to its base.
        // Agent 206 appends messages/models, so preserve that exact prefix.
        let valid = a206_core::providers::base_url(&endpoint).is_ok();
        let base_url = if valid && !config.env.routing {
            format!("{}/v1", endpoint.trim_end_matches('/'))
        } else {
            String::new()
        };
        let action = if config.env.routing {
            ProviderAction::Unsupported {
                reason: UnsupportedProvider::UnknownProvider,
            }
        } else if !valid {
            ProviderAction::Unsupported {
                reason: UnsupportedProvider::InvalidEndpoint,
            }
        } else if let Some(existing) = crate::existing(settings, ProviderApi::Anthropic, &base_url)
        {
            ProviderAction::AlreadyConfigured { existing }
        } else {
            ProviderAction::Add
        };
        providers.push(ProviderImport {
            source_id: "claude-code-endpoint".into(),
            preset: None,
            label: "Imported Claude Code connection".into(),
            base_url,
            api: ProviderApi::Anthropic,
            credential: None,
            model_reference: None,
            action,
        });
    }
    let mut inactive = vec![];
    let mut leave = |category, count| {
        if count > 0 {
            inactive.push(Inactive { category, count });
        }
    };
    leave(Category::Environment, config.env.other);
    leave(
        Category::Credentials,
        config.env.credentials + usize::from(config.credential_helper.is_some()),
    );
    leave(
        Category::ToolPermissions,
        usize::from(config.permissions.is_some()),
    );
    leave(Category::Hooks, usize::from(config.hooks.is_some()));
    leave(
        Category::Plugins,
        usize::from(config.plugins.is_some()) + usize::from(config.marketplaces.is_some()),
    );
    leave(
        Category::ModelSelection,
        usize::from(config.model.is_some()),
    );
    leave(
        Category::McpServers,
        usize::from(config.mcp.is_some())
            + usize::from(config.mcp_auto.is_some())
            + usize::from(config.mcp_enabled.is_some()),
    );
    leave(Category::Sandbox, usize::from(config.sandbox.is_some()));
    Ok(Preview {
        version: PREVIEW_VERSION,
        tool: Tool::ClaudeCode,
        source: root.into(),
        config_sha256: Some(crate::sha256(&bytes)),
        vault: None,
        providers,
        messengers: vec![],
        inactive,
        unsupported: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(value: serde_json::Value) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("settings.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        root
    }
    #[test]
    fn only_explicit_endpoint_is_carried_over_without_authority_or_model_facts() {
        let secret = uuid::Uuid::new_v4().to_string();
        let historical = uuid::Uuid::new_v4().to_string();
        let root = source(
            serde_json::json!({"env": {"ANTHROPIC_BASE_URL":"https://example.test/gateway", "ANTHROPIC_API_KEY":secret, "EXTRA":secret}, "hooks": secret, "permissions":{"allow":[secret], "deny":[secret]}, "model":historical, "enabledPlugins":secret, "mcpServers":secret, "apiKeyHelper":secret}),
        );
        let mut settings = Settings::default();
        let result = preview(root.path(), &settings).unwrap();
        assert_eq!(
            result.providers[0].base_url,
            "https://example.test/gateway/v1"
        );
        assert_eq!(result.providers[0].api, ProviderApi::Anthropic);
        assert!(
            result.providers[0].credential.is_none()
                && result.providers[0].model_reference.is_none()
        );
        let text = toml::to_string(&result).unwrap();
        assert!(!text.contains(&secret) && !text.contains(&historical));
        assert!(result.vault.is_none() && result.messengers.is_empty());
        assert_eq!(result.apply_providers(&mut settings).len(), 1);
        assert!(matches!(
            preview(root.path(), &settings).unwrap().providers[0].action,
            ProviderAction::AlreadyConfigured { .. }
        ));
        assert!(settings.providers[0].credential.is_none());
        assert!(
            preview(
                source(serde_json::json!({"model":historical})).path(),
                &settings
            )
            .unwrap()
            .providers
            .is_empty()
        );
    }
    #[test]
    fn routing_and_secret_bearing_urls_are_never_imported_or_echoed() {
        for routing in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
        ] {
            let mut env = serde_json::Map::new();
            env.insert("ANTHROPIC_BASE_URL".into(), "https://example.test".into());
            env.insert(routing.into(), false.into());
            let result = preview(
                source(serde_json::json!({"env":env})).path(),
                &Settings::default(),
            )
            .unwrap();
            assert!(matches!(
                result.providers[0].action,
                ProviderAction::Unsupported { .. }
            ));
            assert!(result.providers[0].base_url.is_empty());
        }
        let secret = uuid::Uuid::new_v4().to_string();
        for endpoint in [
            format!("https://user:{secret}@example.test"),
            format!("https://example.test/?key={secret}"),
            "http://example.test".into(),
        ] {
            let result = preview(
                source(serde_json::json!({"env":{"ANTHROPIC_BASE_URL": endpoint}})).path(),
                &Settings::default(),
            )
            .unwrap();
            assert!(result.providers[0].base_url.is_empty());
            assert!(!toml::to_string(&result).unwrap().contains(&secret));
        }
    }
    #[test]
    fn malformed_json_retains_only_location_and_regular_file_bounds_apply() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        assert!(matches!(
            preview(root.path(), &Settings::default()),
            Err(ImportError::NotFound { .. })
        ));
        let secret = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            &path,
            format!("{{\"env\":{{\"ANTHROPIC_BASE_URL\":[\"{secret}\"]}}}}"),
        )
        .unwrap();
        let error = preview(root.path(), &Settings::default()).unwrap_err();
        assert!(!format!("{error:?}").contains(&secret));
        assert!(matches!(
            error,
            ImportError::JsonConfig {
                source: crate::JsonConfigFailure { line: 1, .. },
                ..
            }
        ));
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
