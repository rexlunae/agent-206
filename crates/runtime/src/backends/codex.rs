use super::{Progress, isolation::Process, rpc::Rpc};
use a206_core::*;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::mpsc;
struct Turn {
    thread: String,
    id: Option<String>,
    messages: BTreeMap<String, String>,
    order: Vec<String>,
    complete: Option<bool>,
    usage: Usage,
    progress: mpsc::Sender<Progress>,
}
impl Turn {
    async fn receive(&mut self, rpc: &mut Rpc) -> Result<Value> {
        let value = rpc.receive().await?;
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            if let Some(id) = value.get("id") {
                // Never grant escalation, install tools, answer credentials or widen the OS sandbox.
                let response = if matches!(
                    method,
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
                ) {
                    json!({"id": id, "result": {"decision": "decline"}})
                } else {
                    json!({"id": id, "error": {"code": -32601, "message": "Unsupported backend request"}})
                };
                rpc.send(response).await?;
                self.progress
                    .try_send(Progress::Activity(
                        "Backend permission request denied".into(),
                    ))
                    .ignore();
            } else {
                self.notification(method, value.get("params").unwrap_or(&Value::Null))?;
            }
        }
        Ok(value)
    }
    fn notification(&mut self, method: &str, params: &Value) -> Result<()> {
        if params.get("threadId").and_then(Value::as_str) != Some(self.thread.as_str()) {
            return Ok(());
        }
        if let Some(id) = &self.id {
            if params
                .get("turnId")
                .and_then(Value::as_str)
                .is_some_and(|v| v != id)
            {
                return Ok(());
            }
            if params
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .is_some_and(|v| v != id)
            {
                return Ok(());
            }
        }
        match method {
            "item/agentMessage/delta" => {
                let id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .context("backend text omitted its item identifier")?;
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .context("backend text omitted its delta")?;
                ensure!(
                    self.messages.len() < 256 || self.messages.contains_key(id),
                    "too many backend messages"
                );
                if !self.messages.contains_key(id) {
                    self.order.push(id.into());
                }
                self.messages.entry(id.into()).or_default().push_str(delta);
                self.publish()?;
            }
            "item/completed"
                if params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") =>
            {
                let id = params
                    .pointer("/item/id")
                    .and_then(Value::as_str)
                    .context("backend item omitted its identifier")?;
                let text = params
                    .pointer("/item/text")
                    .and_then(Value::as_str)
                    .context("backend item omitted its text")?;
                ensure!(
                    self.messages.len() < 256 || self.messages.contains_key(id),
                    "too many backend messages"
                );
                if !self.messages.contains_key(id) {
                    self.order.push(id.into());
                }
                self.messages.insert(id.into(), text.into());
                self.publish()?;
            }
            "item/started" => {
                if let Some(kind) = params
                    .pointer("/item/type")
                    .and_then(Value::as_str)
                    .filter(|s| s.len() <= 128 && !s.chars().any(char::is_control))
                {
                    self.progress
                        .try_send(Progress::Activity(format!("Codex: {kind}")))
                        .ignore();
                }
            }
            "thread/tokenUsage/updated" => {
                self.usage.input_tokens = params
                    .pointer("/tokenUsage/last/inputTokens")
                    .and_then(Value::as_u64);
                self.usage.output_tokens = params
                    .pointer("/tokenUsage/last/outputTokens")
                    .and_then(Value::as_u64);
            }
            "turn/completed" => {
                self.complete = Some(
                    params.pointer("/turn/status").and_then(Value::as_str) == Some("completed"),
                );
            }
            _ => {}
        }
        Ok(())
    }
    fn text(&self) -> String {
        self.order
            .iter()
            .filter_map(|id| self.messages.get(id))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    fn publish(&self) -> Result<()> {
        let text = self.text();
        ensure!(
            text.len() <= 128 * 1024,
            "backend message exceeds the conversation limit"
        );
        self.progress.try_send(Progress::Text(text)).ignore();
        Ok(())
    }
    async fn request(
        &mut self,
        rpc: &mut Rpc,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        rpc.send(json!({"id": id, "method": method, "params": params}))
            .await?;
        loop {
            let value = self.receive(rpc).await?;
            if value.get("id").and_then(Value::as_u64) == Some(id) && value.get("method").is_none()
            {
                ensure!(value.get("error").is_none(), "Codex rejected {method}");
                return value
                    .get("result")
                    .cloned()
                    .context("Codex response omitted its result");
            }
        }
    }
}
pub async fn run(
    process: &mut Process,
    thread: &Thread,
    prompt: &str,
    input: &str,
    progress: mpsc::Sender<Progress>,
) -> Result<(String, Usage)> {
    let mut rpc = Rpc::new(process)?;
    let mut turn = Turn {
        thread: String::new(),
        id: None,
        messages: BTreeMap::new(),
        order: vec![],
        complete: None,
        usage: Usage {
            input_tokens: None,
            output_tokens: None,
            cost: None,
        },
        progress,
    };
    turn.request(&mut rpc, 1, "initialize", json!({"clientInfo": {"name": "agent206", "title": "Agent 206", "version": env!("CARGO_PKG_VERSION")}})).await?;
    rpc.send(json!({"method": "initialized", "params": {}}))
        .await?;
    let model = &thread.model.as_ref().context("model not selected")?.model;
    let started = turn.request(&mut rpc, 2, "thread/start", json!({"model": model, "modelProvider": "agent206", "cwd": process.cwd,
        "approvalPolicy": "never", "sandbox": "read-only", "baseInstructions": prompt, "ephemeral": true})).await?;
    turn.thread = started
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .context("Codex omitted its thread identifier")?
        .to_owned();
    let mut params = json!({"threadId": turn.thread, "model": model, "cwd": process.cwd,
        "approvalPolicy": "never", "sandboxPolicy": {"type": "externalSandbox", "networkAccess": "restricted"},
        "input": [{"type": "text", "text": input}] });
    if let Some(effort) = &thread.effort {
        params["effort"] = json!(effort);
    }
    let started = turn.request(&mut rpc, 3, "turn/start", params).await?;
    turn.id = Some(
        started
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("Codex omitted its turn identifier")?
            .into(),
    );
    while turn.complete.is_none() {
        turn.receive(&mut rpc).await?;
    }
    if turn.complete != Some(true) {
        bail!("Codex did not complete this turn; inspect the backend activity");
    }
    let text = turn.text();
    ensure!(!text.is_empty(), "Codex completed without a text response");
    Ok((text, turn.usage))
}
