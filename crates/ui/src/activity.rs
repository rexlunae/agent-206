//! What happened on the gateway, newest first, from its event journal.
//!
//! Entries name what changed and link to where it can be seen; the journal
//! records that something changed, not what, so neither does this page.
use crate::{PageFrame, State, error::ErrorNotice, route::Route};
use a206_core::{
    ActivityPage, ApprovalOutcome, Command, DownloadState, ErrorKind, ErrorReport, Event,
    EventKind, Id, MAX_ACTIVITY, PromptOutcome, Reply, Snapshot,
};
use fc_dioxus::prelude::*;

/// Journal events shown so far, newest first, and whether older ones remain.
/// Sequences number one gateway's journal, so `gateway` (the shell's gateway
/// generation) says which journal they came from.
#[derive(Clone, Debug, Default, PartialEq)]
struct Shown {
    gateway: u64,
    events: Vec<Event>,
    more: bool,
}
impl Shown {
    fn start(gateway: u64, page: ActivityPage) -> Self {
        Self {
            gateway,
            events: page.events,
            more: page.more,
        }
    }
    /// Put a fresh newest page from `gateway` on top. Older pages already
    /// loaded stay when the new page reaches them; otherwise events in between
    /// were never fetched (or came from another gateway), so the list restarts
    /// from the new page.
    fn refresh(&mut self, gateway: u64, page: ActivityPage) {
        let top = self.events.first().map(|event| event.sequence);
        let Some(top) = top.filter(|_| self.gateway == gateway) else {
            *self = Self::start(gateway, page);
            return;
        };
        let reaches = !page.more || page.events.last().is_some_and(|e| e.sequence <= top);
        if reaches {
            let mut events: Vec<Event> = page
                .events
                .into_iter()
                .filter(|event| event.sequence > top)
                .collect();
            events.append(&mut self.events);
            self.events = events;
        } else {
            *self = Self::start(gateway, page);
        }
    }
    /// The cursor for the page before the oldest event shown.
    fn bottom(&self) -> Option<u64> {
        self.events.last().map(|event| event.sequence)
    }
    /// Append the page `gateway` returned for events before `before`, unless
    /// the list restarted while it was being fetched.
    fn extend(&mut self, gateway: u64, before: u64, page: ActivityPage) {
        if self.gateway == gateway && self.bottom() == Some(before) {
            self.events.extend(page.events);
            self.more = page.more;
        }
    }
}

async fn fetch(before: Option<u64>) -> Result<ActivityPage, ErrorReport> {
    match crate::call(Command::Activity {
        before,
        limit: MAX_ACTIVITY,
    })
    .await?
    {
        Reply::Activity(page) => Ok(page),
        _ => Err(ErrorReport::new(
            ErrorKind::Protocol,
            "Unexpected activity response",
        )),
    }
}

#[component]
pub fn Activity() -> Element {
    let state = use_context::<State>();
    let mut shown = use_signal(Shown::default);
    let mut loaded = use_signal(|| false);
    let mut loading_older = use_signal(|| false);
    let mut failure = use_signal(|| None::<ErrorReport>);
    // Ages are relative, so the page keeps its own clock.
    let mut clock = use_signal(crate::messengers::now);
    use_future(move || async move {
        loop {
            futures_timer::Delay::new(std::time::Duration::from_secs(30)).await;
            clock.set(crate::messengers::now());
        }
    });
    // Each new journal event the snapshot reports refreshes the newest page,
    // and another gateway starts the list over.
    let last_event = use_memo(move || state.data.read().last_event);
    let gateway = state.gateway_generation;
    use_effect(move || {
        let _ = last_event();
        let generation = gateway();
        if shown.peek().gateway != generation {
            shown.set(Shown {
                gateway: generation,
                ..Shown::default()
            });
            loaded.set(false);
        }
        spawn(async move {
            let result = fetch(None).await;
            // A reply from a gateway the shell has since left is dropped.
            if *gateway.peek() != generation {
                return;
            }
            match result {
                Ok(page) => {
                    shown.write().refresh(generation, page);
                    failure.set(None);
                }
                Err(report) => failure.set(Some(report)),
            }
            loaded.set(true);
        });
    });
    let older = move |_| {
        if *loading_older.peek() {
            return;
        }
        let (generation, bottom) = {
            let shown = shown.peek();
            (shown.gateway, shown.bottom())
        };
        let Some(before) = bottom else {
            return;
        };
        loading_older.set(true);
        spawn(async move {
            let result = fetch(Some(before)).await;
            if *gateway.peek() == generation {
                match result {
                    Ok(page) => {
                        shown.write().extend(generation, before, page);
                        failure.set(None);
                    }
                    Err(report) => failure.set(Some(report)),
                }
            }
            loading_older.set(false);
        });
    };
    let now = clock();
    let snapshot = state.data.read();
    let current = shown();
    rsx! { PageFrame { kicker: "Activity", title: "What happened, newest first.", description: "Approvals, questions, downloads and changes to extensions, connections and settings, from the gateway's event log.",
        if let Some(report) = failure() {
            ErrorNotice { report, title: "Could not read activity".to_string() }
        }
        if !loaded() {
            p { class: "a206-muted", role: "status", "Reading activity…" }
        } else if current.events.is_empty() && failure().is_none() {
            EmptyState { title: rsx! { "Nothing yet" }, icon: rsx! { Icon { name: IconName::Clock } },
                body: rsx! { "Approvals, questions, downloads and configuration changes appear here as they happen." } }
        } else {
            ActivityList { now, events: current.events.clone(), snapshot: snapshot.clone() }
            if current.more {
                Button { variant: ButtonVariant::Secondary, busy: loading_older(), onclick: older, "Show older activity" }
            } else if !current.events.is_empty() {
                p { class: "a206-muted", "This is the start of the retained log." }
            }
        }
    } }
}

#[component]
fn ActivityList(now: u64, events: Vec<Event>, snapshot: Snapshot) -> Element {
    rsx! {
        List { divided: true, aria_label: "Activity",
            for event in events {
                { let entry = describe(&event.kind, &snapshot); rsx! {
                    ListItem { key: "{event.sequence}",
                        leading: rsx! { Icon { name: entry.icon, size: IconSize::Sm } },
                        headline: rsx! { "{entry.text}" },
                        supporting: rsx! { "{ago(now, event.at)}" },
                        action: entry.link.map_or(ListAction::None, |route| ListAction::Link(route.to_string())),
                    }
                } }
            }
        }
    }
}

/// One journal event in words, with the page that shows it.
struct Entry {
    icon: IconName,
    text: String,
    link: Option<Route>,
}

fn describe(kind: &EventKind, snapshot: &Snapshot) -> Entry {
    let conversation = |id: &Id, text: &dyn Fn(&str) -> String| match snapshot
        .threads
        .iter()
        .find(|thread| thread.id == *id)
    {
        Some(thread) => (
            text(&format!("“{}”", thread.title)),
            Some(Route::Chat {
                id: *id,
                split: None,
            }),
        ),
        None => (text("a conversation that no longer exists"), None),
    };
    let (icon, (text, link)) = match kind {
        EventKind::ThreadChanged { id } => (
            IconName::Sparkles,
            conversation(id, &|c| format!("Conversation {c} changed")),
        ),
        EventKind::BackendActivity { thread, title } => (
            IconName::Terminal,
            conversation(thread, &|c| format!("{title} (in {c})")),
        ),
        EventKind::ApprovalRequested { thread, .. } => (
            IconName::Bell,
            conversation(thread, &|c| {
                format!("A tool call is waiting for approval in {c}")
            }),
        ),
        EventKind::ApprovalResolved {
            thread, outcome, ..
        } => {
            let outcome = match outcome {
                ApprovalOutcome::Approved => "approved",
                ApprovalOutcome::Denied => "denied",
                ApprovalOutcome::Expired => "expired unanswered",
                ApprovalOutcome::Revoked => "withdrawn when its permission changed",
            };
            (
                IconName::Check,
                conversation(thread, &|c| format!("Tool call {outcome} in {c}")),
            )
        }
        EventKind::PromptRequested { thread, .. } => (
            IconName::Help,
            conversation(thread, &|c| format!("The agent asked a question in {c}")),
        ),
        EventKind::PromptResolved {
            thread, outcome, ..
        } => {
            let outcome = match outcome {
                PromptOutcome::Answered => "answered",
                PromptOutcome::Declined => "declined",
                PromptOutcome::Expired => "expired unanswered",
            };
            (
                IconName::Check,
                conversation(thread, &|c| format!("Question {outcome} in {c}")),
            )
        }
        EventKind::CatalogChanged { provider } => {
            let name = snapshot
                .settings
                .providers
                .iter()
                .find(|entry| entry.id == *provider)
                .map_or("a removed connection", |entry| entry.label.as_str());
            (
                IconName::Refresh,
                (
                    format!("Model list updated for {name}"),
                    Some(Route::Providers {}),
                ),
            )
        }
        EventKind::DownloadChanged(download) => {
            let what = match download.state {
                DownloadState::Queued => "queued",
                DownloadState::Running => "in progress",
                DownloadState::Complete => "finished",
                DownloadState::Failed => "failed",
                DownloadState::Cancelled => "cancelled",
            };
            (
                IconName::Download,
                (
                    format!("Download {what}: {}", download.name),
                    Some(Route::Downloads {}),
                ),
            )
        }
        EventKind::SettingsChanged => (
            IconName::Settings,
            ("Settings saved".to_owned(), Some(Route::Gateway {})),
        ),
        EventKind::ProjectChanged { id } => {
            let name = snapshot
                .projects
                .iter()
                .find(|project| project.id == *id)
                .map_or("A removed project", |project| project.name.as_str());
            (
                IconName::Folder,
                (format!("Project {name} changed"), Some(Route::Projects {})),
            )
        }
        EventKind::MessengerChanged { name } => (
            IconName::Send,
            (
                format!("Messenger {name} changed"),
                Some(Route::Messengers {}),
            ),
        ),
        EventKind::McpServerChanged { name } => (
            IconName::Code,
            (
                format!("MCP server {name} changed"),
                Some(Route::Extensions {}),
            ),
        ),
        EventKind::ExtensionChanged { id } => {
            let name = snapshot
                .plugins
                .iter()
                .find(|plugin| plugin.id == *id)
                .map_or(id.as_str(), |plugin| plugin.name.as_str());
            (
                IconName::Plus,
                (
                    format!("Extension {name} changed"),
                    Some(Route::Extensions {}),
                ),
            )
        }
        EventKind::JobChanged { id } => (
            IconName::Clock,
            match snapshot.jobs.iter().find(|job| job.id == *id) {
                Some(job) => conversation(&job.thread, &|c| {
                    format!("Scheduled message “{}” for {c} changed", job.name)
                }),
                None => ("A removed scheduled message changed".to_owned(), None),
            },
        ),
    };
    Entry { icon, text, link }
}

/// How long before `now` an event at `at` happened, both in Unix seconds.
fn ago(now: u64, at: u64) -> String {
    let seconds = now.saturating_sub(at);
    let (count, unit) = match seconds {
        0..60 => return "Just now".to_owned(),
        60..3_600 => (seconds / 60, "minute"),
        3_600..86_400 => (seconds / 3_600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{Mode, Thread};

    fn event(sequence: u64, kind: EventKind) -> Event {
        Event {
            sequence,
            at: 0,
            kind,
        }
    }
    fn extension(sequence: u64) -> Event {
        event(
            sequence,
            EventKind::ExtensionChanged {
                id: Id::new_v4().to_string(),
            },
        )
    }
    fn page(events: Vec<Event>, more: bool) -> ActivityPage {
        ActivityPage { events, more }
    }

    #[test]
    fn a_new_page_joins_loaded_history_only_when_nothing_is_skipped() {
        let mut shown = Shown::default();
        shown.refresh(0, page(vec![extension(5), extension(3)], true));
        shown.extend(0, 3, page(vec![extension(2)], false));
        assert!(!shown.more);
        // Overlaps the top: only the new events go on top, history stays.
        shown.refresh(
            0,
            page(vec![extension(8), extension(6), extension(5)], true),
        );
        let sequences = |shown: &Shown| shown.events.iter().map(|e| e.sequence).collect::<Vec<_>>();
        assert_eq!(sequences(&shown), [8, 6, 5, 3, 2]);
        assert!(!shown.more);
        // A full page that stops above the top skipped events: start over.
        shown.refresh(0, page(vec![extension(20), extension(19)], true));
        assert_eq!(sequences(&shown), [20, 19]);
        assert!(shown.more);
        // An older page fetched before that restart no longer fits.
        shown.extend(0, 2, page(vec![], false));
        assert!(shown.more);
        // Another gateway's journal never merges with this one.
        shown.refresh(1, page(vec![extension(21), extension(19)], false));
        assert_eq!((shown.gateway, sequences(&shown)), (1, vec![21, 19]));
        shown.extend(0, 19, page(vec![extension(18)], false));
        assert_eq!(sequences(&shown), [21, 19]);
    }

    #[test]
    fn entries_name_the_conversation_and_link_to_it() {
        let mut snapshot = crate::empty_snapshot();
        let thread = Thread::new(Id::new_v4().simple().to_string(), Mode::Chat);
        snapshot.threads.push(thread.clone());
        let events = vec![
            event(
                2,
                EventKind::ApprovalRequested {
                    thread: thread.id,
                    approval: Id::new_v4(),
                },
            ),
            event(
                1,
                EventKind::PromptResolved {
                    thread: Id::new_v4(),
                    prompt: Id::new_v4(),
                    outcome: PromptOutcome::Declined,
                },
            ),
        ];
        let mut dom = VirtualDom::new_with_props(
            ActivityList,
            ActivityListProps {
                now: 7_200,
                events,
                snapshot,
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains(&format!(
            "A tool call is waiting for approval in “{}”",
            thread.title
        )));
        assert!(
            html.contains(
                &Route::Chat {
                    id: thread.id,
                    split: None
                }
                .to_string()
            )
        );
        assert!(html.contains("Question declined in a conversation that no longer exists"));
        assert!(html.contains("2 hours ago"));
    }

    #[test]
    fn scheduled_message_entries_name_the_job_and_its_conversation() {
        let mut snapshot = crate::empty_snapshot();
        let thread = Thread::new(Id::new_v4().simple().to_string(), Mode::Chat);
        let job = a206_core::schedule::Job {
            id: Id::new_v4(),
            name: Id::new_v4().simple().to_string(),
            thread: thread.id,
            text: String::new(),
            schedule: a206_core::schedule::Schedule::At { at: 0 },
            enabled: true,
            created: 0,
            next: None,
            last: None,
            author: Default::default(),
        };
        snapshot.threads.push(thread.clone());
        snapshot.jobs.push(job.clone());
        let entry = describe(&EventKind::JobChanged { id: job.id }, &snapshot);
        assert_eq!(
            entry.text,
            format!(
                "Scheduled message “{}” for “{}” changed",
                job.name, thread.title
            )
        );
        assert_eq!(
            entry.link,
            Some(Route::Chat {
                id: thread.id,
                split: None
            })
        );
        let removed = describe(&EventKind::JobChanged { id: Id::new_v4() }, &snapshot);
        assert_eq!(removed.text, "A removed scheduled message changed");
        assert_eq!(removed.link, None);
    }

    #[test]
    fn ages_round_down_to_the_largest_whole_unit() {
        assert_eq!(ago(100, 100), "Just now");
        assert_eq!(ago(100, 200), "Just now");
        assert_eq!(ago(3_659, 0), "1 hour ago");
        assert_eq!(ago(179, 0), "2 minutes ago");
        assert_eq!(ago(86_400 * 3, 0), "3 days ago");
    }
}
