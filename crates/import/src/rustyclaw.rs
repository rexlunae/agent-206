//! RustyClaw (reference `481a428c`) installation reader: `config.toml`, the
//! SecureStore vault under `credentials/` and the workspace. Only the fields
//! below are read; values that may hold secrets (messenger tokens, registry
//! tokens, MCP commands) are counted with `IgnoredAny` and never kept. Of a
//! messenger's webhook URL, which is itself the credential, only the origin
//! is kept.
use crate::{
    ImportError, Inactive, InactiveCategory, MAX_CONFIG_BYTES, MessengerAction, MessengerImport,
    PREVIEW_VERSION, Preview, ProviderAction, ProviderImport, Result, Tool, Unsupported,
    UnsupportedMessenger, UnsupportedProvider, VaultPlan, VaultProtection, VaultSource,
};
use a206_core::messengers::{
    IRCS_PORT, IrcServer, MAX_ALLOWED_CHATS, irc_lowercase, valid_field, valid_irc_channel,
    valid_irc_nick, valid_name,
};
use a206_core::{MessengerKind, ProviderApi, ProviderPreset, Settings, WebhookFormat};
use serde::Deserialize;
use serde::de::IgnoredAny;
use std::collections::{BTreeMap, BTreeSet};
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
    messengers: Vec<Messenger>,
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

/// The fields of a RustyClaw `[[messengers]]` entry this importer uses. Its
/// secrets (tokens, passwords, app passwords) are not deserialized; `token`,
/// `access_token`, `password`, `app_token` and `app_password` are only
/// checked for presence, and `webhook_url` is reduced to its origin.
#[derive(Deserialize)]
struct Messenger {
    #[serde(default)]
    name: String,
    #[serde(default)]
    messenger_type: String,
    #[serde(default)]
    config_path: Option<IgnoredAny>,
    #[serde(default)]
    token: Option<IgnoredAny>,
    #[serde(default)]
    access_token: Option<IgnoredAny>,
    #[serde(default)]
    password: Option<IgnoredAny>,
    /// Slack's app-level token for Socket Mode.
    #[serde(default)]
    app_token: Option<IgnoredAny>,
    /// A generic, Teams or Google Chat incoming webhook.
    #[serde(default, deserialize_with = "webhook_origin")]
    webhook_url: Option<WebhookUrl>,
    /// Teams Bot Framework sign-in, which RustyClaw prefers to a webhook.
    #[serde(default)]
    app_id: Option<IgnoredAny>,
    #[serde(default)]
    app_password: Option<IgnoredAny>,
    /// A Google Chat service account key file, which RustyClaw prefers to a
    /// webhook.
    #[serde(default)]
    credentials_path: Option<IgnoredAny>,
    /// Matrix homeserver URL.
    #[serde(default)]
    homeserver: Option<String>,
    /// IRC server host name, port, nickname, channels and whether TLS is on.
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    nick: Option<String>,
    #[serde(default)]
    irc_channels: Vec<String>,
    #[serde(default)]
    use_tls: Option<bool>,
    #[serde(default)]
    allowed_chats: Vec<String>,
    #[serde(default)]
    allowed_users: Vec<String>,
}

/// What is kept of a webhook URL: the origin a webhook messenger would use
/// as its endpoint, or that it has none the gateway accepts.
enum WebhookUrl {
    Origin(String),
    Unusable,
}

/// Read a webhook URL and keep only its origin. The URL itself is the
/// credential (its path holds the secret), so it goes no further than here.
fn webhook_origin<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<WebhookUrl>, D::Error> {
    let url = String::deserialize(deserializer)?;
    let origin = url::Url::parse(&url)
        .ok()
        .filter(|url| {
            url.username().is_empty() && url.password().is_none() && url.fragment().is_none()
        })
        .map(|url| url.origin().ascii_serialization())
        .filter(|origin| a206_core::providers::base_url(origin).is_ok());
    Ok(Some(
        origin.map_or(WebhookUrl::Unusable, WebhookUrl::Origin),
    ))
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
        // Invalid URLs may contain a password or signed token. They are not
        // useful configuration and must not enter review text or DOM state.
        base_url: safe_endpoint(base_url),
        api,
        credential: source.credential,
        model_reference: source.model_reference,
        action,
    }
}

fn safe_endpoint(value: String) -> String {
    if a206_core::providers::base_url(&value).is_ok() {
        value
    } else {
        String::new()
    }
}

fn messenger_kind(id: &str) -> Option<MessengerKind> {
    match id {
        "telegram" => Some(MessengerKind::Telegram),
        "matrix" => Some(MessengerKind::Matrix),
        "discord" => Some(MessengerKind::Discord),
        "slack" => Some(MessengerKind::Slack),
        "irc" => Some(MessengerKind::Irc),
        "signal" => Some(MessengerKind::Signal),
        _ => None,
    }
}

/// `source` reduced to a valid messenger name, or `fallback` if nothing is left.
fn base_name(source: &str, fallback: &str) -> String {
    let mut name = String::new();
    for c in source.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    // Leave room for a `-N` suffix within the 64-byte limit.
    name.truncate(56);
    let name = name.trim_end_matches('-');
    if name.is_empty() {
        fallback.to_owned()
    } else {
        name.to_owned()
    }
}

/// Plan each source messenger. Names are made unique; an entry whose name a
/// same-kind messenger already has is taken to be an earlier import of it.
fn plan_messengers(settings: &Settings, source: Vec<Messenger>) -> Vec<MessengerImport> {
    let mut claimed = BTreeSet::new();
    source
        .into_iter()
        .map(|messenger| {
            // RustyClaw's Teams and Google Chat entries post through a
            // webhook when one is set and no bot or service account is.
            let webhook = messenger.webhook_url.is_some();
            let format = match messenger.messenger_type.as_str() {
                "webhook" => Some(WebhookFormat::Text),
                "teams"
                    if webhook
                        && messenger.app_id.is_none()
                        && messenger.app_password.is_none() =>
                {
                    Some(WebhookFormat::Teams)
                }
                "google_chat" if webhook && messenger.credentials_path.is_none() => {
                    Some(WebhookFormat::Text)
                }
                _ => None,
            };
            let kind = if format.is_some() {
                Some(MessengerKind::Webhook)
            } else {
                messenger_kind(&messenger.messenger_type)
            };
            let fallback = base_name(&messenger.messenger_type, "messenger");
            let base = base_name(&messenger.name, &fallback);
            let mut action = MessengerAction::Add;
            let mut name = base.clone();
            for n in 1.. {
                if n > 1 {
                    name = format!("{base}-{n}");
                }
                if claimed.contains(&name) {
                    continue;
                }
                match settings.messengers.iter().find(|m| m.name == name) {
                    Some(existing) if Some(existing.kind) == kind => {
                        action = MessengerAction::AlreadyConfigured;
                        break;
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
            claimed.insert(name.clone());
            let irc = kind.is_some_and(MessengerKind::is_irc);
            let mut allowed_chats: BTreeSet<String> = messenger.allowed_chats.into_iter().collect();
            let mut allowed_users: BTreeSet<String> = messenger.allowed_users.into_iter().collect();
            if format.is_some() {
                // A webhook only sends; it has no chats or senders to allow.
                allowed_chats.clear();
                allowed_users.clear();
            }
            if irc {
                // Channels it joined are allowed chats here, which still
                // answer only listed senders. Names fold as IRC compares them;
                // listed nicknames become the services accounts of that name.
                allowed_chats.extend(messenger.irc_channels);
                allowed_chats = allowed_chats.iter().map(|c| irc_lowercase(c)).collect();
                allowed_users = allowed_users.iter().map(|u| irc_lowercase(u)).collect();
            }
            let valid = |ids: &BTreeSet<String>, users: bool| {
                ids.len() <= MAX_ALLOWED_CHATS
                    && ids.iter().all(|id| match (irc, users) {
                        (false, _) => valid_field(id),
                        (true, true) => valid_irc_nick(id),
                        (true, false) => valid_irc_channel(id) || valid_irc_nick(id),
                    })
            };
            // RustyClaw's own default nickname when it names none.
            let login = irc.then(|| messenger.nick.unwrap_or_else(|| "RustyClaw".to_owned()));
            // A federated service's server, as the origin the gateway would
            // accept; other kinds keep their official endpoint.
            let endpoint = match kind {
                Some(MessengerKind::Irc) => match (messenger.server, messenger.use_tls) {
                    (Some(server), Some(true)) => IrcServer::parse(&format!(
                        "ircs://{server}:{}",
                        messenger.port.unwrap_or(IRCS_PORT)
                    ))
                    .map(|server| Some(server.origin()))
                    .map_err(|_| UnsupportedMessenger::InvalidEndpoint),
                    (Some(_), _) => Err(UnsupportedMessenger::PlainTextIrc),
                    (None, _) => Err(UnsupportedMessenger::InvalidEndpoint),
                },
                Some(MessengerKind::Signal) => Err(UnsupportedMessenger::NoSignalDaemon),
                Some(MessengerKind::Webhook) => match &messenger.webhook_url {
                    Some(WebhookUrl::Origin(origin)) => Ok(Some(origin.clone())),
                    Some(WebhookUrl::Unusable) | None => Err(UnsupportedMessenger::InvalidEndpoint),
                },
                Some(kind) if kind.default_endpoint().is_none() => messenger
                    .homeserver
                    .as_deref()
                    .and_then(|url| a206_core::providers::base_url(url).ok())
                    .filter(|url| url.path() == "/")
                    .map(|url| Some(url.origin().ascii_serialization()))
                    .ok_or(UnsupportedMessenger::InvalidEndpoint),
                _ => Ok(None),
            };
            let unsupported = if kind.is_none() {
                Some(
                    if matches!(
                        messenger.messenger_type.as_str(),
                        "whatsapp" | "imessage" | "google_chat"
                    ) {
                        UnsupportedMessenger::ThroughBeeper
                    } else {
                        UnsupportedMessenger::UnknownKind
                    },
                )
            } else if messenger.config_path.is_some() {
                Some(UnsupportedMessenger::ExternalConfig)
            } else if !valid(&allowed_chats, false) || !valid(&allowed_users, true) {
                Some(UnsupportedMessenger::InvalidChats)
            } else if login.as_deref().is_some_and(|login| !valid_irc_nick(login)) {
                Some(UnsupportedMessenger::InvalidLogin)
            } else {
                endpoint.as_ref().err().copied()
            };
            if let Some(reason) = unsupported {
                action = MessengerAction::Unsupported { reason };
            }
            debug_assert!(valid_name(&name));
            MessengerImport {
                source_name: messenger.name,
                source_kind: messenger.messenger_type,
                name,
                kind,
                endpoint: endpoint.ok().flatten(),
                login: login.filter(|_| unsupported.is_none()),
                format: format.filter(|_| unsupported.is_none()),
                // Ids the gateway would refuse are not repeated in the preview.
                allowed_chats: if unsupported.is_some() {
                    BTreeSet::new()
                } else {
                    allowed_chats
                },
                allowed_users: if unsupported.is_some() {
                    BTreeSet::new()
                } else {
                    allowed_users
                },
                token_left_behind: messenger.token.is_some()
                    || messenger.access_token.is_some()
                    || messenger.password.is_some()
                    || messenger.app_token.is_some()
                    || messenger.app_password.is_some()
                    || webhook,
                action,
            }
        })
        .collect()
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
    match std::fs::symlink_metadata(root) {
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
                source: source.into(),
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
                base_url: safe_endpoint(model.base_url.unwrap_or_default()),
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
        messengers: plan_messengers(settings, config.messengers),
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
    fn refused_endpoint_credentials_never_enter_a_preview() {
        let secret = random();
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.toml"),
            &format!(
                "[model]\nprovider = \"openai\"\nbase_url = \"https://user:{secret}@example.test/v1?token={secret}\"\n"
            ),
        );
        let preview = preview(dir.path(), &Settings::default(), None).unwrap();
        assert!(matches!(
            preview.providers[0].action,
            ProviderAction::Unsupported {
                reason: UnsupportedProvider::InvalidEndpoint
            }
        ));
        assert!(preview.providers[0].base_url.is_empty());
        assert!(!toml::to_string(&preview).unwrap().contains(&secret));
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
        let bot = &preview.messengers[0];
        assert_eq!(
            (bot.name.as_str(), bot.kind),
            ("bot", Some(MessengerKind::Telegram))
        );
        assert_eq!(bot.action, MessengerAction::Add);
        assert!(bot.token_left_behind);
        for category in [
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
    fn messengers_arrive_disabled_without_tokens_and_never_widen_their_chats() {
        let token = random();
        let chat = random();
        let user = random();
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.toml"),
            &format!(
                r#"
[[messengers]]
name = "Team Bot"
messenger_type = "telegram"
token = "{token}"
allowed_chats = ["{chat}"]

[[messengers]]
name = "team bot"
messenger_type = "telegram"
allowed_chats = ["{chat}"]
allowed_users = ["{user}"]

[[messengers]]
name = "{chat} other"
messenger_type = "other-{chat}"
access_token = "{token}"

[[messengers]]
messenger_type = "telegram"
config_path = "{token}.toml"

[[messengers]]
messenger_type = "telegram"
allowed_chats = [""]
"#
            ),
        );
        let mut settings = Settings::default();
        let preview = preview(dir.path(), &settings, None).unwrap();
        let actions: Vec<_> = preview
            .messengers
            .iter()
            .map(|m| (m.name.as_str(), m.action.clone()))
            .collect();
        let unsupported = |reason| MessengerAction::Unsupported { reason };
        assert_eq!(actions[0], ("team-bot", MessengerAction::Add));
        assert_eq!(actions[1], ("team-bot-2", MessengerAction::Add));
        assert_eq!(actions[2].1, unsupported(UnsupportedMessenger::UnknownKind));
        assert_eq!(
            actions[3],
            (
                "telegram",
                unsupported(UnsupportedMessenger::ExternalConfig)
            )
        );
        assert_eq!(
            actions[4],
            (
                "telegram-2",
                unsupported(UnsupportedMessenger::InvalidChats)
            )
        );
        assert!(preview.messengers.iter().all(|m| valid_name(&m.name)));
        assert_eq!(
            preview.messengers[1].allowed_users,
            BTreeSet::from([user.clone()])
        );
        let text = toml::to_string(&preview).unwrap();
        assert!(!text.contains(&token));

        let added = preview.apply_messengers(&mut settings);
        assert_eq!(added, ["team-bot", "team-bot-2"]);
        for config in &settings.messengers {
            assert!(!config.enabled);
            assert!(config.credential.is_none());
        }
        assert_eq!(
            settings.messengers[0].allowed_chats,
            BTreeSet::from([chat.clone()])
        );
        // The source's restriction to some users still holds.
        assert_eq!(
            settings.messengers[1].allowed_chats,
            BTreeSet::from([chat.clone()])
        );
        assert_eq!(settings.messengers[1].allowed_users, BTreeSet::from([user]));
        assert!(settings.messengers[0].allowed_users.is_empty());

        let again = super::preview(dir.path(), &settings, None).unwrap();
        assert_eq!(
            again.messengers[0].action,
            MessengerAction::AlreadyConfigured
        );
        assert_eq!(
            again.messengers[1].action,
            MessengerAction::AlreadyConfigured
        );
        assert!(again.apply_messengers(&mut settings).is_empty());
        assert!(preview.apply_messengers(&mut settings).is_empty());
        assert_eq!(settings.messengers.len(), 2);
    }

    #[test]
    fn matrix_accounts_keep_their_homeserver_and_leave_secrets_behind() {
        let (token, password, room) = (random(), random(), random());
        let host = format!("{}.test", uuid::Uuid::new_v4().simple());
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.toml"),
            &format!(
                r#"
[[messengers]]
name = "home"
messenger_type = "matrix"
homeserver = "https://{host}/"
user_id = "@bot:{host}"
access_token = "{token}"
allowed_chats = ["!{room}:{host}"]

[[messengers]]
name = "password"
messenger_type = "matrix"
homeserver = "https://{host}"
password = "{password}"

[[messengers]]
name = "nowhere"
messenger_type = "matrix"
access_token = "{token}"

[[messengers]]
name = "plain"
messenger_type = "matrix"
homeserver = "http://{host}"

[[messengers]]
name = "path"
messenger_type = "matrix"
homeserver = "https://{host}/matrix"
"#
            ),
        );
        let mut settings = Settings::default();
        let preview = preview(dir.path(), &settings, None).unwrap();
        let origin = format!("https://{host}");
        let [home, password_entry, rest @ ..] = &preview.messengers[..] else {
            panic!("{:?}", preview.messengers)
        };
        assert_eq!(home.action, MessengerAction::Add);
        assert_eq!(home.endpoint.as_deref(), Some(origin.as_str()));
        assert!(home.token_left_behind && password_entry.token_left_behind);
        assert_eq!(password_entry.action, MessengerAction::Add);
        // No homeserver, plain HTTP to a remote host, or a path: nothing is
        // guessed.
        for entry in rest {
            assert_eq!(
                entry.action,
                MessengerAction::Unsupported {
                    reason: UnsupportedMessenger::InvalidEndpoint
                },
                "{}",
                entry.source_name
            );
            assert!(entry.endpoint.is_none());
        }
        let text = toml::to_string(&preview).unwrap();
        assert!(!text.contains(&token) && !text.contains(&password));

        assert_eq!(
            preview.apply_messengers(&mut settings),
            ["home", "password"]
        );
        let home = &settings.messengers[0];
        assert_eq!(home.kind, MessengerKind::Matrix);
        assert_eq!(home.endpoint(), Some(origin.as_str()));
        assert!(!home.enabled && home.credential.is_none());
        assert_eq!(
            home.allowed_chats,
            BTreeSet::from([format!("!{room}:{host}")])
        );
    }

    #[test]
    fn discord_bots_arrive_disabled_on_the_official_api_without_their_token() {
        let (token, channel, user) = (random(), random(), random());
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.toml"),
            &format!(
                r#"
[[messengers]]
name = "guild"
messenger_type = "discord"
enabled = true
token = "{token}"
allowed_chats = ["{channel}"]
allowed_users = ["{user}"]
"#
            ),
        );
        let mut settings = Settings::default();
        let preview = preview(dir.path(), &settings, None).unwrap();
        let [guild] = &preview.messengers[..] else {
            panic!("{:?}", preview.messengers)
        };
        assert_eq!(guild.action, MessengerAction::Add);
        assert_eq!(guild.kind, Some(MessengerKind::Discord));
        assert!(guild.token_left_behind);
        assert!(!toml::to_string(&preview).unwrap().contains(&token));
        assert_eq!(preview.apply_messengers(&mut settings), ["guild"]);
        let guild = &settings.messengers[0];
        assert!(!guild.enabled && guild.credential.is_none());
        assert_eq!(guild.endpoint(), MessengerKind::Discord.default_endpoint());
        assert_eq!(guild.allowed_chats, BTreeSet::from([channel]));
        assert_eq!(guild.allowed_users, BTreeSet::from([user]));
    }

    #[test]
    fn webhooks_arrive_disabled_with_only_their_origin() {
        let (secret, key, host) = (random(), random(), format!("h{}.example", random()));
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.toml"),
            &format!(
                r#"
[[messengers]]
name = "alerts"
messenger_type = "webhook"
webhook_url = "https://{host}/hooks/{secret}"
allowed_chats = ["{key}"]

[[messengers]]
name = "team"
messenger_type = "teams"
webhook_url = "https://teams.{host}/workflows/{secret}"

[[messengers]]
name = "space"
messenger_type = "google_chat"
webhook_url = "https://chat.{host}:8443/v1/spaces/{key}/messages?key={key}&token={secret}"

[[messengers]]
name = "bot"
messenger_type = "teams"
app_id = "{key}"
app_password = "{secret}"
webhook_url = "https://teams.{host}/workflows/{secret}"

[[messengers]]
name = "service"
messenger_type = "google_chat"
credentials_path = "key.json"
webhook_url = "https://chat.{host}/v1/spaces/{key}/messages?token={secret}"

[[messengers]]
name = "plain"
messenger_type = "webhook"
webhook_url = "http://{host}/hooks/{secret}"

[[messengers]]
name = "signed"
messenger_type = "webhook"
webhook_url = "https://{key}:{secret}@{host}/hooks"

[[messengers]]
name = "marked"
messenger_type = "webhook"
webhook_url = "https://{host}/hooks/{secret}#{key}"

[[messengers]]
name = "missing"
messenger_type = "webhook"
"#
            ),
        );
        let mut settings = Settings::default();
        let preview = preview(dir.path(), &settings, None).unwrap();
        for encoded in [
            toml::to_string(&preview).unwrap().into_bytes(),
            rmp_serde::to_vec_named(&preview).unwrap(),
        ] {
            assert!(!String::from_utf8_lossy(&encoded).contains(&secret));
        }
        let unsupported = |reason| MessengerAction::Unsupported { reason };
        let found: Vec<_> = preview
            .messengers
            .iter()
            .map(|m| {
                (
                    m.name.as_str(),
                    m.kind,
                    m.format,
                    m.endpoint.clone(),
                    m.action.clone(),
                )
            })
            .collect();
        let webhook = Some(MessengerKind::Webhook);
        assert_eq!(
            found,
            [
                (
                    "alerts",
                    webhook,
                    Some(WebhookFormat::Text),
                    Some(format!("https://{host}")),
                    MessengerAction::Add
                ),
                (
                    "team",
                    webhook,
                    Some(WebhookFormat::Teams),
                    Some(format!("https://teams.{host}")),
                    MessengerAction::Add
                ),
                (
                    "space",
                    webhook,
                    Some(WebhookFormat::Text),
                    Some(format!("https://chat.{host}:8443")),
                    MessengerAction::Add
                ),
                (
                    "bot",
                    None,
                    None,
                    None,
                    unsupported(UnsupportedMessenger::UnknownKind)
                ),
                (
                    "service",
                    None,
                    None,
                    None,
                    unsupported(UnsupportedMessenger::ThroughBeeper)
                ),
                (
                    "plain",
                    webhook,
                    None,
                    None,
                    unsupported(UnsupportedMessenger::InvalidEndpoint)
                ),
                (
                    "signed",
                    webhook,
                    None,
                    None,
                    unsupported(UnsupportedMessenger::InvalidEndpoint)
                ),
                (
                    "marked",
                    webhook,
                    None,
                    None,
                    unsupported(UnsupportedMessenger::InvalidEndpoint)
                ),
                (
                    "missing",
                    webhook,
                    None,
                    None,
                    unsupported(UnsupportedMessenger::InvalidEndpoint)
                ),
            ]
        );
        assert!(preview.messengers[..8].iter().all(|m| m.token_left_behind));
        assert!(!preview.messengers[8].token_left_behind);
        assert!(
            preview
                .messengers
                .iter()
                .all(|m| m.allowed_chats.is_empty())
        );
        assert_eq!(
            preview.apply_messengers(&mut settings),
            ["alerts", "team", "space"]
        );
        for messenger in &settings.messengers {
            assert!(!messenger.enabled && messenger.credential.is_none());
            assert!(messenger.allowed_chats.is_empty());
        }
        assert_eq!(settings.messengers[1].format, WebhookFormat::Teams);
        assert_eq!(
            settings.messengers[2].endpoint.as_deref(),
            Some(format!("https://chat.{host}:8443").as_str())
        );
    }

    #[test]
    fn slack_apps_arrive_disabled_without_either_token() {
        let (token, app_token, channel) = (random(), random(), random());
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.toml"),
            &format!(
                r#"
[[messengers]]
name = "workspace"
messenger_type = "slack"
enabled = true
token = "{token}"
app_token = "{app_token}"
allowed_chats = ["{channel}"]
"#
            ),
        );
        let mut settings = Settings::default();
        let preview = preview(dir.path(), &settings, None).unwrap();
        let [workspace] = &preview.messengers[..] else {
            panic!("{:?}", preview.messengers)
        };
        assert_eq!(workspace.kind, Some(MessengerKind::Slack));
        assert!(workspace.token_left_behind);
        let text = toml::to_string(&preview).unwrap();
        assert!(!text.contains(&token) && !text.contains(&app_token));
        assert_eq!(preview.apply_messengers(&mut settings), ["workspace"]);
        let workspace = &settings.messengers[0];
        assert!(!workspace.enabled);
        assert!(workspace.credential.is_none() && workspace.app_credential.is_none());
        assert_eq!(workspace.allowed_chats, BTreeSet::from([channel]));
    }

    #[test]
    fn irc_accounts_arrive_over_tls_with_folded_names_and_no_password() {
        let (password, host) = (random(), format!("{}.test", uuid::Uuid::new_v4().simple()));
        let nick = format!("bot{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.toml"),
            &format!(
                r##"
[[messengers]]
name = "network"
messenger_type = "irc"
server = "{host}"
nick = "{nick}"
password = "{password}"
use_tls = true
irc_channels = ["#Rust"]
allowed_users = ["Friend"]

[[messengers]]
name = "plain"
messenger_type = "irc"
server = "{host}"
port = 6667

[[messengers]]
name = "odd"
messenger_type = "irc"
server = "{host}"
use_tls = true
nick = "not a nick"
"##
            ),
        );
        let mut settings = Settings::default();
        let preview = preview(dir.path(), &settings, None).unwrap();
        let [network, plain, odd] = &preview.messengers[..] else {
            panic!("{:?}", preview.messengers)
        };
        assert_eq!(network.kind, Some(MessengerKind::Irc));
        assert!(network.token_left_behind);
        assert!(!toml::to_string(&preview).unwrap().contains(&password));
        assert_eq!(
            plain.action,
            MessengerAction::Unsupported {
                reason: UnsupportedMessenger::PlainTextIrc
            }
        );
        assert_eq!(
            odd.action,
            MessengerAction::Unsupported {
                reason: UnsupportedMessenger::InvalidLogin
            }
        );
        assert_eq!(preview.apply_messengers(&mut settings), ["network"]);
        let network = &settings.messengers[0];
        assert!(!network.enabled && network.credential.is_none());
        assert_eq!(
            network.endpoint.as_deref(),
            Some(format!("ircs://{host}:6697").as_str())
        );
        assert_eq!(network.login.as_deref(), Some(nick.as_str()));
        assert_eq!(network.allowed_chats, BTreeSet::from(["#rust".to_owned()]));
        assert_eq!(network.allowed_users, BTreeSet::from(["friend".to_owned()]));
    }

    #[test]
    fn networks_reached_through_beeper_say_so_and_others_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = String::new();
        for kind in ["whatsapp", "imessage", "google_chat", "teams"] {
            config.push_str(&format!(
                "[[messengers]]\nname = \"{kind}\"\nmessenger_type = \"{kind}\"\n"
            ));
        }
        write(&dir.path().join("config.toml"), &config);
        let mut settings = Settings::default();
        let preview = preview(dir.path(), &settings, None).unwrap();
        let reasons: Vec<_> = preview
            .messengers
            .iter()
            .map(|m| (m.source_kind.as_str(), m.kind, m.action.clone()))
            .collect();
        let unsupported = |reason| MessengerAction::Unsupported { reason };
        assert_eq!(
            reasons,
            [
                (
                    "whatsapp",
                    None,
                    unsupported(UnsupportedMessenger::ThroughBeeper)
                ),
                (
                    "imessage",
                    None,
                    unsupported(UnsupportedMessenger::ThroughBeeper)
                ),
                (
                    "google_chat",
                    None,
                    unsupported(UnsupportedMessenger::ThroughBeeper)
                ),
                (
                    "teams",
                    None,
                    unsupported(UnsupportedMessenger::UnknownKind)
                ),
            ]
        );
        assert!(preview.apply_messengers(&mut settings).is_empty());
    }

    #[test]
    fn signal_accounts_are_reported_because_no_daemon_is_named() {
        let phone = format!("+1{}", &uuid::Uuid::new_v4().as_u128().to_string()[..10]);
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("config.toml"),
            &format!(
                r##"
[[messengers]]
name = "phone"
messenger_type = "signal"
phone = "{phone}"
allowed_users = ["{phone}"]
"##
            ),
        );
        let mut settings = Settings::default();
        let preview = preview(dir.path(), &settings, None).unwrap();
        let [phone_entry] = &preview.messengers[..] else {
            panic!("{:?}", preview.messengers)
        };
        assert_eq!(phone_entry.kind, Some(MessengerKind::Signal));
        assert_eq!(
            phone_entry.action,
            MessengerAction::Unsupported {
                reason: UnsupportedMessenger::NoSignalDaemon
            }
        );
        assert!(phone_entry.allowed_users.is_empty() && phone_entry.login.is_none());
        assert!(preview.apply_messengers(&mut settings).is_empty());
        assert!(settings.messengers.is_empty());
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
            messengers_added: vec![random()],
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
