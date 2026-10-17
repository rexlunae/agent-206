//! Context management for native runs: project instruction files and
//! compaction, against scripted provider streams. Every identifier, limit,
//! token count and text is generated at runtime.
mod support;

use a206_core::*;
use a206_runtime::{
    gateway::{COMPACTION_THRESHOLD_PERCENT, Gateway},
    instructions,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;
use support::*;
use tokio::sync::mpsc;

/// A context limit generated at runtime, a multiple of 100 so that the
/// threshold falls exactly on a token count.
fn limit() -> u64 {
    ((Id::new_v4().as_u128() % 100_000) as u64 + 100) * 100
}
/// Input tokens exactly at the threshold (not above it), and one more.
fn around_threshold(limit: u64) -> (u64, u64) {
    let at = limit / 100 * COMPACTION_THRESHOLD_PERCENT;
    (at, at + 1)
}
/// Whether any part of a request mentions `text`.
fn mentions(body: &Value, text: &str) -> bool {
    body.to_string().contains(text)
}
/// An upstream error event in the protocol's shape.
fn failure_response(api: ProviderApi, code: &str) -> String {
    match api {
        ProviderApi::OpenAi => sse(None, json!({"error": {"code": code}})),
        ProviderApi::Anthropic => sse(
            Some("error"),
            json!({"type": "error", "error": {"type": code}}),
        ),
        ProviderApi::Gemini => sse(None, json!({"error": {"status": code}})),
    }
}

/// A tool that echoes a generated marker with its arguments.
struct EchoHost {
    name: String,
    marker: String,
}
impl ToolHost for EchoHost {
    fn list<'a>(&'a self, _: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            ToolListing {
                tools: vec![ToolSpec {
                    name: self.name.clone(),
                    description: nonce(),
                    parameters: json!({"type": "object", "properties": {}}),
                    capabilities: Default::default(),
                    permission: ToolPermission::Allow,
                    whole_output: false,
                }],
                unavailable: vec![],
            }
        })
    }
    fn call<'a>(
        &'a self,
        _: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            Ok(ToolOutput {
                text: format!("{}:{}", self.marker, Value::Object(request.arguments)),
                is_error: false,
            })
        })
    }
}
struct Rig {
    api: ProviderApi,
    gateway: Gateway,
    _state: tempfile::TempDir,
    thread: Id,
    model: ModelKey,
    requests: mpsc::Receiver<Opened>,
}
async fn rig(api: ProviderApi, context_limit: Option<u64>, host: Option<EchoHost>) -> Rig {
    let state = tempfile::tempdir().unwrap();
    let mut gateway = Gateway::open(state.path()).unwrap();
    let tool = host.as_ref().map(|h| h.name.clone());
    if let Some(host) = host {
        gateway = gateway.with_tool_host(Arc::new(host));
    }
    let (model, requests) = connect_reporting(&gateway, api, context_limit).await;
    let Reply::Thread(thread) = request(
        &gateway,
        Command::CreateThread {
            title: nonce(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!("thread creation failed");
    };
    let rig = Rig {
        api,
        gateway,
        _state: state,
        thread: thread.id,
        model,
        requests,
    };
    rig.configure(rig.thread).await;
    if let Some(tool) = tool {
        let permitted = rig
            .run(Command::SetToolPermission {
                thread: rig.thread,
                tool,
                permission: Some(ToolPermission::Allow),
            })
            .await;
        assert_eq!(permitted, Reply::Ok);
    }
    rig
}
impl Rig {
    async fn run(&self, body: Command) -> Reply {
        request(&self.gateway, body).await
    }
    async fn configure(&self, id: Id) {
        let mode = self.thread_of(id).mode;
        let configured = self
            .run(Command::ConfigureThread {
                id,
                mode,
                model: Some(self.model.clone()),
                subagent_models: [self.model.clone()].into(),
            })
            .await;
        assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    }
    fn thread_of(&self, id: Id) -> Thread {
        self.gateway
            .snapshot()
            .unwrap()
            .threads
            .into_iter()
            .find(|t| t.id == id)
            .unwrap()
    }
    fn thread(&self) -> Thread {
        self.thread_of(self.thread)
    }
    fn send(&self, id: Id, text: &str) -> tokio::task::JoinHandle<Reply> {
        let gateway = self.gateway.clone();
        let text = text.to_owned();
        tokio::spawn(async move {
            request(
                &gateway,
                Command::Send {
                    thread: id,
                    text,
                    attachments: vec![],
                },
            )
            .await
        })
    }
    fn compact(&self) -> tokio::task::JoinHandle<Reply> {
        let gateway = self.gateway.clone();
        let thread = self.thread;
        tokio::spawn(async move { request(&gateway, Command::CompactThread { thread }).await })
    }
    async fn next(&mut self) -> Opened {
        next_request(&mut self.requests).await
    }
    /// One completed turn whose only request reports `input` tokens.
    /// Returns the user text and the reply text.
    async fn turn(&mut self, input: u64) -> (String, String) {
        let (asked, answered) = (nonce(), nonce());
        let run = self.send(self.thread, &asked);
        let opened = self.next().await;
        assert!(mentions(&opened.body, &asked));
        opened
            .respond(text_response_reporting(self.api, &answered, input))
            .await;
        assert_eq!(finished(run).await.state, RunState::Complete);
        (asked, answered)
    }
}
fn summaries(thread: &Thread) -> Vec<&Message> {
    thread
        .messages
        .iter()
        .filter(|m| m.compaction.is_some())
        .collect()
}

// ── Compaction ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn compaction_starts_only_above_the_threshold_of_a_known_limit() {
    for api in PROTOCOLS {
        let limit = limit();
        let (at, over) = around_threshold(limit);
        let mut rig = rig(api, Some(limit), None).await;
        let first = rig.turn(1).await;
        let second = rig.turn(at).await;
        // At the threshold: the next request is the run's own, uncompacted.
        let third = rig.turn(over).await;
        assert!(summaries(&rig.thread()).is_empty(), "{api:?}");

        // Above it: the next run first summarizes all but the last two turns.
        let asked = nonce();
        let run = rig.send(rig.thread, &asked);
        let summary_request = rig.next().await;
        for text in [&first.0, &first.1, &second.0, &second.1] {
            assert!(mentions(&summary_request.body, text), "{api:?}");
        }
        for text in [&third.0, &asked] {
            assert!(!mentions(&summary_request.body, text), "{api:?}");
        }
        assert!(offered(api, &summary_request.body).is_empty());
        let (summary, summary_input) = (nonce(), u64::from(Id::new_v4().as_u128() as u16) + 1);
        summary_request
            .respond(text_response_reporting(api, &summary, summary_input))
            .await;
        let compacted = rig.next().await;
        for text in [&summary, &third.0, &third.1, &asked] {
            assert!(mentions(&compacted.body, text), "{api:?}");
        }
        for text in [&first.0, &first.1, &second.0, &second.1] {
            assert!(!mentions(&compacted.body, text), "{api:?}");
        }
        // The summary leads the first (user) message.
        let history = history_of(api, &compacted.body);
        let lead = if api == ProviderApi::OpenAi { 1 } else { 0 };
        assert_eq!(history[lead]["role"], "user", "{api:?}");
        assert!(mentions(&history[lead], &summary) && mentions(&history[lead], &third.0));
        let final_input = u64::from(Id::new_v4().as_u128() as u16) + 1;
        compacted
            .respond(text_response_reporting(api, &nonce(), final_input))
            .await;
        let thread = finished(run).await;
        assert_eq!(thread.state, RunState::Complete);
        let found = summaries(&thread);
        assert_eq!(found.len(), 1, "{api:?}");
        let record = found[0].compaction.as_ref().unwrap();
        assert_eq!(found[0].text, summary);
        assert_eq!(record.trigger, CompactionTrigger::Automatic);
        assert_eq!(record.messages, 4);
        assert_eq!(record.previous, None);
        assert_eq!(record.input_tokens, Some(over));
        assert_eq!(record.context_limit, Some(limit));
        assert_eq!(record.usage.input_tokens, Some(summary_input));
        // The summary sits before the turns kept verbatim; nothing was lost.
        let at = thread.messages.iter().position(|m| m.compaction.is_some());
        assert_eq!(thread.messages[at.unwrap() + 1].text, third.0);
        assert_eq!(thread.messages[0].text, first.0);
        // The summary request is part of the run's cost; the context
        // widget shows the final request alone.
        assert_eq!(
            thread.run_usage.unwrap().input_tokens,
            Some(summary_input + final_input)
        );
        assert_eq!(thread.usage.unwrap().input_tokens, Some(final_input));
    }
}

#[tokio::test]
async fn an_unknown_context_limit_never_compacts_automatically() {
    for api in PROTOCOLS {
        let mut rig = rig(api, None, None).await;
        let first = rig.turn(u64::from(u32::MAX)).await;
        rig.turn(u64::from(u32::MAX)).await;
        rig.turn(u64::from(u32::MAX)).await;
        let asked = nonce();
        let run = rig.send(rig.thread, &asked);
        let opened = rig.next().await;
        assert!(mentions(&opened.body, &first.0) && mentions(&opened.body, &asked));
        opened.respond(text_response(api, &nonce())).await;
        let thread = finished(run).await;
        assert_eq!(thread.state, RunState::Complete);
        assert!(summaries(&thread).is_empty(), "{api:?}");
    }
}

#[tokio::test]
async fn the_user_can_compact_without_a_known_limit() {
    for api in PROTOCOLS {
        let mut rig = rig(api, None, None).await;
        rig.turn(1).await;
        // One turn is all the context there is: nothing is sent.
        let Reply::Error(report) = rig.compact().await.unwrap() else {
            panic!("compacting a single turn must fail");
        };
        assert_eq!(report.kind, ErrorKind::Conflict);
        let before = rig.thread();
        let second = rig.turn(1).await;
        let earlier = before.messages.clone();

        let compaction = rig.compact();
        let summary_request = rig.next().await;
        for message in &earlier {
            assert!(mentions(&summary_request.body, &message.text));
        }
        assert!(!mentions(&summary_request.body, &second.0));
        // The state to restore is saved, so a restart restores it too.
        let during = rig.thread();
        assert_eq!(during.state, RunState::Running);
        assert_eq!(during.compacting, Some(RunState::Complete));
        let summary = nonce();
        summary_request
            .respond(text_response_reporting(api, &summary, 1))
            .await;
        let Reply::Thread(thread) = compaction.await.unwrap() else {
            panic!("compaction failed");
        };
        assert_eq!(thread.state, RunState::Complete);
        assert_eq!(thread.compacting, None);
        assert_eq!(thread.backend_activity, None);
        let found = summaries(&thread);
        assert_eq!(found.len(), 1);
        let record = found[0].compaction.as_ref().unwrap();
        assert_eq!(record.trigger, CompactionTrigger::Manual);
        assert_eq!(record.context_limit, None);
        assert_eq!(record.messages as usize, earlier.len());
        // Unknown until the next request reports it.
        assert_eq!(thread.usage, None);

        let asked = nonce();
        let run = rig.send(rig.thread, &asked);
        let opened = rig.next().await;
        for text in [&summary, &second.0, &second.1, &asked] {
            assert!(mentions(&opened.body, text), "{api:?}");
        }
        for message in &earlier {
            assert!(!mentions(&opened.body, &message.text), "{api:?}");
        }
        opened.respond(text_response(api, &nonce())).await;
        assert_eq!(finished(run).await.state, RunState::Complete);

        // A second compaction folds the first summary in.
        rig.turn(1).await;
        let compaction = rig.compact();
        let summary_request = rig.next().await;
        assert!(mentions(&summary_request.body, &summary));
        assert!(mentions(&summary_request.body, &asked));
        summary_request.respond(text_response(api, &nonce())).await;
        let Reply::Thread(thread) = compaction.await.unwrap() else {
            panic!("compaction failed");
        };
        let found = summaries(&thread);
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[1].compaction.as_ref().unwrap().previous,
            Some(found[0].id)
        );
    }
}

#[tokio::test]
async fn cancelling_a_compaction_leaves_the_conversation_unchanged() {
    for api in PROTOCOLS {
        // Asked for by the user.
        let mut rig = rig(api, None, None).await;
        rig.turn(1).await;
        rig.turn(1).await;
        let before = rig.thread();
        let compaction = rig.compact();
        let pending = rig.next().await;
        assert_eq!(rig.thread().state, RunState::Running);
        assert_eq!(
            rig.run(Command::CancelRun { thread: rig.thread }).await,
            Reply::Ok
        );
        let Reply::Error(report) = compaction.await.unwrap() else {
            panic!("a cancelled compaction must fail");
        };
        assert_eq!(report.kind, ErrorKind::Cancelled);
        drop(pending);
        let after = rig.thread();
        assert_eq!(after.messages, before.messages);
        assert_eq!(after.state, before.state);
        assert_eq!(after.backend_activity, None);

        // Started by a run.
        let limit = limit();
        let mut rig = self::rig(api, Some(limit), None).await;
        rig.turn(1).await;
        rig.turn(1).await;
        rig.turn(around_threshold(limit).1).await;
        let asked = nonce();
        let run = rig.send(rig.thread, &asked);
        let pending = rig.next().await;
        assert!(!mentions(&pending.body, &asked));
        assert_eq!(
            rig.run(Command::CancelRun { thread: rig.thread }).await,
            Reply::Ok
        );
        let thread = finished(run).await;
        drop(pending);
        assert_eq!(thread.state, RunState::Cancelled, "{api:?}");
        assert!(summaries(&thread).is_empty());
        assert_eq!(thread.messages.last().unwrap().text, asked);
        assert_eq!(thread.messages.len(), 7);
    }
}

#[tokio::test]
async fn a_failed_summary_reports_its_cause_and_changes_nothing() {
    for api in PROTOCOLS {
        // Asked for by the user: the reply carries the structured failure.
        let mut rig = rig(api, None, None).await;
        rig.turn(1).await;
        rig.turn(1).await;
        let before = rig.thread();
        let compaction = rig.compact();
        let code = nonce();
        rig.next().await.respond(failure_response(api, &code)).await;
        let Reply::Error(report) = compaction.await.unwrap() else {
            panic!("a failed summary must fail the compaction");
        };
        assert_eq!(report.kind, ErrorKind::Provider, "{api:?}");
        let cause = report.source.as_deref().expect("the provider failure");
        assert!(cause.message.contains(&code), "{report:?}");
        let after = rig.thread();
        assert_eq!(after.messages, before.messages);
        assert_eq!(after.state, before.state);

        // Started by a run: the run fails with it; the history stays.
        let limit = limit();
        let mut rig = self::rig(api, Some(limit), None).await;
        rig.turn(1).await;
        rig.turn(1).await;
        rig.turn(around_threshold(limit).1).await;
        let before = rig.thread().messages;
        let run = rig.send(rig.thread, &nonce());
        let code = nonce();
        rig.next().await.respond(failure_response(api, &code)).await;
        let thread = finished(run).await;
        assert_eq!(thread.state, RunState::Failed, "{api:?}");
        let report = thread.error.as_ref().unwrap();
        assert_eq!(report.kind, ErrorKind::Provider);
        assert!(report.source.as_deref().unwrap().message.contains(&code));
        assert!(summaries(&thread).is_empty());
        assert_eq!(thread.messages[..before.len()], before[..]);
    }
}

/// Every tool result answers a call in the message before it, every call
/// has its result right after, and the history starts with a user message.
fn assert_pairs(api: ProviderApi, body: &Value) {
    let history = history_of(api, body);
    let mut calls = vec![];
    let mut results = vec![];
    let skip = usize::from(api == ProviderApi::OpenAi);
    assert_eq!(history[skip]["role"], "user", "{api:?}");
    for (at, message) in history.iter().enumerate() {
        match api {
            ProviderApi::OpenAi => {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    calls.push((at, call["id"].as_str().unwrap().to_owned()));
                }
                if message["role"] == "tool" {
                    results.push((at, message["tool_call_id"].as_str().unwrap().to_owned()));
                }
            }
            ProviderApi::Anthropic => {
                for block in message["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        calls.push((at, block["id"].as_str().unwrap().to_owned()));
                    }
                    if block["type"] == "tool_result" {
                        results.push((at, block["tool_use_id"].as_str().unwrap().to_owned()));
                    }
                }
            }
            ProviderApi::Gemini => {
                for part in message["parts"].as_array().into_iter().flatten() {
                    if let Some(id) = part["functionCall"]["id"].as_str() {
                        calls.push((at, id.to_owned()));
                    }
                    if let Some(id) = part["functionResponse"]["id"].as_str() {
                        results.push((at, id.to_owned()));
                    }
                }
            }
        }
    }
    assert_eq!(calls.len(), results.len(), "{api:?}: {body}");
    for (at, id) in &results {
        let (called_at, _) = calls
            .iter()
            .find(|(_, call)| call == id)
            .unwrap_or_else(|| panic!("{api:?}: result {id} without its call"));
        match api {
            // OpenAI results follow their assistant message one per message.
            ProviderApi::OpenAi => assert!(called_at < at),
            _ => assert_eq!(called_at + 1, *at, "{api:?}"),
        }
    }
}

#[tokio::test]
async fn tool_calls_keep_their_results_after_compaction() {
    for api in PROTOCOLS {
        let limit = limit();
        let tool = format!("echo_{}", nonce());
        let marker = nonce();
        let host = EchoHost {
            name: tool.clone(),
            marker: marker.clone(),
        };
        let mut rig = rig(api, Some(limit), Some(host)).await;
        // Two turns that each run one tool call, the second reporting a
        // context above the threshold.
        let mut calls: Vec<String> = vec![];
        for input in [1, around_threshold(limit).1] {
            let run = rig.send(rig.thread, &nonce());
            let call = Call::new(&tool, json!({"value": nonce()}));
            let first = rig.next().await;
            if let Some(previous) = calls.last() {
                assert_pairs(api, &first.body);
                assert!(result_for(api, &first.body, previous).is_some());
            }
            first
                .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
                .await;
            let second = rig.next().await;
            assert!(result_for(api, &second.body, &call.id).is_some(), "{api:?}");
            assert_pairs(api, &second.body);
            second
                .respond(text_response_reporting(api, &nonce(), input))
                .await;
            assert_eq!(finished(run).await.state, RunState::Complete);
            calls.push(call.id);
        }
        // The next turn summarizes the first: its call and result go into
        // the summary request as text, not as protocol tool blocks.
        let asked = nonce();
        let run = rig.send(rig.thread, &asked);
        let summary_request = rig.next().await;
        assert!(mentions(&summary_request.body, &tool));
        assert!(mentions(&summary_request.body, &marker));
        assert!(result_for(api, &summary_request.body, &calls[0]).is_none());
        assert_eq!(
            history_of(api, &summary_request.body).len(),
            1 + usize::from(api == ProviderApi::OpenAi)
        );
        summary_request.respond(text_response(api, &nonce())).await;
        let compacted = rig.next().await;
        assert_pairs(api, &compacted.body);
        assert!(
            result_for(api, &compacted.body, &calls[0]).is_none(),
            "{api:?}"
        );
        assert!(!mentions(&compacted.body, &calls[0]));
        let kept = result_for(api, &compacted.body, &calls[1]).expect("the kept call's result");
        assert!(kept.starts_with(&marker));
        compacted.respond(text_response(api, &nonce())).await;
        assert_eq!(finished(run).await.state, RunState::Complete);
    }
}

#[tokio::test]
async fn compaction_is_refused_while_a_run_is_in_progress() {
    let mut rig = rig(ProviderApi::OpenAi, None, None).await;
    rig.turn(1).await;
    rig.turn(1).await;
    let run = rig.send(rig.thread, &nonce());
    let pending = rig.next().await;
    let Reply::Error(report) = rig.compact().await.unwrap() else {
        panic!("compaction must wait for the run");
    };
    assert_eq!(report.kind, ErrorKind::Conflict);
    pending
        .respond(text_response(ProviderApi::OpenAi, &nonce()))
        .await;
    let thread = finished(run).await;
    assert!(summaries(&thread).is_empty());
}

// ── Project instruction files ───────────────────────────────────────────────

/// A project rooted in a fresh directory whose coordinator (Plan mode) is
/// the conversation.
async fn project_rig(api: ProviderApi) -> (Rig, tempfile::TempDir) {
    let project = tempfile::tempdir().unwrap();
    let mut rig = rig(api, None, None).await;
    let Reply::Project(created) = rig
        .run(Command::CreateProject {
            name: nonce(),
            root: project.path().to_string_lossy().into_owned(),
        })
        .await
    else {
        panic!("project creation failed");
    };
    rig.thread = created.manager;
    rig.configure(rig.thread).await;
    (rig, project)
}
impl Rig {
    async fn grant(&self, thread: Id, capabilities: &[Capability]) {
        let granted = self
            .run(Command::SetBackend {
                thread,
                backend: CodingBackend::Native,
                capabilities: capabilities.iter().copied().collect(),
            })
            .await;
        assert_eq!(granted, Reply::Ok);
    }
    /// Run one turn of `thread` and return its request's system prompt.
    async fn prompt_of(&mut self, thread: Id) -> String {
        let run = self.send(thread, &nonce());
        let opened = self.next().await;
        let prompt = system_of(self.api, &opened.body);
        opened.respond(text_response(self.api, &nonce())).await;
        assert_eq!(finished(run).await.state, RunState::Complete);
        prompt
    }
}
fn file<'a>(thread: &'a Thread, path: &str) -> &'a InstructionFile {
    thread
        .instructions
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} not reported: {:?}", thread.instructions))
}
fn sha256(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn instruction_files_load_only_with_the_grant_to_read_the_project() {
    for api in PROTOCOLS {
        let (mut rig, project) = project_rig(api).await;
        let agents = nonce();
        std::fs::write(project.path().join("AGENTS.md"), &agents).unwrap();
        let manager = rig.thread;

        // Not granted: nothing is read.
        assert!(!rig.prompt_of(manager).await.contains(&agents));
        assert!(rig.thread().instructions.is_empty());

        rig.grant(manager, &[Capability::ReadWorkspace]).await;
        let prompt = rig.prompt_of(manager).await;
        assert!(prompt.contains(&agents), "{api:?}");
        let thread = rig.thread();
        assert_eq!(thread.instructions.len(), 1);
        let agents_file = file(&thread, "AGENTS.md");
        assert_eq!(agents_file.status, InstructionStatus::Included);
        assert_eq!(agents_file.bytes, agents.len() as u64);
        assert_eq!(
            agents_file.sha256.as_deref(),
            Some(sha256(&agents).as_str())
        );
        assert_eq!(agents_file.error, None);

        // A delegated conversation reads them only while every ancestor may.
        let Reply::Thread(child) = rig
            .run(Command::CreateSubagent {
                parent: manager,
                title: nonce(),
                mode: Mode::Plan,
                model: rig.model.clone(),
                requested: [Capability::ReadWorkspace].into(),
            })
            .await
        else {
            panic!("subagent creation failed");
        };
        assert!(rig.prompt_of(child.id).await.contains(&agents));
        rig.grant(manager, &[]).await;
        assert!(!rig.prompt_of(child.id).await.contains(&agents));
        assert!(rig.thread_of(child.id).instructions.is_empty());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn instruction_files_stay_in_the_project_and_within_their_limits() {
    let api = ProviderApi::OpenAi;
    let (mut rig, project) = project_rig(api).await;
    rig.grant(rig.thread, &[Capability::ReadWorkspace]).await;
    let root = project.path();
    // A symlink out of the project: its target must never be read.
    let outside = tempfile::tempdir().unwrap();
    let secret = nonce();
    std::fs::write(outside.path().join("target.md"), &secret).unwrap();
    std::os::unix::fs::symlink(outside.path().join("target.md"), root.join("AGENTS.md")).unwrap();
    // Two files over the per-file limit, which together use the total.
    let large = |seed: String| seed.repeat(instructions::MAX_FILE / seed.len() + 2);
    let (claude, soul) = (large(nonce()), large(nonce()));
    std::fs::write(root.join("CLAUDE.md"), &claude).unwrap();
    std::fs::write(root.join("SOUL.md"), &soul).unwrap();
    assert_eq!(instructions::MAX_TOTAL, 2 * instructions::MAX_FILE);
    let identity = nonce();
    std::fs::write(root.join("IDENTITY.md"), &identity).unwrap();
    // A symlink within the project to a file already included.
    std::os::unix::fs::symlink("CLAUDE.md", root.join("TOOLS.md")).unwrap();

    let prompt = rig.prompt_of(rig.thread).await;
    assert!(!prompt.contains(&secret));
    assert!(prompt.contains(&claude[..instructions::MAX_FILE]));
    assert!(!prompt.contains(&claude[..instructions::MAX_FILE + 1]));
    assert!(prompt.contains(&soul[..instructions::MAX_FILE]));
    assert!(!prompt.contains(&identity));
    let thread = rig.thread();
    let agents = file(&thread, "AGENTS.md");
    assert_eq!(agents.status, InstructionStatus::Skipped);
    assert_eq!(
        agents.error.as_ref().unwrap().kind,
        ErrorKind::PermissionDenied
    );
    assert_eq!(agents.bytes, 0);
    for (path, text) in [("CLAUDE.md", &claude), ("SOUL.md", &soul)] {
        let entry = file(&thread, path);
        assert_eq!(entry.status, InstructionStatus::Truncated);
        assert_eq!(entry.bytes, instructions::MAX_FILE as u64);
        assert_eq!(entry.size, Some(text.len() as u64));
        assert_eq!(
            entry.sha256.as_deref(),
            Some(sha256(&text[..instructions::MAX_FILE]).as_str())
        );
    }
    let identity_entry = file(&thread, "IDENTITY.md");
    assert_eq!(identity_entry.status, InstructionStatus::Skipped);
    assert_eq!(
        identity_entry.error.as_ref().unwrap().kind,
        ErrorKind::InvalidRequest
    );
    let tools = file(&thread, "TOOLS.md");
    assert_eq!(tools.status, InstructionStatus::Skipped);
    assert_eq!(tools.error.as_ref().unwrap().kind, ErrorKind::Conflict);
    // Nothing anywhere in the request or the thread carries the target.
    assert!(!format!("{thread:?}").contains(&secret));

    // Directories, non-text files and dangling links are skipped; missing
    // files are not listed.
    for path in [
        "AGENTS.md",
        "CLAUDE.md",
        "SOUL.md",
        "IDENTITY.md",
        "TOOLS.md",
    ] {
        std::fs::remove_file(root.join(path)).unwrap();
    }
    std::fs::create_dir(root.join("AGENTS.md")).unwrap();
    std::fs::write(root.join("CLAUDE.md"), [b'#', 0xff, 0xfe]).unwrap();
    std::os::unix::fs::symlink(root.join(nonce()), root.join("SOUL.md")).unwrap();
    let prompt = rig.prompt_of(rig.thread).await;
    assert!(!prompt.contains("Project instructions"));
    let thread = rig.thread();
    let paths: Vec<_> = thread
        .instructions
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(paths, ["AGENTS.md", "CLAUDE.md", "SOUL.md"]);
    assert!(
        thread
            .instructions
            .iter()
            .all(|f| f.status == InstructionStatus::Skipped && f.error.is_some())
    );
}
