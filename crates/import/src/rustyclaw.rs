//! RustyClaw (reference `481a428c`) installation reader: `config.toml`, the
//! SecureStore vault under `credentials/` and the workspace. Only the fields
//! below are read; values that may hold secrets (messenger tokens, registry
//! tokens, MCP commands) are counted with `IgnoredAny` and never kept.
use crate::{
    ImportError, Inactive, InactiveCategory, MAX_CONFIG_BYTES, PREVIEW_VERSION, Preview,
    ProviderAction, ProviderImport, Result, Tool, Unsupported, UnsupportedProvider, VaultPlan,
    VaultProtection, VaultSource,
};
use a206_core::{ProviderApi, ProviderPreset, Settings};
use serde::Deserialize;
use serde::de::IgnoredAny;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// RustyClaw's default settings directory under a home directory.
pub fn default_root(home: &Path) -> PathBuf {
    home.join(".rustyclaw")
}

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    workspace_dir: Option<PathBuf>,
    #[serde(default)]
    credentials_dir: Option<PathBuf>,
    #[serde(default)]
    soul_path: Option<PathBuf>,
    #[serde(default)]
    model: Option<Model>,
    #[serde(default)]
    secrets_password_protected: Option<bool>,
    #[serde(default)]
    custom_providers: Vec<CustomProvider>,
    #[serde(default)]
    messengers: Vec<IgnoredAny>,
    #[serde(default)]
    mcp: Mcp,
    #[serde(default)]
    tool_permissions: BTreeMap<String, IgnoredAny>,
    #[serde(default)]
    disabled_tool_groups: Vec<IgnoredAny>,
    #[serde(default)]
    services: BTreeMap<String, IgnoredAny>,
    #[serde(default)]
    engines: BTreeMap<String, IgnoredAny>,
    #[serde(default)]
    sandbox: Option<IgnoredAny>,
    #[serde(default)]
    clawhub_token: Option<IgnoredAny>,
    #[serde(default)]
    system_prompt: Option<IgnoredAny>,
}

#[derive(Default, Deserialize)]
struct Mcp {
    #[serde(default)]
    servers: BTreeMap<String, IgnoredAny>,
}

#[derive(Deserialize)]
struct Model {
    provider: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
}

#[derive(Deserialize)]
struct CustomProvider {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    base_url: String,
    #[serde(default)]
    api_format: Option<String>,
    #[serde(default)]
    api_key_secret: Option<String>,
}

/// How a RustyClaw provider id maps here: the preset, and the vault entry
/// RustyClaw keeps its key under (`providers/mod.rs` `secret_key`).
enum Mapping {
    Preset(&'static str, Option<&'static str>),
    DeviceFlow,
    Unknown,
}

fn mapping(id: &str) -> Mapping {
    use Mapping::*;
    match id {
        "anthropic" => Preset("anthropic", Some("ANTHROPIC_API_KEY")),
        "openai" => Preset("openai", Some("OPENAI_API_KEY")),
        "google" => Preset("gemini", Some("GEMINI_API_KEY")),
        "xai" => Preset("x-ai", Some("XAI_API_KEY")),
        "deepseek" => Preset("deepseek", Some("DEEPSEEK_API_KEY")),
        "openrouter" => Preset("openrouter", Some("OPENROUTER_API_KEY")),
        "ollama" => Preset("ollama", None),
        "lmstudio" => Preset("lm-studio", None),
        "joshua" => Preset("joshua", None),
        // OpenAI-compatible local servers; their endpoint must be configured.
        "exo" | "llamacpp" => Preset("custom", None),
        "custom" => Preset("custom", Some("CUSTOM_API_KEY")),
        "github-copilot" | "copilot-proxy" => DeviceFlow,
        _ => Unknown,
    }
}

fn api_format(format: Option<&str>) -> ProviderApi {
    match format {
        Some("anthropic") => ProviderApi::Anthropic,
        Some("gemini") => ProviderApi::Gemini,
        // `openai`, `xai` and the default are OpenAI-compatible.
        _ => ProviderApi::OpenAi,
    }
}

/// One provider as the source describes it, before mapping.
struct Source {
    source_id: String,
    preset: &'static str,
    label: Option<String>,
    base_url: Option<String>,
    api: Option<ProviderApi>,
    credential: Option<String>,
    model_reference: Option<String>,
}

fn plan(presets: &[ProviderPreset], settings: &Settings, source: Source) -> ProviderImport {
    let found = presets.iter().find(|p| p.id == source.preset);
    let base_url = source
        .base_url
        .filter(|url| !url.trim().is_empty())
        .or_else(|| found.map(|p| p.base_url.clone()))
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_default();
    let api = source.api.or(found.map(|p| p.api)).unwrap_or_default();
    let action = if found.is_none() {
        ProviderAction::Unsupported {
            reason: UnsupportedProvider::UnknownProvider,
        }
    } else if base_url.is_empty() {
        ProviderAction::Unsupported {
            reason: UnsupportedProvider::NoEndpoint,
        }
    } else if a206_core::providers::base_url(&base_url).is_err() {
        ProviderAction::Unsupported {
            reason: UnsupportedProvider::InvalidEndpoint,
        }
    } else if let Some(existing) = crate::existing(settings, api, &base_url) {
        ProviderAction::AlreadyConfigured { existing }
    } else {
        ProviderAction::Add
    };
    ProviderImport {
        label: source
            .label
            .or_else(|| found.map(|p| p.name.clone()))
            .unwrap_or_else(|| source.source_id.clone()),
        source_id: source.source_id,
        preset: found.map(|p| p.id.clone()),
        base_url,
        api,
        credential: source.credential,
        model_reference: source.model_reference,
        action,
    }
}

fn resolve(root: &Path, path: Option<&PathBuf>, default: PathBuf) -> PathBuf {
    match path {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => root.join(path),
        None => default,
    }
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn count_dir(path: &Path) -> usize {
    std::fs::read_dir(path).map_or(0, |entries| entries.flatten().count())
}

/// Read the installation at `root` and plan its import into `settings`.
/// `vault_exists` is whether this profile already has a vault (`None` when
/// it could not be checked). Nothing is written.
pub fn preview(root: &Path, settings: &Settings, vault_exists: Option<bool>) -> Result<Preview> {
    match std::fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(ImportError::NotDirectory {
                path: root.to_owned(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ImportError::NotFound {
                path: root.to_owned(),
            });
        }
        Err(source) => {
            return Err(ImportError::Read {
                path: root.to_owned(),
                source,
            });
        }
    }
    let config_path = root.join("config.toml");
    let bytes = crate::read_bounded(&config_path, MAX_CONFIG_BYTES)?;
    let config: Config = match &bytes {
        Some(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            toml::from_str(&text).map_err(|source| ImportError::Config {
                path: config_path.clone(),
                source,
            })?
        }
        None => Config::default(),
    };
    let presets = settings.provider_presets.all();
    let mut providers = vec![];
    if let Some(model) = config.model {
        providers.push(match mapping(&model.provider) {
            Mapping::Preset(preset, secret) => plan(
                &presets,
                settings,
                Source {
                    preset,
                    label: None,
                    base_url: model.base_url,
                    api: None,
                    credential: secret.map(str::to_owned),
                    model_reference: model.model,
                    source_id: model.provider,
                },
            ),
            other => ProviderImport {
                label: model.provider.clone(),
                source_id: model.provider,
                preset: None,
                base_url: model.base_url.unwrap_or_default(),
                api: ProviderApi::default(),
                credential: None,
                model_reference: model.model,
                action: ProviderAction::Unsupported {
                    reason: if matches!(other, Mapping::DeviceFlow) {
                        UnsupportedProvider::DeviceFlow
                    } else {
                        UnsupportedProvider::UnknownProvider
                    },
                },
            },
        });
    }
    for custom in config.custom_providers {
        providers.push(plan(
            &presets,
            settings,
            Source {
                preset: "custom",
                label: custom.display_name,
                base_url: Some(custom.base_url),
                api: Some(api_format(custom.api_format.as_deref())),
                credential: custom.api_key_secret,
                model_reference: None,
                source_id: custom.id,
            },
        ));
    }

    let credentials = resolve(
        root,
        config.credentials_dir.as_ref(),
        root.join("credentials"),
    );
    // RustyClaw moved `secrets.json` from the settings root into `credentials/`.
    let vault_path = [credentials.join("secrets.json"), root.join("secrets.json")]
        .into_iter()
        .find(|path| exists(path));
    let vault = match vault_path {
        Some(path) => {
            let bytes = crate::regular_len(&path)?.unwrap_or_default();
            let key_file = path.with_file_name("secrets.key");
            let protection = match config.secrets_password_protected {
                Some(true) => VaultProtection::Password,
                Some(false) => VaultProtection::KeyFile,
                None if exists(&key_file) => VaultProtection::KeyFile,
                None => VaultProtection::Password,
            };
            let plan = match (protection, vault_exists) {
                (VaultProtection::KeyFile, _) => VaultPlan::KeyFileUnsupported,
                (_, Some(true)) => VaultPlan::TargetExists,
                (_, Some(false)) => VaultPlan::Import,
                (_, None) => VaultPlan::TargetUnknown,
            };
            Some(VaultSource {
                path,
                bytes,
                protection,
                plan,
            })
        }
        None => None,
    };

    let mut inactive = vec![];
    let mut leave = |category, count: usize| {
        if count > 0 {
            inactive.push(Inactive { category, count });
        }
    };
    leave(InactiveCategory::Messengers, config.messengers.len());
    leave(InactiveCategory::McpServers, config.mcp.servers.len());
    leave(
        InactiveCategory::ToolPermissions,
        config.tool_permissions.len(),
    );
    leave(
        InactiveCategory::DisabledToolGroups,
        config.disabled_tool_groups.len(),
    );
    leave(InactiveCategory::Services, config.services.len());
    leave(InactiveCategory::Engines, config.engines.len());
    leave(
        InactiveCategory::Sandbox,
        usize::from(config.sandbox.is_some()),
    );
    leave(
        InactiveCategory::PlaintextToken,
        usize::from(config.clawhub_token.is_some()),
    );
    let workspace = resolve(root, config.workspace_dir.as_ref(), root.join("workspace"));
    leave(
        InactiveCategory::Skills,
        count_dir(&workspace.join("skills")),
    );

    let mut unsupported = vec![];
    let soul = resolve(root, config.soul_path.as_ref(), workspace.join("SOUL.md"));
    if exists(&soul) || exists(&workspace.join("AGENTS.md")) {
        unsupported.push(Unsupported::WorkspaceInstructions);
    }
    if exists(&root.join("agents")) || exists(&root.join("threads.json")) {
        unsupported.push(Unsupported::Conversations);
    }
    if exists(&workspace.join("MEMORY.md")) || exists(&workspace.join("memory")) {
        unsupported.push(Unsupported::Memory);
    }
    if config.system_prompt.is_some() {
        unsupported.push(Unsupported::SystemPrompt);
    }

    Ok(Preview {
        version: PREVIEW_VERSION,
        tool: Tool::RustyClaw,
        source: root.to_owned(),
        config_sha256: bytes.as_deref().map(crate::sha256),
        vault,
        providers,
        inactive,
        unsupported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Receipt;

    fn random() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A RustyClaw settings directory written as RustyClaw saves it.
    fn installation(model: &str, token: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let custom_url = format!("http://127.0.0.1:{}/v1", 1024 + rand_port());
        write(
            &root.join("config.toml"),
            &format!(
                r#"
settings_dir = "{root}"
use_secrets = true
secrets_password_protected = true
totp_enabled = true
agent_access = true
clawhub_token = "{token}"
system_prompt = "{token}"

[model]
provider = "anthropic"
model = "{model}"

[[custom_providers]]
id = "lab"
display_name = "Lab server"
base_url = "{custom_url}"
api_format = "anthropic"
api_key_secret = "LAB_KEY"

[[messengers]]
name = "bot"
messenger_type = "telegram"
token = "{token}"

[mcp.servers.files]
command = "{token}"

[tool_permissions]
execute_command = "Deny"
"#,
                root = root.display()
            ),
        );
        write(&root.join("credentials").join("secrets.json"), "{}");
        write(&root.join("workspace").join("SOUL.md"), &random());
        write(
            &root.join("workspace").join("skills").join("a.md"),
            &random(),
        );
        dir
    }

    fn rand_port() -> u16 {
        (uuid::Uuid::new_v4().as_u128() % 20_000) as u16
    }

    #[test]
    fn preview_maps_providers_and_keeps_secrets_and_commands_out() {
        let model = random();
        let token = random();
        let dir = installation(&model, &token);
        let preview = preview(dir.path(), &Settings::default(), Some(false)).unwrap();
        let anthropic = &preview.providers[0];
        assert_eq!(anthropic.preset.as_deref(), Some("anthropic"));
        assert_eq!(anthropic.credential.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(anthropic.model_reference.as_deref(), Some(model.as_str()));
        assert_eq!(anthropic.action, ProviderAction::Add);
        assert!(!anthropic.base_url.is_empty());
        let lab = &preview.providers[1];
        assert_eq!(lab.label, "Lab server");
        assert_eq!(lab.api, ProviderApi::Anthropic);
        assert_eq!(lab.credential.as_deref(), Some("LAB_KEY"));
        assert_eq!(preview.vault.as_ref().unwrap().plan, VaultPlan::Import);
        for category in [
            InactiveCategory::Messengers,
            InactiveCategory::McpServers,
            InactiveCategory::ToolPermissions,
            InactiveCategory::PlaintextToken,
            InactiveCategory::Skills,
        ] {
            assert!(
                preview.inactive.iter().any(|i| i.category == category),
                "{category:?}"
            );
        }
        assert!(
            preview
                .unsupported
                .contains(&Unsupported::WorkspaceInstructions)
        );
        assert!(preview.unsupported.contains(&Unsupported::SystemPrompt));
        // Neither the token nor the MCP command reaches the preview, in
        // either of its encodings.
        let text = toml::to_string(&preview).unwrap();
        assert!(!text.contains(&token));
        let bytes = rmp_serde::to_vec_named(&preview).unwrap();
        assert!(!bytes.windows(token.len()).any(|w| w == token.as_bytes()));
        assert_eq!(rmp_serde::from_slice::<Preview>(&bytes).unwrap(), preview);
    }

    #[test]
    fn applying_is_additive_and_a_repeat_adds_nothing() {
        let dir = installation(&random(), &random());
        let mut settings = Settings::default();
        let first = preview(dir.path(), &settings, Some(false)).unwrap();
        let before = settings.clone();
        let added = first.apply_providers(&mut settings);
        assert_eq!(added.len(), 2);
        assert_eq!(
            &settings.providers[..before.providers.len()],
            &before.providers[..]
        );
        let again = preview(dir.path(), &settings, Some(true)).unwrap();
        assert!(again.additions().next().is_none());
        assert!(matches!(
            again.providers[0].action,
            ProviderAction::AlreadyConfigured { .. }
        ));
        assert_eq!(again.vault.unwrap().plan, VaultPlan::TargetExists);
        // Applying the stale first preview again also adds nothing.
        assert!(first.apply_providers(&mut settings).is_empty());
    }

    #[test]
    fn unknown_device_flow_and_key_file_sources_are_reported_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("config.toml"),
            "[model]\nprovider = \"github-copilot\"\n",
        );
        write(&root.join("secrets.json"), "{}");
        write(&root.join("secrets.key"), "{}");
        let preview = preview(root, &Settings::default(), None).unwrap();
        assert_eq!(
            preview.providers[0].action,
            ProviderAction::Unsupported {
                reason: UnsupportedProvider::DeviceFlow
            }
        );
        let vault = preview.vault.unwrap();
        assert_eq!(vault.protection, VaultProtection::KeyFile);
        assert_eq!(vault.plan, VaultPlan::KeyFileUnsupported);

        write(
            &root.join("config.toml"),
            &format!("[model]\nprovider = \"{}\"\n", random()),
        );
        let preview = super::preview(root, &Settings::default(), None).unwrap();
        assert_eq!(
            preview.providers[0].action,
            ProviderAction::Unsupported {
                reason: UnsupportedProvider::UnknownProvider
            }
        );
    }

    #[test]
    fn endpoints_the_gateway_would_refuse_are_not_offered() {
        let host = random();
        for base_url in [
            format!("http://{host}.example/v1"),
            format!("https://user:{host}@{host}.example/v1"),
            format!("https://{host}.example/v1?key={host}"),
            host.clone(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(
                &dir.path().join("config.toml"),
                &format!("[[custom_providers]]\nid = \"lab\"\nbase_url = \"{base_url}\"\n"),
            );
            let preview = preview(dir.path(), &Settings::default(), None).unwrap();
            assert_eq!(
                preview.providers[0].action,
                ProviderAction::Unsupported {
                    reason: UnsupportedProvider::InvalidEndpoint
                },
                "{base_url}"
            );
            let mut settings = Settings::default();
            assert!(preview.apply_providers(&mut settings).is_empty());
        }
    }

    #[test]
    fn malformed_oversized_symlinked_and_missing_sources_fail_with_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(matches!(
            preview(&root.join(random()), &Settings::default(), None),
            Err(ImportError::NotFound { .. })
        ));
        write(&root.join("config.toml"), "[model");
        assert!(matches!(
            preview(root, &Settings::default(), None),
            Err(ImportError::Config { .. })
        ));
        let big = "#".repeat(MAX_CONFIG_BYTES as usize + 1);
        write(&root.join("config.toml"), &big);
        assert!(matches!(
            preview(root, &Settings::default(), None),
            Err(ImportError::TooLarge { .. })
        ));
        #[cfg(unix)]
        {
            let elsewhere = root.join("elsewhere.toml");
            write(&elsewhere, "");
            std::fs::remove_file(root.join("config.toml")).unwrap();
            std::os::unix::fs::symlink(&elsewhere, root.join("config.toml")).unwrap();
            assert!(matches!(
                preview(root, &Settings::default(), None),
                Err(ImportError::NotRegular { .. })
            ));
        }
    }

    #[test]
    fn receipts_are_private_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = Receipt {
            version: PREVIEW_VERSION,
            id: uuid::Uuid::new_v4(),
            tool: Tool::RustyClaw,
            source: PathBuf::from(random()),
            config_sha256: Some(random()),
            applied_at: 0,
            providers_added: vec![uuid::Uuid::new_v4()],
            vault_imported: true,
        };
        let path = crate::write_receipt(dir.path(), &receipt).unwrap();
        let read: Receipt = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, receipt);
        assert!(matches!(
            crate::write_receipt(dir.path(), &receipt),
            Err(ImportError::Receipt { .. })
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
        }
    }
}
