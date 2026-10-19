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
    tools::{self, ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    Capability, ErrorKind, ErrorReport, Id, ModelKey, RunState, Snapshot, Tally, Thread,
    ToolBudget, ToolPermission, UsageTotal,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const NAME: &str = "session_status";
/// Most subagent conversations listed; the newest are kept.
pub const MAX_SUBAGENTS: usize = 50;
/// Longest text value reported, in bytes; longer ones are cut and end in `…`.
pub const MAX_TEXT: usize = 512;
/// Serialized bytes each list may use: the subagents get a quarter of the
/// output limit and the three other lists an eighth each, so with the
/// bounded text values the whole status stays under [`tools::MAX_OUTPUT`]
/// and is never refused for its size.
const SUBAGENT_ROOM: usize = tools::MAX_OUTPUT / 4;
const LIST_ROOM: usize = tools::MAX_OUTPUT / 8;

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
            .map(|p| text(&p.label))
    };
    let key =
        |key: &ModelKey| json!({"connection": connection(key.provider), "model": text(&key.model)});
    let model = current.model.as_ref().map(|model| {
        let listed = state
            .catalogs
            .iter()
            .filter(|catalog| catalog.provider == model.provider)
            .find_map(|catalog| {
                let info = catalog.models.iter().find(|info| info.key == *model)?;
                Some((catalog, info))
            });
        let mut value = key(model);
        value["name"] = json!(listed.map(|(_, info)| text(&info.name)));
        value["context_limit"] = json!(listed.and_then(|(_, info)| info.context_limit));
        value["output_limit"] = json!(listed.and_then(|(_, info)| info.output_limit));
        // Where the figures came from, and whether they have expired.
        value["catalog"] = json!(listed.map(|(catalog, _)| json!({
            "source": text(&catalog.source),
            "fetched_at_unix": catalog.fetched_at,
            "expires_at_unix": catalog.expires_at,
            "stale": catalog.stale(now),
        })));
        value
    });
    let parent = current.parent.map(|parent| {
        json!({
            "id": parent,
            "title": state.threads.iter().find(|t| t.id == parent).map(|t| text(&t.title)),
        })
    });
    let project = current
        .project
        .and_then(|project| state.projects.iter().find(|p| p.id == project))
        .map(|project| text(&project.name));
    let children: Vec<_> = state
        .threads
        .iter()
        .filter(|t| t.parent == Some(thread))
        .collect();
    let subagents = children
        .iter()
        .skip(children.len().saturating_sub(MAX_SUBAGENTS))
        .map(|child| {
            json!({
                "id": child.id,
                "title": text(&child.title),
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
            "costs": listing(
                total.costs.len(),
                total.costs.iter().map(|cost| json!({
                    "connection": connection(cost.provider),
                    "reported": cost.cost.sum,
                    "responses_without_it": cost.cost.unreported,
                })).collect(),
                LIST_ROOM,
                Keep::First,
            ),
        })
    };
    let budget = effective_budget(state, current);
    Ok(json!({
        "conversation": {
            "id": current.id,
            "title": text(&current.title),
            "mode": current.mode.label(),
            "project": project,
            "parent": parent,
            "state": state_name(&current.state),
            "paused": current.paused,
        },
        "model": model,
        "subagent_models": listing(
            current.subagent_models.len(),
            current.subagent_models.iter().map(key).collect(),
            LIST_ROOM,
            Keep::First,
        ),
        "permissions": permissions.iter().map(|c| c.label()).collect::<Vec<_>>(),
        // What the run engine enforces, which a parent's smaller limits lower.
        "limits_per_response": {
            "model_requests": budget.rounds,
            "tool_calls": budget.tool_calls,
            "tokens": budget.tokens,
        },
        "usage": {
            "this_conversation": usage(&current.usage_total),
            "with_subagents": usage(&UsageTotal::with_subagents(&state.threads, thread)),
        },
        "subagents": listing(children.len(), subagents, SUBAGENT_ROOM, Keep::Last),
        "gateway_time_unix": now,
    }))
}

/// The per-response limits a run of `thread` enforces: for each limit, the
/// smallest of its own and every ancestor's, as the run engine applies them.
fn effective_budget(state: &Snapshot, thread: &Thread) -> ToolBudget {
    let mut budget = thread.tool_budget;
    let mut next = thread.parent;
    // At most one step per conversation, so even a malformed cycle ends.
    for _ in 0..state.threads.len() {
        let Some(parent) = next.and_then(|id| state.threads.iter().find(|t| t.id == id)) else {
            break;
        };
        budget = budget.intersect(parent.tool_budget);
        next = parent.parent;
    }
    budget
}

/// Which end of a list to keep when it does not fit.
#[derive(Clone, Copy)]
enum Keep {
    First,
    Last,
}

/// `items` (out of `total`) that fit in `room` serialized bytes, kept from
/// the front or the back, with how many there are and how many were left out.
fn listing(total: usize, items: Vec<Value>, room: usize, keep: Keep) -> Value {
    let mut room = room;
    let mut fits = |item: &Value| {
        // The item and the comma separating it.
        let size = item.to_string().len() + 1;
        let fits = size <= room;
        room = room.saturating_sub(size);
        fits
    };
    let mut listed: Vec<Value> = match keep {
        Keep::First => items.into_iter().take_while(|item| fits(item)).collect(),
        Keep::Last => items
            .into_iter()
            .rev()
            .take_while(|item| fits(item))
            .collect(),
    };
    if let Keep::Last = keep {
        listed.reverse();
    }
    json!({
        "total": total,
        "listed": listed.len(),
        "omitted": total - listed.len(),
        "items": listed,
    })
}

/// `value` cut to [`MAX_TEXT`] bytes at a character boundary.
fn text(value: &str) -> String {
    if value.len() <= MAX_TEXT {
        return value.into();
    }
    let mut end = MAX_TEXT;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
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
