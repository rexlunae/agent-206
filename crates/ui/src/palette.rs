//! The command palette: one keyboard-opened list of every page, conversation
//! and action the current view offers, filtered as you type.
use crate::{State, dispatch, route::Route};
use a206_core::{CodingBackend, Command, Id, Mode, RunState, Snapshot, Thread};
use fc_dioxus::prelude::*;

/// What choosing an entry does. Each one is something a visible control on a
/// page already does; the palette never offers an action the gateway refuses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    Open(Route),
    NewConversation,
    Compact(Id),
    Stop(Id),
    Pause(Id),
    Resume(Id),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub(crate) title: String,
    pub(crate) group: &'static str,
    pub(crate) detail: Option<String>,
    pub(crate) icon: IconName,
    pub(crate) action: Action,
}

impl Entry {
    fn new(title: impl Into<String>, group: &'static str, icon: IconName, action: Action) -> Self {
        Self {
            title: title.into(),
            group,
            detail: None,
            icon,
            action,
        }
    }
    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Whether `thread` can be summarized now: the same conditions as its own
/// Compact button.
pub(crate) fn can_compact(thread: &Thread) -> bool {
    thread.backend == CodingBackend::Native
        && thread.state != RunState::Running
        && thread.model.is_some()
}

/// Everything the palette offers on `route`: actions on the open conversation,
/// then a new conversation, every page, and every conversation.
/// `can_create` is whether the gateway can take a new conversation now.
pub(crate) fn entries(snapshot: &Snapshot, route: &Route, can_create: bool) -> Vec<Entry> {
    let mut entries = vec![];
    // Both conversations of a split view are on screen, so both offer their
    // actions, named by title when there are two.
    let visible: Vec<Id> = match route {
        Route::Chat { id, split, .. } => std::iter::once(*id)
            .chain(split.filter(|second| second != id))
            .collect(),
        _ => vec![],
    };
    let named = visible.len() > 1;
    for thread in visible
        .iter()
        .filter_map(|id| snapshot.threads.iter().find(|t| t.id == *id))
    {
        let title = |action: &str| {
            if named {
                format!("{action}: {}", thread.title)
            } else {
                action.to_owned()
            }
        };
        let running = thread.state == RunState::Running;
        if running {
            entries.push(Entry::new(
                title("Stop response"),
                "This conversation",
                IconName::Stop,
                Action::Stop(thread.id),
            ));
        }
        // The same conditions as the conversation's own Pause and Resume.
        if crate::running::resumable(thread) {
            entries.push(Entry::new(
                title("Resume response"),
                "This conversation",
                IconName::Play,
                Action::Resume(thread.id),
            ));
        }
        if crate::running::pausable(thread) {
            entries.push(
                Entry::new(
                    title("Pause response"),
                    "This conversation",
                    IconName::Pause,
                    Action::Pause(thread.id),
                )
                .detail("Hold it before its next step"),
            );
        }
        if can_compact(thread) {
            entries.push(
                Entry::new(
                    title("Summarize earlier messages"),
                    "This conversation",
                    IconName::Edit,
                    Action::Compact(thread.id),
                )
                .detail("Compact all but the latest turn"),
            );
        }
    }
    if can_create {
        entries.push(Entry::new(
            "New conversation",
            "Actions",
            IconName::Plus,
            Action::NewConversation,
        ));
    }
    entries.push(
        Entry::new(
            "Search messages",
            "Actions",
            IconName::Search,
            Action::Open(Route::Conversations {}),
        )
        .detail("Find text in any conversation"),
    );
    let pages = Route::navigation()
        .into_iter()
        .flatten()
        .chain([Route::Appearance {}])
        .filter(|page| page != route);
    entries.extend(pages.map(|page| {
        Entry::new(
            page.label(),
            "Pages",
            page.icon(),
            Action::Open(page.clone()),
        )
    }));
    for thread in snapshot.threads.iter().rev() {
        let open = Route::Chat {
            id: thread.id,
            split: None,
            message: None,
        };
        if visible.contains(&thread.id) {
            continue;
        }
        entries.push(
            Entry::new(
                thread.title.clone(),
                "Conversations",
                IconName::Sparkles,
                Action::Open(open),
            )
            .detail(format!(
                "{} · {}",
                thread.mode.label(),
                thread.backend.label()
            )),
        );
    }
    entries
}

/// How well `query` names `entry`, best first; `None` when it does not.
fn rank(entry: &Entry, query: &str) -> Option<u8> {
    let title = entry.title.to_lowercase();
    let word_starts = |text: &str| {
        text.split(|c: char| !c.is_alphanumeric())
            .any(|word| word.starts_with(query))
    };
    if title.starts_with(query) {
        return Some(0);
    }
    if word_starts(&title) {
        return Some(1);
    }
    if title.contains(query) {
        return Some(2);
    }
    let rest = [Some(entry.group), entry.detail.as_deref()];
    if rest
        .into_iter()
        .flatten()
        .any(|text| word_starts(&text.to_lowercase()))
    {
        return Some(3);
    }
    // Letters in order, as in "nc" for "New conversation".
    let mut letters = title.chars();
    query
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|wanted| letters.any(|c| c == wanted))
        .then_some(4)
}

/// The entries `query` matches, best match first and otherwise in offered order.
pub(crate) fn filter(entries: Vec<Entry>, query: &str) -> Vec<Entry> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return entries;
    }
    let mut ranked: Vec<_> = entries
        .into_iter()
        .filter_map(|entry| rank(&entry, &query).map(|rank| (rank, entry)))
        .collect();
    // A stable sort keeps the offered order within each rank.
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, entry)| entry).collect()
}

pub(crate) fn run(state: State, action: Action) {
    match action {
        Action::Open(route) => {
            state.navigator.push(route);
        }
        Action::NewConversation => dispatch(
            state,
            Command::CreateThread {
                title: "New conversation".into(),
                mode: Mode::Chat,
            },
        ),
        Action::Compact(thread) => dispatch(state, Command::CompactThread { thread }),
        Action::Stop(thread) => dispatch(state, Command::CancelRun { thread }),
        Action::Pause(thread) => dispatch(state, Command::PauseRun { thread }),
        Action::Resume(thread) => dispatch(state, Command::ResumeRun { thread }),
    }
}

/// What a key pressed in the filter field does, with `count` matches and
/// `current` highlighted.
#[derive(Debug, PartialEq)]
enum Keyed {
    Highlight(usize),
    Run,
    Pass,
}

fn keyed(key: &Key, composing: bool, count: usize, current: usize) -> Keyed {
    match key {
        Key::ArrowDown if count > 0 => Keyed::Highlight((current + 1) % count),
        Key::ArrowUp if count > 0 => Keyed::Highlight((current + count - 1) % count),
        Key::Enter if !composing && count > 0 => Keyed::Run,
        _ => Keyed::Pass,
    }
}

/// Whether a key event opens the palette: Ctrl+K, or ⌘K on macOS.
pub(crate) fn opens(event: &KeyboardEvent) -> bool {
    let modifiers = event.modifiers();
    matches!(event.key(), Key::Character(c) if c.eq_ignore_ascii_case("k"))
        && (modifiers.ctrl() || modifiers.meta())
        && !modifiers.alt()
        && !modifiers.shift()
}

#[component]
pub(crate) fn CommandPalette(open: Signal<bool>, route: Route) -> Element {
    let state = use_context::<State>();
    let mut query = use_signal(String::new);
    let mut selected = use_signal(|| 0usize);
    let list = use_id();
    let status = use_id();
    let can_create = (state.connected)() && state.gateway_busy.read().is_none();
    let shown = filter(entries(&state.data.read(), &route, can_create), &query());
    let count = shown.len();
    let current = selected().min(count.saturating_sub(1));
    let mut close = move || {
        open.set(false);
        query.set(String::new());
        selected.set(0);
    };
    let mut choose = move |action: Action| {
        close();
        run(state, action);
    };
    let highlighted = shown.get(current).map(|entry| entry.title.clone());
    let first = shown.get(current).map(|entry| entry.action.clone());
    rsx! {
        Dialog { open: open(), onclose: move |_| close(), class: "a206-palette", size: DialogSize::Md,
            title: rsx! { "Go to or run" },
            description: rsx! { "Type to filter. Up and down choose, Enter runs, Escape closes." },
            div { class: "a206-stack",
                // Only the filter field's keys: a focused result button keeps
                // its own Enter.
                div { onkeydown: move |event: KeyboardEvent| match keyed(&event.key(), event.is_composing(), count, current) {
                        Keyed::Highlight(index) => { event.prevent_default(); selected.set(index); }
                        Keyed::Run => {
                            event.prevent_default();
                            if let Some(action) = first.clone() { choose(action); }
                        }
                        Keyed::Pass => {}
                    },
                    Input { value: ReadSignal::from(query), oninput: move |value| { query.set(value); selected.set(0); },
                        aria_label: "Pages, conversations and actions", placeholder: "Search pages, conversations and actions",
                        "autofocus": "true", autocomplete: "off", "spellcheck": "false",
                        aria_controls: "{list}", aria_describedby: "{status}" }
                }
                p { id: "{status}", class: "a206-muted", role: "status",
                    match &highlighted { Some(title) => format!("{count} matches. Enter chooses {title}."), None => "Nothing matches.".to_string() }
                }
                List { id: "{list}", class: "a206-palette-results", aria_label: "Matches",
                    for (index, entry) in shown.into_iter().enumerate() {
                        PaletteRow { key: "{index}-{entry.title}", entry: entry.clone(), current: index == current,
                            onchoose: choose }
                    }
                }
            }
        }
    }
}

#[component]
fn PaletteRow(entry: Entry, current: bool, onchoose: EventHandler<Action>) -> Element {
    let action = entry.action.clone();
    rsx! {
        ListItem { class: if current { "a206-palette-current" } else { "" }, current,
            leading: rsx! { Icon { name: entry.icon, size: IconSize::Sm } },
            headline: rsx! { "{entry.title}" },
            supporting: entry.detail.clone().map(|detail| rsx! { "{detail}" }),
            trailing: rsx! { Badge { "{entry.group}" } },
            action: ListAction::Button(EventHandler::new(move |_| onchoose.call(action.clone()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::ModelKey;
    use uuid::Uuid;

    fn thread(mode: Mode) -> Thread {
        Thread::new(format!("t-{}", Uuid::new_v4().simple()), mode)
    }

    fn snapshot(threads: Vec<Thread>) -> Snapshot {
        Snapshot {
            threads,
            ..crate::empty_snapshot()
        }
    }

    fn titles(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.title.as_str()).collect()
    }

    fn chat(id: Id) -> Route {
        Route::Chat {
            id,
            split: None,
            message: None,
        }
    }

    #[test]
    fn every_page_and_conversation_is_offered_except_where_you_are() {
        let threads = vec![thread(Mode::Chat), thread(Mode::Code)];
        let snap = snapshot(threads.clone());
        let entries = entries(&snap, &Route::Activity {}, true);
        let opened: Vec<_> = entries
            .iter()
            .filter_map(|e| match &e.action {
                Action::Open(route) => Some(route.clone()),
                _ => None,
            })
            .collect();
        for page in Route::navigation().into_iter().flatten() {
            assert_eq!(
                opened.contains(&page),
                page != Route::Activity {},
                "{page:?}"
            );
        }
        assert!(opened.contains(&Route::Appearance {}));
        for t in &threads {
            assert!(titles(&entries).contains(&t.title.as_str()));
        }
        // On a conversation, that one is not offered as a place to go.
        let here = entries_for(&snap, chat(threads[0].id));
        assert!(!titles(&here).contains(&threads[0].title.as_str()));
        assert!(titles(&here).contains(&threads[1].title.as_str()));
    }

    fn entries_for(snap: &Snapshot, route: Route) -> Vec<Entry> {
        entries(snap, &route, true)
    }

    #[test]
    fn conversation_actions_follow_what_the_conversation_allows() {
        let mut idle = thread(Mode::Chat);
        let snap = snapshot(vec![idle.clone()]);
        // No model chosen yet: nothing to summarize with, nothing to stop.
        let actions: Vec<_> = entries_for(&snap, chat(idle.id))
            .into_iter()
            .filter(|e| matches!(e.action, Action::Compact(_) | Action::Stop(_)))
            .collect();
        assert!(actions.is_empty());

        idle.model = Some(ModelKey {
            provider: Uuid::new_v4(),
            model: format!("m-{}", Uuid::new_v4().simple()),
        });
        let snap = snapshot(vec![idle.clone()]);
        let offered = entries_for(&snap, chat(idle.id));
        assert!(offered.iter().any(|e| e.action == Action::Compact(idle.id)));
        assert!(!offered.iter().any(|e| e.action == Action::Stop(idle.id)));

        let mut running = idle.clone();
        running.state = RunState::Running;
        let snap = snapshot(vec![running.clone()]);
        let offered = entries_for(&snap, chat(running.id));
        assert!(offered.iter().any(|e| e.action == Action::Stop(running.id)));
        assert!(
            offered
                .iter()
                .any(|e| e.action == Action::Pause(running.id))
        );
        assert!(
            !offered
                .iter()
                .any(|e| e.action == Action::Resume(running.id))
        );
        // Summarizing is not a run that can be paused.
        let mut compacting = running.clone();
        compacting.compacting = Some(RunState::Complete);
        let offered = entries_for(&snapshot(vec![compacting.clone()]), chat(compacting.id));
        assert!(
            !offered
                .iter()
                .any(|e| e.action == Action::Pause(compacting.id))
        );
        let mut paused = running.clone();
        paused.paused = true;
        let offered = entries_for(&snapshot(vec![paused.clone()]), chat(paused.id));
        assert!(
            offered
                .iter()
                .any(|e| e.action == Action::Resume(paused.id))
        );
        assert!(!offered.iter().any(|e| e.action == Action::Pause(paused.id)));
        assert!(
            !offered
                .iter()
                .any(|e| matches!(e.action, Action::Compact(_)))
        );

        let mut external = idle.clone();
        external.backend = CodingBackend::Codex;
        let snap = snapshot(vec![external.clone()]);
        assert!(
            !entries_for(&snap, chat(external.id))
                .iter()
                .any(|e| matches!(e.action, Action::Compact(_)))
        );
        // Elsewhere, no conversation's actions are offered.
        assert!(
            !entries(&snap, &Route::Workspace {}, true)
                .iter()
                .any(|e| matches!(e.action, Action::Compact(_) | Action::Stop(_)))
        );
    }

    #[test]
    fn a_new_conversation_is_offered_only_when_the_gateway_can_take_one() {
        let snap = snapshot(vec![]);
        let has_new = |can| {
            entries(&snap, &Route::Workspace {}, can)
                .iter()
                .any(|e| e.action == Action::NewConversation)
        };
        assert!(has_new(true));
        assert!(!has_new(false));
    }

    #[test]
    fn filtering_ranks_prefixes_then_words_then_anywhere_then_letters() {
        let one = |title: &str| Entry::new(title, "Pages", IconName::Home, Action::NewConversation);
        let entries = vec![
            one("Agent modes"),
            one("Downloads"),
            one("Get started"),
            one("Documents & git"),
            one("Gateway"),
        ];
        assert_eq!(
            titles(&filter(entries.clone(), "g")),
            ["Get started", "Gateway", "Documents & git", "Agent modes"]
        );
        assert_eq!(titles(&filter(entries.clone(), "  DOWN ")), ["Downloads"]);
        assert_eq!(titles(&filter(entries.clone(), "gst")), ["Get started"]);
        assert_eq!(filter(entries.clone(), "").len(), entries.len());
        assert!(filter(entries.clone(), "zzz").is_empty());
        // A word of the group or detail matches too, after any title match.
        let detailed = vec![
            one("Gateway"),
            Entry::new(
                "Report",
                "Conversations",
                IconName::Sparkles,
                Action::NewConversation,
            )
            .detail("Code · Native"),
        ];
        assert_eq!(titles(&filter(detailed.clone(), "native")), ["Report"]);
        assert_eq!(titles(&filter(detailed, "convers")), ["Report"]);
    }

    #[test]
    fn a_split_view_offers_both_conversations_actions_by_title() {
        let model = || ModelKey {
            provider: Uuid::new_v4(),
            model: format!("m-{}", Uuid::new_v4().simple()),
        };
        let mut first = thread(Mode::Chat);
        first.model = Some(model());
        let mut second = thread(Mode::Code);
        second.model = Some(model());
        second.state = RunState::Running;
        let other = thread(Mode::Chat);
        let snap = snapshot(vec![first.clone(), second.clone(), other.clone()]);
        let route = Route::Chat {
            id: first.id,
            split: Some(second.id),
            message: None,
        };
        let offered = entries_for(&snap, route);
        let stop = offered
            .iter()
            .find(|e| e.action == Action::Stop(second.id))
            .map(|e| e.title.clone());
        assert_eq!(stop, Some(format!("Stop response: {}", second.title)));
        let compact = offered
            .iter()
            .find(|e| e.action == Action::Compact(first.id))
            .map(|e| e.title.clone());
        assert_eq!(
            compact,
            Some(format!("Summarize earlier messages: {}", first.title))
        );
        // Neither visible conversation is offered as a place to go.
        let places = titles(&offered);
        assert!(!places.contains(&first.title.as_str()));
        assert!(!places.contains(&second.title.as_str()));
        assert!(places.contains(&other.title.as_str()));
    }

    #[test]
    fn search_messages_opens_the_page_that_has_message_search() {
        let snap = snapshot(vec![]);
        let search = entries(&snap, &Route::Workspace {}, true)
            .into_iter()
            .find(|e| e.title == "Search messages")
            .map(|e| e.action);
        assert_eq!(search, Some(Action::Open(Route::Conversations {})));
    }

    #[test]
    fn filter_keys_wrap_the_highlight_and_enter_runs_only_a_match() {
        assert_eq!(keyed(&Key::ArrowDown, false, 3, 2), Keyed::Highlight(0));
        assert_eq!(keyed(&Key::ArrowUp, false, 3, 0), Keyed::Highlight(2));
        assert_eq!(keyed(&Key::ArrowDown, false, 0, 0), Keyed::Pass);
        assert_eq!(keyed(&Key::Enter, false, 3, 1), Keyed::Run);
        // Nothing to run, or an input method still composing.
        assert_eq!(keyed(&Key::Enter, false, 0, 0), Keyed::Pass);
        assert_eq!(keyed(&Key::Enter, true, 3, 1), Keyed::Pass);
        assert_eq!(keyed(&Key::Tab, false, 3, 1), Keyed::Pass);
    }

    #[derive(Clone, Debug, PartialEq, Routable)]
    enum PageRoute {
        #[route("/")]
        Page {},
    }
    #[component]
    fn Page() -> Element {
        let snapshot = use_context::<Snapshot>();
        let state = crate::route::tests::loaded_state(snapshot);
        use_context_provider(|| state);
        let open = use_signal(|| true);
        rsx! { CommandPalette { open, route: Route::Workspace {} } }
    }

    #[test]
    fn the_open_palette_lists_matches_and_names_the_one_enter_runs() {
        use dioxus::history::{History, MemoryHistory};
        use std::rc::Rc;
        crate::route::tests::configured();
        fn app() -> Element {
            rsx! { Router::<PageRoute> {} }
        }
        let conversation = thread(Mode::Chat);
        let mut dom = VirtualDom::new(app);
        dom.provide_root_context(snapshot(vec![conversation.clone()]));
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Go to or run"), "{html}");
        assert!(html.contains("Enter chooses New conversation"), "{html}");
        assert!(html.contains(&conversation.title), "{html}");
        assert!(html.contains("a206-palette-current"), "{html}");
    }
}
