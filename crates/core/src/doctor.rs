//! Health check results shared by `agent206 doctor`, the native clients and
//! the web bridge. The checks themselves run in `a206_runtime::doctor`;
//! findings are typed and serialize as such, and only display code renders them.
use crate::ErrorReport;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Ok,
    /// Works, but weaker or less complete than it should be.
    Warning,
    /// Not known from here; never reported as healthy.
    Unknown,
    /// Something the user must fix before this part works.
    Problem,
}

/// What a check found. Each variant carries the facts it is about, so
/// clients can match on it; [`std::fmt::Display`] is its user-facing text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "finding", rename_all = "snake_case")]
pub enum Finding {
    ProfileMissing {
        path: PathBuf,
    },
    ProfileOpen {
        path: PathBuf,
        mode: u32,
    },
    ProfileReady {
        path: PathBuf,
    },
    GatewayNotConfigured,
    GatewaySetupInvalid,
    GatewayRunning {
        target: String,
        version: String,
    },
    GatewayStopped {
        target: String,
    },
    GatewayOffline {
        target: String,
    },
    /// The vault broker runs on the gateway host; check it there.
    VaultRemote {
        target: String,
    },
    /// This platform has no vault broker yet.
    VaultUnsupported,
    VaultMissing,
    VaultBrokerStopped,
    VaultBrokerUnreachable,
    VaultFileOpen {
        mode: u32,
    },
    VaultDirectoryOpen {
        mode: u32,
    },
    VaultLocked,
    VaultUnlocked,
    TotpEnrolled,
    TotpNotEnrolled,
    TotpUnknown,
    PairingActive,
    PairingMissing,
    PairingUnknown,
    WorkspaceUnknown,
    NoProviders,
    ProviderReady {
        label: String,
        models: usize,
    },
    ProviderCredentialMissing {
        label: String,
        credential: String,
    },
    ProviderCredentialUntyped {
        label: String,
        credential: String,
    },
    ProviderCredentialDisabled {
        label: String,
        credential: String,
    },
    ProviderCredentialKind {
        label: String,
        credential: String,
    },
    ProviderCredentialUnbound {
        label: String,
        credential: String,
        origin: String,
    },
    ProviderCredentialUnknown {
        label: String,
        credential: String,
    },
    ProviderEndpointInvalid {
        label: String,
    },
    ProviderNoCatalog {
        label: String,
    },
    ProviderCatalogStale {
        label: String,
        models: usize,
    },
}

impl Finding {
    pub fn health(&self) -> Health {
        use Finding::*;
        match self {
            ProfileReady { .. }
            | GatewayRunning { .. }
            | VaultUnlocked
            | TotpEnrolled
            | PairingActive
            | ProviderReady { .. } => Health::Ok,
            ProfileOpen { .. }
            | GatewayStopped { .. }
            | VaultLocked
            | VaultBrokerStopped
            | TotpNotEnrolled
            | ProviderCatalogStale { .. }
            | ProviderNoCatalog { .. } => Health::Warning,
            VaultRemote { .. }
            | VaultUnsupported
            | TotpUnknown
            | PairingUnknown
            | WorkspaceUnknown
            | ProviderCredentialUnknown { .. } => Health::Unknown,
            ProfileMissing { .. }
            | GatewayNotConfigured
            | GatewaySetupInvalid
            | GatewayOffline { .. }
            | VaultMissing
            | VaultBrokerUnreachable
            | VaultFileOpen { .. }
            | VaultDirectoryOpen { .. }
            | PairingMissing
            | NoProviders
            | ProviderCredentialMissing { .. }
            | ProviderCredentialUntyped { .. }
            | ProviderCredentialDisabled { .. }
            | ProviderCredentialKind { .. }
            | ProviderCredentialUnbound { .. }
            | ProviderEndpointInvalid { .. } => Health::Problem,
        }
    }

    /// The command that addresses this finding, when one exists.
    pub fn recovery(&self) -> Option<String> {
        use Finding::*;
        Some(match self {
            ProfileMissing { .. } | GatewayNotConfigured => {
                "agent206 setup --local (or --ssh ssh://user@host)".into()
            }
            ProfileOpen { path, .. } => format!("chmod 700 {}", path.display()),
            GatewaySetupInvalid => "agent206 gateway setup --local (or --ssh ...)".into(),
            GatewayStopped { .. } => "agent206 gateway start".into(),
            GatewayOffline { .. } => "agent206 gateway status".into(),
            VaultRemote { target } => format!("agent206 doctor (run on {target})"),
            VaultMissing => "agent206 setup, or agent206 import rustyclaw --apply".into(),
            VaultBrokerStopped | VaultLocked => "agent206 vault unlock".into(),
            VaultBrokerUnreachable => "agent206 vault stop, then agent206 vault unlock".into(),
            VaultFileOpen { .. } => "chmod 600 the vault file".into(),
            VaultDirectoryOpen { .. } => "chmod 700 the vault directory".into(),
            TotpNotEnrolled => "agent206 vault totp enroll".into(),
            PairingMissing => "agent206 vault pair".into(),
            NoProviders => "agent206 providers presets, then agent206 providers add".into(),
            ProviderCredentialMissing { credential, .. } => {
                format!("agent206 vault add {credential} --destination <provider origin>")
            }
            ProviderCredentialUntyped { credential, .. } => {
                format!("agent206 vault adopt {credential} --destination <provider origin>")
            }
            ProviderCredentialDisabled { credential, .. } => {
                format!("agent206 vault enable {credential}")
            }
            ProviderCredentialUnbound {
                credential, origin, ..
            } => format!("agent206 vault bind {credential} {origin}"),
            ProviderCredentialUnknown { .. } => "agent206 vault unlock".into(),
            ProviderNoCatalog { .. } | ProviderCatalogStale { .. } => {
                "agent206 refresh-models <provider id>".into()
            }
            _ => return None,
        })
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use Finding::*;
        match self {
            ProfileMissing { path } => write!(f, "profile {} does not exist yet", path.display()),
            ProfileOpen { path, mode } => write!(
                f,
                "profile {} is readable by other users (mode {mode:o})",
                path.display()
            ),
            ProfileReady { path } => write!(f, "profile {} is private", path.display()),
            GatewayNotConfigured => f.write_str("no gateway is configured for this profile"),
            GatewaySetupInvalid => f.write_str("the saved gateway setup cannot be used"),
            GatewayRunning { target, version } => {
                write!(f, "gateway {target} is running version {version}")
            }
            GatewayStopped { target } => write!(f, "gateway {target} is stopped"),
            GatewayOffline { target } => write!(f, "gateway {target} cannot be reached"),
            VaultRemote { target } => write!(
                f,
                "the vault broker runs on {target}; check it on that host"
            ),
            VaultUnsupported => f.write_str("vault controls are not available on this platform"),
            VaultMissing => f.write_str("no vault has been created"),
            VaultBrokerStopped => f.write_str("the vault broker is not running"),
            VaultBrokerUnreachable => f.write_str("the vault broker did not answer"),
            VaultFileOpen { mode } => {
                write!(
                    f,
                    "the vault file is readable by other users (mode {mode:o})"
                )
            }
            VaultDirectoryOpen { mode } => write!(
                f,
                "the vault directory is accessible to other users (mode {mode:o})"
            ),
            VaultLocked => f.write_str("the vault is locked"),
            VaultUnlocked => f.write_str("the vault is unlocked"),
            TotpEnrolled => f.write_str("two-factor authentication is enrolled"),
            TotpNotEnrolled => f.write_str("two-factor authentication is not enrolled"),
            TotpUnknown => {
                f.write_str("two-factor enrollment is unknown while the vault is locked")
            }
            PairingActive => f.write_str("the gateway is paired with the unlocked vault"),
            PairingMissing => {
                f.write_str("the gateway is not paired with the vault, so credentials are unusable")
            }
            PairingUnknown => f.write_str("gateway pairing is unknown"),
            WorkspaceUnknown => {
                f.write_str("provider accounts are unknown until the gateway is running")
            }
            NoProviders => f.write_str("no provider account is configured"),
            ProviderReady { label, models } => {
                write!(f, "{label}: ready, {models} models from its API")
            }
            ProviderCredentialMissing { label, credential } => {
                write!(f, "{label}: credential {credential} is not in the vault")
            }
            ProviderCredentialUntyped { label, credential } => write!(
                f,
                "{label}: credential {credential} is an untyped entry from an older vault"
            ),
            ProviderCredentialDisabled { label, credential } => {
                write!(f, "{label}: credential {credential} is disabled")
            }
            ProviderCredentialKind { label, credential } => write!(
                f,
                "{label}: credential {credential} is not an API key or token"
            ),
            ProviderCredentialUnbound {
                label,
                credential,
                origin,
            } => write!(
                f,
                "{label}: credential {credential} may not be sent to {origin}"
            ),
            ProviderCredentialUnknown { label, credential } => write!(
                f,
                "{label}: credential {credential} cannot be checked while the vault is locked"
            ),
            ProviderEndpointInvalid { label } => write!(f, "{label}: endpoint is not a valid URL"),
            ProviderNoCatalog { label } => write!(f, "{label}: no model catalog from its API yet"),
            ProviderCatalogStale { label, models } => {
                write!(f, "{label}: model catalog ({models} models) has expired")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub health: Health,
    #[serde(flatten)]
    pub finding: Finding,
    /// The failure behind the finding, kept structured for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
    /// Repairs these checks offer; each is made only after the user confirms
    /// it. Empty from clients older than repairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repairs: Vec<Repair>,
}
impl DoctorReport {
    /// The worst health of any check; `Ok` for an empty report.
    pub fn health(&self) -> Health {
        self.checks
            .iter()
            .map(|check| check.health)
            .max()
            .unwrap_or(Health::Ok)
    }
}

/// A change `agent206 doctor --repair` can make after the user confirms it.
/// Repairs only remove permission bits or ask the gateway to fetch a model
/// list it would fetch anyway; none creates, deletes, unlocks or grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "repair", rename_all = "snake_case")]
pub enum Repair {
    /// Remove group and other permissions from the profile directory.
    RestrictProfile { path: PathBuf, mode: u32 },
    /// Remove group and other permissions from the vault file.
    RestrictVault { path: PathBuf, mode: u32 },
    /// Remove group and other permissions from the vault directory.
    RestrictVaultDirectory { path: PathBuf, mode: u32 },
    /// Fetch this provider's model list from its API through the gateway.
    RefreshCatalog { provider: crate::Id, label: String },
}

impl std::fmt::Display for Repair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RestrictProfile { path, mode } => write!(
                f,
                "make profile {} private (mode {mode:o} to {:o})",
                path.display(),
                mode & !0o077
            ),
            Self::RestrictVault { path, mode } => write!(
                f,
                "make vault file {} private (mode {mode:o} to {:o})",
                path.display(),
                mode & !0o077
            ),
            Self::RestrictVaultDirectory { path, mode } => write!(
                f,
                "make vault directory {} private (mode {mode:o} to {:o})",
                path.display(),
                mode & !0o077
            ),
            Self::RefreshCatalog { label, .. } => {
                write!(f, "{label}: fetch the model list from its API")
            }
        }
    }
}
