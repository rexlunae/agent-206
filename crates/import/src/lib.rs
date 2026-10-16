//! Reviewed migration from other tools (docs/imports.md). An adapter reads a
//! source installation into a [`Preview`] without changing anything; applying
//! a preview only ever adds to Agent 206 and records a [`Receipt`].
//!
//! Readers treat every source file as untrusted data: bounded size, regular
//! files only (no symlinks followed), and nothing is executed or activated.
//! Credentials never pass through here; the vault broker copies an encrypted
//! vault itself (`a206_broker::Request::ImportVault`).
#![forbid(unsafe_code)]
pub mod rustyclaw;

use a206_core::{Id, ProviderConfig, Settings};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Version of the preview and receipt shapes below.
pub const PREVIEW_VERSION: u32 = 1;
/// No configuration file we read is anywhere near this large.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{} does not exist", path.display())]
    NotFound { path: PathBuf },
    #[error("{} is not a directory", path.display())]
    NotDirectory { path: PathBuf },
    #[error("{} is a symbolic link or not a regular file; imports read only regular files", path.display())]
    NotRegular { path: PathBuf },
    #[error("{} is larger than {limit} bytes", path.display())]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("could not read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not a configuration this importer understands", path.display())]
    Config {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("could not write the import receipt {}", path.display())]
    Receipt {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not encode the import receipt")]
    EncodeReceipt(#[source] toml::ser::Error),
}
type Result<T, E = ImportError> = std::result::Result<T, E>;

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
    pub api: a206_core::ProviderApi,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InactiveCategory {
    /// Messenger accounts (and any tokens in them) stay behind.
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
}

/// The account in `settings` with this API and endpoint, ignoring a
/// trailing slash, so a repeated import adds nothing.
pub fn existing(settings: &Settings, api: a206_core::ProviderApi, base_url: &str) -> Option<Id> {
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
    pub vault_imported: bool,
}

/// Write `receipt` under `<root>/imports/`, owner-only, and return its path.
pub fn write_receipt(root: &Path, receipt: &Receipt) -> Result<PathBuf> {
    use std::io::Write;
    let dir = root.join("imports");
    let path = dir.join(format!("{}.toml", receipt.id));
    let failed = |source| ImportError::Receipt {
        path: path.clone(),
        source,
    };
    std::fs::create_dir_all(&dir).map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(failed)?;
    }
    let text = toml::to_string_pretty(receipt).map_err(ImportError::EncodeReceipt)?;
    // NamedTempFile is owner-only before it is renamed into place.
    let mut file = tempfile::NamedTempFile::new_in(&dir).map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.as_file().sync_all().map_err(failed)?;
    file.persist_noclobber(&path)
        .map_err(|error| failed(error.error))?;
    Ok(path)
}

/// Read a regular file of at most `limit` bytes, or `None` if it is absent.
pub(crate) fn read_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let read = |source| ImportError::Read {
        path: path.to_owned(),
        source,
    };
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(read(error)),
    };
    if !metadata.is_file() {
        return Err(ImportError::NotRegular {
            path: path.to_owned(),
        });
    }
    let too_large = || ImportError::TooLarge {
        path: path.to_owned(),
        limit,
    };
    if metadata.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(read)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(Some(bytes))
}

/// Size of a regular file, `None` if absent; symlinks are refused.
pub(crate) fn regular_len(path: &Path) -> Result<Option<u64>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        Ok(_) => Err(ImportError::NotRegular {
            path: path.to_owned(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ImportError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
