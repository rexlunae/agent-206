//! The native `session_status` tool (RustyClaw's `session_status`): what a
//! model may know about the conversation it is answering in. It reports the
//! conversation's own mode, model, permissions, limits, usage and subagents,
//! and the gateway clock, so a model can name its conversation, pace itself
//! against its budget and see what its subagents are doing.
//!
//! It reads only the calling conversation and the subagent conversations it
//! started, never another conversation, and changes nothing, so it uses no
//! capability; like every tool it is denied until the user permits it in
//! the conversation. Usage figures are only what providers reported: a
//! figure some report left out, or a request that never finished, is
//! counted as missing rather than as zero.
use crate::{
    gateway::Statuses,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    Capability, ErrorKind, ErrorReport, Id, ModelKey, RunState, Snapshot, Tally, ToolPermission,
    UsageTotal,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const NAME: &str = "session_status";
/// Most subagent conversations listed; the newest are kept.
pub const MAX_SUBAGENTS: usize = 50;

/// Why the status could not be read.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("`{NAME}` takes no arguments; got `{0}`")]
    Unexpected(String),
    #[error("unknown status tool `{0}`")]
    UnknownTool(String),
    #[error("this conversation no longer exists")]
    NoConversation,
}
impl crate::error::Classified for StatusError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Unexpected(_) => ErrorKind::InvalidRequest,
            Self::UnknownTool(_) | Self::NoConversation => ErrorKind::NotFound,
        })
    }
}
fn fail(error: StatusError) -> ErrorReport {
    crate::error::report(&error)
}

/// Offers `session_status` to native runs.
pub struct StatusHost {
    statuses: Statuses,
}
impl StatusHost {
    pub fn new(statuses: Statuses) -> Self {
        Self { statuses }
    }

    fn spec() -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: "Read this conversation's status: its ID, title, mode, model and the model's API-reported limits, the permissions it has, its per-response limits, the usage providers reported for it and for its subagents, its subagent conversations and their states, and the gateway's current time. Missing figures are unknown, not zero.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            capabilities: BTreeSet::new(),
            permission: ToolPermission::Allow,
            // The result is bounded, and a cut status would misreport.
            whole_output: true,
        }
    }
}

impl ToolHost for StatusHost {
    fn list<'a>(&'a self, _scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            ToolListing {
                tools: vec![Self::spec()],
                unavailable: vec![],
            }
        })
    }
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            if request.name != NAME {
                return Err(fail(StatusError::UnknownTool(request.name)));
            }
            if let Some(key) = request.arguments.keys().next() {
                return Err(fail(StatusError::Unexpected(key.clone())));
            }
            let status = self
                .statuses
                .read(|state, now| describe(state, scope.thread, &scope.capabilities, now))??;
            Ok(ToolOutput {
                text: status.to_string(),
                is_error: false,
            })
        })
    }
}

/// `thread`'s status in `state` at gateway time `now`, with the permissions
/// its current tool scope holds.
pub fn describe(
    state: &Snapshot,
    thread: Id,
    permissions: &BTreeSet<Capability>,
    now: u64,
) -> Result<Value, ErrorReport> {
    let current = state
        .threads
        .iter()
        .find(|t| t.id == thread)
        .ok_or_else(|| fail(StatusError::NoConversation))?;
    let connection = |provider: Id| {
        state
            .settings
            .providers
            .iter()
            .find(|p| p.id == provider)
            .map(|p| p.label.clone())
    };
    let key = |key: &ModelKey| json!({"connection": connection(key.provider), "model": key.model});
    let model = current.model.as_ref().map(|model| {
        let info = state
            .catalogs
            .iter()
            .filter(|catalog| catalog.provider == model.provider)
            .flat_map(|catalog| &catalog.models)
            .find(|info| info.key == *model);
        let mut value = key(model);
        value["name"] = json!(info.map(|info| &info.name));
        value["context_limit"] = json!(info.and_then(|info| info.context_limit));
        value["output_limit"] = json!(info.and_then(|info| info.output_limit));
        value
    });
    let parent = current.parent.map(|parent| {
        json!({
            "id": parent,
            "title": state.threads.iter().find(|t| t.id == parent).map(|t| &t.title),
        })
    });
    let project = current
        .project
        .and_then(|project| state.projects.iter().find(|p| p.id == project))
        .map(|project| &project.name);
    let children: Vec<_> = state
        .threads
        .iter()
        .filter(|t| t.parent == Some(thread))
        .collect();
    let subagents: Vec<_> = children
        .iter()
        .skip(children.len().saturating_sub(MAX_SUBAGENTS))
        .map(|child| {
            json!({
                "id": child.id,
                "title": child.title,
                "state": state_name(&child.state),
                "paused": child.paused,
            })
        })
        .collect();
    let usage = |total: &UsageTotal| {
        let tally = |tally: &Tally<u64>| {
            json!({
                "reported": tally.sum,
                "responses_without_it": tally.unreported,
                "exact": total.exact(tally).is_some(),
            })
        };
        json!({
            "counted_from_start": total.from_start,
            "responses": total.reports,
            "unfinished_requests": total.incomplete,
            "input_tokens": tally(&total.input_tokens),
            "output_tokens": tally(&total.output_tokens),
            // Units differ between providers, so costs are never summed.
            "costs": total.costs.iter().map(|cost| json!({
                "connection": connection(cost.provider),
                "reported": cost.cost.sum,
                "responses_without_it": cost.cost.unreported,
            })).collect::<Vec<_>>(),
        })
    };
    Ok(json!({
        "conversation": {
            "id": current.id,
            "title": current.title,
            "mode": current.mode.label(),
            "project": project,
            "parent": parent,
            "state": state_name(&current.state),
            "paused": current.paused,
        },
        "model": model,
        "subagent_models": current.subagent_models.iter().map(key).collect::<Vec<_>>(),
        "permissions": permissions.iter().map(|c| c.label()).collect::<Vec<_>>(),
        "limits_per_response": {
            "model_requests": current.tool_budget.rounds,
            "tool_calls": current.tool_budget.tool_calls,
            "tokens": current.tool_budget.tokens,
        },
        "usage": {
            "this_conversation": usage(&current.usage_total),
            "with_subagents": usage(&UsageTotal::with_subagents(&state.threads, thread)),
        },
        "subagents": {
            "total": children.len(),
            "newest": subagents,
        },
        "gateway_time_unix": now,
    }))
}

fn state_name(state: &RunState) -> &'static str {
    match state {
        RunState::Idle => "idle",
        RunState::Running => "running",
        RunState::Complete => "complete",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests;
