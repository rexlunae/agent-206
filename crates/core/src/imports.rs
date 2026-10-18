//! Reviewed import data shared by clients and the gateway. No source I/O.
use crate::{Id, MessengerConfig, MessengerKind, ProviderConfig, Settings};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};
pub const PREVIEW_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub id: Id,
    pub settings_revision: u64,
    pub preview: Preview,
}
/// Indices in the reviewed lists, not client-supplied account configurations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub providers: BTreeSet<usize>,
    pub messengers: BTreeSet<usize>,
}
impl Selection {
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty() && self.messengers.is_empty()
    }
    pub fn valid_for(&self, preview: &Preview) -> bool {
        !self.is_empty()
            && self.providers.iter().all(|&index| {
                preview
                    .providers
                    .get(index)
                    .is_some_and(|item| item.action == ProviderAction::Add)
            })
            && self.messengers.iter().all(|&index| {
                preview
                    .messengers
                    .get(index)
                    .is_some_and(|item| item.action == MessengerAction::Add && item.kind.is_some())
            })
    }
    pub fn selected(&self, preview: &Preview) -> Preview {
        let mut preview = preview.clone();
        preview.providers = preview
            .providers
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| self.providers.contains(&index).then_some(item))
            .collect();
        preview.messengers = preview
            .messengers
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| self.messengers.contains(&index).then_some(item))
            .collect();
        preview
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub receipt: Receipt,
    /// Settings were saved; a receipt-write failure is an explicit partial result.
    pub receipt_error: Option<crate::ErrorReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    RustyClaw,
}

/// What would change, and what will not, if this preview were applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    pub version: u32,
    pub tool: Tool,
    pub source: PathBuf,
    /// SHA-256 of the configuration file as read, for the receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<VaultSource>,
    #[serde(default)]
    pub providers: Vec<ProviderImport>,
    #[serde(default)]
    pub messengers: Vec<MessengerImport>,
    /// Present in the source and deliberately left inactive.
    #[serde(default)]
    pub inactive: Vec<Inactive>,
    /// Present in the source but not importable yet.
    #[serde(default)]
    pub unsupported: Vec<Unsupported>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultSource {
    pub path: PathBuf,
    pub bytes: u64,
    pub protection: VaultProtection,
    pub plan: VaultPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultProtection {
    Password,
    /// Encrypted with an auto-generated `secrets.key` instead of a password.
    KeyFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultPlan {
    /// The broker can copy it in after verifying its password.
    Import,
    /// This profile already has a vault; vaults are never merged or replaced.
    TargetExists,
    /// Key-file vaults need the key-file reader, which is not built yet.
    KeyFileUnsupported,
    /// Whether this profile has a vault could not be checked.
    TargetUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderImport {
    /// The provider's identifier in the source tool.
    pub source_id: String,
    /// The Agent 206 preset it maps to, when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub label: String,
    pub base_url: String,
    pub api: crate::ProviderApi,
    /// Vault entry name the source used for this provider's key. The account
    /// refers to it; the broker refuses to use it until the user types and
    /// binds it (`agent206 vault adopt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Historical default model, kept as provenance only. It is never bound
    /// to a thread; models come from the provider's API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reference: Option<String>,
    pub action: ProviderAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ProviderAction {
    Add,
    /// An account with the same API and endpoint already exists.
    AlreadyConfigured {
        existing: Id,
    },
    Unsupported {
        reason: UnsupportedProvider,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedProvider {
    /// No Agent 206 preset corresponds to the source provider.
    UnknownProvider,
    /// Signs in with an OAuth device flow, not yet supported.
    DeviceFlow,
    /// Neither the source nor the preset names an endpoint.
    NoEndpoint,
    /// The endpoint is not HTTPS or loopback HTTP, or carries credentials,
    /// a query or a fragment; the gateway would refuse to call it.
    InvalidEndpoint,
}

/// A messenger account from the source. Imported entries are disabled, have
/// no credential (tokens stay behind) and allow only the chats the source
/// allowed, or none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessengerImport {
    /// The account's name in the source tool, as display text.
    pub source_name: String,
    /// The service as the source names it.
    pub source_kind: String,
    /// The Agent 206 name it is (or would be) saved under.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MessengerKind>,
    /// The server the source connected to, when the service is federated
    /// (a Matrix homeserver). An origin, carried over as the endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The nickname an IRC account signed in with, carried over as the login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// Chats the source allowed; carried over as this entry's allow-list.
    #[serde(default)]
    pub allowed_chats: BTreeSet<String>,
    /// Senders the source allowed; carried over, so a restriction to some
    /// users still holds.
    #[serde(default)]
    pub allowed_users: BTreeSet<String>,
    /// The source kept a token or password in its configuration in plain
    /// text. It is not read; the user stores a token in the vault.
    #[serde(default)]
    pub token_left_behind: bool,
    pub action: MessengerAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MessengerAction {
    Add,
    /// A messenger of the same kind already has this name.
    AlreadyConfigured,
    Unsupported {
        reason: UnsupportedMessenger,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedMessenger {
    /// Agent 206 has no adapter for this service yet.
    UnknownKind,
    /// The account's settings live in a separate file, which is not read.
    ExternalConfig,
    /// More allowed chats or users than Agent 206 accepts, or ids it would
    /// refuse.
    InvalidChats,
    /// The service needs a server URL and the source names none, or one
    /// that is not an HTTPS (or local HTTP) origin.
    InvalidEndpoint,
    /// IRC without TLS: the account password would cross the network in
    /// the clear, so the entry is not carried over.
    PlainTextIrc,
    /// The IRC nickname is not one Agent 206 can sign in with.
    InvalidLogin,
    /// Signal: the source ran signal-cli itself, while Agent 206 connects
    /// to a signal-cli daemon the user runs, which the source does not name.
    NoSignalDaemon,
    /// A network Agent 206 reaches through Beeper Desktop rather than on its
    /// own (WhatsApp, iMessage, Google Chat); the source's account does not
    /// carry over.
    ThroughBeeper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InactiveCategory {
    /// Messenger accounts. Previews now list them under `messengers`; kept
    /// so earlier previews still read.
    Messengers,
    McpServers,
    /// Per-tool allow/ask/deny overrides. Agent 206 has no per-tool model
    /// yet, so a deny cannot be carried over; nothing is granted instead.
    ToolPermissions,
    DisabledToolGroups,
    Services,
    Engines,
    Sandbox,
    /// A registry token stored in plain text in the configuration.
    PlaintextToken,
    Skills,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inactive {
    pub category: InactiveCategory,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unsupported {
    /// SOUL.md, AGENTS.md and other workspace instruction files.
    WorkspaceInstructions,
    /// Conversation threads and session history.
    Conversations,
    Memory,
    /// The messenger system prompt.
    SystemPrompt,
}

impl ProviderImport {
    /// The exact origin the account's credential must be bound to.
    pub fn origin(&self) -> Option<String> {
        url::Url::parse(&self.base_url)
            .ok()
            .map(|url| url.origin().ascii_serialization())
            .filter(|origin| origin != "null")
    }
}

impl Preview {
    /// Providers this preview would add.
    pub fn additions(&self) -> impl Iterator<Item = &ProviderImport> {
        self.providers
            .iter()
            .filter(|p| p.action == ProviderAction::Add)
    }

    /// Add this preview's new provider accounts to `settings`, skipping any
    /// that `settings` gained meanwhile. Returns the accounts added.
    pub fn apply_providers(&self, settings: &mut Settings) -> Vec<Id> {
        let mut added = vec![];
        for import in self.additions() {
            if existing(settings, import.api, &import.base_url).is_some() {
                continue;
            }
            let config = ProviderConfig {
                id: Id::new_v4(),
                label: import.label.clone(),
                base_url: import.base_url.clone(),
                credential: import.credential.clone(),
                api: import.api,
            };
            added.push(config.id);
            settings.providers.push(config);
        }
        added
    }

    /// Add this preview's new messengers to `settings`, disabled and without
    /// a credential, skipping names `settings` gained meanwhile. Returns the
    /// names added.
    pub fn apply_messengers(&self, settings: &mut Settings) -> Vec<String> {
        let mut added = vec![];
        for import in &self.messengers {
            let (MessengerAction::Add, Some(kind)) = (&import.action, import.kind) else {
                continue;
            };
            if settings.messengers.iter().any(|m| m.name == import.name) {
                continue;
            }
            let mut config = MessengerConfig::new(import.name.clone(), kind);
            config.endpoint = import.endpoint.clone();
            config.login = import.login.clone();
            config.allowed_chats = import.allowed_chats.clone();
            config.allowed_users = import.allowed_users.clone();
            added.push(config.name.clone());
            settings.messengers.push(config);
        }
        added
    }
}

/// The account in `settings` with this API and endpoint, ignoring a
/// trailing slash, so a repeated import adds nothing.
pub fn existing(settings: &Settings, api: crate::ProviderApi, base_url: &str) -> Option<Id> {
    let key = base_url.trim_end_matches('/');
    settings
        .providers
        .iter()
        .find(|p| p.api == api && p.base_url.trim_end_matches('/') == key)
        .map(|p| p.id)
}

/// A durable record of one applied import, enough to see what changed and
/// undo it by hand: the accounts added and whether the vault was copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub version: u32,
    pub id: Id,
    pub tool: Tool,
    pub source: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sha256: Option<String>,
    pub applied_at: u64,
    pub providers_added: Vec<Id>,
    #[serde(default)]
    pub messengers_added: Vec<String>,
    pub vault_imported: bool,
}
