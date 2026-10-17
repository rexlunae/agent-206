//! Subagents for the native run engine: a [`ToolHost`] that lets a model start
//! and supervise subagent conversations, as RustyClaw's `sessions_*` tools do.
//!
//! A subagent is an ordinary delegated thread, created through the same
//! `CreateSubagent` path as one the user creates: its model must be one the
//! user permitted for this conversation's subagents, and its grant is the
//! intersection of this conversation's grant, the requested capabilities and
//! its mode's ceiling. It starts with this conversation's tool permissions
//! except these tools, so it cannot delegate further unless the user permits
//! that on it; every decision still takes the minimum along the delegation
//! chain. The tools are offered only to conversations the user gave subagent
//! models, and each conversation denies them until the user permits them.
//!
//! The host holds a [`Delegator`], not the gateway: it can create, message,
//! cancel and read subagents, and nothing that changes a grant, a permission
//! or an approval. A model addresses only its own conversation's subagents.
use crate::{
    gateway::{Caller, Child, Delegator},
    tools::{self, ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    Capability, ErrorKind, ErrorReport, Id, Mode, ModelKey, RunState, ToolPermission,
    UnavailableTools,
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Starts a subagent on a task.
pub const SPAWN: &str = "subagent_spawn";
/// Sends a finished subagent another message.
pub const SEND: &str = "subagent_send";
/// Lists this conversation's subagents.
pub const LIST: &str = "subagent_list";
/// Shows one subagent's state and latest reply.
pub const STATUS: &str = "subagent_status";
/// Waits for a subagent's run to end.
pub const WAIT: &str = "subagent_wait";
/// Cancels a subagent's run.
pub const CANCEL: &str = "subagent_cancel";
/// Every subagent tool; a subagent does not inherit permission for them.
pub const ALL: [&str; 6] = [SPAWN, SEND, LIST, STATUS, WAIT, CANCEL];
/// The source named when subagents cannot be listed.
pub const SOURCE: &str = "subagents";
/// Most conversations a delegating conversation may itself descend from.
pub const MAX_DEPTH: usize = 3;
/// Most subagents of one conversation running at once.
pub const MAX_RUNNING: usize = 8;
/// Longest `subagent_wait`, in seconds.
pub const MAX_WAIT: u64 = 600;
const DEFAULT_WAIT: u64 = 60;
/// Longest title derived from a task.
const TITLE: usize = 80;

/// Why a subagent tool call failed.
#[derive(Debug, thiserror::Error)]
pub enum DelegationToolError {
    #[error("`{0}` is required")]
    Missing(&'static str),
    #[error("`{0}` has the wrong type")]
    Type(&'static str),
    #[error("unknown argument `{0}`")]
    Unknown(String),
    #[error("unknown subagent tool `{0}`")]
    UnknownTool(String),
    #[error("`thread` is not a subagent ID")]
    Id(#[source] uuid::Error),
    #[error("no subagent of this conversation has that ID")]
    NotAChild,
    #[error("unknown mode `{0}`")]
    Mode(String),
    #[error("unknown capability `{0}`")]
    Capability(String),
    #[error("the user has not permitted any subagent models for this conversation")]
    NoModels,
    #[error("several subagent models are permitted; name one with `model`")]
    ChooseModel,
    #[error("model `{0}` is not permitted for this conversation's subagents")]
    ModelDenied(String),
    #[error("model `{0}` matches several providers; name it as `provider/model`")]
    AmbiguousModel(String),
    #[error("subagents may delegate at most {limit} levels deep")]
    TooDeep { limit: usize },
    #[error("this conversation already has {limit} subagents running; wait for one to finish")]
    TooMany { limit: usize },
    #[error("the subagent is still running; wait for it or cancel it first")]
    Busy,
    #[error("`seconds` must be 1–{MAX_WAIT}")]
    WaitRange,
}
impl crate::error::Classified for DelegationToolError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Missing(_)
            | Self::Type(_)
            | Self::Unknown(_)
            | Self::Id(_)
            | Self::Mode(_)
            | Self::Capability(_)
            | Self::ChooseModel
            | Self::AmbiguousModel(_)
            | Self::WaitRange => ErrorKind::InvalidRequest,
            Self::UnknownTool(_) | Self::NotAChild => ErrorKind::NotFound,
            Self::NoModels | Self::ModelDenied(_) | Self::TooDeep { .. } => {
                ErrorKind::PermissionDenied
            }
            Self::TooMany { .. } | Self::Busy => ErrorKind::Conflict,
        })
    }
}
fn fail(error: DelegationToolError) -> ErrorReport {
    crate::error::report(&error)
}

/// A run this host started, until it ends.
struct Started {
    run: Id,
    done: CancellationToken,
}
/// Ends a started run's record however its task ends, so waiters always wake.
struct Finish {
    runs: Arc<Mutex<Runs>>,
    child: Id,
    run: Id,
    done: CancellationToken,
    refused: Option<ErrorReport>,
}
impl Finish {
    /// Record why the gateway refused the message, if it did.
    fn ended(&mut self, refused: Option<ErrorReport>) {
        self.refused = refused;
    }
}
impl Drop for Finish {
    fn drop(&mut self) {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        if runs
            .active
            .get(&self.child)
            .is_some_and(|s| s.run == self.run)
        {
            runs.active.remove(&self.child);
            if let Some(report) = self.refused.take() {
                runs.refused.insert(self.child, report);
            }
        }
        self.done.cancel();
    }
}
/// What this host knows about the runs it started.
#[derive(Default)]
struct Runs {
    active: BTreeMap<Id, Started>,
    /// Messages the gateway refused to run, by subagent, until the next one.
    refused: BTreeMap<Id, ErrorReport>,
}

/// Offers the subagent tools to native runs.
pub struct DelegationHost {
    delegator: Arc<Delegator>,
    runs: Arc<Mutex<Runs>>,
}
impl DelegationHost {
    pub fn new(delegator: Delegator) -> Self {
        Self {
            delegator: Arc::new(delegator),
            runs: Arc::default(),
        }
    }

    /// Run `child` on `text` in the background. The run belongs to the
    /// gateway like any other: the user sees it in the subagent's thread and
    /// can cancel it, and cancelling this conversation's run cancels it too.
    fn start(&self, child: Id, text: String) {
        let run = Id::new_v4();
        let done = CancellationToken::new();
        {
            let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            runs.refused.remove(&child);
            runs.active.insert(
                child,
                Started {
                    run,
                    done: done.clone(),
                },
            );
        }
        let delegator = self.delegator.clone();
        let mut finish = Finish {
            runs: self.runs.clone(),
            child,
            run,
            done,
            refused: None,
        };
        // Detached: the run outlives the tool call that started it, and ends
        // when it finishes, is cancelled or the gateway shuts down, which
        // cancels every run. Its outcome is stored in the subagent's thread.
        drop(tokio::spawn(async move {
            finish.ended(delegator.send(child, text).await.err());
        }));
    }

    fn specs(caller: &Caller) -> Vec<ToolSpec> {
        let thread = json!({"type": "string", "description": "The subagent's ID, as subagent_spawn or subagent_list returned it."});
        let modes: Vec<Value> = Mode::ALL.iter().map(|m| json!(m)).collect();
        let capabilities: Vec<Value> = caller.capabilities.iter().map(|c| json!(c)).collect();
        let models: Vec<Value> = model_names(&caller.subagent_models)
            .into_values()
            .map(Value::String)
            .collect();
        let spec = |name: &str, description: &str, parameters: Value| ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters,
            capabilities: Default::default(),
            permission: ToolPermission::Allow,
        };
        vec![
            spec(
                SPAWN,
                "Start a subagent: a separate conversation that works on `task` in the background while you continue. It sees only the task, not this conversation, so include everything it needs. Its permissions are at most yours. Check on it with subagent_wait or subagent_status.",
                json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": "Complete instructions for the subagent."},
                        "title": {"type": "string", "description": "A short name for the subagent's conversation."},
                        "mode": {"type": "string", "enum": modes, "description": "The subagent's mode; defaults to yours."},
                        "model": {"type": "string", "enum": models, "description": "Defaults to your model when it is permitted for subagents."},
                        "capabilities": {"type": "array", "items": {"type": "string", "enum": capabilities}, "description": "Capabilities to give it; defaults to all of yours its mode allows."}
                    },
                    "required": ["task"],
                    "additionalProperties": false
                }),
            ),
            spec(
                SEND,
                "Send a subagent whose run has ended another message; it continues in the background.",
                json!({
                    "type": "object",
                    "properties": {"thread": thread, "message": {"type": "string"}},
                    "required": ["thread", "message"],
                    "additionalProperties": false
                }),
            ),
            spec(
                LIST,
                "List your subagents and whether each is running.",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
            ),
            spec(
                STATUS,
                "Show a subagent's state, its latest reply and any failure.",
                json!({
                    "type": "object",
                    "properties": {"thread": thread},
                    "required": ["thread"],
                    "additionalProperties": false
                }),
            ),
            spec(
                WAIT,
                "Wait until a subagent's run ends, or `seconds` pass, then show its status.",
                json!({
                    "type": "object",
                    "properties": {
                        "thread": thread,
                        "seconds": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT, "description": format!("Defaults to {DEFAULT_WAIT}.")}
                    },
                    "required": ["thread"],
                    "additionalProperties": false
                }),
            ),
            spec(
                CANCEL,
                "Cancel a subagent's run, and its own subagents' runs.",
                json!({
                    "type": "object",
                    "properties": {"thread": thread},
                    "required": ["thread"],
                    "additionalProperties": false
                }),
            ),
        ]
    }

    async fn run(&self, scope: &ToolScope, request: &ToolRequest) -> Result<Value, ErrorReport> {
        let arguments = &request.arguments;
        let (caller, children) = self.delegator.family(scope.thread)?;
        match request.name.as_str() {
            SPAWN => {
                check_keys(
                    arguments,
                    &["task", "title", "mode", "model", "capabilities"],
                )
                .map_err(fail)?;
                let task = text(arguments, "task")
                    .and_then(|t| t.ok_or(DelegationToolError::Missing("task")))
                    .map_err(fail)?;
                let title = match text(arguments, "title").map_err(fail)? {
                    Some(title) => title,
                    None => title_of(&task),
                };
                let mode = match text(arguments, "mode").map_err(fail)? {
                    Some(mode) => parse(&mode).ok_or(DelegationToolError::Mode(mode)),
                    None => Ok(caller.mode),
                }
                .map_err(fail)?;
                let requested = match arguments.get("capabilities") {
                    None | Some(Value::Null) => Ok(caller.capabilities.clone()),
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|item| match item {
                            Value::String(name) => parse::<Capability>(name)
                                .ok_or_else(|| DelegationToolError::Capability(name.clone())),
                            _ => Err(DelegationToolError::Type("capabilities")),
                        })
                        .collect::<Result<BTreeSet<_>, _>>(),
                    Some(_) => Err(DelegationToolError::Type("capabilities")),
                }
                .map_err(fail)?;
                let model =
                    choose_model(&caller, text(arguments, "model").map_err(fail)?).map_err(fail)?;
                if caller.ancestors >= MAX_DEPTH {
                    return Err(fail(DelegationToolError::TooDeep { limit: MAX_DEPTH }));
                }
                if self.running(&children) >= MAX_RUNNING {
                    return Err(fail(DelegationToolError::TooMany { limit: MAX_RUNNING }));
                }
                let child = self
                    .delegator
                    .create(scope.thread, title, mode, model, requested, &ALL)
                    .await?;
                self.start(child.id, task);
                Ok(json!({
                    "thread": child.id,
                    "title": child.title,
                    "mode": child.mode,
                    "model": child.model.as_ref().map(|m| &m.model),
                    "capabilities": child.capabilities,
                    "state": "running",
                }))
            }
            SEND => {
                check_keys(arguments, &["thread", "message"]).map_err(fail)?;
                let child = find(&children, arguments).map_err(fail)?;
                let message = text(arguments, "message")
                    .and_then(|t| t.ok_or(DelegationToolError::Missing("message")))
                    .map_err(fail)?;
                if child.state == RunState::Running || self.started(child.id).is_some() {
                    return Err(fail(DelegationToolError::Busy));
                }
                if self.running(&children) >= MAX_RUNNING {
                    return Err(fail(DelegationToolError::TooMany { limit: MAX_RUNNING }));
                }
                self.start(child.id, message);
                Ok(json!({"thread": child.id, "state": "running"}))
            }
            LIST => {
                check_keys(arguments, &[]).map_err(fail)?;
                Ok(Value::Array(
                    children
                        .iter()
                        .map(|child| {
                            json!({
                                "thread": child.id,
                                "title": child.title,
                                "mode": child.mode,
                                "state": self.state(child),
                            })
                        })
                        .collect(),
                ))
            }
            STATUS => {
                check_keys(arguments, &["thread"]).map_err(fail)?;
                let child = find(&children, arguments).map_err(fail)?;
                Ok(self.status(child))
            }
            WAIT => {
                check_keys(arguments, &["thread", "seconds"]).map_err(fail)?;
                let child = find(&children, arguments).map_err(fail)?;
                let seconds = match arguments.get("seconds") {
                    None | Some(Value::Null) => Ok(DEFAULT_WAIT),
                    Some(Value::Number(n)) => n
                        .as_u64()
                        .filter(|s| (1..=MAX_WAIT).contains(s))
                        .ok_or(DelegationToolError::WaitRange),
                    Some(_) => Err(DelegationToolError::Type("seconds")),
                }
                .map_err(fail)?;
                let id = child.id;
                // Waiting is cancel-safe: dropping this call stops only the wait.
                if let Some(done) = self.started(id) {
                    let ended =
                        tokio::time::timeout(Duration::from_secs(seconds), done.cancelled())
                            .await
                            .is_ok();
                    if !ended {
                        let mut status = self.status(child);
                        status["waited_seconds"] = json!(seconds);
                        return Ok(status);
                    }
                }
                let (_, children) = self.delegator.family(scope.thread)?;
                let child = children
                    .iter()
                    .find(|c| c.id == id)
                    .ok_or_else(|| fail(DelegationToolError::NotAChild))?;
                Ok(self.status(child))
            }
            CANCEL => {
                check_keys(arguments, &["thread"]).map_err(fail)?;
                let child = find(&children, arguments).map_err(fail)?;
                self.delegator.cancel(child.id).await?;
                Ok(json!({"thread": child.id, "state": "cancelling"}))
            }
            other => Err(fail(DelegationToolError::UnknownTool(other.into()))),
        }
    }

    /// Subagents running, or started here and about to run.
    fn running(&self, children: &[Child]) -> usize {
        children
            .iter()
            .filter(|c| self.state(c) == "running")
            .count()
    }
    fn started(&self, child: Id) -> Option<CancellationToken> {
        let runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        runs.active.get(&child).map(|s| s.done.clone())
    }
    /// A run this host started counts as running until the gateway has
    /// stored its outcome, even before the thread shows it.
    fn state(&self, child: &Child) -> &'static str {
        {
            let runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            if runs.active.contains_key(&child.id) {
                return "running";
            }
            if child.state != RunState::Running && runs.refused.contains_key(&child.id) {
                return "refused";
            }
        }
        match child.state {
            RunState::Idle => "idle",
            RunState::Running => "running",
            RunState::Complete => "complete",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
        }
    }
    fn status(&self, child: &Child) -> Value {
        let refused = self
            .runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refused
            .get(&child.id)
            .cloned();
        let state = self.state(child);
        let error = match state {
            "refused" => refused.as_ref(),
            _ => child.error.as_ref(),
        };
        // Keep the reply within one tool result, with room for the rest.
        let reply = child
            .reply
            .as_deref()
            .map(|reply| truncate(reply, tools::MAX_OUTPUT - 4096));
        json!({
            "thread": child.id,
            "title": child.title,
            "state": state,
            "reply": reply,
            "error": error.map(|e| &e.message),
            "waiting_for_approval": child.approvals,
        })
    }
}

/// The name the model uses for each permitted model: its own name, or
/// `provider/model` where several providers offer the same name.
fn model_names(models: &BTreeSet<ModelKey>) -> BTreeMap<ModelKey, String> {
    models
        .iter()
        .map(|key| {
            let shared = models
                .iter()
                .filter(|other| other.model == key.model)
                .count()
                > 1;
            let name = if shared {
                format!("{}/{}", key.provider, key.model)
            } else {
                key.model.clone()
            };
            (key.clone(), name)
        })
        .collect()
}
fn choose_model(caller: &Caller, named: Option<String>) -> Result<ModelKey, DelegationToolError> {
    let permitted = &caller.subagent_models;
    if permitted.is_empty() {
        return Err(DelegationToolError::NoModels);
    }
    let Some(named) = named else {
        if let Some(own) = caller.model.as_ref().filter(|m| permitted.contains(m)) {
            return Ok(own.clone());
        }
        let mut models = permitted.iter();
        return match (models.next(), models.next()) {
            (Some(only), None) => Ok(only.clone()),
            _ => Err(DelegationToolError::ChooseModel),
        };
    };
    if let Some(key) = permitted
        .iter()
        .find(|key| format!("{}/{}", key.provider, key.model) == named)
    {
        return Ok(key.clone());
    }
    let mut matching = permitted.iter().filter(|key| key.model == named);
    match (matching.next(), matching.next()) {
        (Some(key), None) => Ok(key.clone()),
        (Some(_), Some(_)) => Err(DelegationToolError::AmbiguousModel(named)),
        (None, _) => Err(DelegationToolError::ModelDenied(named)),
    }
}
fn find<'a>(
    children: &'a [Child],
    arguments: &Map<String, Value>,
) -> Result<&'a Child, DelegationToolError> {
    let id: Id = text(arguments, "thread")?
        .ok_or(DelegationToolError::Missing("thread"))?
        .parse()
        .map_err(DelegationToolError::Id)?;
    children
        .iter()
        .find(|c| c.id == id)
        .ok_or(DelegationToolError::NotAChild)
}
fn parse<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(Value::String(name.into())).ok()
}
fn title_of(task: &str) -> String {
    let line = task.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    truncate(line.trim(), TITLE)
}
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
fn check_keys(arguments: &Map<String, Value>, known: &[&str]) -> Result<(), DelegationToolError> {
    match arguments.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(DelegationToolError::Unknown(key.clone())),
        None => Ok(()),
    }
}
fn text(
    arguments: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<String>, DelegationToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(DelegationToolError::Type(key)),
    }
}

impl ToolHost for DelegationHost {
    /// The tools are offered only to conversations with subagent models.
    fn list<'a>(&'a self, scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            match self.delegator.family(scope.thread) {
                Ok((caller, _)) if caller.subagent_models.is_empty() => ToolListing::default(),
                Ok((caller, _)) => ToolListing {
                    tools: Self::specs(&caller),
                    unavailable: vec![],
                },
                Err(report) => {
                    crate::error::record("list_subagent_tools", &report);
                    ToolListing {
                        tools: vec![],
                        unavailable: vec![UnavailableTools {
                            source: SOURCE.into(),
                            error: report,
                        }],
                    }
                }
            }
        })
    }
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            self.run(scope, &request).await.map(|value| ToolOutput {
                text: value.to_string(),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests;
