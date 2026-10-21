//! Work in progress across every conversation: each running response with
//! the subagents it delegated to, what is waiting on the user, and scheduled
//! messages being answered. Everything shown comes from the gateway's state.
use crate::{PageFrame, State, conversation::run_status, dispatch, route::Route};
use a206_core::{CodingBackend, Command, Job, JobOutcome, PlanStatus, RunState, Snapshot, Thread};
use fc_dioxus::prelude::*;
use std::collections::BTreeSet;
use uuid::Uuid;

/// Whether `thread` has a native run that can be paused now. A conversation
/// being compacted is Running without one.
pub(crate) fn pausable(thread: &Thread) -> bool {
    thread.state == RunState::Running
        && thread.backend == CodingBackend::Native
        && thread.compacting.is_none()
        && !thread.paused
}

/// Whether `thread`'s run is paused and can be resumed.
pub(crate) fn resumable(thread: &Thread) -> bool {
    thread.state == RunState::Running && thread.backend == CodingBackend::Native && thread.paused
}

/// A running conversation and the running conversations it delegated to.
#[derive(Clone, Debug, PartialEq)]
struct Node {
    thread: Thread,
    below: Vec<Node>,
}

/// Running conversations as trees. A running conversation whose parent is
/// not running, or no longer exists, starts a tree of its own.
fn running(snapshot: &Snapshot) -> Vec<Node> {
    let live: BTreeSet<Uuid> = snapshot
        .threads
        .iter()
        .filter(|t| t.state == RunState::Running)
        .map(|t| t.id)
        .collect();
    let roots = snapshot
        .threads
        .iter()
        .filter(|t| t.state == RunState::Running && !t.parent.is_some_and(|p| live.contains(&p)));
    let mut seen = BTreeSet::new();
    let mut trees: Vec<Node> = roots.map(|t| grow(snapshot, t, &mut seen)).collect();
    // Running conversations whose parent links form a cycle have no root;
    // they are still shown.
    for thread in &snapshot.threads {
        if thread.state == RunState::Running && !seen.contains(&thread.id) {
            trees.push(grow(snapshot, thread, &mut seen));
        }
    }
    trees
}

/// `seen` ends a branch at a conversation already placed, so malformed
/// parent links cannot recurse.
fn grow(snapshot: &Snapshot, thread: &Thread, seen: &mut BTreeSet<Uuid>) -> Node {
    seen.insert(thread.id);
    let children: Vec<&Thread> = snapshot
        .threads
        .iter()
        .filter(|t| {
            t.parent == Some(thread.id) && t.state == RunState::Running && !seen.contains(&t.id)
        })
        .collect();
    let below = children
        .into_iter()
        .map(|child| grow(snapshot, child, seen))
        .collect();
    Node {
        thread: thread.clone(),
        below,
    }
}

fn count(node: &Node) -> usize {
    1 + node.below.iter().map(count).sum::<usize>()
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What `thread` is waiting on, in words.
fn asks(thread: &Thread) -> String {
    let mut parts = Vec::new();
    if !thread.approvals.is_empty() {
        parts.push(plural(thread.approvals.len(), "approval", "approvals"));
    }
    if !thread.prompts.is_empty() {
        parts.push(plural(thread.prompts.len(), "question", "questions"));
    }
    parts.join(" and ")
}

fn chat(id: Uuid) -> String {
    Route::Chat {
        id,
        split: None,
        message: None,
    }
    .to_string()
}

#[component]
pub fn Running() -> Element {
    let state = use_context::<State>();
    let connected = (state.connected)();
    let snapshot = state.data.read();
    let trees = running(&snapshot);
    let waiting: Vec<Thread> = crate::inbox::waiting(&snapshot)
        .into_iter()
        .cloned()
        .collect();
    let scheduled: Vec<Job> = snapshot
        .jobs
        .iter()
        .filter(|job| {
            job.last
                .as_ref()
                .is_some_and(|run| run.outcome == JobOutcome::Running)
        })
        .cloned()
        .collect();
    let responses: usize = trees.iter().map(count).sum();
    let paused = snapshot.threads.iter().filter(|t| resumable(t)).count();
    let title = |id: Uuid| {
        snapshot
            .threads
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone())
    };
    rsx! { PageFrame { icon: IconName::Play, kicker: "Supervision", title: "Running work.", description: "Every response in progress, across conversations, with the subagents it delegated to. Pausing or stopping a conversation also holds or stops its subagents.",
        div { class: "a206-grid",
            Metric { label: name!("Running responses"), title: rsx! { "Running" }, value: Some(responses as f64), display: responses.to_string(), meta: rsx! { "Including subagents" } }
            Metric { label: name!("Waiting on you"), title: rsx! { "Waiting on you" }, value: Some(waiting.len() as f64), display: waiting.len().to_string(), meta: rsx! { "Conversations with approvals or questions" } }
            Metric { label: name!("Paused"), title: rsx! { "Paused" }, value: Some(paused as f64), display: paused.to_string(), meta: rsx! { "Paused by you; their subagents wait too" } }
        }
        if !connected {
            Alert { tone: AlertTone::Warning, title: rsx! { "The gateway is not connected" }, "This is the last state it reported, and these responses may have ended since. Controls return when it reconnects." }
        }
        if !waiting.is_empty() {
            Card { variant: CardVariant::Feature,
                CardHeader { CardTitle { "Waiting on you" } }
                CardBody { NavList {
                    for thread in waiting {
                        NavLink { key: "{thread.id}", label: thread.title.clone(), href: chat(thread.id),
                            meta: rsx! { Badge { tone: BadgeTone::Warning, "{asks(&thread)}" } } }
                    }
                } }
                CardFooter { LinkButton { variant: ButtonVariant::Ghost, href: Route::Inbox {}.to_string(), "Answer them all on one page" } }
            }
        }
        if trees.is_empty() {
            EmptyState { compact: true, title: rsx! { "Nothing is running" }, body: rsx! { "Responses in progress appear here, with their subagents, while they run." } }
        }
        for tree in trees {
            RunCard { key: "{tree.thread.id}", node: tree, connected }
        }
        if !scheduled.is_empty() {
            Card {
                CardHeader { CardTitle { "Scheduled messages being answered" } }
                CardBody { NavList {
                    for job in scheduled {
                        NavLink { key: "{job.id}", label: job.name.clone(), href: chat(job.thread),
                            meta: rsx! { match title(job.thread) {
                                Some(title) => rsx! { span { class: "a206-muted", "In {title}" } },
                                None => rsx! { span { class: "a206-muted", "In a conversation that no longer exists" } },
                            } } }
                    }
                } }
                CardFooter { LinkButton { variant: ButtonVariant::Ghost, href: Route::Schedules {}.to_string(), "All schedules" } }
            }
        }
    } }
}

/// One running conversation that no other running conversation delegated,
/// with its actions and the running subagents below it.
#[component]
fn RunCard(node: Node, connected: bool) -> Element {
    let thread = node.thread;
    let id = thread.id;
    let (lifecycle, status) = run_status(&thread);
    let done = thread
        .plan
        .iter()
        .filter(|step| step.status == PlanStatus::Done)
        .count();
    let added = plural(thread.steering.len(), "added message", "added messages");
    let mut below = Vec::new();
    flatten(&node.below, &thread.title, &mut below);
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader {
            CardTitle { LinkButton { variant: ButtonVariant::Ghost, href: chat(id), "{thread.title}" } }
            CardSubtitle { "{thread.mode.label()} · {thread.backend.label()}" }
        }
        CardBody {
            AgentStatus { state: lifecycle, label: rsx! { "{status}" } }
            div { class: "a206-row",
                if !thread.plan.is_empty() { Badge { tone: BadgeTone::Accent, "Plan {done} of {thread.plan.len()} done" } }
                if !thread.steering.is_empty() { Badge { "{added} to answer" } }
            }
            if !below.is_empty() {
                h3 { "Subagents" }
                List { divided: true,
                    for (delegator, child) in below {
                        { let (_, status) = run_status(&child);
                          let child_id = child.id;
                          rsx! { ListItem { key: "{child_id}", action: ListAction::Link(chat(child_id)),
                              overline: rsx! { "Delegated by {delegator}" },
                              headline: rsx! { "{child.title}" },
                              supporting: rsx! { "{status}" },
                              trailing: connected.then(|| rsx! { RunControls { thread: child.clone(), small: true } }),
                          } } }
                    }
                }
            }
        }
        if connected { CardFooter { RunControls { thread: thread.clone(), small: false } } }
    } }
}

/// The subagents under a running conversation, depth first, each with the
/// title of the conversation that delegated it.
fn flatten(nodes: &[Node], delegator: &str, out: &mut Vec<(String, Thread)>) {
    for node in nodes {
        out.push((delegator.to_owned(), node.thread.clone()));
        flatten(&node.below, &node.thread.title, out);
    }
}

/// Pause or Resume where they apply, and Stop, for one running conversation.
#[component]
fn RunControls(thread: Thread, small: bool) -> Element {
    let state = use_context::<State>();
    let id = thread.id;
    let size = if small {
        ButtonSize::Sm
    } else {
        ButtonSize::Md
    };
    rsx! {
        if resumable(&thread) {
            Button { size, variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::ResumeRun { thread: id }), "Resume" }
        }
        if pausable(&thread) {
            Button { size, variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::PauseRun { thread: id }), "Pause" }
        }
        Button { size, variant: ButtonVariant::Ghost, onclick: move |_| dispatch(state, Command::CancelRun { thread: id }), "Stop" }
    }
}

#[cfg(test)]
mod tests;
