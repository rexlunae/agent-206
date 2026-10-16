//! The native run engine: model → tool calls → policy → approval → execution
//! → results back to the model, until it stops, a budget is used up or the
//! user cancels.
//!
//! The gateway decides every call; tool hosts only execute what was decided.
//! Each decision re-reads the conversation, so a permission the user revokes
//! mid-run applies to the next call. Approvals are answered only through
//! [`Command::ResolveApproval`] from a client; the model sees no approval ID
//! and no tool can resolve one.
use super::{Gateway, GatewayError, PartialReply, Result, poisoned};
use crate::{
    error::check,
    provider::{self, now},
    store::Store,
    tool_calls::{self, ArgumentError},
    tools::{self, ToolHost, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::*;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

/// A run that stopped because it used up the conversation's budget.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(
        "this run used its budget of {rounds} model rounds; send a message to continue or raise the conversation's tool budget"
    )]
    Rounds { rounds: u32 },
    #[error(
        "this run used its budget of {calls} tool calls; send a message to continue or raise the conversation's tool budget"
    )]
    ToolCalls { calls: u32 },
}
impl crate::error::Classified for RunError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(ErrorKind::PermissionDenied)
    }
}

/// Why one tool call did not produce a result. Its display text is what the
/// model receives; the captured report is what clients receive.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool `{0}` is not available in this conversation")]
    Unavailable(String),
    #[error("tool `{0}` is not permitted in this conversation")]
    Denied(String),
    #[error("tool `{tool}` needs capabilities this conversation was not granted: {missing:?}")]
    Capability {
        tool: String,
        missing: BTreeSet<Capability>,
    },
    #[error("the user denied this tool call")]
    Rejected,
    #[error("the run was cancelled before this tool call finished")]
    Cancelled,
    #[error("this call repeats an earlier call in the same response, which runs once")]
    Duplicate,
    #[error("the run's tool budget was used up before this call")]
    Budget(#[source] RunError),
    #[error("the tool arguments are unusable")]
    Arguments(#[source] ArgumentError),
    #[error("tool `{tool}` failed")]
    Host {
        tool: String,
        #[source]
        cause: ErrorReport,
    },
    #[error("tool `{0}` reported an error")]
    Reported(String),
}
impl crate::error::Classified for ToolError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Unavailable(_) => ErrorKind::NotFound,
            Self::Denied(_) | Self::Capability { .. } | Self::Rejected => {
                ErrorKind::PermissionDenied
            }
            Self::Cancelled => ErrorKind::Cancelled,
            Self::Duplicate => ErrorKind::Conflict,
            Self::Reported(_) => ErrorKind::Other,
            Self::Budget(_) | Self::Arguments(_) | Self::Host { .. } => return None,
        })
    }
}

/// A run waiting for the user's answer to one approval.
pub(super) struct Waiter {
    thread: Id,
    digest: String,
    decision: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

/// How a native run ended.
pub(super) enum Ended {
    Complete(Usage),
    Cancelled,
    Failed(GatewayError),
}
/// What a native run needs that does not change between rounds.
pub(super) struct Native {
    pub thread: Id,
    pub config: ProviderConfig,
    pub info: ModelInfo,
    pub prompt: String,
    pub root: PathBuf,
    pub cancel: CancellationToken,
}
/// The conversation as of one decision: the thread, its ancestors and the
/// scope its tools run in.
struct View {
    thread: Thread,
    ancestors: Vec<Thread>,
    scope: ToolScope,
}
/// Add usage only while every round reported it; an unreported count makes
/// the total unknown rather than an undercount.
fn add_usage(total: &mut Option<Usage>, round: Usage) {
    fn add<T: std::ops::Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
        Some(a? + b?)
    }
    *total = Some(match total.take() {
        None => round,
        Some(sum) => Usage {
            input_tokens: add(sum.input_tokens, round.input_tokens),
            output_tokens: add(sum.output_tokens, round.output_tokens),
            cost: add(sum.cost, round.cost),
        },
    });
}
fn bounded_output(mut text: String) -> String {
    if text.len() > tools::MAX_OUTPUT {
        let mut end = tools::MAX_OUTPUT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(&format!(
            "\n[output truncated by the gateway at {end} bytes]"
        ));
    }
    text
}
/// Most ancestors followed when combining delegated permissions.
const MAX_LINEAGE: usize = 32;
fn view(store: &Store, id: Id) -> Result<View> {
    let thread = store.thread(id)?.clone();
    let mut ancestors = vec![];
    let mut parent = thread.parent;
    while let Some(next) = parent {
        check(
            ancestors.len() < MAX_LINEAGE,
            GatewayError::Invalid("conversation delegation is too deep"),
        )?;
        let ancestor = store.thread(next)?.clone();
        parent = ancestor.parent;
        ancestors.push(ancestor);
    }
    let workspace = thread
        .project
        .and_then(|p| store.state.projects.iter().find(|project| project.id == p))
        .map(|project| PathBuf::from(&project.root));
    let scope = ToolScope {
        thread: id,
        workspace,
        capabilities: tools::scope_capabilities(&thread),
    };
    Ok(View {
        thread,
        ancestors,
        scope,
    })
}
impl View {
    fn decide(&self, spec: &ToolSpec) -> (ToolPermission, BTreeSet<Capability>) {
        let ancestors: Vec<_> = self.ancestors.iter().collect();
        tools::decide(spec, &self.thread, &ancestors)
    }
}

impl Gateway {
    /// Add a source of tools. Hosts are consulted in the order they were added.
    pub fn with_tool_host(mut self, host: Arc<dyn ToolHost>) -> Self {
        Arc::make_mut(&mut self.tool_hosts).push(host);
        self
    }
    fn view(&self, id: Id) -> Result<View> {
        view(&*self.lock()?, id)
    }
    pub(super) async fn list_tools(&self, id: Id) -> Result<Reply> {
        let view = self.view(id)?;
        let catalog = tools::catalog(&self.tool_hosts, &view.scope).await;
        let ancestors: Vec<_> = view.ancestors.iter().collect();
        Ok(Reply::Tools(tools::offers(
            &catalog,
            &view.thread,
            &ancestors,
        )))
    }
    pub(super) fn set_tool_permission(
        &self,
        id: Id,
        tool: String,
        permission: Option<ToolPermission>,
    ) -> Result<Reply> {
        check(
            tools::valid_name(&tool),
            GatewayError::Invalid("tool names are 1–64 ASCII letters, digits, `_` or `-`"),
        )?;
        let mut store = self.lock()?;
        let view = view(&store, id)?;
        let ceiling = view
            .ancestors
            .iter()
            .map(|t| t.tool_permissions.get(&tool).copied().unwrap_or_default())
            .min();
        check(
            permission.is_none_or(|p| ceiling.is_none_or(|ceiling| p <= ceiling)),
            GatewayError::Denied("a delegated conversation cannot permit more than its parent"),
        )?;
        check(
            store.thread(id)?.tool_permissions.len() < 1024
                || store.thread(id)?.tool_permissions.contains_key(&tool),
            GatewayError::Invalid("too many tool permissions in one conversation"),
        )?;
        let thread = store.thread_mut(id)?;
        match permission {
            Some(permission) => thread.tool_permissions.insert(tool, permission),
            None => thread.tool_permissions.remove(&tool),
        };
        store.persist(Some(EventKind::ThreadChanged { id }))?;
        Ok(Reply::Ok)
    }
    pub(super) fn set_tool_budget(&self, id: Id, budget: ToolBudget) -> Result<Reply> {
        check(
            (1..=ToolBudget::MAX).contains(&budget.rounds) && budget.tool_calls <= ToolBudget::MAX,
            GatewayError::Invalid("tool budgets are 1–256 rounds and 0–256 tool calls"),
        )?;
        let mut store = self.lock()?;
        if let Some(parent) = store.thread(id)?.parent {
            let parent = store.thread(parent)?.tool_budget;
            check(
                budget.rounds <= parent.rounds && budget.tool_calls <= parent.tool_calls,
                GatewayError::Denied("a delegated conversation cannot exceed its parent's budget"),
            )?;
        }
        store.thread_mut(id)?.tool_budget = budget;
        store.persist(Some(EventKind::ThreadChanged { id }))?;
        Ok(Reply::Ok)
    }
    /// Deliver the user's decision to the run waiting on `approval`. The
    /// decision applies only if `digest` is the one of the pending call.
    pub(super) fn resolve_approval(
        &self,
        thread: Id,
        approval: Id,
        digest: &str,
        decision: ApprovalDecision,
    ) -> Result<Reply> {
        let waiter = {
            let mut approvals = self.approvals.lock().map_err(poisoned("approval"))?;
            let pending = approvals
                .get(&approval)
                .filter(|waiter| waiter.thread == thread)
                .ok_or(GatewayError::NotFound("this approval is no longer pending"))?;
            check(
                pending.digest == digest,
                GatewayError::Conflict(
                    "the approval does not match the pending tool call; review it again",
                ),
            )?;
            approvals
                .remove(&approval)
                .ok_or(GatewayError::NotFound("this approval is no longer pending"))?
        };
        waiter
            .decision
            .send(decision)
            .map_err(|_| GatewayError::NotFound("this approval expired with its run"))?;
        Ok(Reply::Ok)
    }

    /// Run model rounds until the model stops, the budget is used up or the
    /// run is cancelled. Messages and tool calls are journaled as they change.
    pub(super) async fn native_run(&self, run: &Native) -> Ended {
        match self.rounds(run).await {
            Ok(ended) => ended,
            Err(error) => Ended::Failed(error),
        }
    }
    async fn rounds(&self, run: &Native) -> Result<Ended> {
        let pairing = self.pairing()?;
        let mut usage = None;
        let mut calls_used = 0;
        let mut round = 0;
        loop {
            round += 1;
            let view = self.view(run.thread)?;
            let budget = view.thread.tool_budget;
            let catalog = tokio::select! {
                catalog = tools::catalog(&self.tool_hosts, &view.scope) => catalog,
                _ = run.cancel.cancelled() => return Ok(Ended::Cancelled),
            };
            for unavailable in &catalog.unavailable {
                let _source = tracing::warn_span!("tools", source = %unavailable.source).entered();
                crate::error::record("list_tools", &unavailable.error);
            }
            let offered: Vec<ToolSpec> = catalog
                .tools
                .values()
                .filter(|routed| view.decide(&routed.spec).0 != ToolPermission::Deny)
                .map(|routed| routed.spec.clone())
                .collect();
            let broker = provider::Broker {
                root: &run.root,
                pairing: pairing.as_ref(),
            };
            let (turn, message) = match self
                .stream_round(run, &view.thread, broker, &offered)
                .await?
            {
                Some(Ok(turn)) => turn,
                Some(Err(error)) => return Ok(Ended::Failed(error.into())),
                None => return Ok(Ended::Cancelled),
            };
            add_usage(&mut usage, turn.usage);
            if turn.calls.is_empty() {
                return Ok(Ended::Complete(usage.unwrap_or(Usage {
                    input_tokens: None,
                    output_tokens: None,
                    cost: None,
                })));
            }
            let parsed = self.record_calls(run.thread, message, turn.calls)?;
            if round >= budget.rounds {
                let error = RunError::Rounds {
                    rounds: budget.rounds,
                };
                self.stop_calls(
                    run.thread,
                    message,
                    &ToolError::Budget(RunError::Rounds {
                        rounds: budget.rounds,
                    }),
                )?;
                return Ok(Ended::Failed(error.into()));
            }
            for (call, arguments) in parsed {
                let Some(arguments) = arguments else { continue };
                if calls_used >= budget.tool_calls {
                    let calls = budget.tool_calls;
                    self.stop_calls(
                        run.thread,
                        message,
                        &ToolError::Budget(RunError::ToolCalls { calls }),
                    )?;
                    return Ok(Ended::Failed(RunError::ToolCalls { calls }.into()));
                }
                calls_used += 1;
                if !self
                    .decide_call(run, &catalog, message, call, arguments)
                    .await?
                {
                    self.stop_calls(run.thread, message, &ToolError::Cancelled)?;
                    return Ok(Ended::Cancelled);
                }
            }
        }
    }
    /// Stream one model response, publishing text as it arrives. `None` when
    /// the run was cancelled; partial text stays in the conversation.
    async fn stream_round(
        &self,
        run: &Native,
        thread: &Thread,
        broker: provider::Broker<'_>,
        offered: &[ToolSpec],
    ) -> Result<Option<Result<(provider::Turn, Id), provider::ProviderError>>> {
        let (sender, mut progress) = tokio::sync::mpsc::channel(32);
        let operation = provider::complete(
            &run.config,
            broker,
            thread,
            &run.prompt,
            &run.info,
            offered,
            &sender,
        );
        let mut operation = std::pin::pin!(operation);
        let mut partial = PartialReply::default();
        let result = loop {
            let due = partial.due();
            tokio::select! {
                result = &mut operation => break (!run.cancel.is_cancelled()).then_some(result),
                _ = run.cancel.cancelled() => break None,
                _ = tokio::time::sleep_until(due.unwrap_or_else(tokio::time::Instant::now)), if due.is_some() => {
                    let mut store = self.lock()?;
                    partial.publish(&mut store, run.thread)?;
                }
                Some(update) = progress.recv() => {
                    if partial.update(update) {
                        let mut store = self.lock()?;
                        partial.publish(&mut store, run.thread)?;
                    }
                }
            }
        };
        // Text sent before the stream ended still belongs to its reply.
        while let Ok(update) = progress.try_recv() {
            partial.update(update);
        }
        let mut store = self.lock()?;
        let thread = store.thread_mut(run.thread)?;
        let result = match result {
            Some(Ok(turn)) => {
                let message = partial.finish(thread, turn.text.clone());
                Some(Ok((turn, message)))
            }
            Some(Err(error)) => {
                partial.apply(thread);
                Some(Err(error))
            }
            None => {
                partial.apply(thread);
                None
            }
        };
        store.persist(Some(EventKind::ThreadChanged { id: run.thread }))?;
        Ok(result)
    }
    /// Attach the response's calls to its message. Calls whose arguments are
    /// unusable or that repeat an earlier call are settled here; the rest are
    /// returned with their arguments, in order, for a decision.
    fn record_calls(
        &self,
        id: Id,
        message: Id,
        calls: Vec<tool_calls::StreamedCall>,
    ) -> Result<Vec<(String, Option<tool_calls::Arguments>)>> {
        let mut records = vec![];
        let mut decisions = vec![];
        let mut seen_ids = BTreeSet::new();
        let mut seen_calls = BTreeSet::new();
        for streamed in calls {
            let (id, generated_id) = match streamed.id {
                Some(id) if seen_ids.insert(id.clone()) => (id, false),
                _ => (format!("call_{}", Id::new_v4().simple()), true),
            };
            seen_ids.insert(id.clone());
            let parsed = tool_calls::parse_arguments(&streamed.arguments);
            let (arguments, healed) = match &parsed {
                Ok(arguments) => (arguments.text.clone(), arguments.healed),
                Err(_) => (streamed.arguments.clone(), false),
            };
            let mut record = ToolCall {
                digest: tool_calls::digest(&streamed.name, &arguments),
                id: id.clone(),
                generated_id,
                tool: streamed.name,
                arguments,
                healed,
                state: ToolCallState::Requested,
                output: None,
                error: None,
                signature: streamed.signature,
            };
            let usable = match parsed {
                Err(error) => {
                    settle(
                        &mut record,
                        ToolCallState::Failed,
                        &ToolError::Arguments(error),
                    );
                    None
                }
                Ok(_) if !seen_calls.insert(record.digest.clone()) => {
                    settle(&mut record, ToolCallState::Skipped, &ToolError::Duplicate);
                    None
                }
                Ok(arguments) => Some(arguments),
            };
            decisions.push((id, usable));
            records.push(record);
        }
        let mut store = self.lock()?;
        message_mut(&mut store, id, message)?.tool_calls = records;
        store.persist(Some(EventKind::ThreadChanged { id }))?;
        Ok(decisions)
    }
    /// Settle every call of `message` that has not finished.
    fn stop_calls(&self, id: Id, message: Id, why: &ToolError) -> Result<()> {
        let mut store = self.lock()?;
        for call in &mut message_mut(&mut store, id, message)?.tool_calls {
            if !call.state.is_final() {
                settle(call, ToolCallState::Cancelled, why);
            }
        }
        store.persist(Some(EventKind::ThreadChanged { id }))?;
        Ok(())
    }
    fn update_call(
        &self,
        id: Id,
        message: Id,
        call: &str,
        event: EventKind,
        update: impl FnOnce(&mut Thread, &mut ToolCall),
    ) -> Result<()> {
        let mut store = self.lock()?;
        let thread = store.thread_mut(id)?;
        let mut record = thread
            .messages
            .iter()
            .find(|m| m.id == message)
            .and_then(|m| m.tool_calls.iter().find(|c| c.id == call))
            .cloned()
            .ok_or(GatewayError::NotFound("tool call not found"))?;
        update(thread, &mut record);
        if let Some(slot) = thread
            .messages
            .iter_mut()
            .find(|m| m.id == message)
            .and_then(|m| m.tool_calls.iter_mut().find(|c| c.id == call))
        {
            *slot = record;
        }
        store.persist(Some(event))?;
        Ok(())
    }
    /// Decide and, if permitted, run one call. `false` when the run was
    /// cancelled meanwhile.
    async fn decide_call(
        &self,
        run: &Native,
        catalog: &tools::Catalog,
        message: Id,
        call: String,
        arguments: tool_calls::Arguments,
    ) -> Result<bool> {
        let id = run.thread;
        let changed = EventKind::ThreadChanged { id };
        let record = {
            let store = self.lock()?;
            store
                .thread(id)?
                .messages
                .iter()
                .find(|m| m.id == message)
                .and_then(|m| m.tool_calls.iter().find(|c| c.id == call))
                .cloned()
                .ok_or(GatewayError::NotFound("tool call not found"))?
        };
        let refuse = |error: ToolError| {
            self.update_call(id, message, &call, changed.clone(), |_, record| {
                settle(record, ToolCallState::Denied, &error)
            })
        };
        let Some(routed) = catalog.tools.get(&record.tool) else {
            refuse(ToolError::Unavailable(record.tool.clone()))?;
            return Ok(true);
        };
        let view = self.view(id)?;
        let (permission, missing) = view.decide(&routed.spec);
        let approved = match permission {
            ToolPermission::Deny if !missing.is_empty() => {
                refuse(ToolError::Capability {
                    tool: record.tool.clone(),
                    missing,
                })?;
                return Ok(true);
            }
            ToolPermission::Deny => {
                refuse(ToolError::Denied(record.tool.clone()))?;
                return Ok(true);
            }
            ToolPermission::Allow => false,
            ToolPermission::Ask => {
                match self.ask(run, message, &record, &routed.spec).await? {
                    None => return Ok(false),
                    Some(ApprovalDecision::Deny) => {
                        refuse(ToolError::Rejected)?;
                        return Ok(true);
                    }
                    Some(ApprovalDecision::Approve) => {}
                }
                // The user may have revoked the tool while deciding.
                let view = self.view(id)?;
                if view.decide(&routed.spec).0 == ToolPermission::Deny {
                    refuse(ToolError::Denied(record.tool.clone()))?;
                    return Ok(true);
                }
                true
            }
        };
        self.update_call(id, message, &call, changed.clone(), |_, record| {
            record.state = ToolCallState::Running;
        })?;
        let request = ToolRequest {
            call: call.clone(),
            name: record.tool.clone(),
            arguments: arguments.object,
            approved,
        };
        let span = tracing::info_span!("tool_call", thread = %id, tool = %record.tool);
        let result = tokio::select! {
            // Dropping the host's future on cancellation stops the call.
            result = tracing::Instrument::instrument(routed.host.call(&view.scope, request), span) => result,
            _ = run.cancel.cancelled() => return Ok(false),
        };
        self.update_call(id, message, &call, changed, |_, record| match result {
            Ok(output) if !output.is_error => {
                record.state = ToolCallState::Succeeded;
                record.output = Some(bounded_output(output.text));
            }
            Ok(output) => {
                settle(
                    record,
                    ToolCallState::Failed,
                    &ToolError::Reported(record.tool.clone()),
                );
                // The tool's own failure text is what the model needs.
                record.output = Some(bounded_output(output.text));
            }
            Err(cause) => settle(
                record,
                ToolCallState::Failed,
                &ToolError::Host {
                    tool: record.tool.clone(),
                    cause,
                },
            ),
        })?;
        Ok(true)
    }
    /// Publish an approval request and wait for the user. `None` when the run
    /// was cancelled first; the request then expires.
    async fn ask(
        &self,
        run: &Native,
        message: Id,
        record: &ToolCall,
        spec: &ToolSpec,
    ) -> Result<Option<ApprovalDecision>> {
        let id = run.thread;
        let approval = Id::new_v4();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.approvals.lock().map_err(poisoned("approval"))?.insert(
            approval,
            Waiter {
                thread: id,
                digest: record.digest.clone(),
                decision: sender,
            },
        );
        let request = ApprovalRequest {
            id: approval,
            message,
            call: record.id.clone(),
            tool: record.tool.clone(),
            arguments: record.arguments.clone(),
            digest: record.digest.clone(),
            capabilities: spec.capabilities.clone(),
            requested_at: now(),
        };
        let published = self.update_call(
            id,
            message,
            &record.id,
            EventKind::ApprovalRequested {
                thread: id,
                approval,
            },
            |thread, call| {
                call.state = ToolCallState::AwaitingApproval;
                thread.approvals.push(request);
            },
        );
        if let Err(error) = published {
            self.approvals
                .lock()
                .map_err(poisoned("approval"))?
                .remove(&approval);
            return Err(error);
        }
        let decision = tokio::select! {
            decision = receiver => decision.ok(),
            _ = run.cancel.cancelled() => None,
        };
        // Expire the request whatever happened: a late answer finds nothing.
        self.approvals
            .lock()
            .map_err(poisoned("approval"))?
            .remove(&approval);
        let outcome = match decision {
            Some(ApprovalDecision::Approve) => ApprovalOutcome::Approved,
            Some(ApprovalDecision::Deny) => ApprovalOutcome::Denied,
            None => ApprovalOutcome::Expired,
        };
        self.update_call(
            id,
            message,
            &record.id,
            EventKind::ApprovalResolved {
                thread: id,
                approval,
                outcome,
            },
            |thread, call| {
                thread.approvals.retain(|pending| pending.id != approval);
                if decision.is_none() {
                    settle(call, ToolCallState::Cancelled, &ToolError::Cancelled);
                }
            },
        )?;
        Ok(decision)
    }
}
impl Gateway {
    /// Forget every approval a finished run left behind, so a late answer
    /// cannot reach anything.
    pub(super) fn expire_approvals(&self, thread: Id) -> Result<()> {
        self.approvals
            .lock()
            .map_err(poisoned("approval"))?
            .retain(|_, waiter| waiter.thread != thread);
        Ok(())
    }
}
/// After a run ends, nothing of it may still look pending: requests expire
/// and calls that never finished are cancelled.
pub(crate) fn settle_unfinished(thread: &mut Thread) {
    thread.approvals.clear();
    for message in &mut thread.messages {
        for call in &mut message.tool_calls {
            if !call.state.is_final() {
                settle(call, ToolCallState::Cancelled, &ToolError::Cancelled);
            }
        }
    }
}
fn message_mut(store: &mut Store, thread: Id, message: Id) -> Result<&mut Message> {
    store
        .thread_mut(thread)?
        .messages
        .iter_mut()
        .find(|m| m.id == message)
        .ok_or(GatewayError::NotFound("reply message not found"))
}
/// Finish a call that produced no result: clients get the structured
/// report, the model gets its text.
fn settle(call: &mut ToolCall, state: ToolCallState, why: &ToolError) {
    let report = crate::error::handle("tool_call", why);
    call.state = state;
    call.output = Some(report.detail());
    call.error = Some(report);
}
