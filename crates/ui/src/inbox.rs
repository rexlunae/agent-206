//! Every tool approval and `ask_user` question waiting on the user, across
//! conversations, answered in place with the same trusted cards the
//! conversation shows.
use crate::{PageFrame, State, route::Route};
use a206_core::{Snapshot, Thread};
use fc_dioxus::prelude::*;

/// Conversations with an approval or question waiting, the longest-waiting
/// first, so nothing asked early sinks below later asks.
pub(crate) fn waiting(snapshot: &Snapshot) -> Vec<&Thread> {
    let mut waiting: Vec<_> = snapshot
        .threads
        .iter()
        .filter(|t| !t.approvals.is_empty() || !t.prompts.is_empty())
        .collect();
    waiting.sort_by_key(|t| oldest(t));
    waiting
}

fn oldest(thread: &Thread) -> u64 {
    let approvals = thread.approvals.iter().map(|a| a.requested_at);
    let prompts = thread.prompts.iter().map(|p| p.requested_at);
    approvals.chain(prompts).min().unwrap_or(u64::MAX)
}

/// How many approvals and questions are waiting, for the sidebar.
pub(crate) fn count(snapshot: &Snapshot) -> usize {
    snapshot
        .threads
        .iter()
        .map(|t| t.approvals.len() + t.prompts.len())
        .sum()
}

#[component]
pub fn Inbox() -> Element {
    let state = use_context::<State>();
    let connected = (state.connected)();
    let snapshot = state.data.read();
    let waiting: Vec<Thread> = waiting(&snapshot).into_iter().cloned().collect();
    let delegator = |thread: &Thread| {
        thread
            .parent
            .and_then(|parent| snapshot.threads.iter().find(|t| t.id == parent))
            .map(|t| t.title.clone())
    };
    rsx! { PageFrame { icon: IconName::Bell, kicker: "Supervision", title: "Waiting on you.", description: "Tool calls asking for approval and questions agents asked, from every conversation and subagent, oldest first. Each answer goes to the conversation that asked.",
        if !connected {
            // The cards would only fail; the last report may be stale too.
            Alert { tone: AlertTone::Warning, title: rsx! { "The gateway is not connected" }, "Approvals and questions can be answered once it reconnects." }
        } else if waiting.is_empty() {
            EmptyState { title: rsx! { "Nothing is waiting on you" }, body: rsx! { "When an agent asks to run a tool or asks you a question, it appears here and in its conversation." } }
        } else {
            for thread in waiting {
                { let id = thread.id;
                  let parent = delegator(&thread);
                  rsx! { Card { key: "{id}", variant: CardVariant::Elevated,
                    CardHeader {
                        CardTitle { LinkButton { variant: ButtonVariant::Ghost, href: Route::Chat { id, split: None, message: None }.to_string(), "{thread.title}" } }
                        CardSubtitle {
                            "{thread.mode.label()} · {thread.backend.label()}"
                            if let Some(parent) = parent { " · Subagent of {parent}" }
                        }
                    }
                    CardBody {
                        for request in &thread.approvals { crate::tools::PendingApproval { key: "{request.id}", thread: id, request: request.clone() } }
                        for question in &thread.prompts { crate::prompts::PendingPrompt { key: "{question.id}", thread: id, question: question.clone() } }
                    }
                } } }
            }
        }
    } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{ApprovalRequest, Mode, UserPrompt};
    use dioxus::history::{History, MemoryHistory};
    use std::rc::Rc;
    use uuid::Uuid;

    fn text() -> String {
        format!("t-{}", Uuid::new_v4().simple())
    }
    fn approval(requested_at: u64) -> ApprovalRequest {
        ApprovalRequest {
            id: Uuid::new_v4(),
            message: Uuid::new_v4(),
            call: text(),
            tool: text(),
            arguments: format!("{{\"path\":\"{}\"}}", text()),
            digest: text(),
            capabilities: Default::default(),
            requested_at,
        }
    }
    fn question(requested_at: u64) -> UserPrompt {
        UserPrompt {
            id: Uuid::new_v4(),
            call: text(),
            question: text(),
            choices: vec![text()],
            other: false,
            requested_at,
        }
    }

    #[test]
    fn the_longest_waiting_conversation_comes_first() {
        let mut snapshot = crate::empty_snapshot();
        let idle = Thread::new(text(), Mode::Chat);
        let mut late = Thread::new(text(), Mode::Chat);
        late.approvals = vec![approval(30)];
        let mut early = Thread::new(text(), Mode::Chat);
        // A later approval does not hide that its question came first.
        early.approvals = vec![approval(40)];
        early.prompts = vec![question(10), question(50)];
        snapshot.threads = vec![idle, late.clone(), early.clone()];
        let ids: Vec<_> = waiting(&snapshot).iter().map(|t| t.id).collect();
        assert_eq!(ids, [early.id, late.id]);
        assert_eq!(count(&snapshot), 4);
    }

    #[derive(Clone, Debug, PartialEq, Routable)]
    enum Fixture {
        #[route("/")]
        Page {},
    }

    #[derive(Clone, Copy)]
    struct Online(bool);

    #[component]
    fn Page() -> Element {
        let snapshot = use_context::<Snapshot>();
        let Online(online) = use_context::<Online>();
        let mut state = crate::route::tests::loaded_state(snapshot);
        state.connected = use_signal(|| online);
        use_context_provider(|| state);
        rsx! { Inbox {} }
    }

    fn render(online: bool, threads: Vec<Thread>) -> String {
        fn app() -> Element {
            rsx! { Router::<Fixture> {} }
        }
        let mut snapshot = crate::empty_snapshot();
        snapshot.threads = threads;
        let mut dom = VirtualDom::new(app);
        dom.provide_root_context(snapshot);
        dom.provide_root_context(Online(online));
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn each_ask_is_answerable_under_its_conversation() {
        let parent = Thread::new(text(), Mode::Chat);
        let mut child = Thread::new(text(), Mode::Chat);
        child.parent = Some(parent.id);
        let request = approval(1);
        let asked = question(2);
        child.approvals = vec![request.clone()];
        child.prompts = vec![asked.clone()];
        let html = render(true, vec![parent.clone(), child.clone()]);
        assert!(html.contains(&child.title), "{html}");
        assert!(html.contains(&format!("Subagent of {}", parent.title)));
        assert!(html.contains(&format!("/conversations/{}", child.id)));
        // The arguments the decision is bound to, and the question itself.
        assert!(html.contains(&request.tool));
        assert!(html.contains("Allow once"));
        assert!(html.contains(&asked.question));
        // A conversation with nothing waiting is not listed.
        assert!(!html.contains(&format!("/conversations/{}\"", parent.id)));
    }

    #[test]
    fn nothing_is_offered_while_the_gateway_is_offline() {
        let mut thread = Thread::new(text(), Mode::Chat);
        thread.approvals = vec![approval(1)];
        let html = render(false, vec![thread.clone()]);
        assert!(html.contains("The gateway is not connected"));
        assert!(!html.contains("Allow once"));
        assert!(render(true, vec![]).contains("Nothing is waiting on you"));
    }
}
