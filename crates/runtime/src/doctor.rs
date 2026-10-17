//! Read-only health checks shared by `agent206 doctor`, `agent206 setup` and
//! the desktop and terminal clients: the profile directory, the configured
//! gateway, the vault broker and each provider account.
//!
//! [`gather`] collects facts without starting a gateway or broker, unlocking
//! the vault or writing a file; [`evaluate`] turns those facts into findings.
//! Findings are typed and serialize as such; only display code renders them.
use a206_broker::BrokerStatus;
use a206_core::{
    ErrorReport, GatewayPhase, GatewayReport, GatewayTarget, ProviderConfig, Reply, Snapshot,
};
use a206_vault::{CredentialKind, CredentialMeta};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
            VaultFileOpen { .. } => "chmod 600 the vault file and 700 its directory".into(),
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

/// What the broker said, or why it could not be asked.
#[derive(Debug, Clone, PartialEq)]
pub enum BrokerFacts {
    /// The gateway is remote; its broker lives on that host.
    Remote,
    Unsupported,
    /// No broker answers; `vault_mode` is the vault file's mode, if one exists.
    Stopped {
        vault_mode: Option<u32>,
    },
    Unreachable(ErrorReport),
    Running {
        status: BrokerStatus,
        vault_mode: Option<u32>,
        /// Present when the vault is unlocked.
        credentials: Option<Credentials>,
    },
}

/// Credential names and metadata, never values.
#[derive(Debug, Clone, PartialEq)]
pub struct Credentials {
    pub typed: Vec<(String, CredentialMeta)>,
    /// Untyped entries from an older vault.
    pub legacy: Vec<String>,
}

/// Everything [`evaluate`] needs. Built by [`gather`]; tests build it directly.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub profile: PathBuf,
    /// Permission bits, or `None` when the directory does not exist.
    pub profile_mode: Option<u32>,
    pub target: Result<Option<GatewayTarget>, ErrorReport>,
    pub gateway: Option<GatewayReport>,
    /// The workspace, when the gateway is running and answered.
    pub snapshot: Option<Result<Box<Snapshot>, ErrorReport>>,
    pub broker: BrokerFacts,
    pub now: u64,
}

fn check(finding: Finding) -> Check {
    Check {
        health: finding.health(),
        finding,
        error: None,
    }
}
/// A check whose facts could not be read failed, whatever it would otherwise
/// have found, so it always counts as a problem.
fn failed(finding: Finding, error: ErrorReport) -> Check {
    Check {
        health: Health::Problem,
        error: Some(error),
        finding,
    }
}

/// Owner-only means no group or other permission bits.
fn open(mode: u32) -> bool {
    mode & 0o077 != 0
}

pub fn evaluate(facts: &Facts) -> DoctorReport {
    let mut checks = vec![match facts.profile_mode {
        None => check(Finding::ProfileMissing {
            path: facts.profile.clone(),
        }),
        Some(mode) if open(mode) => check(Finding::ProfileOpen {
            path: facts.profile.clone(),
            mode,
        }),
        Some(_) => check(Finding::ProfileReady {
            path: facts.profile.clone(),
        }),
    }];
    let target = match &facts.target {
        Ok(Some(target)) => Some(target),
        Ok(None) => {
            checks.push(check(Finding::GatewayNotConfigured));
            None
        }
        Err(error) => {
            checks.push(failed(Finding::GatewaySetupInvalid, error.clone()));
            None
        }
    };
    let gateway = facts.gateway.as_ref().filter(|_| target.is_some());
    if let Some(report) = gateway {
        let target = report.target.clone();
        let finding = match (report.phase, &report.info) {
            (GatewayPhase::Running, Some(info)) => Finding::GatewayRunning {
                target,
                version: info.version.clone(),
            },
            (GatewayPhase::Running, None) | (GatewayPhase::Offline, _) => {
                Finding::GatewayOffline { target }
            }
            (GatewayPhase::Stopped, _) => Finding::GatewayStopped { target },
        };
        checks.push(match report.failure_report() {
            Some(error) => failed(finding, error),
            None => check(finding),
        });
    }
    let running = gateway.is_some_and(|r| r.phase == GatewayPhase::Running && r.info.is_some());
    vault_checks(facts, running, &mut checks);
    match &facts.snapshot {
        Some(Ok(snapshot)) => provider_checks(facts, snapshot, &mut checks),
        Some(Err(error)) => checks.push(failed(Finding::WorkspaceUnknown, error.clone())),
        None if target.is_some() => checks.push(check(Finding::WorkspaceUnknown)),
        None => {}
    }
    DoctorReport { checks }
}

fn vault_checks(facts: &Facts, gateway_running: bool, checks: &mut Vec<Check>) {
    let vault_mode = match &facts.broker {
        BrokerFacts::Remote => {
            if let Some(report) = &facts.gateway {
                checks.push(check(Finding::VaultRemote {
                    target: report.target.clone(),
                }));
            }
            return;
        }
        BrokerFacts::Unsupported => {
            checks.push(check(Finding::VaultUnsupported));
            return;
        }
        BrokerFacts::Unreachable(error) => {
            checks.push(failed(Finding::VaultBrokerUnreachable, error.clone()));
            return;
        }
        BrokerFacts::Stopped { vault_mode } => {
            checks.push(check(if vault_mode.is_some() {
                Finding::VaultBrokerStopped
            } else {
                Finding::VaultMissing
            }));
            *vault_mode
        }
        BrokerFacts::Running {
            status, vault_mode, ..
        } => {
            checks.push(check(match (status.exists, status.unlocked) {
                (false, _) => Finding::VaultMissing,
                (true, false) => Finding::VaultLocked,
                (true, true) => Finding::VaultUnlocked,
            }));
            if status.exists {
                checks.push(check(match status.totp_enrolled {
                    Some(true) => Finding::TotpEnrolled,
                    Some(false) => Finding::TotpNotEnrolled,
                    None => Finding::TotpUnknown,
                }));
            }
            // A pairing only matters while the vault is unlocked. The broker
            // voids it on lock, so a token the gateway still holds is active
            // only while the broker also has a pairing for this unlock.
            if status.unlocked && gateway_running {
                let held = facts
                    .gateway
                    .as_ref()
                    .and_then(|r| r.info.as_ref())
                    .and_then(|info| info.vault_paired);
                checks.push(check(match (held, status.gateway_paired) {
                    (Some(false), _) | (_, Some(false)) => Finding::PairingMissing,
                    (Some(true), Some(true)) => Finding::PairingActive,
                    _ => Finding::PairingUnknown,
                }));
            }
            *vault_mode
        }
    };
    if let Some(mode) = vault_mode.filter(|mode| open(*mode)) {
        checks.push(check(Finding::VaultFileOpen { mode }));
    }
}

fn provider_checks(facts: &Facts, snapshot: &Snapshot, checks: &mut Vec<Check>) {
    if snapshot.settings.providers.is_empty() {
        checks.push(check(Finding::NoProviders));
        return;
    }
    for provider in &snapshot.settings.providers {
        let label = provider.label.clone();
        if let Some(problem) = credential_problem(facts, provider) {
            checks.push(check(problem));
            continue;
        }
        let catalog = snapshot.catalogs.iter().find(|c| c.provider == provider.id);
        checks.push(check(match catalog {
            None => Finding::ProviderNoCatalog { label },
            Some(c) if c.stale(facts.now) => Finding::ProviderCatalogStale {
                label,
                models: c.models.len(),
            },
            Some(c) => Finding::ProviderReady {
                label,
                models: c.models.len(),
            },
        }));
    }
}

/// Mirrors the broker's destination rule (`use_for_destination`) so a
/// provider that would be refused is reported before a request fails.
fn credential_problem(facts: &Facts, provider: &ProviderConfig) -> Option<Finding> {
    let label = provider.label.clone();
    let credential = provider.credential.clone()?;
    let Ok(url) = url::Url::parse(&provider.base_url) else {
        return Some(Finding::ProviderEndpointInvalid { label });
    };
    let origin = url.origin().ascii_serialization();
    let BrokerFacts::Running {
        credentials: Some(Credentials { typed, legacy }),
        ..
    } = &facts.broker
    else {
        // A remote broker cannot be asked from here; a locked one cannot say.
        return match facts.broker {
            BrokerFacts::Remote => None,
            _ => Some(Finding::ProviderCredentialUnknown { label, credential }),
        };
    };
    let Some((_, meta)) = typed.iter().find(|(name, _)| *name == credential) else {
        return Some(if legacy.contains(&credential) {
            Finding::ProviderCredentialUntyped { label, credential }
        } else {
            Finding::ProviderCredentialMissing { label, credential }
        });
    };
    if meta.disabled {
        Some(Finding::ProviderCredentialDisabled { label, credential })
    } else if !matches!(meta.kind, CredentialKind::ApiKey | CredentialKind::Token) {
        Some(Finding::ProviderCredentialKind { label, credential })
    } else if !meta.destinations.contains(&origin) {
        Some(Finding::ProviderCredentialUnbound {
            label,
            credential,
            origin,
        })
    } else {
        None
    }
}

fn mode(path: &Path) -> Option<u32> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        // No POSIX modes; report as private rather than invent a failure.
        drop(metadata);
        Some(0o700)
    }
}

#[cfg(not(unix))]
async fn broker(_root: &Path) -> BrokerFacts {
    BrokerFacts::Unsupported
}

/// Ask the broker, never starting one. A missing socket is a stopped broker.
#[cfg(unix)]
async fn broker(root: &Path) -> BrokerFacts {
    use a206_broker::{Client, ClientError, Reply as BrokerReply, Request, Role};
    let vault_mode = mode(&a206_broker::vault_path(root));
    let mut client = match Client::connect(root, Role::User).await {
        Ok(client) => client,
        Err(ClientError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return BrokerFacts::Stopped { vault_mode };
        }
        Err(error) => return BrokerFacts::Unreachable(crate::error::handle("doctor", &error)),
    };
    let status = match client.call(Request::Status).await {
        Ok(BrokerReply::Status(status)) => status,
        Ok(_) => {
            let error = crate::vault::VaultError::UnexpectedReply;
            return BrokerFacts::Unreachable(crate::error::handle("doctor", &error));
        }
        Err(error) => return BrokerFacts::Unreachable(crate::error::handle("doctor", &error)),
    };
    let credentials = if status.unlocked {
        match client.call(Request::Credentials).await {
            Ok(BrokerReply::Credentials { typed, legacy }) => Some(Credentials { typed, legacy }),
            // Locked in between, or refused: credential checks report unknown.
            Ok(_) | Err(_) => None,
        }
    } else {
        None
    };
    BrokerFacts::Running {
        status,
        vault_mode,
        credentials,
    }
}

/// Collect facts for `root` without starting or changing anything.
/// `remote` overrides the saved gateway as `--remote` does.
pub async fn gather(root: &Path, remote: Option<String>, now: u64) -> Facts {
    let target = crate::connection::resolve(root, remote)
        .map_err(|error| crate::error::handle("doctor", &error));
    let mut gateway = None;
    let mut snapshot = None;
    if let Ok(Some(target)) = &target {
        let controller = crate::control::GatewayController::attach(
            root.to_owned(),
            target.remote().map(str::to_owned),
        );
        let report = match controller.control(a206_core::GatewayAction::Status).await {
            Ok(report) => report,
            Err(error) => GatewayReport::new(
                target.remote().unwrap_or("local").to_owned(),
                GatewayPhase::Offline,
                None,
                Some(crate::error::handle("doctor", &error)),
            ),
        };
        if report.phase == GatewayPhase::Running {
            snapshot = Some(match controller.call(a206_core::Command::Snapshot).await {
                Ok(Reply::Snapshot(snapshot)) => Ok(snapshot),
                Ok(Reply::Error(report)) => Err(report),
                Ok(_) => Err(crate::error::handle(
                    "doctor",
                    &crate::control::ControlError::NoStatus,
                )),
                Err(error) => Err(crate::error::handle("doctor", &error)),
            });
        }
        gateway = Some(report);
    }
    let remote = matches!(&target, Ok(Some(GatewayTarget::Ssh { .. })));
    let broker = if remote {
        BrokerFacts::Remote
    } else {
        broker(root).await
    };
    Facts {
        profile: root.to_owned(),
        profile_mode: mode(root),
        target,
        gateway,
        snapshot,
        broker,
        now,
    }
}

/// Why [`check_detached`] could not run the checks at all.
#[derive(Debug, thiserror::Error)]
pub enum DoctorError {
    #[error("could not start the health check worker")]
    Runtime(#[source] std::io::Error),
    #[error("the health check worker stopped before reporting")]
    Worker,
}
impl crate::error::Classified for DoctorError {
    fn kind(&self) -> Option<a206_core::ErrorKind> {
        Some(a206_core::ErrorKind::Internal)
    }
}

/// [`gather`] and [`evaluate`] on a thread with its own Tokio runtime, for
/// clients whose executor is not Tokio (the desktop and terminal UI).
pub async fn check_detached(
    root: PathBuf,
    remote: Option<String>,
) -> Result<DoctorReport, DoctorError> {
    let (reply, receiver) = tokio::sync::oneshot::channel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(DoctorError::Runtime)?;
    // Deliberately detached: it ends after one report, and a caller that
    // stopped waiting needs none.
    std::thread::spawn(move || {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let facts = runtime.block_on(gather(&root, remote, now));
        reply.send(evaluate(&facts)).ok();
    });
    receiver.await.map_err(|_| DoctorError::Worker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{Catalog, GatewayInfo, ProviderApi, Settings};
    use a206_vault::AccessPolicy;

    fn random() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn info(paired: Option<bool>) -> GatewayInfo {
        GatewayInfo {
            pid: std::process::id(),
            version: random(),
            protocol: 1,
            uptime_seconds: 0,
            data_dir: random(),
            active_runs: 0,
            active_downloads: 0,
            accepting_work: Some(true),
            vault_paired: paired,
        }
    }

    fn running(paired: Option<bool>) -> GatewayReport {
        GatewayReport::new(
            "local".into(),
            GatewayPhase::Running,
            Some(info(paired)),
            None,
        )
    }

    fn unlocked(credentials: Vec<(String, CredentialMeta)>, legacy: Vec<String>) -> BrokerFacts {
        BrokerFacts::Running {
            status: BrokerStatus {
                exists: true,
                unlocked: true,
                totp_enrolled: Some(true),
                agent_access: Some(false),
                step_up_active: false,
                gateway_paired: Some(true),
            },
            vault_mode: Some(0o600),
            credentials: Some(Credentials {
                typed: credentials,
                legacy,
            }),
        }
    }

    fn facts(snapshot: Option<Snapshot>, broker: BrokerFacts, paired: Option<bool>) -> Facts {
        Facts {
            profile: PathBuf::from(random()),
            profile_mode: Some(0o700),
            target: Ok(Some(GatewayTarget::Local)),
            gateway: Some(running(paired)),
            snapshot: snapshot.map(|s| Ok(Box::new(s))),
            broker,
            now: 1_000,
        }
    }

    fn provider(credential: Option<String>) -> ProviderConfig {
        ProviderConfig {
            id: uuid::Uuid::new_v4(),
            label: random(),
            base_url: format!("https://{}.example", random()),
            credential,
            api: ProviderApi::OpenAi,
        }
    }

    fn snapshot(providers: Vec<ProviderConfig>, catalogs: Vec<Catalog>) -> Snapshot {
        Snapshot {
            settings: Settings {
                providers,
                ..Settings::default()
            },
            threads: vec![],
            projects: vec![],
            catalogs,
            downloads: vec![],
            plugins: vec![],
            messengers: vec![],
            mcp_servers: vec![],
            last_event: 0,
            managed_joshua_provider: None,
        }
    }

    fn catalog(provider: &ProviderConfig, models: usize, expires_at: u64) -> Catalog {
        Catalog {
            provider: provider.id,
            source: random(),
            fetched_at: 0,
            expires_at,
            models: (0..models)
                .map(|_| a206_core::ModelInfo {
                    key: a206_core::ModelKey {
                        provider: provider.id,
                        model: random(),
                    },
                    name: random(),
                    context_limit: None,
                    output_limit: None,
                    input_cost_per_token: None,
                    output_cost_per_token: None,
                    reasoning: None,
                    reasoning_efforts: vec![],
                })
                .collect(),
            balance: None,
            balance_currency: None,
            token_balance: None,
        }
    }

    fn meta(destinations: Vec<String>, disabled: bool) -> CredentialMeta {
        CredentialMeta {
            label: random(),
            kind: CredentialKind::ApiKey,
            policy: AccessPolicy::WithApproval,
            description: None,
            disabled,
            destinations,
        }
    }

    fn findings(report: &DoctorReport) -> Vec<&Finding> {
        report.checks.iter().map(|c| &c.finding).collect()
    }

    #[test]
    fn an_unconfigured_profile_needs_setup_and_never_reads_as_healthy() {
        let facts = Facts {
            profile: PathBuf::from(random()),
            profile_mode: None,
            target: Ok(None),
            gateway: None,
            snapshot: None,
            broker: BrokerFacts::Stopped { vault_mode: None },
            now: 0,
        };
        let report = evaluate(&facts);
        assert_eq!(report.health(), Health::Problem);
        let found = findings(&report);
        assert!(found.contains(&&Finding::GatewayNotConfigured));
        assert!(found.contains(&&Finding::VaultMissing));
        assert!(matches!(found[0], Finding::ProfileMissing { .. }));
        assert!(report.checks.iter().all(|c| c.finding.recovery().is_some()));
    }

    #[test]
    fn provider_credentials_follow_the_broker_destination_rule() {
        let name = random();
        let mut bound = provider(Some(name.clone()));
        let origin = url::Url::parse(&bound.base_url)
            .unwrap()
            .origin()
            .ascii_serialization();
        bound.label = random();
        let cases = [
            (
                vec![(name.clone(), meta(vec![origin.clone()], false))],
                vec![],
                None,
            ),
            (
                vec![(name.clone(), meta(vec![random()], false))],
                vec![],
                Some("unbound"),
            ),
            (
                vec![(name.clone(), meta(vec![origin.clone()], true))],
                vec![],
                Some("disabled"),
            ),
            (vec![], vec![name.clone()], Some("untyped")),
            (vec![], vec![], Some("missing")),
        ];
        for (typed, legacy, expected) in cases {
            let snap = snapshot(vec![bound.clone()], vec![catalog(&bound, 2, 2_000)]);
            let report = evaluate(&facts(Some(snap), unlocked(typed, legacy), Some(true)));
            let last = report.checks.last().unwrap();
            let kind = match &last.finding {
                Finding::ProviderReady { models: 2, .. } => None,
                Finding::ProviderCredentialUnbound { origin: o, .. } if *o == origin => {
                    Some("unbound")
                }
                Finding::ProviderCredentialDisabled { .. } => Some("disabled"),
                Finding::ProviderCredentialUntyped { .. } => Some("untyped"),
                Finding::ProviderCredentialMissing { .. } => Some("missing"),
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(kind, expected);
            assert_eq!(last.health == Health::Ok, expected.is_none());
        }
    }

    #[test]
    fn a_locked_vault_leaves_credentials_and_pairing_unknown() {
        let name = random();
        let bound = provider(Some(name));
        let snap = snapshot(vec![bound.clone()], vec![catalog(&bound, 1, 2_000)]);
        let locked = BrokerFacts::Running {
            status: BrokerStatus {
                exists: true,
                unlocked: false,
                totp_enrolled: None,
                agent_access: None,
                step_up_active: false,
                gateway_paired: None,
            },
            vault_mode: Some(0o644),
            credentials: None,
        };
        let report = evaluate(&facts(Some(snap), locked, Some(true)));
        let found = findings(&report);
        assert!(found.contains(&&Finding::VaultLocked));
        assert!(found.contains(&&Finding::TotpUnknown));
        assert!(found.contains(&&Finding::VaultFileOpen { mode: 0o644 }));
        assert!(!found.contains(&&Finding::PairingActive));
        assert!(matches!(
            found.last().unwrap(),
            Finding::ProviderCredentialUnknown { .. }
        ));
    }

    #[test]
    fn pairing_catalog_age_and_empty_workspaces_are_reported() {
        let keyless = provider(None);
        let stale = provider(None);
        let snap = snapshot(
            vec![keyless.clone(), stale.clone()],
            vec![catalog(&keyless, 3, 2_000), catalog(&stale, 1, 10)],
        );
        let report = evaluate(&facts(Some(snap), unlocked(vec![], vec![]), Some(false)));
        let found = findings(&report);
        assert!(found.contains(&&Finding::PairingMissing));
        assert!(found.contains(&&Finding::ProviderReady {
            label: keyless.label.clone(),
            models: 3
        }));
        assert!(found.contains(&&Finding::ProviderCatalogStale {
            label: stale.label.clone(),
            models: 1
        }));
        let report = evaluate(&facts(
            Some(snapshot(vec![], vec![])),
            unlocked(vec![], vec![]),
            None,
        ));
        let found = findings(&report);
        assert!(found.contains(&&Finding::NoProviders));
        assert!(found.contains(&&Finding::PairingUnknown));
    }

    #[test]
    fn a_token_the_broker_voided_and_a_failed_status_are_problems() {
        let snap = || snapshot(vec![], vec![]);
        let BrokerFacts::Running {
            mut status,
            vault_mode,
            credentials,
        } = unlocked(vec![], vec![])
        else {
            unreachable!("unlocked() builds a running broker")
        };
        // The gateway still holds a token from before a lock and unlock.
        status.gateway_paired = Some(false);
        let relocked = BrokerFacts::Running {
            status,
            vault_mode,
            credentials,
        };
        let report = evaluate(&facts(Some(snap()), relocked, Some(true)));
        let found = findings(&report);
        assert!(found.contains(&&Finding::PairingMissing));
        assert!(!found.contains(&&Finding::PairingActive));

        let mut facts = facts(None, BrokerFacts::Remote, None);
        facts.gateway = Some(GatewayReport::new(
            "local".into(),
            GatewayPhase::Stopped,
            None,
            Some(ErrorReport::new(a206_core::ErrorKind::Internal, random())),
        ));
        let report = evaluate(&facts);
        let stopped = report
            .checks
            .iter()
            .find(|c| matches!(c.finding, Finding::GatewayStopped { .. }))
            .unwrap();
        assert_eq!(stopped.health, Health::Problem);
        assert!(stopped.error.is_some());
        assert_eq!(report.health(), Health::Problem);
    }

    #[test]
    fn findings_round_trip_through_messagepack_and_toml() {
        let mut report = evaluate(&facts(None, BrokerFacts::Remote, None));
        report.checks.push(failed(
            Finding::GatewaySetupInvalid,
            ErrorReport::new(a206_core::ErrorKind::InvalidRequest, random()),
        ));
        let bytes = rmp_serde::to_vec_named(&report).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<DoctorReport>(&bytes).unwrap(),
            report
        );
        let text = toml::to_string(&report).unwrap();
        assert!(text.contains("finding = \"vault_remote\""));
    }

    #[tokio::test]
    async fn gathering_an_empty_profile_starts_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("profile");
        let facts = gather(&root, None, 0).await;
        assert!(!root.exists());
        assert_eq!(facts.target, Ok(None));
        assert_eq!(facts.broker, BrokerFacts::Stopped { vault_mode: None });
        assert_eq!(facts.profile_mode, None);
    }
}
