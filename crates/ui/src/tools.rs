//! Tool traces, pending approvals and per-tool permissions in a conversation.
use crate::{State, dispatch, error::ErrorNotice};
use a206_core::{
    ApprovalDecision, ApprovalRequest, Command, ErrorKind, ErrorReport, OutputCount,
    OutputReduction, Reply, ToolCall, ToolCallState, ToolPermission,
};
use fc_dioxus::prelude::*;
use uuid::Uuid;

fn capability_list(capabilities: &std::collections::BTreeSet<a206_core::Capability>) -> String {
    capabilities
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}
fn lifecycle(state: ToolCallState) -> Lifecycle {
    match state {
        ToolCallState::Requested => Lifecycle::Queued,
        ToolCallState::AwaitingApproval => Lifecycle::Pending,
        ToolCallState::Running => Lifecycle::Running,
        ToolCallState::Succeeded => Lifecycle::Complete,
        ToolCallState::Failed => Lifecycle::Error,
        ToolCallState::Denied => Lifecycle::Denied,
        ToolCallState::Cancelled | ToolCallState::Skipped => Lifecycle::Aborted,
    }
}

/// One tool call: its arguments, then its result or why it did not run.
#[component]
pub(crate) fn ToolTrace(call: ToolCall) -> Element {
    let state = lifecycle(call.state);
    let failed = call.state.is_final() && call.state != ToolCallState::Succeeded;
    rsx! {
        ToolCall { name: call.tool.clone(), state,
            progress: (call.state == ToolCallState::Running).then_some(ToolProgress::Indeterminate),
            payload_label: rsx! { if call.healed { "Arguments (repaired)" } else { "Arguments" } },
            payload: Clipped::chars(call.arguments.clone(), 240),
            error: if failed { call.error.clone().map(|report| rsx! { ErrorNotice { report } }) } else { None },
        }
        if call.state == ToolCallState::Succeeded && let Some(output) = call.output.clone() {
            Disclosure { summary: rsx! { "Result from {call.tool}" },
                pre { class: "a206-tool-output", "{output}" }
            }
        }
        if !call.redactions.is_empty() {
            p { class: "a206-muted", {redaction_note(&call.redactions)} }
        }
        if !call.injection.is_empty() {
            p { class: "a206-muted", {injection_note(&call.injection)} }
        }
        if let Some(reduction) = call.reduction.as_ref() {
            p { class: "a206-muted", {reduction_note(reduction)} }
        }
    }
}

/// Which kinds of credential were removed before the model saw a result.
fn redaction_note(redactions: &[OutputCount]) -> String {
    let kinds: Vec<String> = redactions
        .iter()
        .map(|found| format!("{} × {}", found.name, found.count))
        .collect();
    format!(
        "Credentials removed before the model saw this result: {}",
        kinds.join(", ")
    )
}

/// The kinds of text in a result that read as instructions to the model,
/// which the model was told came from the tool.
fn injection_note(found: &[OutputCount]) -> String {
    let kinds: Vec<String> = found
        .iter()
        .map(|found| format!("{} × {}", found.name, found.count))
        .collect();
    format!(
        "This result contains text addressed to the model, marked as tool data for it: {}",
        kinds.join(", ")
    )
}

/// What an output rule did to a result, which is all the model saw of it.
fn reduction_note(reduction: &OutputReduction) -> String {
    let layer = if reduction.user_rule {
        "your"
    } else {
        "the built-in"
    };
    let counts: String = reduction
        .counts
        .iter()
        .map(|count| format!("; {}: {}", count.name, count.count))
        .collect();
    format!(
        "Shortened for the model by {layer} output rule `{}` from {} bytes{counts}",
        reduction.rule, reduction.original_bytes
    )
}

/// A call waiting for the user. Only this trusted surface can answer it; the
/// decision is bound to the digest of the arguments shown here.
#[component]
pub(crate) fn PendingApproval(thread: Uuid, request: ApprovalRequest) -> Element {
    let state = use_context::<State>();
    let mut deciding = use_signal(|| false);
    let availability = if deciding() {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let (approval, digest) = (request.id, request.digest.clone());
    let capabilities = capability_list(&request.capabilities);
    let decide = move |decision| {
        let digest = digest.clone();
        move |_| {
            let mut state = state;
            let digest = digest.clone();
            deciding.set(true);
            spawn(async move {
                let command = Command::ResolveApproval {
                    thread,
                    approval,
                    digest,
                    decision,
                };
                match crate::call(command).await {
                    Ok(_) => state.error.set(None),
                    // The request stays until the gateway removes it; let the
                    // user try again after seeing why this answer failed.
                    Err(report) => {
                        deciding.set(false);
                        state.error.set(Some(report));
                    }
                }
            });
        }
    };
    rsx! {
        Approval { label: name!("Tool call waiting for approval"), region: true,
            record: Record { state: Some(Lifecycle::Pending), ..Record::default() },
            title: rsx! { "Allow {request.tool} to run once?" },
            meta: rsx! { if capabilities.is_empty() { "Uses no granted capabilities." } else { "Uses: {capabilities}" } },
            actions: rsx! {
                Button { variant: ButtonVariant::Secondary, availability: availability.clone(), onclick: decide(ApprovalDecision::Deny), "Deny" }
                Button { variant: ButtonVariant::Primary, availability, onclick: decide(ApprovalDecision::Approve), "Allow once" }
            },
            p { "The model asked to run this tool with exactly these arguments:" }
            pre { class: "a206-tool-output", "{request.arguments}" }
        }
    }
}

/// Everything a conversation's effective tool permissions are derived from:
/// its own and each ancestor's settings, grant and mode, nearest first.
/// The gateway combines the whole chain, so a parent's change must refresh
/// a child's listing too.
type PolicyInputs = Vec<(
    std::collections::BTreeMap<String, ToolPermission>,
    a206_core::Grant,
    a206_core::Mode,
)>;
fn policy_revision(threads: &[a206_core::Thread], thread: Uuid) -> PolicyInputs {
    // The gateway's own bound on delegation depth; a cycle cannot loop here.
    const MAX_LINEAGE: usize = 33;
    std::iter::successors(threads.iter().find(|t| t.id == thread), |t| {
        t.parent
            .and_then(|parent| threads.iter().find(|p| p.id == parent))
    })
    .take(MAX_LINEAGE)
    .map(|t| (t.tool_permissions.clone(), t.grant.clone(), t.mode))
    .collect()
}

/// The tools this conversation could use, with the user's Allow/Ask/Deny for each.
#[component]
pub(crate) fn ToolPermissions(thread: Uuid) -> Element {
    let state = use_context::<State>();
    // Reload when the permissions, grants or modes this listing depends on change.
    let revision = use_memo(move || policy_revision(&state.data.read().threads, thread));
    let listing = use_resource(move || async move {
        // Reading subscribes this listing to the conversation's changes.
        revision.read();
        match crate::call(Command::ListTools { thread }).await {
            Ok(Reply::Tools(list)) => Ok(list),
            Ok(_) => Err(ErrorReport::new(
                ErrorKind::Protocol,
                "Unexpected tool listing response",
            )),
            Err(report) => Err(report),
        }
    });
    let current = listing.read().clone();
    rsx! {
        h3 { "Tools" }
        p { class: "a206-muted", "Unset tools are denied. Ask waits for your approval of each call." }
        match current {
            None => rsx! { AgentStatus { state: Lifecycle::Running, label: rsx! { "Loading tools…" } } },
            Some(Err(report)) => rsx! { ErrorNotice { report, title: "Tools unavailable".to_string() } },
            Some(Ok(list)) => rsx! {
                if list.tools.is_empty() { p { class: "a206-number", "No tool sources are connected to this gateway." } }
                for offer in list.tools {
                    div { key: "{offer.name}", class: "a206-stack",
                        div { class: "a206-row a206-between",
                            strong { "{offer.name}" }
                            Badge { tone: match offer.effective { ToolPermission::Allow => BadgeTone::Success, ToolPermission::Ask => BadgeTone::Warning, ToolPermission::Deny => BadgeTone::Neutral }, "{offer.effective.label()}" }
                        }
                        if !offer.description.is_empty() { p { class: "a206-muted", "{offer.description}" } }
                        if !offer.missing.is_empty() { p { class: "a206-muted", "Needs a grant for: {capability_list(&offer.missing)}" } }
                        NativeSelect { size: InputSize::Sm, aria_label: "Permission for {offer.name}",
                            value: offer.permission.map(|p| p.label().to_owned()).unwrap_or_default(),
                            onchange: move |value: String| {
                                let permission = ToolPermission::ALL.into_iter().find(|p| p.label() == value);
                                dispatch(state, Command::SetToolPermission { thread, tool: offer.name.clone(), permission });
                            },
                            option { value: "", selected: offer.permission.is_none(), "Not set (deny)" }
                            for permission in ToolPermission::ALL {
                                option { value: "{permission.label()}", selected: offer.permission == Some(permission), "{permission.label()}" }
                            }
                        }
                    }
                }
                for unavailable in list.unavailable {
                    ErrorNotice { report: unavailable.error, title: format!("{} could not list its tools", unavailable.source) }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn render(call: ToolCall) -> String {
        fn root(call: ToolCall) -> Element {
            rsx! { ToolTrace { call } }
        }
        let mut dom = VirtualDom::new_with_props(root, call);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }
    fn call(state: ToolCallState) -> ToolCall {
        let id = Uuid::new_v4().simple().to_string();
        ToolCall {
            id: id.clone(),
            generated_id: false,
            tool: Uuid::new_v4().simple().to_string(),
            arguments: format!("{{\"value\":\"{id}\"}}"),
            digest: Uuid::new_v4().to_string(),
            healed: false,
            state,
            output: None,
            error: None,
            signature: None,
            reduction: None,
            redactions: Vec::new(),
            injection: Vec::new(),
        }
    }
    #[test]
    fn a_listing_refreshes_when_any_ancestor_changes() {
        use a206_core::{Capability, Mode, Thread};
        let tool = Uuid::new_v4().simple().to_string();
        let mut root = Thread::new(String::new(), Mode::Code);
        let mut parent = Thread::new(String::new(), Mode::Plan);
        parent.parent = Some(root.id);
        let mut child = Thread::new(String::new(), Mode::Plan);
        child.parent = Some(parent.id);
        let unrelated = Thread::new(String::new(), Mode::Chat);
        let child_id = child.id;
        let mut threads = vec![root.clone(), parent.clone(), child, unrelated];
        let before = policy_revision(&threads, child_id);
        assert_eq!(before.len(), 3);
        // An unrelated conversation changing does not matter.
        threads[3]
            .tool_permissions
            .insert(tool.clone(), ToolPermission::Allow);
        assert_eq!(policy_revision(&threads, child_id), before);
        // Each ancestor's settings, grant and mode do.
        root.tool_permissions
            .insert(tool.clone(), ToolPermission::Ask);
        threads[0] = root.clone();
        let after_root = policy_revision(&threads, child_id);
        assert_ne!(after_root, before);
        parent.grant.capabilities.insert(Capability::ReadWorkspace);
        threads[1] = parent.clone();
        let after_grant = policy_revision(&threads, child_id);
        assert_ne!(after_grant, after_root);
        parent.mode = Mode::Chat;
        threads[1] = parent;
        assert_ne!(policy_revision(&threads, child_id), after_grant);
    }
    #[test]
    fn a_tool_reported_failure_shows_its_explanation() {
        let mut failed = call(ToolCallState::Failed);
        let explanation = Uuid::new_v4().to_string();
        failed.error = Some(
            ErrorReport::new(
                ErrorKind::Other,
                format!("tool `{}` reported an error", failed.tool),
            )
            .caused_by(ErrorReport::new(ErrorKind::Other, explanation.clone())),
        );
        let html = render(failed);
        assert!(html.contains(&explanation), "{html}");
    }
    #[test]
    fn a_trace_shows_the_tool_its_arguments_and_its_result() {
        let mut done = call(ToolCallState::Succeeded);
        let output = Uuid::new_v4().to_string();
        done.output = Some(output.clone());
        let html = render(done.clone());
        assert!(
            html.contains(&done.tool) && html.contains(&done.id),
            "{html}"
        );
        assert!(html.contains(&output), "{html}");
        assert!(html.contains(Lifecycle::Complete.as_str()), "{html}");
    }
    #[test]
    fn a_result_says_how_it_was_changed_for_the_model() {
        let mut done = call(ToolCallState::Succeeded);
        done.output = Some(Uuid::new_v4().to_string());
        let plain = render(done.clone());
        assert!(!plain.contains("Shortened"), "{plain}");
        let (rule, name) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
        let bytes = Uuid::new_v4().as_u128() as u64;
        done.reduction = Some(OutputReduction {
            rule: rule.clone(),
            user_rule: true,
            original_bytes: bytes,
            counts: vec![OutputCount {
                name: name.clone(),
                count: 3,
            }],
        });
        let html = render(done.clone());
        assert!(
            html.contains(&format!(
                "Shortened for the model by your output rule `{rule}` from {bytes} bytes; {name}: 3"
            )),
            "{html}"
        );
        assert!(!html.contains("Credentials removed"), "{html}");
        let kind = Uuid::new_v4().simple().to_string();
        done.redactions = vec![OutputCount {
            name: kind.clone(),
            count: 2,
        }];
        let html = render(done.clone());
        assert!(
            html.contains(&format!(
                "Credentials removed before the model saw this result: {kind} × 2"
            )),
            "{html}"
        );
        assert!(!html.contains("addressed to the model"), "{html}");
        let kind = Uuid::new_v4().simple().to_string();
        done.injection = vec![OutputCount {
            name: kind.clone(),
            count: 4,
        }];
        let html = render(done);
        assert!(
            html.contains(&format!(
                "This result contains text addressed to the model, marked as tool data for it: {kind} × 4"
            )),
            "{html}"
        );
    }
    #[test]
    fn a_refused_call_shows_its_structured_reason_not_its_output() {
        let mut denied = call(ToolCallState::Denied);
        let reason = Uuid::new_v4().to_string();
        denied.error = Some(ErrorReport::new(
            ErrorKind::PermissionDenied,
            reason.clone(),
        ));
        denied.output = Some(reason.clone());
        let html = render(denied);
        assert!(html.contains(Lifecycle::Denied.as_str()), "{html}");
        assert!(html.contains(ErrorKind::PermissionDenied.label()), "{html}");
        assert!(html.contains(&reason), "{html}");
        assert!(!html.contains("Result from"), "{html}");
    }
}
