//! Where a conversation sits among its delegated conversations: the one it was
//! delegated from and the tree of subagents below it, each with its state.
use crate::{State, route::Route};
use a206_core::{RunState, Snapshot, Thread};
use fc_dioxus::prelude::*;
use std::collections::BTreeSet;
use uuid::Uuid;

/// Levels nested in one view. Deeper subagents are not dropped: the last
/// level shown says it has more, and opening it shows the next levels.
const LEVELS: usize = 6;

fn state(state: &RunState) -> (BadgeTone, &'static str) {
    match state {
        RunState::Idle => (BadgeTone::Neutral, "Idle"),
        RunState::Running => (BadgeTone::Info, "Running"),
        RunState::Complete => (BadgeTone::Success, "Done"),
        RunState::Failed => (BadgeTone::Danger, "Failed"),
        RunState::Cancelled => (BadgeTone::Neutral, "Cancelled"),
    }
}

fn children(snapshot: &Snapshot, parent: Uuid) -> Vec<Thread> {
    snapshot
        .threads
        .iter()
        .filter(|t| t.parent == Some(parent))
        .cloned()
        .collect()
}

/// The conversation `thread` was delegated from and the subagents below it.
#[component]
pub(crate) fn Subagents(thread: Uuid) -> Element {
    let state = use_context::<State>();
    let snapshot = state.data.read();
    let current = snapshot.threads.iter().find(|t| t.id == thread);
    let parent = current
        .and_then(|t| t.parent)
        .map(|id| (id, snapshot.threads.iter().find(|t| t.id == id)));
    let below: Vec<Thread> = children(&snapshot, thread)
        .into_iter()
        .filter(|child| child.id != thread)
        .collect();
    rsx! { div { class: "a206-stack",
        h3 { "Subagents" }
        match parent {
            Some((_, Some(parent))) => rsx! { p { class: "a206-muted", "Delegated from "
                LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: Route::Chat { id: parent.id, split: None, message: None }.to_string(), "{parent.title}" }
            } },
            Some((_, None)) => rsx! { p { class: "a206-muted", "Delegated from a conversation that no longer exists." } },
            None => rsx! {},
        }
        if below.is_empty() {
            p { class: "a206-muted", "No subagents yet." }
        } else {
            NavList { Branch { threads: below, level: 1, seen: BTreeSet::from([thread]) } }
        }
    } }
}

/// `seen` holds this branch's ancestors, so a cycle in malformed parent
/// links ends instead of recursing.
#[component]
fn Branch(threads: Vec<Thread>, level: usize, seen: BTreeSet<Uuid>) -> Element {
    let state = use_context::<State>();
    rsx! {
        for thread in threads {
            { let (tone, label) = self::state(&thread.state);
              let mut seen = seen.clone();
              seen.insert(thread.id);
              // Children already shown above (a malformed cycle) are not "more".
              let below: Vec<Thread> = children(&state.data.read(), thread.id)
                  .into_iter()
                  .filter(|child| !seen.contains(&child.id))
                  .collect();
              let more = !below.is_empty() && level >= LEVELS;
              rsx! { NavLink { key: "{thread.id}", label: thread.title.clone(), href: Route::Chat { id: thread.id, split: None, message: None }.to_string(),
                  meta: rsx! { Badge { tone, "{label}" } " " Badge { tone: BadgeTone::Accent, "{thread.mode.label()}" }
                      if more { " " Badge { "More subagents inside" } } },
                  nested: (!below.is_empty() && !more).then(|| rsx! { NavList { nested: true, Branch { threads: below, level: level + 1, seen } } }),
              } }
            }
        }
    }
}

#[cfg(test)]
mod tests;
