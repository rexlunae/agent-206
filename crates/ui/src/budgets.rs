//! User-owned native-run limits, shared by all renderers. The gateway enforces
//! and validates them; the lineage calculation here only explains the snapshot.
use crate::{State, error::ErrorNotice};
use a206_core::{Command, ErrorKind, ErrorReport, Id, Reply, Thread, ToolBudget};
use fc_dioxus::prelude::*;
use std::collections::BTreeSet;

fn effective(threads: &[Thread], id: Id) -> Option<ToolBudget> {
    let mut next = Some(id);
    let mut seen = BTreeSet::new();
    let mut budget: Option<ToolBudget> = None;
    while let Some(id) = next {
        if !seen.insert(id) {
            return None;
        }
        let thread = threads.iter().find(|t| t.id == id)?;
        budget = Some(budget.map_or(thread.tool_budget, |b| b.intersect(thread.tool_budget)));
        next = thread.parent;
    }
    budget
}

fn parse(rounds: &str, calls: &str, tokens: &str) -> Option<ToolBudget> {
    let budget = ToolBudget {
        rounds: rounds.trim().parse().ok()?,
        tool_calls: calls.trim().parse().ok()?,
        tokens: if tokens.trim().is_empty() {
            None
        } else {
            Some(tokens.trim().parse().ok()?)
        },
    };
    ((1..=ToolBudget::MAX).contains(&budget.rounds)
        && budget.tool_calls <= ToolBudget::MAX
        && budget.tokens != Some(0))
    .then_some(budget)
}
fn tokens(limit: Option<u64>) -> String {
    limit.map(|n| n.to_string()).unwrap_or_default()
}
fn description(budget: ToolBudget) -> String {
    format!(
        "{} model rounds · {} tool calls · {}",
        budget.rounds,
        budget.tool_calls,
        budget
            .tokens
            .map(|n| format!("{n} tokens"))
            .unwrap_or_else(|| "no token budget set".into())
    )
}

#[component]
pub(crate) fn RunBudgets(thread: Id) -> Element {
    let state = use_context::<State>();
    let mut busy = use_signal(|| false);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let snapshot = state.data.read();
    let Some(current) = snapshot.threads.iter().find(|t| t.id == thread) else {
        return rsx! { p { "Run limits unavailable: conversation is missing." } };
    };
    let current = current.tool_budget;
    let inherited = effective(&snapshot.threads, thread);
    let online = (state.connected)();
    rsx! {
        BudgetEditor { current, inherited, online, busy: busy(),
            onsave: move |(expected, budget)| {
                if *busy.peek() || !*state.connected.peek() {
                    return;
                }
                busy.set(true);
                failure.set(None);
                spawn(async move {
                    match crate::call(Command::UpdateToolBudget { thread, budget, expected }).await {
                        Ok(Reply::Ok) => {},
                        Ok(_) => failure.set(Some(ErrorReport::new(ErrorKind::Protocol, "Unexpected run limit response"))),
                        Err(report) => failure.set(Some(report)),
                    }
                    crate::refresh(state).await;
                    busy.set(false);
                });
            },
        }
        if let Some(report) = failure() {
            ErrorNotice { report, title: "Run limit update error".to_string() }
        }
    }
}

#[component]
fn BudgetEditor(
    current: ToolBudget,
    inherited: Option<ToolBudget>,
    online: bool,
    busy: bool,
    onsave: EventHandler<(ToolBudget, ToolBudget)>,
) -> Element {
    let mut base = use_signal(|| current);
    let mut rounds = use_signal(|| current.rounds.to_string());
    let mut calls = use_signal(|| current.tool_calls.to_string());
    let mut token_limit = use_signal(|| tokens(current.tokens));
    // Polls and another editor may update the saved values. Never erase a
    // draft with unsaved changes; require explicit reload if it conflicts.
    use_effect(use_reactive!(|current| {
        let parsed = parse(&rounds.peek(), &calls.peek(), &token_limit.peek());
        if parsed == Some(*base.peek()) || parsed == Some(current) {
            base.set(current);
            rounds.set(current.rounds.to_string());
            calls.set(current.tool_calls.to_string());
            token_limit.set(tokens(current.tokens));
        }
    }));
    let parsed = parse(&rounds(), &calls(), &token_limit());
    let stale = base() != current;
    let disabled = busy || !online;
    let availability = if disabled {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let save = if disabled || stale || parsed.is_none() || parsed == Some(current) {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    rsx! {
        Disclosure { summary: rsx! { "Run limits" },
            p { class: "a206-muted", "Limits per response; token counts include summaries. Ancestor ceilings still apply. The gateway stops a token-limited run if the provider does not report usage. Changes apply at the next step." }
            p { class: "a206-fact-note", "Saved here: {description(current)}" }
            match inherited {
                Some(budget) => rsx! { p { class: "a206-fact-note", "Effective with ancestors: {description(budget)}" } },
                None => rsx! { p { class: "a206-muted", "Inherited limits are unavailable in this snapshot. The gateway still enforces them." } },
            }
            Field { label: rsx! { "Model rounds" }, hint: rsx! { "Includes the first model request." },
                Input { size: InputSize::Sm, r#type: InputType::Number, min: "1", max: "{ToolBudget::MAX}", step: "1", availability: availability.clone(), value: ReadSignal::from(rounds), oninput: move |value| rounds.set(value) }
            }
            Field { label: rsx! { "Tool calls" }, hint: rsx! { "Zero prevents tool calls for this response." },
                Input { size: InputSize::Sm, r#type: InputType::Number, min: "0", max: "{ToolBudget::MAX}", step: "1", availability: availability.clone(), value: ReadSignal::from(calls), oninput: move |value| calls.set(value) }
            }
            Field { label: rsx! { "Token budget" }, hint: rsx! { "Input plus output, checked after each request; a request can exceed this budget. Blank sets no local limit; inherited limits still apply." },
                Input { size: InputSize::Sm, r#type: InputType::Number, min: "1", step: "1", availability: availability.clone(), value: ReadSignal::from(token_limit), oninput: move |value| token_limit.set(value) }
            }
            if parsed.is_none() { p { role: "status", "Enter whole numbers: 1–{ToolBudget::MAX} rounds, 0–{ToolBudget::MAX} calls, and a positive token budget or blank." } }
            if stale { p { role: "status", "Saved limits changed while you were editing. Reload before saving." } }
            if !online { p { class: "a206-muted", "Connect to the gateway to save limits." } }
            div { class: "a206-row",
                Button { size: ButtonSize::Sm, variant: ButtonVariant::Primary, availability: save, busy,
                    onclick: move |_| {
                        if !disabled && !stale && let Some(budget) = parse(&rounds(), &calls(), &token_limit()) && budget != current {
                            onsave.call((base(), budget));
                        }
                    }, "Save limits" }
                Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, availability,
                    onclick: move |_| { base.set(current); rounds.set(current.rounds.to_string()); calls.set(current.tool_calls.to_string()); token_limit.set(tokens(current.tokens)); }, "Reload saved limits" }
            }
        }
    }
}

#[cfg(test)]
mod tests;
