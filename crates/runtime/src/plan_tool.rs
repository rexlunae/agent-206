//! A planning checklist for the native run engine: a [`ToolHost`] whose
//! `plan_update` tool lets a model keep a short list of steps for a
//! multi-step task, as RustyClaw's `todo` tool does.
//!
//! The plan belongs to the conversation (`Thread::plan`): it is shown to the
//! user beside the conversation, kept across runs, restarts and compaction,
//! and replaced whole by each call, so a model never edits a step it has not
//! seen. It only records what the model says it is doing; it grants nothing.
//! Like every tool it is denied until the user permits it on the
//! conversation.
use crate::{
    gateway::Plans,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    ErrorKind, ErrorReport, PlanStatus, PlanStep, Thread, ToolCallState, ToolPermission,
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

/// Replaces the conversation's plan.
pub const UPDATE: &str = "plan_update";
/// Most steps a plan holds.
pub const MAX_STEPS: usize = 50;
/// Longest step text, in bytes.
pub const MAX_STEP_BYTES: usize = 500;

/// Why a plan tool call failed.
#[derive(Debug, thiserror::Error)]
pub enum PlanToolError {
    #[error("`steps` is required")]
    MissingSteps,
    #[error("`steps` must be a list of steps")]
    StepsType,
    #[error("a plan holds at most {MAX_STEPS} steps")]
    TooManySteps,
    #[error("step {0} must be an object with `text` and `status`")]
    StepType(usize),
    #[error("step {0} has no text")]
    EmptyStep(usize),
    #[error("step {0} is longer than {MAX_STEP_BYTES} bytes")]
    LongStep(usize),
    #[error("step {step} has status `{status}`; use pending, in_progress or done")]
    Status { step: usize, status: String },
    #[error("unknown argument `{0}`")]
    Unknown(String),
    #[error("unknown plan tool `{0}`")]
    UnknownTool(String),
}
impl crate::error::Classified for PlanToolError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::UnknownTool(_) => ErrorKind::NotFound,
            _ => ErrorKind::InvalidRequest,
        })
    }
}
fn fail(error: PlanToolError) -> ErrorReport {
    crate::error::report(&error)
}

/// Offers the plan tool to native runs.
pub struct PlanHost {
    plans: Plans,
}
impl PlanHost {
    pub fn new(plans: Plans) -> Self {
        Self { plans }
    }

    fn spec() -> ToolSpec {
        ToolSpec {
            name: UPDATE.into(),
            description: format!(
                "Keep a short checklist for a task with several steps; the user sees it beside this conversation. Each call replaces the whole plan, so send every step, in order, with its current status. Mark a step in_progress when you start it and done when it is finished; send an empty list to clear the plan. At most {MAX_STEPS} steps of {MAX_STEP_BYTES} bytes."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "maxItems": MAX_STEPS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string", "description": "What the step does."},
                                "status": {"type": "string", "enum": PlanStatus::ALL.map(PlanStatus::name)}
                            },
                            "required": ["text", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["steps"],
                "additionalProperties": false
            }),
            capabilities: Default::default(),
            permission: ToolPermission::Allow,
            // The result is the plan the model acts on, and is bounded.
            whole_output: true,
        }
    }

    fn run(&self, thread: a206_core::Id, request: &ToolRequest) -> Result<Value, ErrorReport> {
        if request.name != UPDATE {
            return Err(fail(PlanToolError::UnknownTool(request.name.clone())));
        }
        let plan = parse(&request.arguments)?;
        let result = describe(&plan);
        self.plans.set(thread, plan, result.to_string().len())?;
        Ok(result)
    }
}

/// The plan `plan_update`'s arguments describe.
pub fn parse(args: &Map<String, Value>) -> Result<Vec<PlanStep>, ErrorReport> {
    if let Some(key) = args.keys().find(|key| *key != "steps") {
        return Err(fail(PlanToolError::Unknown(key.clone())));
    }
    let steps = args
        .get("steps")
        .ok_or_else(|| fail(PlanToolError::MissingSteps))?
        .as_array()
        .ok_or_else(|| fail(PlanToolError::StepsType))?;
    if steps.len() > MAX_STEPS {
        return Err(fail(PlanToolError::TooManySteps));
    }
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let number = index + 1;
            let step = step
                .as_object()
                .filter(|step| step.keys().all(|key| key == "text" || key == "status"))
                .ok_or_else(|| fail(PlanToolError::StepType(number)))?;
            let text = step
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(PlanToolError::StepType(number)))?
                .trim();
            if text.is_empty() {
                return Err(fail(PlanToolError::EmptyStep(number)));
            }
            if text.len() > MAX_STEP_BYTES {
                return Err(fail(PlanToolError::LongStep(number)));
            }
            let status = step
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(PlanToolError::StepType(number)))?;
            let status = PlanStatus::ALL
                .into_iter()
                .find(|known| known.name() == status)
                .ok_or_else(|| {
                    fail(PlanToolError::Status {
                        step: number,
                        status: status.to_owned(),
                    })
                })?;
            Ok(PlanStep {
                text: text.to_owned(),
                status,
            })
        })
        .collect()
}

/// The plan as a tool result.
fn describe(plan: &[PlanStep]) -> Value {
    json!({
        "plan": plan
            .iter()
            .map(|step| json!({"text": step.text, "status": step.status.name()}))
            .collect::<Vec<_>>(),
        "done": plan.iter().filter(|step| step.status == PlanStatus::Done).count(),
        "total": plan.len(),
    })
}

/// The system-prompt section that restores a plan the model can no longer
/// see: the conversation has one, and no `plan_update` call remains in the
/// history it receives (a compaction summarized them away). Otherwise empty,
/// so the prompt does not change with every update.
pub fn reminder(thread: &Thread) -> String {
    if thread.plan.is_empty() {
        return String::new();
    }
    let since = thread
        .messages
        .iter()
        .rposition(|m| m.compaction.is_some())
        .map_or(0, |at| at + 1);
    let visible = thread.messages[since..]
        .iter()
        .flat_map(|m| &m.tool_calls)
        .any(|call| call.tool == UPDATE && call.state == ToolCallState::Succeeded);
    if visible {
        return String::new();
    }
    let steps: String = thread
        .plan
        .iter()
        .map(|step| format!("\n- [{}] {}", step.status.name(), step.text))
        .collect();
    format!(
        "\n\n# Current plan\n\nThe plan you kept with `{UPDATE}` earlier in this conversation:\n{steps}"
    )
}

impl ToolHost for PlanHost {
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
            self.run(scope.thread, &request).map(|value| ToolOutput {
                text: value.to_string(),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests;
