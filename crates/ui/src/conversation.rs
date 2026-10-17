use crate::{State, dispatch, route::Route};
use a206_core::{
    Capability, CodingBackend, Command, CompactionTrigger, ErrorKind, ErrorReport,
    InstructionStatus, Mode, ModelKey, PlanStatus, Reply, Role, RunState, TextAttachment, Thread,
};
use fc_dioxus::prelude::*;
use uuid::Uuid;
/// Scrolls a linked message into view where the renderer can scroll.
async fn reveal(mounted: MountedEvent) {
    match mounted.data().scroll_to(ScrollBehavior::Instant).await {
        Ok(()) => {}
        // The terminal renderer cannot scroll an element into view; the
        // message is still marked.
        Err(MountedError::NotSupported) => {}
        Err(error) => {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "could not scroll to the linked message"
            );
        }
    }
}
#[component]
pub fn Workspace(id: Uuid, split: Option<Uuid>, message: Option<Uuid>) -> Element {
    let state = use_context::<State>();
    let snapshot = state.data.read();
    let thread = snapshot.threads.iter().find(|t| t.id == id).cloned();
    let secondary = split
        .filter(|second| *second != id)
        .and_then(|second| snapshot.threads.iter().find(|t| t.id == second))
        .cloned();
    let visible_split = secondary.as_ref().map(|thread| thread.id);
    rsx! {
        if let Some(thread) = thread {
            div { class: "a206-chat-header a206-row a206-between",
                strong { "Conversations" }
                div { class: "a206-row a206-chat-actions",
                    NativeSelect { class: "a206-split-selector", size: InputSize::Sm, aria_label: "Open a second conversation", value: visible_split.map(|id| id.to_string()).unwrap_or_default(),
                        onchange: move |value: String| { state.navigator.push(Route::Chat { id, split: Uuid::parse_str(&value).ok().filter(|second| *second != id), message: None }); },
                        option { value: "", selected: visible_split.is_none(), "Split view…" }
                        for other in snapshot.threads.iter().filter(|t| t.id != id) { option { value: "{other.id}", selected: visible_split == Some(other.id), "{other.title}" } }
                    }
                    RenameTitle { key: "{thread.id}:{thread.title}", id: thread.id, initial: thread.title.clone() }
                }
            }
            if split.is_some_and(|second| second != id) && secondary.is_none() {
                Alert { title: rsx! { "Second conversation unavailable" }, "It may have been removed or belong to another gateway.",
                    LinkButton { href: Route::Chat { id, split: None, message: None }.to_string(), variant: ButtonVariant::Ghost, "Close split view" }
                }
            }
            if message.is_some_and(|message| !thread.messages.iter().any(|m| m.id == message)) {
                Alert { title: rsx! { "Message unavailable" }, "That message is no longer in this conversation.",
                    LinkButton { href: Route::Chat { id, split, message: None }.to_string(), variant: ButtonVariant::Ghost, "Show the whole conversation" }
                }
            }
            div { class: "a206-chat-layout",
                if let Some(second) = secondary {
                    Split { key: "columns:{thread.id}", label: name!("Conversation columns"), default_value: 50.0, min: 25.0, max: 75.0,
                        primary: rsx! { Conversation { key: "{thread.id}", thread, target: message } },
                        secondary: rsx! { Conversation { key: "{second.id}", thread: second, target: None } }
                    }
                } else {
                    Split { key: "inspector:{thread.id}", label: name!("Conversation and inspector"), default_value: 73.0, min: 45.0, max: 85.0,
                        primary: rsx! { Conversation { key: "{thread.id}", thread: thread.clone(), target: message } },
                        secondary: rsx! { Inspector { thread } }
                    }
                }
            }
        } else if !(state.loaded)() {
            EmptyState { title: rsx! { "Opening conversation" }, body: rsx! { "Waiting for the gateway to load this workspace." } }
        } else {
            EmptyState { title: rsx! { "Conversation unavailable" }, body: rsx! { "It may have been removed or belong to another gateway." },
                actions: rsx! { LinkButton { href: Route::Conversations {}.to_string(), variant: ButtonVariant::Primary, "All conversations" } }
            }
        }
    }
}
/// What a conversation's composer offers.
#[derive(Debug, PartialEq)]
struct Composer {
    /// A response is running or being requested: Send becomes Stop.
    running: bool,
    /// Enter adds the draft to the running response.
    steerable: bool,
}
/// `sending` is this composer's `Send`, pending until its run ends;
/// `steering` is a `Steer` it has not yet had answered.
fn composer(thread: &Thread, sending: bool, steering: bool) -> Composer {
    let active = thread.state == RunState::Running;
    Composer {
        running: active || sending,
        // A native run takes further messages while it works, including the
        // run this composer started, one steered message at a time.
        steerable: active && thread.backend == CodingBackend::Native && !steering,
    }
}
/// The activity line under a running conversation.
fn run_status(thread: &Thread) -> (Lifecycle, String) {
    match (thread.held, thread.paused) {
        (true, true) => (Lifecycle::Queued, "Paused before its next step".into()),
        (true, false) => (
            Lifecycle::Queued,
            "Waiting: a conversation that delegated this one is paused".into(),
        ),
        (false, true) => (Lifecycle::Running, "Pausing after this step…".into()),
        (false, false) => (
            Lifecycle::Running,
            thread
                .backend_activity
                .clone()
                .unwrap_or_else(|| format!("{} is working…", thread.backend.label())),
        ),
    }
}
/// One conversation; `target` is a message to scroll to and mark.
#[component]
fn Conversation(thread: Thread, target: Option<Uuid>) -> Element {
    let mut state = use_context::<State>();
    let mut draft = use_signal(String::new);
    let mut attachments = use_signal(Vec::<TextAttachment>::new);
    let mut attachment_error = use_signal(|| None::<ErrorReport>);
    let mut command_error = use_signal(|| None::<ErrorReport>);
    let mut reading_files = use_signal(|| false);
    let mut file_input_version = use_signal(|| 0_u64);
    let mut submitting = use_signal(|| false);
    let mut steering = use_signal(|| false);
    let Composer { running, steerable } = composer(&thread, submitting(), steering());
    let steered: Vec<String> = thread.steering.iter().map(|m| m.text.clone()).collect();
    let has_model = thread.model.is_some();
    let selected_model = thread.model.clone();
    let id = thread.id;
    let allowed_for_mode = thread.subagent_models.clone();
    let allowed_for_model = thread.subagent_models.clone();
    let mode = thread.mode;
    let mode_value = use_memo(move || {
        state
            .data
            .read()
            .threads
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.mode.label().to_owned())
    });
    let model_value = use_memo(move || {
        state
            .data
            .read()
            .threads
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.model.as_ref())
            .map(|k| format!("{}:{}", k.provider, k.model))
    });
    let can_create = (state.connected)() && state.gateway_busy.read().is_none();
    let mut submit = move || {
        // A slash command runs instead of sending; anything else is a message.
        // A failed send restores what was typed, so `//new` stays escaped.
        let typed = draft().trim().to_owned();
        let text = match crate::slash::parse(&typed) {
            Ok(crate::slash::Typed::Send(text)) => text,
            Ok(crate::slash::Typed::Run(slash)) => {
                // The conversation as the gateway last reported it.
                let resolved = {
                    let snapshot = state.data.peek();
                    snapshot
                        .threads
                        .iter()
                        .find(|t| t.id == id)
                        .ok_or_else(|| rejected("This conversation is no longer available"))
                        .and_then(|thread| {
                            crate::slash::resolve(
                                slash,
                                thread,
                                &snapshot,
                                can_create,
                                crate::messengers::now(),
                            )
                        })
                };
                match resolved {
                    Ok(command) => {
                        draft.set(String::new());
                        command_error.set(None);
                        crate::slash::run(state, command);
                    }
                    Err(report) => {
                        crate::error::record("slash_command", &report);
                        command_error.set(Some(report));
                    }
                }
                return;
            }
            Err(report) => {
                crate::error::record("slash_command", &report);
                command_error.set(Some(report));
                return;
            }
        };
        if steerable && !text.is_empty() {
            steering.set(true);
            draft.set(String::new());
            let typed = typed.clone();
            spawn(async move {
                match crate::call(Command::Steer {
                    thread: id,
                    text: text.clone(),
                })
                .await
                {
                    Ok(_) => state.error.set(None),
                    Err(error) => {
                        draft.set(typed);
                        state.error.set(Some(error));
                    }
                }
                steering.set(false);
                crate::refresh(state).await;
            });
            return;
        }
        if !running
            && has_model
            && !reading_files()
            && (!text.is_empty() || !attachments.read().is_empty())
        {
            let files = attachments();
            if text.len() + files.iter().map(|a| a.text.len()).sum::<usize>() > 128 * 1024 {
                attachment_error.set(Some(rejected("Message and attachments exceed 128 KiB")));
                return;
            }
            submitting.set(true);
            draft.set(String::new());
            attachments.set(vec![]);
            spawn(async move {
                let result = crate::call(Command::Send {
                    thread: id,
                    text: text.clone(),
                    attachments: files.clone(),
                })
                .await;
                match result {
                    Ok(Reply::Thread(_)) => {
                        state.status.set(String::new());
                        state.error.set(None);
                    }
                    other => {
                        draft.set(typed);
                        attachments.set(files);
                        state.error.set(Some(match other {
                            Err(error) => error,
                            _ => ErrorReport::new(ErrorKind::Protocol, "Unexpected send response"),
                        }));
                    }
                }
                submitting.set(false);
                crate::refresh(state).await;
            });
        }
    };
    let snapshot = state.data.read();
    let models: Vec<_> = snapshot.catalogs.iter().flat_map(|c| &c.models).collect();
    let model_options = models
        .iter()
        .map(|model| {
            let provider = snapshot
                .settings
                .providers
                .iter()
                .find(|p| p.id == model.key.provider)
                .map(|p| p.label.as_str())
                .unwrap_or("Unavailable connection");
            SelectChoice::new(
                format!("{}:{}", model.key.provider, model.key.model),
                &model.name,
            )
            .in_group(provider)
        })
        .collect::<Vec<_>>();
    let details = use_id();
    let model = thread
        .model
        .as_ref()
        .and_then(|key| models.iter().find(|m| &m.key == key));
    let limit = model.and_then(|m| m.context_limit);
    let used = thread.usage.as_ref().and_then(|u| u.input_tokens);
    let context = limit
        .zip(used)
        .and_then(|(limit, used)| (limit > 0).then_some(used as f64 / limit as f64 * 100.0));
    let short_tokens = |v: Option<u64>| {
        v.map(|v| {
            if v >= 1000 {
                format!("{:.1}k", v as f64 / 1000.0)
            } else {
                v.to_string()
            }
        })
        .unwrap_or_else(|| "—".into())
    };
    let context_label = format!("{} / {} ctx", short_tokens(used), short_tokens(limit));
    let catalog = thread
        .model
        .as_ref()
        .and_then(|k| snapshot.catalogs.iter().find(|c| c.provider == k.provider));
    let balance_label = catalog
        .and_then(|c| c.token_balance)
        .map(|v| format!("{} tokens", short_tokens(Some(v))))
        .or_else(|| {
            catalog.and_then(|c| {
                c.balance.map(|v| {
                    format!(
                        "{v:.2} {}",
                        c.balance_currency.as_deref().unwrap_or("units")
                    )
                })
            })
        })
        .unwrap_or_else(|| "Balance —".into());
    // Cost is the whole run's: every model request it made.
    let cost_label = thread
        .run_usage
        .as_ref()
        .and_then(|u| u.cost)
        .map(|v| format!("Cost {v:.4}"))
        .unwrap_or_else(|| "Cost —".into());
    let effort_value = use_memo(move || {
        state
            .data
            .read()
            .threads
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.effort.clone())
            .or_else(|| Some(String::new()))
    });
    let efforts = model
        .map(|m| m.reasoning_efforts.clone())
        .unwrap_or_default();
    let effort_options = if efforts.is_empty() {
        vec![]
    } else {
        std::iter::once(SelectChoice::new("", "Auto effort"))
            .chain(efforts.into_iter().map(|v| SelectChoice::new(v.clone(), v)))
            .collect()
    };
    let effort_unavailable = effort_options.is_empty() || running;
    let commands = crate::slash::suggestions(&draft());
    // A command needs no model; Send runs it (or says why it cannot run).
    let typed_command = !matches!(
        crate::slash::parse(draft().trim()),
        Ok(crate::slash::Typed::Send(_))
    );
    let tool_count = thread.grant.capabilities.len();
    let availability = if running {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    rsx! { Card { as_: CardTag::Div, class: "a206-conversation", aria_label: "Conversation",
        div { class: "a206-pane-title a206-row a206-between", strong { "{thread.title}" } Badge { tone: BadgeTone::Accent, "{mode.label()} · {thread.backend.label()}" } }
        div { class: "a206-messages",
            if thread.messages.is_empty() {
                Card { variant: CardVariant::Feature, class: "a206-chat-welcome",
                EmptyState { compact: true, title: rsx! { "Let's make something worthwhile." },
                    body: rsx! { if selected_model.is_some() { "Your conversation is ready. Describe the work you have in mind." } else { "Choose a model from a connected provider to begin." } },
                    actions: rsx! { if models.is_empty() { LinkButton { variant: ButtonVariant::Secondary, href: Route::Providers {}.to_string(), "Connect a provider" } } }
                }
                }
            }
            if !thread.messages.is_empty() {
            Chat { label: name!("Messages"), ChatGroup {
                for message in &thread.messages {
                    Message { key: "{message.id}", role: match message.role { Role::User => MessageRole::User, Role::Assistant => MessageRole::Assistant, Role::System => MessageRole::System, Role::Tool => MessageRole::Tool },
                        state: Lifecycle::Complete,
                        author: rsx! { match (&message.role, &message.compaction) { (_, Some(_)) => "Summary of earlier messages", (Role::User, _) => "You", (Role::Assistant, _) => message.backend.label(), (Role::System, _) => "System", (Role::Tool, _) => "Tool" } },
                        if let Some(compaction) = &message.compaction {
                            Badge { tone: BadgeTone::Accent,
                                {format!("{} {} earlier messages", match compaction.trigger { CompactionTrigger::Automatic => "Automatically summarized", CompactionTrigger::Manual => "Summarized" }, compaction.messages)}
                            }
                        }
                        if target == Some(message.id) {
                            span { onmounted: move |mounted: MountedEvent| async move { reveal(mounted).await },
                                Badge { tone: BadgeTone::Accent, Icon { name: IconName::Search, size: IconSize::Sm } "Linked message" }
                            }
                        }
                        p { class: "a206-message-text", "{message.text}" }
                        for file in &message.attachments { Badge { Icon { name: IconName::File, size: IconSize::Sm } "{file.name} · {file.text.len()} B" } }
                        for call in &message.tool_calls { crate::tools::ToolTrace { key: "{call.id}", call: call.clone() } }
                    }
                }
            } }
            }
            for request in &thread.approvals { crate::tools::PendingApproval { key: "{request.id}", thread: id, request: request.clone() } }
            for question in &thread.prompts { crate::prompts::PendingPrompt { key: "{question.id}", thread: id, question: question.clone() } }
            if !thread.plan.is_empty() { ThreadPlan { thread: thread.clone() } }
            if running { {let (state, label) = run_status(&thread); rsx! { AgentStatus { state, label: rsx! { "{label}" } } }} }
            if let Some(report) = thread.error.clone().filter(|_| !running && thread.state == RunState::Failed) {
                crate::error::ErrorNotice { report, title: "Run failed".to_string() }
            }
        }
        div { class: "a206-composer",
            Card { as_: CardTag::Div, variant: CardVariant::Elevated, class: "a206-compose-surface", role: "group", aria_label: "Message composer",
                if !commands.is_empty() {
                    List { class: "a206-slash-commands", aria_label: "Commands · Tab completes the first",
                        for spec in commands.iter().copied() {
                            ListItem { key: "{spec.name}", headline: rsx! { "{spec.usage()}" }, supporting: rsx! { "{spec.about}" },
                                action: ListAction::Button(EventHandler::new(move |_| draft.set(spec.completion()))) }
                        }
                    }
                }
                if let Some(report) = command_error() { crate::error::ErrorNotice { report, title: "Command not run".to_string(), dismiss: move |_| command_error.set(None) } }
                div { class: "a206-compose-entry", onkeydown: move |event: KeyboardEvent| {
                    if event.key() == Key::Enter && !event.is_composing() && !event.modifiers().shift() { event.prevent_default(); submit(); }
                    // Tab completes a command once; with nothing left to add it moves focus as usual.
                    else if event.key() == Key::Tab && !event.modifiers().shift()
                        && let Some(completion) = crate::slash::suggestions(&draft()).first().map(|spec| spec.completion()).filter(|c| *c != draft()) {
                        event.prevent_default();
                        draft.set(completion);
                    }
                },
                Textarea { class: "a206-compose-input", aria_label: "Message your agent", rows: "1", autosize: true,
                    placeholder: if has_model { "Describe what you'd like to do… (/ for commands)" } else { "Choose a model to start… (/ for commands)" },
                    value: ReadSignal::from(draft), readonly: running && !steerable,
                    oninput: move |value| { draft.set(value); if command_error.peek().is_some() { command_error.set(None); } },
                }
                }
                if !steered.is_empty() {
                    List { class: "a206-steered", aria_label: "Messages waiting for the running response",
                        for (index, text) in steered.iter().enumerate() {
                            ListItem { key: "{index}", overline: rsx! { "Read before its next step" }, headline: rsx! { "{text}" } }
                        }
                    }
                }
                if !attachments.read().is_empty() {
                    div { class: "a206-attachment-chips", aria_label: "Attached text documents",
                        for (index, file) in attachments.read().iter().enumerate() {
                            Button { size: ButtonSize::Sm, variant: ButtonVariant::Subtle, availability: if running { Availability::Disabled } else { Availability::Enabled },
                                aria_label: format!("Remove attachment {}", file.name), title: "Remove attachment",
                                onclick: move |_| { attachments.write().remove(index); },
                                Icon { name: IconName::File, size: IconSize::Sm } "{file.name} · {file.text.len()} B" Icon { name: IconName::Close, size: IconSize::Sm }
                            }
                        }
                    }
                }
                if let Some(report) = attachment_error() { crate::error::ErrorNotice { report, title: "Attachment not added".to_string(), dismiss: move |_| attachment_error.set(None) } }

                div { class: "a206-compose-toolbar",
                    div { class: "a206-compose-selectors",
                        label { class: "a206-attach fc-btn fc-btn--ghost fc-btn--sm fc-btn--icon", title: "Attach UTF-8 text documents · up to 128 KiB total",
                            Icon { name: IconName::Attach }
                            // A keyed fragment replaces the native input after reading it,
                            // so choosing the same file again emits a fresh change event.
                            for version in [file_input_version()] {
                            input { key: "{version}", r#type: "file", multiple: true, aria_label: "Attach text documents", disabled: running || reading_files(),
                                onchange: move |event: FormEvent| {
                                    let files = event.files();
                                    if files.is_empty() { return; }
                                    reading_files.set(true);
                                    attachment_error.set(None);
                                    spawn(async move {
                                        let mut pending = attachments();
                                        let result = async {
                                            if pending.len() + files.len() > 8 { return Err(rejected("Attach at most eight text documents")); }
                                            let mut total = pending.iter().map(|f| f.text.len()).sum::<usize>();
                                            for file in files {
                                                let text = read_attachment(&file, 128 * 1024 - total).await?;
                                                total += text.len();
                                                pending.push(TextAttachment { name: file.name(), text });
                                            }
                                            Ok(())
                                        }.await;
                                        match result { Ok(()) => attachments.set(pending), Err(report) => { crate::error::record("attach", &report); attachment_error.set(Some(report)) } }
                                        reading_files.set(false);
                                        file_input_version.set(file_input_version().wrapping_add(1));
                                    });
                                }
                            }
                            }
                        }
                        Select { class: "a206-compose-select a206-mode-select", label: name!("Agent mode"), value: ReadSignal::from(mode_value), availability: availability.clone(),
                            options: Mode::ALL.into_iter().map(|m| SelectChoice::new(m.label(), m.label())).collect(),
                            onchange: move |value: String| { if let Some(mode) = Mode::ALL.into_iter().find(|m| m.label() == value) { dispatch(state, Command::ConfigureThread { id, mode, model: selected_model.clone(), subagent_models: allowed_for_mode.clone() }); } }
                        }
                        if model_options.is_empty() {
                            LinkButton { class: "a206-model-connect", size: ButtonSize::Sm, variant: ButtonVariant::Ghost,
                                href: Route::Providers {}.to_string(), "Connect model" Icon { name: IconName::ChevronDown, size: IconSize::Sm }
                            }
                        } else {
                            Select { class: "a206-compose-select a206-model-select", label: name!("Provider and model"), value: ReadSignal::from(model_value), availability,
                                options: model_options, placeholder: "Choose model",
                                onchange: move |value: String| {
                                    let model = value.split_once(':').and_then(|(provider, model)| Some(ModelKey { provider: Uuid::parse_str(provider).ok()?, model: model.into() }));
                                    dispatch(state, Command::ConfigureThread { id, mode, model, subagent_models: allowed_for_model.clone() });
                                }
                            }
                        }
                        Select { class: "a206-compose-select a206-effort-select", label: name!("Reasoning effort"), value: ReadSignal::from(effort_value),
                            options: effort_options, placeholder: "Effort —", title: if effort_unavailable { "Effort choices not reported by the selected model API" } else { "Reasoning effort" },
                            availability: if effort_unavailable { Availability::Disabled } else { Availability::Enabled },
                            onchange: move |effort: String| dispatch(state, Command::SetEffort { thread: id, effort: (!effort.is_empty()).then_some(effort) })
                        }
                    }
                    div { class: "a206-compose-widgets",
                        Button { class: "a206-context-widget", size: ButtonSize::Sm, variant: ButtonVariant::Ghost, popovertarget: "{details}",
                            aria_label: "Context, usage and tool permissions", title: "Context, usage and tool permissions",
                            span { class: "a206-context-dot", aria_hidden: "true", style: context.map(|v| format!("background: conic-gradient(currentColor {}%, transparent 0)", v.clamp(0.0, 100.0))) } "{context_label}"
                            span { class: "a206-tools-count", "· {tool_count} tools" }
                        }
                        Button { class: "a206-account-widget", size: ButtonSize::Sm, variant: ButtonVariant::Ghost, popovertarget: "{details}",
                            aria_label: "API balance and model costs", title: "API balance and model costs", "{balance_label}" span { "· {cost_label}" }
                        }
                        LinkButton { icon_label: name!("Edit mode prompts"), title: "Edit mode prompts", size: ButtonSize::Sm, variant: ButtonVariant::Ghost,
                            href: Route::Prompts {}.to_string(), Icon { name: IconName::Settings } }
                        if steerable {
                            IconButton { icon: IconName::Send, label: name!("Add to the running response"),
                                title: "Add to the running response · Enter",
                                size: ButtonSize::Sm, variant: ButtonVariant::Ghost,
                                availability: if draft().trim().is_empty() { Availability::Disabled } else { Availability::Enabled },
                                onclick: move |_| submit(),
                            }
                        }
                        // A conversation being compacted has no run to pause.
                        if thread.state == RunState::Running && thread.backend == CodingBackend::Native
                            && (thread.paused || thread.compacting.is_none()) {
                            if thread.paused {
                                IconButton { icon: IconName::Play, label: name!("Resume response"), title: "Resume response",
                                    size: ButtonSize::Sm, variant: ButtonVariant::Ghost,
                                    onclick: move |_| dispatch(state, Command::ResumeRun { thread: id }),
                                }
                            } else {
                                IconButton { icon: IconName::Pause, label: name!("Pause response"), title: "Pause before its next step",
                                    size: ButtonSize::Sm, variant: ButtonVariant::Ghost,
                                    onclick: move |_| dispatch(state, Command::PauseRun { thread: id }),
                                }
                            }
                        }
                        IconButton { icon: if running { IconName::Stop } else { IconName::ArrowUp }, label: if running { name!("Stop response") } else { name!("Send message") },
                            title: if running { "Stop response" } else { "Send · Enter (Shift+Enter for a new line)" },
                            class: "a206-compose-send", size: ButtonSize::Sm, variant: ButtonVariant::Primary,
                            availability: if running || typed_command || (has_model && !reading_files() && (!draft().trim().is_empty() || !attachments.read().is_empty())) { Availability::Enabled } else { Availability::Disabled },
                            onclick: move |_| { if running { dispatch(state, Command::CancelRun { thread: id }); } else { submit(); } }
                        }
                    }
                }
                Popover { id: details, class: "a206-chat-popover", title: rsx! { "Conversation details" },
                    ThreadFacts { thread: thread.clone() }
                    ContextControls { thread: thread.clone() }
                    BackendControls { thread: thread.clone() }
                    // Here too, so split view, which has no inspector, keeps it.
                    crate::subagents::Subagents { thread: id }
                }
            }
        }
    } }
}
#[component]
fn Inspector(thread: Thread) -> Element {
    let state = use_context::<State>();
    let snapshot = state.data.read();
    let id = thread.id;
    rsx! { aside { class: "a206-inspector a206-stack", aria_label: "Conversation details",
        h3 { "Conversation details" }
        ThreadFacts { thread: thread.clone() }
        BackendControls { thread: thread.clone() }
        if thread.backend == CodingBackend::Native {
            Divider {}
            crate::tools::ToolPermissions { thread: id }
        }
        Divider {}
        crate::subagents::Subagents { thread: id }
        Divider {}
        h3 { "Subagent models" }
        p { class: "a206-muted", "Choose which models this conversation may delegate to." }
        if snapshot.catalogs.iter().all(|c| c.models.is_empty()) { p { class: "a206-number", "Connect a provider to discover models." } }
        for model in snapshot.catalogs.iter().flat_map(|c| &c.models) {
            SubagentModel { key: "{model.key.provider}:{model.key.model}", thread: id, model: model.clone() }
        }
        if !thread.subagent_models.is_empty() {
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, onclick: move |_| {
                if let Some(model) = state.data.read().threads.iter().find(|t| t.id == id).and_then(|t| t.subagent_models.first()).cloned() {
                    dispatch(state, Command::CreateSubagent { parent: id, title: "Delegated task".into(), mode: Mode::Plan, model, requested: Default::default() });
                }
            }, "Create delegated conversation" }
        }
    } }
}

#[component]
fn SubagentModel(thread: Uuid, model: a206_core::ModelInfo) -> Element {
    let state = use_context::<State>();
    let provider = state
        .data
        .read()
        .settings
        .providers
        .iter()
        .find(|p| p.id == model.key.provider)
        .map(|p| p.label.clone())
        .unwrap_or_else(|| "Unavailable connection".into());
    let observed = model.key.clone();
    let checked = use_memo(move || {
        if state
            .data
            .read()
            .threads
            .iter()
            .any(|t| t.id == thread && t.subagent_models.contains(&observed))
        {
            CheckState::On
        } else {
            CheckState::Off
        }
    });
    rsx! { Checkbox { label: rsx! { "{provider} · {model.name}" }, checked: ReadSignal::from(checked),
        onchange: move |allowed| dispatch(state, Command::SetSubagentModel { id: thread, model: model.key.clone(), allowed })
    } }
}

#[component]
fn BackendControls(thread: Thread) -> Element {
    let state = use_context::<State>();
    let id = thread.id;
    let selected = use_memo(move || {
        state
            .data
            .read()
            .threads
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.backend.label().to_owned())
    });
    let options = CodingBackend::ALL
        .into_iter()
        .filter(|kind| {
            *kind == CodingBackend::Native
                || state
                    .data
                    .read()
                    .settings
                    .backends
                    .iter()
                    .any(|b| b.kind == *kind)
        })
        .map(|kind| SelectChoice::new(kind.label(), kind.label()))
        .collect();
    let availability = if thread.state == RunState::Running || thread.parent.is_some() {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let grants = thread.grant.capabilities.clone();
    rsx! { div { class: "a206-stack",
        h3 { "Agent engine" }
        Select { label: name!("Coding-agent backend"), value: ReadSignal::from(selected), options, availability,
            // A grant the new backend cannot be held to is dropped, not refused.
            onchange: move |value: String| { if let Some(backend) = CodingBackend::ALL.into_iter().find(|kind| kind.label() == value) { dispatch(state, Command::SetBackend { thread: id, backend, capabilities: grants.iter().copied().filter(|c| backend.grantable().contains(c)).collect() }); } }
        }
        LinkButton { variant: ButtonVariant::Ghost, size: ButtonSize::Sm, href: Route::Providers {}.to_string(), "Manage connections" }
        if thread.backend == CodingBackend::Native {
            p { class: "a206-muted", "Tools are off until you grant them here, and each tool still needs your permission in this conversation. The mode limits what can be granted here." }
        } else {
            p { class: "a206-muted", "Project tools are off until you grant them. External subagents and direct network access are disabled." }
        }
        if thread.project.is_none() { p { class: "a206-muted", "Open a project coordinator to grant workspace and git tools." } }
        for capability in thread.backend.grantable().iter().copied() {
            BackendGrant { key: "{capability:?}", thread: id, capability }
        }
    } }
}
#[component]
fn BackendGrant(thread: Uuid, capability: Capability) -> Element {
    let state = use_context::<State>();
    let checked = use_memo(move || {
        if state
            .data
            .read()
            .threads
            .iter()
            .find(|t| t.id == thread)
            .is_some_and(|t| t.grant.capabilities.contains(&capability))
        {
            CheckState::On
        } else {
            CheckState::Off
        }
    });
    let snapshot = state.data.read();
    let current = snapshot.threads.iter().find(|t| t.id == thread);
    let enabled = current.is_some_and(|t| {
        (t.project.is_some() || capability == Capability::Network)
            && t.parent.is_none()
            && t.state != RunState::Running
            && t.mode.ceiling().contains(&capability)
    });
    let label = capability.label();
    rsx! { Checkbox { label: rsx! { "{label}" }, checked: ReadSignal::from(checked), availability: if enabled { Availability::Enabled } else { Availability::Disabled }, onchange: move |allowed| {
        if let Some(current) = state.data.read().threads.iter().find(|t| t.id == thread).cloned() {
            let mut capabilities = current.grant.capabilities;
            // Writing needs reading: granting a write grant adds its read grant,
            // and withdrawing a read grant withdraws what depends on it.
            if allowed {
                capabilities.insert(capability);
                match capability {
                    Capability::WriteWorkspace => { capabilities.insert(Capability::ReadWorkspace); }
                    Capability::GitWrite => { capabilities.insert(Capability::GitRead); }
                    _ => {}
                }
            } else {
                capabilities.remove(&capability);
                match capability {
                    Capability::ReadWorkspace => { capabilities.remove(&Capability::WriteWorkspace); capabilities.remove(&Capability::Execute); }
                    Capability::GitRead => { capabilities.remove(&Capability::GitWrite); }
                    _ => {}
                }
            }
            dispatch(state, Command::SetBackend { thread, backend: current.backend, capabilities });
        }
    } } }
}

#[component]
fn ThreadFacts(thread: Thread) -> Element {
    let state = use_context::<State>();
    let snapshot = state.data.read();
    let model = thread.model.as_ref().and_then(|key| {
        snapshot
            .catalogs
            .iter()
            .flat_map(|c| &c.models)
            .find(|m| &m.key == key)
    });
    let limit = model.and_then(|m| m.context_limit);
    let input = thread.usage.as_ref().and_then(|u| u.input_tokens);
    let output = thread.usage.as_ref().and_then(|u| u.output_tokens);
    let run = thread.run_usage.as_ref();
    let (run_input, run_output) = (
        run.and_then(|u| u.input_tokens),
        run.and_then(|u| u.output_tokens),
    );
    let cost = run.and_then(|u| u.cost);
    let pct = limit
        .zip(input)
        .and_then(|(limit, used)| (limit > 0).then_some(used as f64 / limit as f64 * 100.0));
    let catalog = thread.model.as_ref().and_then(|key| {
        snapshot
            .catalogs
            .iter()
            .find(|c| c.provider == key.provider)
    });
    let price = |v: Option<f64>| {
        v.map(|v| format!("{:.4} / 1M", v * 1_000_000.0))
            .unwrap_or_else(|| "Not reported".into())
    };
    let number = |v: Option<u64>| {
        v.map(|v| v.to_string())
            .unwrap_or_else(|| "Not reported".into())
    };
    rsx! { div { class: "a206-thread-facts",
        dl { class: "a206-fact-grid",
            dt { "Agent engine" } dd { "{thread.backend.label()}" }
            if let Some(activity) = &thread.backend_activity { dt { "Last operation" } dd { "{activity}" } }
            dt { "Context used" } dd { {pct.map(|p| format!("{p:.1}%")).unwrap_or_else(|| "Not reported".into())} }
            dt { "Context limit" } dd { "{number(limit)}" }
            dt { "Output limit" } dd { "{number(model.and_then(|m| m.output_limit))}" }
            dt { "Last request input / output" } dd { "{number(input)} / {number(output)}" }
            dt { "Run input / output" } dd { "{number(run_input)} / {number(run_output)}" }
            dt { "Run cost" } dd { {cost.map(|c| format!("{c:.6}")).unwrap_or_else(|| "Not reported".into())} }
            dt { "Input / output rate" } dd { "{price(model.and_then(|m| m.input_cost_per_token))} / {price(model.and_then(|m| m.output_cost_per_token))}" }
            dt { "Account balance" } dd { {catalog.and_then(|c| c.balance.map(|v| format!("{v:.4} {}", c.balance_currency.as_deref().unwrap_or("provider units")))).unwrap_or_else(|| "Not reported".into())} }
            dt { "Token balance" } dd { "{number(catalog.and_then(|c| c.token_balance))}" }
            dt { "Reasoning effort" } dd { {thread.effort.clone().unwrap_or_else(|| "Provider default".into())} }
            dt { "Subagent models" } dd { "{thread.subagent_models.len()} allowed" }
        }
        if let Some(percent) = pct { Progress { label: name!("Context window used"), value: ProgressValue::Of { value: percent.min(100.0), max: 100.0 } } }
        p { class: "a206-fact-note", "Last response · provider-reported usage" }
        div { class: "a206-permissions",
            strong { "Tool permissions" }
            if thread.grant.capabilities.is_empty() { span { class: "a206-muted", "None granted" } }
            for grant in &thread.grant.capabilities { Badge { "{grant:?}" } }
        }
    } }
}

/// The checklist the conversation's models keep with `plan_update`.
#[component]
fn ThreadPlan(thread: Thread) -> Element {
    let done = thread
        .plan
        .iter()
        .filter(|step| step.status == PlanStatus::Done)
        .count();
    let total = thread.plan.len();
    rsx! { Card { as_: CardTag::Section, class: "a206-plan", aria_label: "Plan",
        div { class: "a206-row",
            strong { "Plan" }
            span { class: "a206-muted", {format!("{done} of {total} done")} }
        }
        Progress { label: name!("Plan steps done"), value: ProgressValue::Of { value: done as f64, max: total as f64 } }
        ol { class: "a206-plan-steps",
            for (index, step) in thread.plan.iter().enumerate() {
                li { key: "{index}", class: "a206-row",
                    Badge { tone: match step.status { PlanStatus::Done => BadgeTone::Success, PlanStatus::InProgress => BadgeTone::Accent, PlanStatus::Pending => BadgeTone::Neutral },
                        if step.status == PlanStatus::Done { Icon { name: IconName::Check, size: IconSize::Sm } }
                        "{step.status.label()}"
                    }
                    span { "{step.text}" }
                }
            }
        }
    } }
}

/// Compaction and the project instruction files of the latest run.
#[component]
fn ContextControls(thread: Thread) -> Element {
    let state = use_context::<State>();
    let id = thread.id;
    let native = thread.backend == CodingBackend::Native;
    let summaries = thread
        .messages
        .iter()
        .filter(|m| m.compaction.is_some())
        .count();
    let availability = if native && thread.state != RunState::Running && thread.model.is_some() {
        Availability::Enabled
    } else {
        Availability::Disabled
    };
    rsx! { div { class: "a206-stack",
        strong { "Context" }
        if native {
            p { class: "a206-muted",
                {format!("Earlier messages are summarized automatically when a request uses more than {}% of the context limit the model's API reports. With no reported limit, only you can compact.", a206_core::COMPACTION_THRESHOLD_PERCENT)}
            }
            p { class: "a206-fact-note", {format!("{summaries} summaries in this conversation")} }
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, availability,
                title: "Summarize all but the latest turn with this conversation's model",
                onclick: move |_| dispatch(state, Command::CompactThread { thread: id }),
                "Compact earlier messages"
            }
        }
        strong { "Project instructions" }
        if thread.instructions.is_empty() {
            span { class: "a206-muted", "None loaded by the latest run" }
        }
        for file in &thread.instructions {
            div { key: "{file.path}", class: "a206-row",
                Badge { tone: match file.status { InstructionStatus::Included => BadgeTone::Success, InstructionStatus::Truncated => BadgeTone::Warning, InstructionStatus::Skipped => BadgeTone::Neutral },
                    {match file.status { InstructionStatus::Included => "Included", InstructionStatus::Truncated => "Truncated", InstructionStatus::Skipped => "Skipped" }}
                }
                span { "{file.path}" }
                span { class: "a206-muted", {format!("{} bytes", file.bytes)} }
                if let Some(report) = &file.error { span { class: "a206-muted", "{report.message}" } }
            }
        }
    } }
}

#[component]
fn RenameTitle(id: Uuid, initial: String) -> Element {
    let state = use_context::<State>();
    let mut title = use_signal(|| initial);
    let popover = use_id();
    rsx! {
        Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, popovertarget: "{popover}", "Rename" }
        Popover { id: popover.clone(), title: rsx! { "Rename conversation" },
            Field { label: rsx! { "Conversation title" }, Input { value: ReadSignal::from(title), oninput: move |v| title.set(v) } }
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Primary, popovertarget: "{popover}", popovertargetaction: "hide",
                onclick: move |_| dispatch(state, Command::RenameThread { id, title: title() }), "Save title" }
        }
    }
}

async fn read_attachment(
    file: &dioxus::html::FileData,
    remaining: usize,
) -> Result<String, ErrorReport> {
    let bytes = crate::files::read_bounded(file, remaining)
        .await
        .map_err(unreadable)?
        .ok_or_else(|| rejected("Attachments exceed 128 KiB"))?;
    let text = String::from_utf8(bytes).map_err(|error| {
        rejected("Use UTF-8 text documents; image, PDF and audio attachments are not supported yet")
            .caused_by(ErrorReport::capture(&error, |_| None))
    })?;
    if text.contains('\0') {
        return Err(rejected(
            "Use UTF-8 text documents totaling at most 128 KiB",
        ));
    }
    Ok(text)
}
fn rejected(message: &str) -> ErrorReport {
    ErrorReport::new(ErrorKind::InvalidRequest, message)
}
/// Keep the browser or file system's own reason as the cause.
fn unreadable(error: dioxus::CapturedError) -> ErrorReport {
    ErrorReport::new(ErrorKind::Other, "Could not read the attachment")
        .caused_by(ErrorReport::capture(&**error, |_| None))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_running_conversation_says_whether_a_pause_holds_it() {
        let mut thread = Thread::new(Uuid::new_v4().to_string(), Mode::Chat);
        thread.state = RunState::Running;
        let working = run_status(&thread);
        assert_eq!(working.0, Lifecycle::Running);
        let mut statuses = vec![working.1];
        for (held, paused) in [(false, true), (true, true), (true, false)] {
            (thread.held, thread.paused) = (held, paused);
            let (lifecycle, label) = run_status(&thread);
            assert_eq!(lifecycle == Lifecycle::Queued, held, "{label}");
            statuses.push(label);
        }
        statuses.sort();
        statuses.dedup();
        assert_eq!(statuses.len(), 4, "{statuses:?}");
    }

    #[test]
    fn the_composer_steers_the_run_it_started_one_message_at_a_time() {
        let mut thread = Thread::new(Uuid::new_v4().to_string(), Mode::Chat);
        let offers = |thread: &Thread, sending, steering| composer(thread, sending, steering);
        // Before the gateway reports the run, a pending Send only stops.
        assert_eq!(
            offers(&thread, true, false),
            Composer {
                running: true,
                steerable: false
            }
        );
        thread.state = RunState::Running;
        // This composer's own Send stays pending for the whole run.
        assert_eq!(
            offers(&thread, true, false),
            Composer {
                running: true,
                steerable: true
            }
        );
        assert_eq!(
            offers(&thread, false, false),
            Composer {
                running: true,
                steerable: true
            }
        );
        // Until a steered message is accepted, no second one is sent.
        assert!(!offers(&thread, true, true).steerable);
        thread.backend = CodingBackend::ALL
            .into_iter()
            .find(|b| *b != CodingBackend::Native)
            .unwrap();
        assert!(!offers(&thread, false, false).steerable);
    }

    use a206_core::PlanStep;

    fn render(thread: Thread) -> String {
        #[derive(Props, Clone, PartialEq)]
        struct Shown {
            thread: Thread,
        }
        fn root(props: Shown) -> Element {
            rsx! { ThreadPlan { thread: props.thread } }
        }
        let mut dom = VirtualDom::new_with_props(root, Shown { thread });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_plan_shows_each_step_in_order_with_its_status() {
        let mut thread = Thread::new(Uuid::new_v4().to_string(), Mode::default());
        let mut statuses = PlanStatus::ALL.to_vec();
        statuses.push(PlanStatus::Done);
        thread.plan = statuses
            .into_iter()
            .map(|status| PlanStep {
                text: Uuid::new_v4().simple().to_string(),
                status,
            })
            .collect();
        let html = render(thread.clone());
        let at: Vec<_> = thread
            .plan
            .iter()
            .map(|step| html.find(&step.text).unwrap())
            .collect();
        assert!(at.windows(2).all(|pair| pair[0] < pair[1]), "{html}");
        for status in PlanStatus::ALL {
            assert!(html.contains(status.label()), "{html}");
        }
        assert!(html.contains("2 of 4 done"), "{html}");
    }
}
