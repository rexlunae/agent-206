//! Renderer-independent state, authority and command contracts.
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub mod audit;
pub mod doctor;
pub mod error;
pub mod extension;
pub mod ignore;
pub mod imports;
pub mod profiles;
pub mod prompts;
pub mod search;
pub mod teams;
pub mod tools;
pub use audit::{AuditBuilder, AuditField, AuditFinding, AuditLocation, AuditReport, AuditRule};
pub use error::{ErrorKind, ErrorReport};
pub use ignore::Ignore;
pub use profiles::{AgentProfile, AgentProfiles, ProfileProblem, ThreadProfile};
pub use prompts::{AnswerError, PromptAnswer, PromptOutcome, UserPrompt};
pub use search::{MessageHit, SearchError};
pub use teams::{AgentTeam, AgentTeams, TeamMember, TeamRole, ThreadTeam};
pub use tools::{
    ApprovalDecision, ApprovalOutcome, ApprovalRequest, OutputCount, OutputReduction, ToolBudget,
    ToolCall, ToolCallState, ToolList, ToolOffer, ToolPermission, UnavailableTools,
};

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
impl Capability {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadWorkspace => "Read project files",
            Self::WriteWorkspace => "Edit project files",
            Self::Execute => "Run project commands",
            Self::Network => "Use the network",
            Self::GitRead => "Read git history",
            Self::GitWrite => "Stage and commit in git",
            Self::ReviewPullRequests => "Review pull requests",
            Self::Download => "Download files",
            Self::Browser => "Use the browser",
            Self::UseSecret => "Use vault credentials",
            Self::SpawnAgent => "Start subagents",
            Self::ReadHarness => "Read harness state",
        }
    }
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

pub mod messengers;
pub mod providers;
pub use messengers::{
    ChatBinding, MessengerConfig, MessengerKind, MessengerState, MessengerStatus, RefusedChat,
    WebhookFormat,
};
pub mod mcp;
pub mod schedule;
pub use mcp::{McpServerConfig, McpServerState, McpServerStatus, McpToolInfo, ToolPolicy};
pub use providers::{ProviderApi, ProviderPreset, ProviderPresetList};
pub use schedule::{Job, JobAuthor, JobKind, JobOutcome, JobRun, Schedule, ScheduleInputError};

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
    /// What the user can grant a conversation on this backend, in display
    /// order. Agent 206 enforces git and network grants in its own tools; an
    /// external runtime is confined only by project access and commands, so
    /// it is never offered a grant it could not be held to.
    pub fn grantable(self) -> &'static [Capability] {
        use Capability::*;
        match self {
            Self::Native => &[
                ReadWorkspace,
                WriteWorkspace,
                Execute,
                GitRead,
                GitWrite,
                Network,
            ],
            Self::Codex | Self::ClaudeAgent => &[ReadWorkspace, WriteWorkspace, Execute],
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
    /// Whether the models API reports that the model accepts images;
    /// `None` when it does not say.
    #[serde(default)]
    pub image_input: Option<bool>,
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
    #[serde(default)]
    pub messengers: Vec<MessengerConfig>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Enabled skills by name, each pinned to the SHA-256 of the `SKILL.md`
    /// the user approved. Any edit to a skill disables it until re-approved.
    #[serde(default)]
    pub enabled_skills: std::collections::BTreeMap<String, String>,
    /// Absolute directories, beyond `/usr/bin` and `/bin`, whose programs
    /// sandboxed commands may run read-only, such as a Homebrew prefix or a
    /// toolchain. The gateway stores them canonicalized.
    #[serde(default)]
    pub program_directories: Vec<std::path::PathBuf>,
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
            messengers: vec![],
            mcp_servers: vec![],
            enabled_skills: Default::default(),
            program_directories: vec![],
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
/// The largest image the gateway stores, in bytes: base64-encoded, one image
/// and its message still fit in a provider request the vault broker carries.
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
/// The most images one message carries.
pub const MAX_IMAGES: usize = 4;
/// An image format the gateway accepts, recognised from the file's own
/// bytes rather than its name or a client's claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageType {
    Png,
    Jpeg,
    Gif,
    Webp,
}
impl ImageType {
    /// The format whose signature `bytes` starts with.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}
/// An image attached to a user message. The gateway keeps its bytes outside
/// the conversation, named by their SHA-256 digest; `Command::StageImage`
/// stores them and returns this description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub name: String,
    pub media_type: ImageType,
    /// Lowercase hexadecimal SHA-256 of the bytes.
    pub digest: String,
    pub size: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub role: Role,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<TextAttachment>,
    /// Images attached to a user message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
    pub at: u64,
    #[serde(default)]
    pub backend: CodingBackend,
    /// Tool calls an assistant message requested, with their outcomes.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Set on a [`Role::System`] message whose text is a model-written
    /// summary of every message before it. Native runs send the model the
    /// latest summary and the messages after it; the earlier messages stay
    /// in the thread for the user.
    #[serde(default)]
    pub compaction: Option<Compaction>,
    /// The reasoning text a model streamed before or between its reply,
    /// for display only: it is never sent back to a model.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    /// The model streamed more reasoning than a message keeps; the rest was
    /// not recorded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reasoning_shortened: bool,
}
impl Message {
    pub fn new(role: Role, text: String, at: u64, backend: CodingBackend) -> Self {
        Self {
            id: Id::new_v4(),
            role,
            text,
            attachments: vec![],
            images: vec![],
            at,
            backend,
            tool_calls: vec![],
            compaction: None,
            reasoning: String::new(),
            reasoning_shortened: false,
        }
    }
}
/// A policy choice, not a model fact: a native run compacts its earlier
/// messages before its next request once the previous request's reported
/// input tokens exceed this percentage of the model's API-reported context
/// limit. With no reported limit or input count there is no automatic
/// compaction. RustyClaw uses the same 75%.
pub const COMPACTION_THRESHOLD_PERCENT: u64 = 75;
/// Why a conversation's earlier messages were summarized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// The previous model request used more than the gateway's compaction
    /// threshold of the API-reported context limit.
    Automatic,
    /// The user asked for it (`CompactThread`).
    Manual,
}
/// What one compaction summarized and what it cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    pub trigger: CompactionTrigger,
    /// Messages this summary covers that no earlier summary covered.
    pub messages: u32,
    /// The latest summary it folded in, if there was one.
    pub previous: Option<Id>,
    /// The last request's input tokens when it started (unknown when the
    /// provider did not report them).
    pub input_tokens: Option<u64>,
    /// The model's API-reported context limit when it started, if known.
    pub context_limit: Option<u64>,
    /// The summarization request's own usage.
    pub usage: Usage,
}
/// What happened to one project instruction file a native run looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionStatus {
    Included,
    /// Only the first `bytes` were included.
    Truncated,
    /// Not included; `error` says why.
    Skipped,
}
/// Provenance of one project instruction file in a native run's system prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstructionFile {
    /// Relative to the project root.
    pub path: String,
    pub status: InstructionStatus,
    /// Bytes included in the prompt.
    pub bytes: u64,
    /// The file's size on disk, when it could be read.
    pub size: Option<u64>,
    /// SHA-256 of the included text.
    pub sha256: Option<String>,
    pub error: Option<ErrorReport>,
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
    /// The latest model request alone: what the context window holds.
    pub usage: Option<Usage>,
    /// Every model request of the latest run summed: what the run cost. A
    /// count any request did not report makes its total unknown.
    #[serde(default)]
    pub run_usage: Option<Usage>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub backend: CodingBackend,
    #[serde(default)]
    pub backend_activity: Option<String>,
    /// Why the most recent run failed, cleared when a new run starts.
    #[serde(default)]
    pub error: Option<ErrorReport>,
    /// The user's per-tool permissions; a tool not listed is denied.
    #[serde(default)]
    pub tool_permissions: BTreeMap<String, ToolPermission>,
    #[serde(default)]
    pub tool_budget: ToolBudget,
    /// Tool calls of the current run waiting for the user.
    #[serde(default)]
    pub approvals: Vec<ApprovalRequest>,
    /// Questions the current run asked the user and is waiting on.
    #[serde(default)]
    pub prompts: Vec<UserPrompt>,
    /// The messenger chat this thread converses with, if any.
    #[serde(default)]
    pub chat: Option<Box<ChatBinding>>,
    /// Project instruction files the latest native run looked for, and what
    /// it did with each. Empty when the run could not read the project.
    #[serde(default)]
    pub instructions: Vec<InstructionFile>,
    /// While a user-requested compaction marks the thread `Running`, the
    /// state it restores afterwards, so a restart restores it too rather
    /// than reporting an interrupted run.
    #[serde(default)]
    pub compacting: Option<RunState>,
    /// When the latest pre-compaction memory flush was offered: the summary
    /// that was then the latest (`None` before the first). A flush is
    /// offered once for each summary, before the next one is made.
    #[serde(default)]
    pub memory_flush: Option<MemoryFlush>,
    /// Messages sent to the running native run with `Steer`, waiting for the
    /// run to reach its next round, where they join `messages` in order.
    #[serde(default)]
    pub steering: Vec<Message>,
    /// The user paused this conversation's work with `PauseRun`: its native
    /// run, and those of its subagents, wait before their next model round
    /// until `ResumeRun`. Cleared when the conversation's run ends.
    #[serde(default)]
    pub paused: bool,
    /// The native run is waiting before its next model round because this
    /// conversation or an ancestor is paused.
    #[serde(default)]
    pub held: bool,
    /// The checklist the conversation's models keep with `plan_update`,
    /// shown to the user beside the conversation. Empty when there is none.
    #[serde(default)]
    pub plan: Vec<PlanStep>,
    /// The agent profile this subagent was started from, if any.
    #[serde(default)]
    pub profile: Option<Box<ThreadProfile>>,
    /// The agent team this subagent coordinates or works for, if any.
    #[serde(default)]
    pub team: Option<Box<ThreadTeam>>,
}
/// One step of a conversation's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub text: String,
    pub status: PlanStatus,
}
/// How far a plan step has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
}
impl PlanStatus {
    pub const ALL: [Self; 3] = [Self::Pending, Self::InProgress, Self::Done];
    /// The name models and the API use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Not started",
            Self::InProgress => "In progress",
            Self::Done => "Done",
        }
    }
}
/// A pre-compaction memory flush offered to a native run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFlush {
    /// The latest compaction summary when the flush was offered.
    pub after: Option<Id>,
}
impl Thread {
    /// The latest compaction summary in the conversation, if any.
    pub fn latest_compaction(&self) -> Option<Id> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.compaction.is_some())
            .map(|m| m.id)
    }
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
            run_usage: None,
            effort: None,
            backend: CodingBackend::Native,
            backend_activity: None,
            error: None,
            tool_permissions: BTreeMap::new(),
            tool_budget: ToolBudget::default(),
            approvals: vec![],
            prompts: vec![],
            chat: None,
            instructions: vec![],
            compacting: None,
            memory_flush: None,
            steering: vec![],
            paused: false,
            held: false,
            plan: vec![],
            profile: None,
            team: None,
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
    /// Capabilities the manifest requests. Requesting grants nothing.
    pub capabilities: BTreeSet<Capability>,
    pub enabled: bool,
    #[serde(default)]
    pub version: String,
    /// Capabilities the user granted; always a subset of `capabilities`.
    #[serde(default)]
    pub granted: BTreeSet<Capability>,
    #[serde(default)]
    pub tools: Vec<extension::ToolSpec>,
    #[serde(default)]
    pub panels: Vec<extension::PanelSpec>,
    /// Where the `network` permission may connect; `*` is any public host.
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub source: PluginSource,
}
/// Largest `plugin.toml` sent with [`Command::InstallExtension`].
pub const MAX_EXTENSION_MANIFEST: usize = 64 * 1024;
/// Largest module sent with [`Command::InstallExtension`]; with its manifest
/// it fits one protocol frame.
pub const MAX_EXTENSION_MODULE: usize = 3 * 1024 * 1024;
/// Where an extension's package came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginSource {
    /// Compiled into this application.
    #[default]
    Bundled,
    /// Installed by the user. `digest` is the SHA-256 of the stored package,
    /// which the gateway verifies before every run.
    Installed { digest: String },
}
/// Most packages agents may leave waiting for review at once.
pub const MAX_EXTENSION_PROPOSALS: usize = 8;
/// A package an agent prepared, waiting for the user's review. It grants
/// nothing and never runs; installing it is a user operation, and it arrives
/// disabled with nothing granted, even when it updates an installed extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionProposal {
    /// SHA-256 of the stored package; pass it to [`Command::InstallProposal`].
    pub digest: String,
    /// What the package declares. Text fields are written by its author.
    pub summary: PluginSummary,
    /// The conversation whose agent prepared it.
    pub thread: Id,
    pub proposed_at: u64,
}
/// A skill installed in the gateway's profile, as the user reviews it.
/// Text fields are written by the skill's author.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    /// The skill's directory name, which is also its name.
    pub name: String,
    pub description: Option<String>,
    pub emoji: Option<String>,
    /// Enabled, and unchanged since the user approved it.
    pub enabled: bool,
    /// Enabled once, but the package changed since; not offered until re-enabled.
    pub changed: bool,
    /// Content digest of the package as it is now; approving requires it.
    pub digest: Option<String>,
    /// Requirements this gateway's computer does not meet.
    pub unmet: Vec<String>,
    /// Environment variables the author expects; tools never inherit them.
    pub expects_env: Vec<String>,
    /// Why the skill could not be read.
    pub failure: Option<ErrorReport>,
    /// What the pre-install audit found in the package, for the user's review.
    #[serde(default)]
    pub audit: AuditReport,
}
/// The exact package a user reviews, with the digest that approves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillText {
    pub name: String,
    pub text: String,
    /// Content digest of this package; pass it to [`Command::SetSkillEnabled`].
    pub digest: String,
    #[serde(default)]
    pub files: Vec<SkillReviewFile>,
    #[serde(default)]
    pub audit: AuditReport,
}
/// A supporting file in the exact package snapshot being approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillReviewFile {
    pub path: String,
    pub bytes: u64,
    /// Binary content is listed, but never interpreted or executed.
    pub text: Option<String>,
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
    /// Runtime state of configured messengers; never a grant.
    #[serde(default)]
    pub messengers: Vec<MessengerStatus>,
    /// Runtime state of configured MCP servers; never a grant.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerStatus>,
    /// Scheduled messages.
    #[serde(default)]
    pub jobs: Vec<Job>,
    /// Extension packages agents prepared, waiting for the user.
    #[serde(default)]
    pub extension_proposals: Vec<ExtensionProposal>,
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
                thread.run_usage = None;
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
/// Most events one [`Command::Activity`] page holds.
pub const MAX_ACTIVITY: u32 = 100;
/// Journal events newest first, leaving out [progress
/// notices](EventKind::is_progress).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityPage {
    pub events: Vec<Event>,
    /// Older activity is still journaled before the last event here.
    pub more: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    ThreadChanged {
        id: Id,
    },
    /// A conversation was created by the user, a messenger or a delegating
    /// agent.
    ThreadCreated {
        id: Id,
    },
    /// A conversation's title changed; the thread has the new one.
    ThreadRenamed {
        id: Id,
    },
    /// A reply ended; `state` is `Complete`, `Failed` or `Cancelled`.
    RunEnded {
        thread: Id,
        state: RunState,
    },
    CatalogChanged {
        provider: Id,
    },
    DownloadChanged(Download),
    SettingsChanged,
    BackendActivity {
        thread: Id,
        title: String,
    },
    ProjectChanged {
        id: Id,
    },
    MessengerChanged {
        name: String,
    },
    McpServerChanged {
        name: String,
    },
    ExtensionChanged {
        id: String,
    },
    /// A tool call is waiting for the user; the request is in the thread.
    ApprovalRequested {
        thread: Id,
        approval: Id,
    },
    ApprovalResolved {
        thread: Id,
        approval: Id,
        outcome: ApprovalOutcome,
    },
    /// An agent asked the user a question; it is in the thread.
    PromptRequested {
        thread: Id,
        prompt: Id,
    },
    PromptResolved {
        thread: Id,
        prompt: Id,
        outcome: PromptOutcome,
    },
    /// A scheduled message was added, changed, ran or was removed.
    JobChanged {
        id: Id,
    },
}

impl EventKind {
    /// Notices that only tell clients to re-read something that is still
    /// changing: conversation updates (sent while replies stream) and
    /// running-download progress. Activity views leave them out.
    pub fn is_progress(&self) -> bool {
        matches!(
            self,
            Self::ThreadChanged { .. }
                | Self::DownloadChanged(Download {
                    state: DownloadState::Running,
                    ..
                })
        )
    }
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
    SaveSettings(Box<Settings>),
    /// Read-only review of a user-selected installation on the gateway host.
    PreviewImport {
        source: std::path::PathBuf,
    },
    /// Explicit adapter selection. Older gateways reject this opcode rather
    /// than silently interpreting another tool's directory as RustyClaw.
    PreviewSourceImport {
        tool: imports::Tool,
        source: std::path::PathBuf,
    },
    /// Apply only selected non-secret settings from a held, unchanged review.
    ApplyImport {
        review: Id,
        selection: imports::Selection,
    },
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
        /// Images staged with [`Command::StageImage`].
        #[serde(default)]
        images: Vec<ImageAttachment>,
    },
    /// Store an image for a later `Send`. Replies with
    /// [`Reply::Image`] describing it.
    StageImage {
        name: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// The bytes of a stored image, for display.
    ReadImage {
        digest: String,
    },
    CancelRun {
        thread: Id,
    },
    /// Messages in any conversation containing every term of `query`,
    /// newest first; see [`search::search`].
    SearchMessages {
        query: String,
        limit: u32,
    },
    /// Summarize a native conversation's earlier messages now, whatever its
    /// context usage. Replies with the thread; `CancelRun` stops it.
    CompactThread {
        thread: Id,
    },
    /// The tools this conversation could use and how each is permitted.
    ListTools {
        thread: Id,
    },
    /// The newest [`ActivityPage`] of the event journal, or the page before
    /// sequence `before`; `limit` events, between one and [`MAX_ACTIVITY`].
    Activity {
        before: Option<u64>,
        limit: u32,
    },
    /// Set (or with `None`, clear back to deny) one tool's permission.
    SetToolPermission {
        thread: Id,
        tool: String,
        permission: Option<ToolPermission>,
    },
    SetToolBudget {
        thread: Id,
        budget: ToolBudget,
    },
    /// Atomically update limits only if the saved values still match the review.
    UpdateToolBudget {
        thread: Id,
        budget: ToolBudget,
        expected: ToolBudget,
    },
    /// Answer a pending approval. `digest` must be the one the user was
    /// shown, so a decision applies only to that exact call.
    ResolveApproval {
        thread: Id,
        approval: Id,
        digest: String,
        decision: ApprovalDecision,
    },
    /// Answer a question an agent asked in `thread`.
    AnswerPrompt {
        thread: Id,
        prompt: Id,
        answer: PromptAnswer,
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
    /// Create with enforced profile constraints. A distinct opcode ensures an
    /// older gateway refuses rather than ignoring an optional policy field.
    CreateProfileSubagent {
        parent: Id,
        title: String,
        mode: Mode,
        model: ModelKey,
        requested: BTreeSet<Capability>,
        profile: String,
    },
    /// Start `team` on `parent`: a subagent from the team's coordinator
    /// profile, checked as `CreateProfileSubagent` checks it, that may start
    /// its own subagents only from the team's member profiles.
    StartTeam {
        parent: Id,
        team: String,
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
    /// Render an extension panel, optionally after a press on one of its
    /// elements. Workspace capabilities need `project`.
    OpenPanel {
        extension: String,
        panel: String,
        project: Option<Id>,
        event: Option<extension::UiEvent>,
    },
    /// Run an extension tool as the user, within the extension's grant.
    CallTool {
        extension: String,
        tool: String,
        args: extension::Args,
        project: Option<Id>,
    },
    /// Replace the capabilities the user grants an extension. Only
    /// capabilities its manifest requests can be granted.
    GrantExtension {
        extension: String,
        capabilities: BTreeSet<Capability>,
    },
    EnableExtension {
        extension: String,
        enabled: bool,
    },
    /// Install or update a third-party extension from its `plugin.toml` and
    /// compiled module (user operation). It arrives disabled with nothing
    /// granted; an update is disabled again and keeps only the grants its new
    /// manifest still requests.
    InstallExtension {
        manifest: String,
        #[serde(with = "serde_bytes")]
        module: Vec<u8>,
    },
    /// Remove an installed extension and its stored state (user operation).
    UninstallExtension {
        extension: String,
    },
    /// Install a package an agent prepared (user operation). It arrives
    /// disabled with nothing granted, also when it updates an extension.
    InstallProposal {
        digest: String,
    },
    /// Discard a package an agent prepared, and its files.
    DismissProposal {
        digest: String,
    },
    Shutdown,
    GatewayStatus,
    /// Hand the gateway a vault broker pairing minted by a trusted client.
    PairBroker {
        pairing: BrokerPairing,
    },
    /// Connect an enabled messenger the user disconnected (user operation).
    ConnectMessenger {
        name: String,
    },
    /// Disconnect a messenger; it stays stopped until the user connects it.
    DisconnectMessenger {
        name: String,
    },
    /// Start an enabled MCP server and list its tools (user operation).
    ConnectMcpServer {
        name: String,
    },
    /// Stop an MCP server; it stays stopped until the user connects it again.
    DisconnectMcpServer {
        name: String,
    },
    /// List skills installed in the gateway's profile.
    Skills,
    /// The agent profiles in the gateway's profile directory.
    AgentProfiles,
    /// The agent teams in the gateway's team directory.
    AgentTeams,
    /// The full `SKILL.md` of one skill, for review.
    ReadSkill {
        name: String,
    },
    /// Approve a skill, or withdraw approval (user operation). Enabling
    /// requires the `digest` of the text the user reviewed; it fails with
    /// `Changed` if `SKILL.md` no longer matches it.
    SetSkillEnabled {
        name: String,
        enabled: bool,
        #[serde(default)]
        digest: Option<String>,
    },
    /// Send `text` (or, for a heartbeat check, `HEARTBEAT.md`) to `thread`
    /// on `schedule`. The job is enabled.
    ScheduleJob {
        name: String,
        thread: Id,
        /// The message; empty for a heartbeat check.
        text: String,
        schedule: Schedule,
        #[serde(default)]
        kind: JobKind,
    },
    /// Pause or resume a job; resuming computes its next time from now.
    SetJobEnabled {
        id: Id,
        enabled: bool,
    },
    RemoveJob {
        id: Id,
    },
    /// Run a job now, whatever its schedule.
    RunJob {
        id: Id,
    },
    /// A job's latest runs, newest first: up to
    /// [`schedule::MAX_JOB_RUNS`], kept while the job exists.
    JobRuns {
        id: Id,
    },
    /// Add a message to `thread`'s running native run. The run reads it
    /// before its next model request, after any tool calls in progress, and
    /// continues instead of ending while one waits. Refused when no native
    /// run is taking messages; send it with `Send` then.
    Steer {
        thread: Id,
        text: String,
    },
    /// Hold `thread`'s running native run, and its subagents' runs, before
    /// their next model round. A round in progress, with its tool calls and
    /// approvals, finishes first. Refused when no native run is going.
    PauseRun {
        thread: Id,
    },
    /// Let a paused conversation's runs continue. Refused when it is not
    /// paused.
    ResumeRun {
        thread: Id,
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
            Self::PreviewImport { .. } => "preview_import",
            Self::PreviewSourceImport { .. } => "preview_source_import",
            Self::ApplyImport { .. } => "apply_import",
            Self::RefreshCatalog { .. } => "refresh_catalog",
            Self::CreateThread { .. } => "create_thread",
            Self::SetSubagentModel { .. } => "set_subagent_model",
            Self::RenameThread { .. } => "rename_thread",
            Self::ConfigureThread { .. } => "configure_thread",
            Self::SetEffort { .. } => "set_effort",
            Self::Send { .. } => "send",
            Self::StageImage { .. } => "stage_image",
            Self::ReadImage { .. } => "read_image",
            Self::CancelRun { .. } => "cancel_run",
            Self::SearchMessages { .. } => "search_messages",
            Self::Activity { .. } => "activity",
            Self::CompactThread { .. } => "compact_thread",
            Self::ListTools { .. } => "list_tools",
            Self::SetToolPermission { .. } => "set_tool_permission",
            Self::SetToolBudget { .. } => "set_tool_budget",
            Self::UpdateToolBudget { .. } => "update_tool_budget",
            Self::ResolveApproval { .. } => "resolve_approval",
            Self::AnswerPrompt { .. } => "answer_prompt",
            Self::CreateProject { .. } => "create_project",
            Self::CreateSubagent { .. } => "create_subagent",
            Self::CreateProfileSubagent { .. } => "create_profile_subagent",
            Self::StartTeam { .. } => "start_team",
            Self::Events { .. } => "events",
            Self::Download { .. } => "download",
            Self::CancelDownload { .. } => "cancel_download",
            Self::GitStatus { .. } => "git_status",
            Self::ReadDocument { .. } => "read_document",
            Self::OpenPanel { .. } => "open_panel",
            Self::CallTool { .. } => "call_tool",
            Self::GrantExtension { .. } => "grant_extension",
            Self::EnableExtension { .. } => "enable_extension",
            Self::InstallExtension { .. } => "install_extension",
            Self::UninstallExtension { .. } => "uninstall_extension",
            Self::InstallProposal { .. } => "install_proposal",
            Self::DismissProposal { .. } => "dismiss_proposal",
            Self::Shutdown => "shutdown",
            Self::GatewayStatus => "gateway_status",
            Self::PairBroker { .. } => "pair_broker",
            Self::ConnectMessenger { .. } => "connect_messenger",
            Self::DisconnectMessenger { .. } => "disconnect_messenger",
            Self::ConnectMcpServer { .. } => "connect_mcp_server",
            Self::DisconnectMcpServer { .. } => "disconnect_mcp_server",
            Self::Skills => "skills",
            Self::AgentProfiles => "agent_profiles",
            Self::AgentTeams => "agent_teams",
            Self::ReadSkill { .. } => "read_skill",
            Self::SetSkillEnabled { .. } => "set_skill_enabled",
            Self::ScheduleJob { .. } => "schedule_job",
            Self::SetJobEnabled { .. } => "set_job_enabled",
            Self::RemoveJob { .. } => "remove_job",
            Self::RunJob { .. } => "run_job",
            Self::JobRuns { .. } => "job_runs",
            Self::Steer { .. } => "steer",
            Self::PauseRun { .. } => "pause_run",
            Self::ResumeRun { .. } => "resume_run",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    Snapshot(Box<Snapshot>),
    Thread(Box<Thread>),
    Project(Project),
    Catalog(Catalog),
    Events(Vec<Event>),
    Download(Download),
    Text(String),
    Panel(extension::View),
    Ok,
    /// Wire-compatible with the earlier `Error { message }` variant.
    Error(ErrorReport),
    Gateway(GatewayInfo),
    Skills(Vec<SkillSummary>),
    AgentProfiles(AgentProfiles),
    AgentTeams(AgentTeams),
    ImportPreview(Box<imports::Review>),
    ImportApplied(Box<imports::Applied>),
    Skill(SkillText),
    Tools(ToolList),
    Messages(Vec<MessageHit>),
    Activity(ActivityPage),
    Extension(Box<PluginSummary>),
    Job(Box<Job>),
    Image(ImageAttachment),
    ImageData(#[serde(with = "serde_bytes")] Vec<u8>),
    JobRuns(Vec<JobRun>),
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

/// The outcome of checking an SSH gateway during setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayProbe {
    /// The host key is trusted and the remote reports this `agent206 --version`.
    Reachable { version: String },
    /// The host is not in known_hosts; it offers these keys for the user to verify.
    Unverified { keys: Vec<HostKey> },
}

/// A client's gateway setup request to the web bridge, which runs SSH and owns
/// known_hosts on the browser's behalf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetupRequest {
    Status,
    /// Check an SSH endpoint with strict host key checking.
    Probe {
        endpoint: String,
    },
    /// Trust the key with this fingerprint, if the host still offers it, then check again.
    Trust {
        endpoint: String,
        fingerprint: String,
    },
    /// Verify, save and switch to a gateway.
    Save {
        target: GatewayTarget,
    },
}

/// What a client needs to show gateway setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupStatus {
    /// The gateway this client is using now: the saved one, or `--remote`.
    /// Setup saved by another client takes effect when this one restarts.
    pub active: Option<GatewayTarget>,
    /// True when `active` came from `--remote` rather than the saved setup.
    pub pinned: bool,
    /// Why no gateway is active, such as a damaged saved setup.
    pub problem: Option<ErrorReport>,
}
impl SetupStatus {
    /// True until this client has a gateway to use.
    pub fn required(&self) -> bool {
        self.active.is_none()
    }
}

/// Signing in to a web server opened to other devices. The vault broker checks
/// codes against the vault's enrolled authenticator; a loopback server signs
/// browsers in with its one-time link instead.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignInRequest {
    /// Which sign-in this server offers. Needs no session.
    Methods,
    Totp {
        code: String,
    },
    /// End the session presenting this request.
    SignOut,
}
impl std::fmt::Debug for SignInRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Methods => f.write_str("Methods"),
            Self::Totp { .. } => f.write_str("Totp { code: [REDACTED] }"),
            Self::SignOut => f.write_str("SignOut"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignInReply {
    /// `totp` is true when browsers sign in with an authenticator code.
    Methods {
        totp: bool,
    },
    /// The bearer for this browser's session on this server.
    SignedIn {
        session: String,
    },
    SignedOut,
    Error(ErrorReport),
}
impl std::fmt::Debug for SignInReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Methods { totp } => f.debug_struct("Methods").field("totp", totp).finish(),
            Self::SignedIn { .. } => f.write_str("SignedIn { session: [REDACTED] }"),
            Self::SignedOut => f.write_str("SignedOut"),
            Self::Error(report) => f.debug_tuple("Error").field(report).finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetupReply {
    Status(SetupStatus),
    Probe(GatewayProbe),
    Saved,
    Error(ErrorReport),
}

/// The web client's view of the vault on the bridge's computer. Unlocking,
/// creating and enrolling need the vault password or an enrollment key, which
/// never pass through a browser, so only reading and locking are offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VaultRequest {
    Status,
    Lock,
}

/// What the vault broker reports; `None` is unknown (locked, or no broker).
/// A stopped broker holds no unlocked vault, so its vault reads as locked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultState {
    pub broker_running: bool,
    pub exists: bool,
    pub unlocked: bool,
    pub totp_enrolled: Option<bool>,
    pub agent_access: Option<bool>,
    /// Whether the broker holds a gateway pairing for this unlock; unknown
    /// while locked or from an older bridge.
    #[serde(default)]
    pub gateway_paired: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VaultReply {
    State(VaultState),
    /// The vault is locked. Reading its state afterwards may still fail;
    /// that failure is reported beside the lock, never instead of it.
    Locked {
        state: Option<VaultState>,
        refresh: Option<ErrorReport>,
    },
    /// The bridge uses a remote gateway, whose vault lives on that host.
    Remote {
        endpoint: String,
    },
    Error(ErrorReport),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_creation_is_refused_by_gateways_without_profile_policy() {
        #[derive(Serialize, Deserialize)]
        enum LegacyCommand {
            CreateSubagent {
                parent: Id,
                title: String,
                mode: Mode,
                model: ModelKey,
                requested: BTreeSet<Capability>,
            },
        }
        let parent = Id::new_v4();
        let model = ModelKey {
            provider: Id::new_v4(),
            model: Id::new_v4().to_string(),
        };
        let title = Id::new_v4().to_string();
        let legacy = LegacyCommand::CreateSubagent {
            parent,
            title: title.clone(),
            mode: Mode::Plan,
            model: model.clone(),
            requested: BTreeSet::new(),
        };
        let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
        assert!(
            matches!(rmp_serde::from_slice::<Command>(&bytes).unwrap(), Command::CreateSubagent { parent: found, .. } if found == parent)
        );
        let command = Command::CreateProfileSubagent {
            parent,
            title,
            mode: Mode::Plan,
            model,
            requested: BTreeSet::new(),
            profile: Id::new_v4().to_string(),
        };
        let bytes = rmp_serde::to_vec_named(&command).unwrap();
        assert_eq!(rmp_serde::from_slice::<Command>(&bytes).unwrap(), command);
        assert!(rmp_serde::from_slice::<LegacyCommand>(&bytes).is_err());
    }
    #[test]
    fn settings_saved_before_later_sections_still_load() {
        let mut table: toml::Table = toml::from_str(
            &toml::to_string(&Settings {
                messengers: vec![MessengerConfig::new(
                    format!("m-{}", Id::new_v4().simple()),
                    MessengerKind::Telegram,
                )],
                ..Settings::default()
            })
            .unwrap(),
        )
        .unwrap();
        for section in ["backends", "messengers", "mcp_servers"] {
            table.remove(section);
        }
        let settings: Settings = toml::from_str(&toml::to_string(&table).unwrap()).unwrap();
        assert!(settings.messengers.is_empty() && settings.mcp_servers.is_empty());
    }
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
