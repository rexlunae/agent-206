//! Renderer-independent state, authority and command contracts.
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

pub mod error;
pub mod ignore;
pub use error::{ErrorKind, ErrorReport};
pub use ignore::Ignore;

pub type Id = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadWorkspace,
    WriteWorkspace,
    Execute,
    Network,
    GitRead,
    GitWrite,
    ReviewPullRequests,
    Download,
    Browser,
    UseSecret,
    SpawnAgent,
    ReadHarness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Chat,
    Plan,
    Code,
    Review,
    Research,
}
impl Mode {
    pub const ALL: [Self; 5] = [
        Self::Chat,
        Self::Plan,
        Self::Code,
        Self::Review,
        Self::Research,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Plan => "Plan",
            Self::Code => "Code",
            Self::Review => "Review",
            Self::Research => "Research",
        }
    }
    pub fn ceiling(self) -> BTreeSet<Capability> {
        use Capability::*;
        let grants: &[Capability] = match self {
            Self::Chat => &[ReadHarness],
            Self::Plan => &[ReadHarness, ReadWorkspace, GitRead],
            Self::Code => &[
                ReadHarness,
                ReadWorkspace,
                WriteWorkspace,
                Execute,
                GitRead,
                GitWrite,
                SpawnAgent,
            ],
            Self::Review => &[ReadHarness, ReadWorkspace, GitRead, ReviewPullRequests],
            Self::Research => &[ReadHarness, ReadWorkspace, Network, Download, Browser],
        };
        grants.iter().copied().collect()
    }
    pub fn default_prompt(self) -> &'static str {
        match self {
            Self::Chat => {
                "Help the user clearly. Ask before taking actions outside this conversation."
            }
            Self::Plan => {
                "Inspect available context and prepare a concrete plan. Do not modify the workspace."
            }
            Self::Code => {
                "Implement the assigned task in small, reviewable changes. Verify behavior and report limitations."
            }
            Self::Review => {
                "Review changes for correctness and security. Report actionable findings with evidence. Do not modify files."
            }
            Self::Research => {
                "Research the assigned question, cite primary sources, and distinguish evidence from inference."
            }
        }
    }
}

/// A grant has no wildcard semantics. Delegation can only remove capabilities/models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Grant {
    pub capabilities: BTreeSet<Capability>,
    pub models: BTreeSet<ModelKey>,
}
impl Grant {
    pub fn attenuate(&self, mode: Mode, requested: &Self, manifest: &BTreeSet<Capability>) -> Self {
        let ceiling = mode.ceiling();
        Self {
            capabilities: self
                .capabilities
                .intersection(&requested.capabilities)
                .filter(|c| ceiling.contains(c) && manifest.contains(c))
                .copied()
                .collect(),
            models: self
                .models
                .intersection(&requested.models)
                .cloned()
                .collect(),
        }
    }
    pub fn require(&self, capability: Capability) -> Result<(), PolicyError> {
        if self.capabilities.contains(&capability) {
            Ok(())
        } else {
            Err(PolicyError::Denied(capability))
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("permission denied: {0:?}")]
    Denied(Capability),
    #[error("model is not allowed for this thread")]
    ModelDenied,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelKey {
    pub provider: Id,
    pub model: String,
}

pub mod providers;
pub use providers::{ProviderApi, ProviderPreset, ProviderPresetList};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingBackend {
    #[default]
    Native,
    Codex,
    ClaudeAgent,
}
impl CodingBackend {
    pub const ALL: [Self; 3] = [Self::Native, Self::Codex, Self::ClaudeAgent];
    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "Agent 206",
            Self::Codex => "Codex",
            Self::ClaudeAgent => "Claude Agent",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfig {
    pub kind: CodingBackend,
    /// Absolute executable path: Codex binary, or Node for the Claude ACP adapter.
    pub executable: String,
    /// Installation directory whose code the isolated runtime may read.
    pub runtime_root: String,
    /// Absolute Claude ACP adapter JavaScript entry point; absent for Codex.
    pub adapter_script: Option<String>,
}

/// Configured accounts are separate from bundled connector identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: Id,
    pub label: String,
    pub base_url: String,
    pub credential: Option<String>,
    #[serde(default)]
    pub api: ProviderApi,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub key: ModelKey,
    pub name: String,
    pub context_limit: Option<u64>,
    #[serde(default)]
    pub output_limit: Option<u64>,
    pub input_cost_per_token: Option<f64>,
    pub output_cost_per_token: Option<f64>,
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub provider: Id,
    pub source: String,
    pub fetched_at: u64,
    pub expires_at: u64,
    pub models: Vec<ModelInfo>,
    pub balance: Option<f64>,
    pub balance_currency: Option<String>,
    #[serde(default)]
    pub token_balance: Option<u64>,
}
impl Catalog {
    pub fn stale(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModePrompt {
    pub mode: Mode,
    pub prompt: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub revision: u64,
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub provider_presets: ProviderPresetList,
    #[serde(default)]
    pub backends: Vec<BackendConfig>,
    pub prompts: Vec<ModePrompt>,
    pub theme_design: String,
    pub theme_palette: Option<String>,
    pub theme_scheme: String,
    pub theme_contrast: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: 0,
            providers: vec![],
            provider_presets: ProviderPresetList::default(),
            backends: vec![],
            prompts: Mode::ALL
                .into_iter()
                .map(|mode| ModePrompt {
                    mode,
                    prompt: mode.default_prompt().into(),
                })
                .collect(),
            theme_design: "helix".into(),
            theme_palette: None,
            theme_scheme: "system".into(),
            theme_contrast: "system".into(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Idle,
    Running,
    Complete,
    Failed,
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextAttachment {
    pub name: String,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub role: Role,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<TextAttachment>,
    pub at: u64,
    #[serde(default)]
    pub backend: CodingBackend,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost: Option<f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub id: Id,
    pub project: Option<Id>,
    pub parent: Option<Id>,
    pub title: String,
    pub mode: Mode,
    pub model: Option<ModelKey>,
    pub subagent_models: BTreeSet<ModelKey>,
    pub grant: Grant,
    pub messages: Vec<Message>,
    pub state: RunState,
    pub usage: Option<Usage>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub backend: CodingBackend,
    #[serde(default)]
    pub backend_activity: Option<String>,
    /// Why the most recent run failed, cleared when a new run starts.
    #[serde(default)]
    pub error: Option<ErrorReport>,
}
impl Thread {
    pub fn new(title: String, mode: Mode) -> Self {
        Self {
            id: Id::new_v4(),
            project: None,
            parent: None,
            title,
            mode,
            model: None,
            subagent_models: BTreeSet::new(),
            grant: Grant::default(),
            messages: vec![],
            state: RunState::Idle,
            usage: None,
            effort: None,
            backend: CodingBackend::Native,
            backend_activity: None,
            error: None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: Id,
    pub name: String,
    pub root: String,
    pub manager: Id,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadState {
    Queued,
    Running,
    Complete,
    Failed,
    Cancelled,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Download {
    pub id: Id,
    pub name: String,
    pub source: String,
    pub received: u64,
    pub total: Option<u64>,
    pub state: DownloadState,
    /// Flattened failure text, kept for clients that predate `failure`.
    pub error: Option<String>,
    pub sha256: Option<String>,
    #[serde(default)]
    pub failure: Option<ErrorReport>,
}
impl Download {
    /// Record a failure in both the structured and the legacy text field.
    pub fn fail(&mut self, report: ErrorReport) {
        self.error = Some(report.detail());
        self.failure = Some(report);
    }
    /// The structured failure, or one recovered from legacy text.
    pub fn failure_report(&self) -> Option<ErrorReport> {
        legacy_failure(&self.failure, &self.error)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub capabilities: BTreeSet<Capability>,
    pub enabled: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub settings: Settings,
    pub threads: Vec<Thread>,
    pub projects: Vec<Project>,
    pub catalogs: Vec<Catalog>,
    pub downloads: Vec<Download>,
    pub plugins: Vec<PluginSummary>,
    pub last_event: u64,
    #[serde(default)]
    pub managed_joshua_provider: Option<Id>,
}
impl Snapshot {
    /// Retire only the connection owned by the managed process, including its model grants.
    pub fn clear_managed_joshua(&mut self) -> Option<Id> {
        let provider = self.managed_joshua_provider.take()?;
        self.settings.providers.retain(|entry| entry.id != provider);
        self.catalogs.retain(|catalog| catalog.provider != provider);
        for thread in &mut self.threads {
            if thread
                .model
                .as_ref()
                .is_some_and(|key| key.provider == provider)
            {
                thread.model = None;
                thread.effort = None;
                thread.usage = None;
            }
            thread
                .subagent_models
                .retain(|key| key.provider != provider);
            thread.grant.models.retain(|key| key.provider != provider);
        }
        self.settings.revision += 1;
        Some(provider)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub at: u64,
    pub kind: EventKind,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    ThreadChanged { id: Id },
    CatalogChanged { provider: Id },
    DownloadChanged(Download),
    SettingsChanged,
    BackendActivity { thread: Id, title: String },
    ProjectChanged { id: Id },
}

/// A capability the vault broker issues to the gateway for one unlock, so the
/// gateway (and nothing else) can ask the broker to spend bound credentials.
/// It is never journaled, logged or exposed back to clients.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BrokerPairing(zeroize::Zeroizing<String>);
impl BrokerPairing {
    pub fn new(token: String) -> Self {
        Self(zeroize::Zeroizing::new(token))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for BrokerPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// CLI/GUI requests. Agent access must go through the narrower harness interface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    SetBackend {
        thread: Id,
        backend: CodingBackend,
        capabilities: BTreeSet<Capability>,
    },
    StartJoshua {
        label: String,
        model_file: String,
        port: u16,
        remote: Option<String>,
    },
    StopJoshua,
    JoshuaStatus,
    Snapshot,
    SaveSettings(Settings),
    RefreshCatalog {
        provider: Id,
    },
    CreateThread {
        title: String,
        mode: Mode,
    },
    SetSubagentModel {
        id: Id,
        model: ModelKey,
        allowed: bool,
    },
    RenameThread {
        id: Id,
        title: String,
    },
    ConfigureThread {
        id: Id,
        mode: Mode,
        model: Option<ModelKey>,
        subagent_models: BTreeSet<ModelKey>,
    },
    SetEffort {
        thread: Id,
        effort: Option<String>,
    },
    Send {
        thread: Id,
        text: String,
        #[serde(default)]
        attachments: Vec<TextAttachment>,
    },
    CancelRun {
        thread: Id,
    },
    CreateProject {
        name: String,
        root: String,
    },
    CreateSubagent {
        parent: Id,
        title: String,
        mode: Mode,
        model: ModelKey,
        requested: BTreeSet<Capability>,
    },
    Events {
        after: u64,
    },
    Download {
        url: String,
        name: String,
        sha256: Option<String>,
        #[serde(default)]
        models_directory: bool,
    },
    CancelDownload {
        id: Id,
    },
    GitStatus {
        project: Id,
    },
    ReadDocument {
        project: Id,
        path: String,
    },
    RunExtension {
        id: String,
        input: String,
    },
    Shutdown,
    GatewayStatus,
    /// Hand the gateway a vault broker pairing minted by a trusted client.
    PairBroker {
        pairing: BrokerPairing,
    },
}
impl Command {
    /// A stable operation name for logs and diagnostics; never includes arguments.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SetBackend { .. } => "set_backend",
            Self::StartJoshua { .. } => "start_joshua",
            Self::StopJoshua => "stop_joshua",
            Self::JoshuaStatus => "joshua_status",
            Self::Snapshot => "snapshot",
            Self::SaveSettings(..) => "save_settings",
            Self::RefreshCatalog { .. } => "refresh_catalog",
            Self::CreateThread { .. } => "create_thread",
            Self::SetSubagentModel { .. } => "set_subagent_model",
            Self::RenameThread { .. } => "rename_thread",
            Self::ConfigureThread { .. } => "configure_thread",
            Self::SetEffort { .. } => "set_effort",
            Self::Send { .. } => "send",
            Self::CancelRun { .. } => "cancel_run",
            Self::CreateProject { .. } => "create_project",
            Self::CreateSubagent { .. } => "create_subagent",
            Self::Events { .. } => "events",
            Self::Download { .. } => "download",
            Self::CancelDownload { .. } => "cancel_download",
            Self::GitStatus { .. } => "git_status",
            Self::ReadDocument { .. } => "read_document",
            Self::RunExtension { .. } => "run_extension",
            Self::Shutdown => "shutdown",
            Self::GatewayStatus => "gateway_status",
            Self::PairBroker { .. } => "pair_broker",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    Snapshot(Box<Snapshot>),
    Thread(Thread),
    Project(Project),
    Catalog(Catalog),
    Events(Vec<Event>),
    Download(Download),
    Text(String),
    Ok,
    /// Wire-compatible with the earlier `Error { message }` variant.
    Error(ErrorReport),
    Gateway(GatewayInfo),
}
impl Reply {
    /// Separate a gateway-reported failure from a successful reply.
    pub fn into_result(self) -> Result<Reply, ErrorReport> {
        match self {
            Reply::Error(report) => Err(report),
            reply => Ok(reply),
        }
    }
}

/// Facts reported by the gateway process itself, never inferred from a socket file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayInfo {
    pub pid: u32,
    pub version: String,
    pub protocol: u16,
    pub uptime_seconds: u64,
    pub data_dir: String,
    pub active_runs: usize,
    pub active_downloads: usize,
    #[serde(default)]
    pub accepting_work: Option<bool>,
    /// Whether the gateway holds a vault pairing. It is void once the vault
    /// locks, which only the broker knows; `None` from older gateways.
    #[serde(default)]
    pub vault_paired: Option<bool>,
}

/// User control operations live outside the agent command/tool surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayAction {
    Status,
    Start,
    Stop,
    Restart,
    Reconnect,
}

impl GatewayAction {
    /// A stable operation name for logs and diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Self::Status => "gateway_status",
            Self::Start => "gateway_start",
            Self::Stop => "gateway_stop",
            Self::Restart => "gateway_restart",
            Self::Reconnect => "gateway_reconnect",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayPhase {
    Running,
    Stopped,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayReport {
    pub target: String,
    pub phase: GatewayPhase,
    pub info: Option<GatewayInfo>,
    /// Flattened failure text, kept for clients that predate `failure`.
    pub error: Option<String>,
    #[serde(default)]
    pub failure: Option<ErrorReport>,
}
impl GatewayReport {
    pub fn new(
        target: String,
        phase: GatewayPhase,
        info: Option<GatewayInfo>,
        failure: Option<ErrorReport>,
    ) -> Self {
        Self {
            target,
            phase,
            info,
            error: failure.as_ref().map(ErrorReport::detail),
            failure,
        }
    }
    /// The structured failure, or one recovered from legacy text.
    pub fn failure_report(&self) -> Option<ErrorReport> {
        legacy_failure(&self.failure, &self.error)
    }
}
fn legacy_failure(failure: &Option<ErrorReport>, text: &Option<String>) -> Option<ErrorReport> {
    failure.clone().or_else(|| {
        text.as_ref()
            .map(|message| ErrorReport::new(ErrorKind::Other, message.clone()))
    })
}

/// Where a client finds its gateway. Saved by first-time setup as human-editable
/// TOML; no client connects (or launches a local gateway) until one is chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GatewayTarget {
    /// A gateway on this computer, launched on demand with this profile.
    Local,
    /// An installed `agent206` reached with OpenSSH: `ssh://[user@]host[:port]`.
    Ssh { endpoint: String },
}
impl GatewayTarget {
    /// The `--remote` endpoint for transports, or `None` for a local gateway.
    pub fn remote(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Ssh { endpoint } => Some(endpoint),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayConnection {
    pub target: GatewayTarget,
}

/// An SSH host key offered during setup. Only the fingerprint is shown to the user;
/// `entry` is the known_hosts line recorded once they confirm it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKey {
    pub algorithm: String,
    pub fingerprint: String,
    pub entry: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delegation_never_increases_authority() {
        use Capability::*;
        let parent = Grant {
            capabilities: [ReadWorkspace, Execute, SpawnAgent].into(),
            models: BTreeSet::new(),
        };
        let requested = Grant {
            capabilities: [ReadWorkspace, Execute, UseSecret, Network].into(),
            models: BTreeSet::new(),
        };
        let effective = parent.attenuate(
            Mode::Plan,
            &requested,
            &[ReadWorkspace, Execute, Network].into(),
        );
        assert_eq!(effective.capabilities, [ReadWorkspace].into());
        assert!(effective.require(UseSecret).is_err());
        assert!(effective.models.is_empty());
    }
    #[test]
    fn error_replies_interoperate_with_the_message_only_format() {
        #[derive(Serialize, Deserialize)]
        enum LegacyReply {
            Error { message: String },
        }
        let message = Uuid::new_v4().to_string();
        let legacy = rmp_serde::to_vec_named(&LegacyReply::Error {
            message: message.clone(),
        })
        .unwrap();
        let Reply::Error(report) = rmp_serde::from_slice(&legacy).unwrap() else {
            panic!("legacy error reply must decode as an error");
        };
        assert_eq!((report.kind, report.message), (ErrorKind::Other, message));

        let current = Reply::Error(
            ErrorReport::new(ErrorKind::NotFound, Uuid::new_v4().to_string())
                .caused_by(ErrorReport::new(ErrorKind::Storage, "cause")),
        );
        let bytes = rmp_serde::to_vec_named(&current).unwrap();
        let LegacyReply::Error { message } = rmp_serde::from_slice(&bytes).unwrap();
        let Reply::Error(report) = &current else {
            unreachable!()
        };
        assert_eq!(message, report.message);
        assert_eq!(rmp_serde::from_slice::<Reply>(&bytes).unwrap(), current);
    }
    #[test]
    fn failed_downloads_stay_readable_by_message_only_clients() {
        #[derive(Serialize, Deserialize)]
        struct LegacyDownload {
            id: Id,
            name: String,
            source: String,
            received: u64,
            total: Option<u64>,
            state: DownloadState,
            error: Option<String>,
            sha256: Option<String>,
        }
        let mut download = Download {
            id: Id::new_v4(),
            name: Uuid::new_v4().to_string(),
            source: Uuid::new_v4().to_string(),
            received: 0,
            total: None,
            state: DownloadState::Failed,
            error: None,
            sha256: None,
            failure: None,
        };
        let report = ErrorReport::new(ErrorKind::Provider, Uuid::new_v4().to_string()).caused_by(
            ErrorReport::new(ErrorKind::Other, Uuid::new_v4().to_string()),
        );
        download.fail(report.clone());
        let bytes = rmp_serde::to_vec_named(&download).unwrap();
        let legacy: LegacyDownload = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(legacy.error, Some(report.detail()));

        let old = rmp_serde::to_vec_named(&legacy).unwrap();
        let current: Download = rmp_serde::from_slice(&old).unwrap();
        let recovered = current.failure_report().unwrap();
        assert_eq!(recovered.message, report.detail());
        assert_eq!(rmp_serde::from_slice::<Download>(&bytes).unwrap(), download);
        assert_eq!(download.failure_report(), Some(report));
    }
    #[test]
    fn startup_has_no_provider_facts_or_implicit_grants() {
        assert!(Settings::default().providers.is_empty());
        assert!(
            Thread::new("New chat".into(), Mode::Code)
                .grant
                .capabilities
                .is_empty()
        );
    }
}
