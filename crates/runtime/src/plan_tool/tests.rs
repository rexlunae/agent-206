use super::*;
use a206_core::{
    CodingBackend, Compaction, CompactionTrigger, Id, Message, Mode, Role, ToolCall, Usage,
};

fn args(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}
fn word() -> String {
    Id::new_v4().to_string()
}
fn code(result: Result<Vec<PlanStep>, ErrorReport>) -> ErrorKind {
    result.unwrap_err().kind
}

#[test]
fn steps_keep_their_order_text_and_status() {
    let steps: Vec<_> = PlanStatus::ALL
        .iter()
        .map(|status| (word(), *status))
        .collect();
    let given: Vec<_> = steps
        .iter()
        .map(|(text, status)| json!({"text": format!("  {text} "), "status": status.name()}))
        .collect();
    let plan = parse(&args(json!({ "steps": given }))).unwrap();
    let got: Vec<_> = plan.iter().map(|s| (s.text.clone(), s.status)).collect();
    assert_eq!(got, steps);
    assert!(parse(&args(json!({"steps": []}))).unwrap().is_empty());
}

#[test]
fn malformed_plans_are_refused() {
    let step = |text: Value, status: Value| json!({"steps": [{"text": text, "status": status}]});
    for bad in [
        json!({}),
        json!({"steps": word()}),
        json!({"steps": [word()]}),
        json!({"steps": [], "extra": 1}),
        json!({"steps": [{"text": word(), "status": "pending", "id": 1}]}),
        step(json!(" "), json!("pending")),
        step(json!("x".repeat(MAX_STEP_BYTES + 1)), json!("pending")),
        step(json!(word()), json!(word())),
        step(json!(word()), json!(null)),
        step(json!(1), json!("done")),
    ] {
        assert_eq!(
            code(parse(&args(bad.clone()))),
            ErrorKind::InvalidRequest,
            "{bad}"
        );
    }
    let many: Vec<_> = (0..=MAX_STEPS)
        .map(|_| json!({"text": word(), "status": "pending"}))
        .collect();
    assert_eq!(
        code(parse(&args(json!({ "steps": many })))),
        ErrorKind::InvalidRequest
    );
    let most: Vec<_> = (0..MAX_STEPS)
        .map(|_| json!({"text": "x".repeat(MAX_STEP_BYTES), "status": "done"}))
        .collect();
    assert_eq!(
        parse(&args(json!({ "steps": most }))).unwrap().len(),
        MAX_STEPS
    );
}

fn update(state: ToolCallState) -> Message {
    let mut message = Message::new(Role::Assistant, word(), 0, CodingBackend::Native);
    message.tool_calls.push(ToolCall {
        id: word(),
        generated_id: false,
        tool: UPDATE.into(),
        arguments: "{}".into(),
        digest: word(),
        healed: false,
        state,
        output: None,
        error: None,
        signature: None,
    });
    message
}
fn summary() -> Message {
    let mut message = Message::new(Role::System, word(), 0, CodingBackend::Native);
    message.compaction = Some(Compaction {
        trigger: CompactionTrigger::Automatic,
        messages: 1,
        previous: None,
        input_tokens: None,
        context_limit: None,
        usage: Usage {
            input_tokens: None,
            output_tokens: None,
            cost: None,
        },
    });
    message
}

#[test]
fn the_reminder_restores_only_a_plan_the_history_no_longer_shows() {
    let mut thread = Thread::new(word(), Mode::default());
    assert_eq!(reminder(&thread), "");
    thread.plan = vec![
        PlanStep {
            text: word(),
            status: PlanStatus::Done,
        },
        PlanStep {
            text: word(),
            status: PlanStatus::InProgress,
        },
    ];
    // A successful update is still in the history.
    thread.messages.push(update(ToolCallState::Succeeded));
    assert_eq!(reminder(&thread), "");
    // Summarized away: the plan is restored, in order, with statuses.
    thread.messages.push(summary());
    let text = reminder(&thread);
    let done = text
        .find(&format!("[done] {}", thread.plan[0].text))
        .unwrap();
    let doing = text
        .find(&format!("[in_progress] {}", thread.plan[1].text))
        .unwrap();
    assert!(done < doing, "{text}");
    // A failed update after the summary does not show the plan.
    thread.messages.push(update(ToolCallState::Failed));
    assert_eq!(reminder(&thread), text);
    thread.messages.push(update(ToolCallState::Succeeded));
    assert_eq!(reminder(&thread), "");
    // An empty plan needs no reminder.
    thread.messages.push(summary());
    thread.plan.clear();
    assert_eq!(reminder(&thread), "");
}
