//! Read-only health checks shared by `agent206 doctor`, `agent206 setup` and
//! the desktop, terminal and web clients: the profile directory, the configured
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
use std::path::{Path, PathBuf};

pub use a206_core::doctor::{Check, DoctorReport, Finding, Health, Repair};

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
    /// Permission bits of the local vault directory, `None` when it does
    /// not exist or the broker is remote.
    pub vault_directory_mode: Option<u32>,
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
    if let Some(mode) = facts.vault_directory_mode.filter(|mode| open(*mode)) {
        checks.push(check(Finding::VaultDirectoryOpen { mode }));
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

/// The repairs `doctor --repair` offers for these facts: open profile and
/// vault permissions, and provider model lists that are missing or stale
/// while the provider's credential is usable. Everything else needs the
/// user's own decision (a vault password, a credential, a gateway target).
pub fn repairs(facts: &Facts) -> Vec<Repair> {
    let mut repairs = vec![];
    if let Some(mode) = facts.profile_mode.filter(|mode| open(*mode)) {
        repairs.push(Repair::RestrictProfile {
            path: facts.profile.clone(),
            mode,
        });
    }
    let vault_mode = match &facts.broker {
        BrokerFacts::Stopped { vault_mode } | BrokerFacts::Running { vault_mode, .. } => {
            *vault_mode
        }
        _ => None,
    };
    let vault = a206_broker::vault_path(&facts.profile);
    if let Some(mode) = vault_mode.filter(|mode| open(*mode)) {
        repairs.push(Repair::RestrictVault {
            path: vault.clone(),
            mode,
        });
    }
    if let (Some(mode), Some(path)) = (
        facts.vault_directory_mode.filter(|mode| open(*mode)),
        vault.parent(),
    ) {
        repairs.push(Repair::RestrictVaultDirectory {
            path: path.to_owned(),
            mode,
        });
    }
    if let Some(Ok(snapshot)) = &facts.snapshot {
        for provider in &snapshot.settings.providers {
            let catalog = snapshot.catalogs.iter().find(|c| c.provider == provider.id);
            // The gateway spends a credential only through its pairing, so a
            // credentialed refresh is offered only while both sides hold one.
            if (provider.credential.is_none() || paired(facts))
                && credential_problem(facts, provider).is_none()
                && catalog.is_none_or(|catalog| catalog.stale(facts.now))
            {
                repairs.push(Repair::RefreshCatalog {
                    provider: provider.id,
                    label: provider.label.clone(),
                });
            }
        }
    }
    repairs
}

/// True only when the gateway and the unlocked local broker both report an
/// active pairing; a remote or locked broker cannot confirm one from here.
fn paired(facts: &Facts) -> bool {
    let held = facts
        .gateway
        .as_ref()
        .and_then(|report| report.info.as_ref())
        .and_then(|info| info.vault_paired);
    held == Some(true)
        && matches!(&facts.broker, BrokerFacts::Running { status, .. }
            if status.unlocked && status.gateway_paired == Some(true))
}

/// Why one repair could not be made. Nothing is retried or worked around.
#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    #[error("{} is a symbolic link or not the expected kind of file; it was left unchanged", path.display())]
    NotPlain { path: PathBuf },
    #[error("could not change the permissions of {}", path.display())]
    Permissions {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{} cannot be read by its owner and /proc is not mounted, so its permissions could not be changed safely; remove group and other access by hand",
        path.display()
    )]
    NeedsProcfs {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not reach the gateway")]
    Gateway(#[source] crate::control::ControlError),
    #[error("the gateway refused the request")]
    Refused(#[source] ErrorReport),
    #[error("the gateway gave an unexpected reply")]
    UnexpectedReply,
    #[error("permission repairs are not available on this platform")]
    Unsupported,
}
impl crate::error::Classified for RepairError {
    fn kind(&self) -> Option<a206_core::ErrorKind> {
        match self {
            Self::Refused(report) => Some(report.kind),
            Self::Gateway(error) => error.kind(),
            _ => None,
        }
    }
}

/// Make one repair. Permission repairs only clear group and other bits and
/// refuse symbolic links; a catalog refresh is the same gateway command the
/// Connections page sends.
pub async fn repair(
    root: &Path,
    remote: Option<String>,
    repair: &Repair,
) -> Result<(), RepairError> {
    match repair {
        Repair::RestrictProfile { path, .. } => restrict(path, true),
        Repair::RestrictVault { path, .. } => restrict(path, false),
        Repair::RestrictVaultDirectory { path, .. } => restrict(path, true),
        Repair::RefreshCatalog { provider, .. } => {
            let controller = crate::control::GatewayController::attach(root.to_owned(), remote);
            match controller
                .call(a206_core::Command::RefreshCatalog {
                    provider: *provider,
                })
                .await
                .map_err(RepairError::Gateway)?
            {
                Reply::Error(report) => Err(RepairError::Refused(report)),
                Reply::Catalog(_) => Ok(()),
                _ => Err(RepairError::UnexpectedReply),
            }
        }
    }
}

#[cfg(unix)]
fn restrict(path: &Path, directory: bool) -> Result<(), RepairError> {
    let failed = |source: rustix::io::Errno| RepairError::Permissions {
        path: path.to_owned(),
        source: source.into(),
    };
    let opened = |error| match error {
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => RepairError::NotPlain {
            path: path.to_owned(),
        },
        error => failed(error),
    };
    pinned_chmod(path, directory, opened, |st_mode| {
        use rustix::fs::{FileType, Mode};
        let expected = if directory {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        if FileType::from_raw_mode(st_mode) != expected {
            return Err(RepairError::NotPlain {
                path: path.to_owned(),
            });
        }
        let mode = Mode::from_raw_mode(st_mode).bits() & 0o7777;
        Ok((mode & 0o077 != 0).then(|| Mode::from_raw_mode(mode & !0o077)))
    })
    .map_err(|error| match error {
        Pinned::Open(error) => error,
        Pinned::Change(error) => failed(error),
    })
}

#[cfg(unix)]
enum Pinned {
    Open(RepairError),
    Change(rustix::io::Errno),
}

/// Change the mode of `path` itself, never a final link's target, so a link
/// swapped in after the check cannot redirect the change. `decide` sees the
/// mode of the entry that will change and returns its new mode, if any.
/// A readable entry is changed through its own descriptor; one its owner
/// cannot read (the owner may still change its mode) takes [`unreadable`].
#[cfg(unix)]
fn pinned_chmod(
    path: &Path,
    directory: bool,
    opened: impl FnOnce(rustix::io::Errno) -> RepairError,
    decide: impl FnOnce(u32) -> Result<Option<rustix::fs::Mode>, RepairError>,
) -> Result<(), Pinned> {
    use rustix::fs::{Mode, OFlags};
    // NONBLOCK keeps a FIFO put in its place from stalling the open.
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => {
            let stat = rustix::fs::fstat(&fd).map_err(Pinned::Change)?;
            match decide(stat.st_mode).map_err(Pinned::Open)? {
                Some(mode) => rustix::fs::fchmod(&fd, mode).map_err(Pinned::Change),
                None => Ok(()),
            }
        }
        Err(rustix::io::Errno::ACCESS) => unreadable(path, directory, opened, decide),
        Err(error) => Err(Pinned::Open(opened(error))),
    }
}

/// Pin the entry with O_PATH, which needs no read access, and change it
/// through its /proc descriptor link (fchmod refuses an O_PATH descriptor).
/// That link names the same inode and no other process can replace it.
/// Without procfs the repair fails with [`RepairError::NeedsProcfs`].
#[cfg(target_os = "linux")]
fn unreadable(
    path: &Path,
    directory: bool,
    opened: impl FnOnce(rustix::io::Errno) -> RepairError,
    decide: impl FnOnce(u32) -> Result<Option<rustix::fs::Mode>, RepairError>,
) -> Result<(), Pinned> {
    use rustix::fs::{Mode, OFlags};
    use std::os::fd::AsRawFd;
    let mut flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    let fd = rustix::fs::open(path, flags, Mode::empty()).map_err(|e| Pinned::Open(opened(e)))?;
    let stat = rustix::fs::fstat(&fd).map_err(Pinned::Change)?;
    let Some(mode) = decide(stat.st_mode).map_err(Pinned::Open)? else {
        return Ok(());
    };
    match rustix::fs::chmod(format!("/proc/self/fd/{}", fd.as_raw_fd()), mode) {
        Err(rustix::io::Errno::NOENT) => Err(Pinned::Open(RepairError::NeedsProcfs {
            path: path.to_owned(),
            source: rustix::io::Errno::NOENT.into(),
        })),
        result => result.map_err(Pinned::Change),
    }
}

/// Check the entry, then change it by name without following a final link
/// (a link swapped in changes only the link's own mode).
#[cfg(all(unix, not(target_os = "linux")))]
fn unreadable(
    path: &Path,
    _directory: bool,
    opened: impl FnOnce(rustix::io::Errno) -> RepairError,
    decide: impl FnOnce(u32) -> Result<Option<rustix::fs::Mode>, RepairError>,
) -> Result<(), Pinned> {
    use rustix::fs::{AtFlags, CWD, FileType};
    let stat = rustix::fs::lstat(path).map_err(Pinned::Change)?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
        return Err(Pinned::Open(opened(rustix::io::Errno::LOOP)));
    }
    match decide(stat.st_mode).map_err(Pinned::Open)? {
        Some(mode) => {
            rustix::fs::chmodat(CWD, path, mode, AtFlags::SYMLINK_NOFOLLOW).map_err(Pinned::Change)
        }
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _directory: bool) -> Result<(), RepairError> {
    Err(RepairError::Unsupported)
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
    use a206_broker::{Reply as BrokerReply, Request};
    let vault_mode = mode(&a206_broker::vault_path(root));
    let mut client = match crate::vault::connect_running(root).await {
        Ok(Some(client)) => client,
        Ok(None) => return BrokerFacts::Stopped { vault_mode },
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
    gather_for(root, target, now).await
}

/// Collect facts for `target`, already resolved by a client that keeps its
/// own active gateway (the web bridge), so a setup file saved since by
/// another client is not consulted.
pub async fn gather_for(
    root: &Path,
    target: Result<Option<GatewayTarget>, ErrorReport>,
    now: u64,
) -> Facts {
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
    let (broker, vault_directory_mode) = if remote {
        (BrokerFacts::Remote, None)
    } else {
        let directory = a206_broker::vault_path(root);
        let directory = directory.parent().unwrap_or(root);
        (broker(root).await, mode(directory))
    };
    Facts {
        profile: root.to_owned(),
        profile_mode: mode(root),
        target,
        gateway,
        snapshot,
        broker,
        vault_directory_mode,
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
    use a206_core::{Catalog, GatewayInfo, Id, ProviderApi, Settings};
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
            vault_directory_mode: Some(0o700),
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
            jobs: vec![],
            extension_proposals: vec![],
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
                    image_input: None,
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
            vault_directory_mode: None,
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

    #[test]
    fn repairs_cover_open_permissions_and_catalogs_a_usable_credential_can_refresh() {
        let missing = provider(None);
        let stale = provider(None);
        let fresh = provider(None);
        let unusable = provider(Some(random()));
        let catalogs = vec![catalog(&stale, 1, 1_000), catalog(&fresh, 1, 2_000)];
        let mut broker = unlocked(vec![], vec![]);
        if let BrokerFacts::Running { vault_mode, .. } = &mut broker {
            *vault_mode = Some(0o644);
        }
        let mut facts = facts(
            Some(snapshot(
                vec![
                    missing.clone(),
                    stale.clone(),
                    fresh.clone(),
                    unusable.clone(),
                ],
                catalogs.clone(),
            )),
            broker,
            Some(true),
        );
        facts.profile_mode = Some(0o755);
        assert_eq!(
            repairs(&facts),
            [
                Repair::RestrictProfile {
                    path: facts.profile.clone(),
                    mode: 0o755
                },
                Repair::RestrictVault {
                    path: a206_broker::vault_path(&facts.profile),
                    mode: 0o644
                },
                Repair::RefreshCatalog {
                    provider: missing.id,
                    label: missing.label.clone()
                },
                Repair::RefreshCatalog {
                    provider: stale.id,
                    label: stale.label.clone()
                },
            ]
        );
        let healthy = super::tests::facts(
            Some(snapshot(vec![fresh], catalogs)),
            unlocked(vec![], vec![]),
            Some(true),
        );
        assert!(repairs(&healthy).is_empty());
        let mut remote = healthy;
        remote.profile_mode = Some(0o700);
        remote.broker = BrokerFacts::Remote;
        remote.snapshot = None;
        assert!(repairs(&remote).is_empty());
    }

    #[test]
    fn a_credentialed_refresh_is_offered_only_while_the_gateway_is_paired() {
        let name = random();
        let keyed = provider(Some(name.clone()));
        let origin = url::Url::parse(&keyed.base_url)
            .unwrap()
            .origin()
            .ascii_serialization();
        let keyless = provider(None);
        let snap = snapshot(vec![keyed.clone(), keyless.clone()], vec![]);
        let credentials = || vec![(name.clone(), meta(vec![origin.clone()], false))];
        let offered = |facts: &Facts| -> Vec<Id> {
            repairs(facts)
                .into_iter()
                .filter_map(|repair| match repair {
                    Repair::RefreshCatalog { provider, .. } => Some(provider),
                    _ => None,
                })
                .collect()
        };
        let paired = facts(
            Some(snap.clone()),
            unlocked(credentials(), vec![]),
            Some(true),
        );
        assert_eq!(offered(&paired), [keyed.id, keyless.id]);
        let unpaired = facts(
            Some(snap.clone()),
            unlocked(credentials(), vec![]),
            Some(false),
        );
        assert_eq!(offered(&unpaired), [keyless.id]);
        let mut broker_unpaired = unlocked(credentials(), vec![]);
        if let BrokerFacts::Running { status, .. } = &mut broker_unpaired {
            status.gateway_paired = Some(false);
        }
        let broker_side = facts(Some(snap.clone()), broker_unpaired, Some(true));
        assert_eq!(offered(&broker_side), [keyless.id]);
        let remote = facts(Some(snap), BrokerFacts::Remote, Some(true));
        assert_eq!(offered(&remote), [keyless.id]);
    }

    #[test]
    fn an_open_vault_directory_is_reported_and_repaired_on_its_own() {
        let mut facts = facts(None, unlocked(vec![], vec![]), Some(true));
        facts.vault_directory_mode = Some(0o755);
        assert!(
            findings(&evaluate(&facts)).contains(&&Finding::VaultDirectoryOpen { mode: 0o755 })
        );
        let vault = a206_broker::vault_path(&facts.profile);
        assert_eq!(
            repairs(&facts),
            [Repair::RestrictVaultDirectory {
                path: vault.parent().unwrap().to_owned(),
                mode: 0o755
            }]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permission_repairs_clear_only_group_and_other_bits_and_refuse_links() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let set = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join(random());
        std::fs::create_dir(&profile).unwrap();
        set(&profile, 0o751);
        let fix = Repair::RestrictProfile {
            path: profile.clone(),
            mode: 0o751,
        };
        repair(&profile, None, &fix).await.unwrap();
        assert_eq!(mode(&profile), 0o700);
        let vault = a206_broker::vault_path(&profile);
        std::fs::create_dir(vault.parent().unwrap()).unwrap();
        std::fs::write(&vault, random()).unwrap();
        set(&vault, 0o464);
        set(vault.parent().unwrap(), 0o755);
        let fix = Repair::RestrictVault {
            path: vault.clone(),
            mode: 0o464,
        };
        repair(&profile, None, &fix).await.unwrap();
        assert_eq!(mode(&vault), 0o400);
        assert_eq!(mode(vault.parent().unwrap()), 0o755);
        let directory = Repair::RestrictVaultDirectory {
            path: vault.parent().unwrap().to_owned(),
            mode: 0o755,
        };
        repair(&profile, None, &directory).await.unwrap();
        assert_eq!(mode(vault.parent().unwrap()), 0o700);

        let elsewhere = root.path().join(random());
        std::fs::write(&elsewhere, random()).unwrap();
        set(&elsewhere, 0o644);
        std::fs::remove_file(&vault).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &vault).unwrap();
        assert!(matches!(
            repair(&profile, None, &fix).await,
            Err(RepairError::NotPlain { .. })
        ));
        assert_eq!(mode(&elsewhere), 0o644);
        let wrong_kind = Repair::RestrictProfile {
            path: elsewhere.clone(),
            mode: 0o644,
        };
        assert!(matches!(
            repair(&profile, None, &wrong_kind).await,
            Err(RepairError::NotPlain { .. })
        ));
        assert_eq!(mode(&elsewhere), 0o644);
    }

    #[tokio::test]
    async fn permission_repairs_need_no_read_access_from_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let set = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join(random());
        let vault = a206_broker::vault_path(&profile);
        std::fs::create_dir_all(vault.parent().unwrap()).unwrap();
        std::fs::write(&vault, random()).unwrap();
        set(&vault, 0o066);
        let fix = Repair::RestrictVault {
            path: vault.clone(),
            mode: 0o066,
        };
        repair(&profile, None, &fix).await.unwrap();
        assert_eq!(mode(&vault), 0o000);
        set(&vault, 0o600);
        let directory = vault.parent().unwrap().to_owned();
        set(&directory, 0o355);
        let fix = Repair::RestrictVaultDirectory {
            path: directory.clone(),
            mode: 0o355,
        };
        repair(&profile, None, &fix).await.unwrap();
        assert_eq!(mode(&directory), 0o300);
        set(&directory, 0o700);
    }

    #[tokio::test]
    async fn a_refresh_without_a_reachable_gateway_reports_why() {
        let root = tempfile::tempdir().unwrap();
        let fix = Repair::RefreshCatalog {
            provider: uuid::Uuid::new_v4(),
            label: random(),
        };
        let error = repair(root.path(), None, &fix).await.unwrap_err();
        assert!(matches!(error, RepairError::Gateway(_)), "{error:?}");
        assert!(std::error::Error::source(&error).is_some());
    }
}
