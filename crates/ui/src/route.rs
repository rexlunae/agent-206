//! Shared, typed navigation. Thread identity and split view have a single source of truth.
use crate::pages::Gateway;
use crate::{
    PageFrame, Shell,
    activity::Activity,
    conversation::Workspace as Chat,
    messengers::Messengers,
    pages::{
        Browser, Conversations, Documents, Downloads, Extensions, Joshua, Overview as Workspace,
        Projects, Security,
    },
    schedules::Schedules,
    settings::{Appearance, Prompts, Providers},
    welcome::Welcome,
};
use fc_dioxus::prelude::*;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Routable)]
pub(crate) enum Route {
    #[layout(Shell)]
    #[route("/")]
    Workspace {},
    #[route("/conversations")]
    Conversations {},
    /// `message` asks the first column to show one message, for example a
    /// search result.
    #[route("/conversations/:id?:split&:message")]
    Chat {
        id: Uuid,
        split: Option<Columns>,
        message: Option<Uuid>,
    },
    #[route("/projects")]
    Projects {},
    #[route("/activity")]
    Activity {},
    #[route("/connections")]
    Providers {},
    #[route("/messengers")]
    Messengers {},
    #[route("/downloads")]
    Downloads {},
    #[route("/extensions")]
    Extensions {},
    #[route("/browser")]
    Browser {},
    #[route("/documents")]
    Documents {},
    #[route("/schedules")]
    Schedules {},
    #[route("/settings/appearance")]
    Appearance {},
    #[route("/settings/modes")]
    Prompts {},
    #[route("/settings/security")]
    Security {},
    #[route("/joshua")]
    Joshua {},
    #[route("/gateway")]
    Gateway {},
    #[route("/welcome")]
    Welcome {},
    #[route("/:..segments")]
    NotFound { segments: Vec<String> },
}

impl Route {
    pub(crate) fn icon(&self) -> IconName {
        match self {
            Self::Workspace {} | Self::NotFound { .. } => IconName::Home,
            Self::Chat { .. } | Self::Conversations {} => IconName::Sparkles,
            Self::Projects {} => IconName::Folder,
            Self::Activity {} => IconName::Clock,
            Self::Providers {} => IconName::Link,
            Self::Messengers {} => IconName::Send,
            Self::Downloads {} => IconName::Download,
            Self::Extensions {} => IconName::Plus,
            Self::Browser {} => IconName::Globe,
            Self::Documents {} => IconName::File,
            Self::Schedules {} => IconName::Clock,
            Self::Appearance {} => IconName::Settings,
            Self::Prompts {} => IconName::Edit,
            Self::Security {} => IconName::Lock,
            Self::Joshua {} | Self::Gateway {} => IconName::Terminal,
            Self::Welcome {} => IconName::Check,
        }
    }
    /// The sidebar's page groups, also what the command palette can open.
    pub(crate) fn navigation() -> [Vec<Self>; 3] {
        [
            vec![
                Self::Workspace {},
                Self::Projects {},
                Self::Conversations {},
                Self::Activity {},
            ],
            vec![
                Self::Documents {},
                Self::Browser {},
                Self::Downloads {},
                Self::Schedules {},
                Self::Extensions {},
            ],
            vec![
                Self::Welcome {},
                Self::Gateway {},
                Self::Providers {},
                Self::Messengers {},
                Self::Joshua {},
                Self::Prompts {},
                Self::Security {},
            ],
        ]
    }
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Workspace {} => "Workspace",
            Self::Chat { .. } => "Conversation",
            Self::Conversations {} => "Conversations",
            Self::Projects {} => "Projects",
            Self::Activity {} => "Activity",
            Self::Providers {} => "Connections",
            Self::Messengers {} => "Messengers",
            Self::Downloads {} => "Downloads",
            Self::Extensions {} => "Extensions",
            Self::Browser {} => "Browser",
            Self::Documents {} => "Documents & git",
            Self::Schedules {} => "Schedules",
            Self::Appearance {} => "Appearance",
            Self::Prompts {} => "Agent modes",
            Self::Security {} => "Security",
            Self::Joshua {} => "Joshua",
            Self::Gateway {} => "Gateway",
            Self::Welcome {} => "Get started",
            Self::NotFound { .. } => "Page not found",
        }
    }
}

/// Conversations open beside the first, in order. In an address they are
/// comma-separated ids, so a link naming one `split` conversation still opens.
/// The router requires a default; an empty list shows nothing beside.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Columns(Vec<Uuid>);

impl Columns {
    /// Most conversations a view shows side by side, the first included.
    pub(crate) const MAX_VISIBLE: usize = 4;

    /// The columns `ids` name, or none for an empty list.
    pub(crate) fn of(ids: impl IntoIterator<Item = Uuid>) -> Option<Self> {
        let ids: Vec<_> = ids.into_iter().collect();
        (!ids.is_empty()).then_some(Self(ids))
    }
    /// The conversations to show beside `first`: each once, never `first`
    /// itself, and no more than fit.
    pub(crate) fn beside(&self, first: Uuid) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = vec![];
        for id in &self.0 {
            if *id != first && !ids.contains(id) {
                ids.push(*id);
            }
        }
        ids.truncate(Self::MAX_VISIBLE - 1);
        ids
    }
}

impl std::fmt::Display for Columns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (n, id) in self.0.iter().enumerate() {
            if n > 0 {
                f.write_str(",")?;
            }
            write!(f, "{id}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Columns {
    type Err = uuid::Error;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.split(',')
            .map(Uuid::parse_str)
            .collect::<Result<_, _>>()
            .map(Self)
    }
}

#[component]
fn NotFound(segments: Vec<String>) -> Element {
    rsx! {
        PageFrame { kicker: "Navigation", title: "Page not found.", description: "This address does not point to an Agent 206 page.",
            LinkButton { href: Route::Workspace {}.to_string(), variant: ButtonVariant::Primary, "Return to workspace" }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dioxus::history::{History, MemoryHistory};
    use std::rc::Rc;

    #[test]
    fn conversation_addresses_round_trip_and_reject_malformed_identity() {
        let id = Uuid::new_v4();
        let split = Uuid::new_v4();
        for route in [
            Route::Chat {
                id,
                split: None,
                message: None,
            },
            Route::Chat {
                id,
                split: Columns::of([split]),
                message: None,
            },
            Route::Chat {
                id,
                split: None,
                message: Some(split),
            },
            Route::Chat {
                id,
                split: Columns::of([split]),
                message: Some(id),
            },
            Route::Chat {
                id,
                split: Columns::of([split, Uuid::new_v4(), Uuid::new_v4()]),
                message: None,
            },
        ] {
            assert_eq!(route.to_string().parse::<Route>().unwrap(), route);
        }
        assert_eq!(
            format!("/conversations/{id}").parse::<Route>().unwrap(),
            Route::Chat {
                id,
                split: None,
                message: None
            }
        );
        for path in ["/conversations/not-a-uuid", "/unknown/page"] {
            assert!(matches!(
                path.parse::<Route>().unwrap(),
                Route::NotFound { .. }
            ));
        }
    }

    /// Complete first-time setup once for this test process; the client stays lazy,
    /// so rendering never launches a gateway.
    pub(crate) fn configured() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let root = std::env::temp_dir().join(format!("a206-ui-{}", Uuid::new_v4()));
            a206_runtime::connection::save(
                &root,
                &a206_core::GatewayConnection {
                    target: a206_core::GatewayTarget::Local,
                },
            )
            .unwrap();
            crate::configure(root, None);
        });
    }

    fn app_at(path: &str) -> (VirtualDom, Rc<dyn History>) {
        configured();
        let history: Rc<dyn History> = Rc::new(MemoryHistory::with_initial_path(path));
        let mut dom = VirtualDom::new(crate::App);
        dom.provide_root_context(history.clone());
        dom.rebuild_in_place();
        (dom, history)
    }

    #[test]
    fn shared_router_renders_initial_page_and_back_forward_history() {
        let (mut dom, history) = app_at("/connections");
        let initial = dioxus_ssr::render(&dom);
        assert!(initial.contains("Intelligence, on your terms."));
        assert!(initial.contains("href=\"/connections\" aria-current=\"page\""));
        let router = dom.in_scope(ScopeId::ROOT, || dioxus::router::root_router().unwrap());
        dom.in_scope(ScopeId::ROOT, || {
            router.push(Route::Projects {});
        });
        dom.render_immediate(&mut dioxus::core::NoOpMutations);
        assert_eq!(history.current_route(), "/projects");
        assert!(dioxus_ssr::render(&dom).contains("Give your work a home."));
        dom.in_scope(ScopeId::ROOT, || router.go_back());
        dom.render_immediate(&mut dioxus::core::NoOpMutations);
        assert_eq!(history.current_route(), "/connections");
        assert!(dioxus_ssr::render(&dom).contains("Intelligence, on your terms."));
        dom.in_scope(ScopeId::ROOT, || router.go_forward());
        dom.render_immediate(&mut dioxus::core::NoOpMutations);
        assert_eq!(history.current_route(), "/projects");
        assert!(dioxus_ssr::render(&dom).contains("Give your work a home."));
        // The shell remains mounted: existing navigation history survives another page.
        dom.in_scope(ScopeId::ROOT, || {
            router.push(Route::Appearance {});
        });
        dom.render_immediate(&mut dioxus::core::NoOpMutations);
        assert!(dioxus_ssr::render(&dom).contains("A workspace that feels right."));
        assert!(history.can_go_back());
        assert!(!history.can_go_forward());
    }

    #[test]
    fn deep_links_wait_for_gateway_and_unknown_routes_offer_recovery() {
        let (dom, _) = app_at(&format!("/conversations/{}", Uuid::new_v4()));
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Opening conversation"));
        assert!(!html.contains("Conversation unavailable"));
        let (dom, _) = app_at("/conversations/invalid");
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Page not found."));
        assert!(html.contains("Return to workspace"));
    }

    #[derive(Clone)]
    struct ThreadFixture {
        snapshot: a206_core::Snapshot,
        id: Uuid,
        split: Option<Columns>,
        message: Option<Uuid>,
    }

    #[derive(Clone, Debug, PartialEq, Routable)]
    enum FixtureRoute {
        #[route("/")]
        ThreadView {},
    }

    #[component]
    fn SetupView() -> Element {
        let state = crate::State {
            data: use_signal(crate::empty_snapshot),
            loaded: use_signal(|| false),
            connected: use_signal(|| false),
            gateway: use_signal(|| None),
            gateway_busy: use_signal(|| None),
            gateway_generation: use_signal(|| 0),
            status: use_signal(String::new),
            error: use_signal(|| None),
            outage: use_signal(|| None),
            theme: use_signal(|| ThemeChoice::from_design(Design::Helix)),
            setup: use_signal(|| true),
            navigator: navigator(),
        };
        use_context_provider(|| state);
        rsx! { crate::setup::GatewaySetup {} }
    }
    #[derive(Clone, Debug, PartialEq, Routable)]
    enum SetupRoute {
        #[route("/")]
        SetupView {},
    }
    #[test]
    fn gateway_setup_offers_local_and_verified_ssh_targets() {
        configured();
        fn setup_app() -> Element {
            rsx! { Router::<SetupRoute> {} }
        }
        let mut dom = VirtualDom::new(setup_app);
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        // Let the setup status resource resolve, then render what it shows.
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(dom.wait_for_work());
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Choose your gateway."));
        assert!(html.contains("This computer"));
        assert!(html.contains("A remote gateway over SSH"));
        assert!(html.contains("value=\"local\" checked"), "{html}");
        assert!(html.contains("Use this computer"));
        // A configured client may leave setup without changing its gateway.
        assert!(html.contains("Keep current gateway"));
    }

    /// Loaded, connected client state showing `snapshot`.
    pub(crate) fn loaded_state(snapshot: a206_core::Snapshot) -> crate::State {
        crate::State {
            data: use_signal(|| snapshot),
            loaded: use_signal(|| true),
            connected: use_signal(|| true),
            gateway: use_signal(|| None),
            gateway_busy: use_signal(|| None),
            gateway_generation: use_signal(|| 0),
            status: use_signal(String::new),
            error: use_signal(|| None),
            outage: use_signal(|| None),
            theme: use_signal(|| ThemeChoice::from_design(Design::Helix)),
            setup: use_signal(|| false),
            navigator: navigator(),
        }
    }

    #[component]
    fn ThreadView() -> Element {
        let fixture = use_context::<ThreadFixture>();
        let state = loaded_state(fixture.snapshot.clone());
        use_context_provider(|| state);
        use_context_provider(|| crate::conversation::Drafts(Signal::new(Default::default())));
        rsx! { Chat { id: fixture.id, split: fixture.split, message: fixture.message } }
    }

    /// A workspace whose columns follow `step` through `steps`.
    #[derive(Clone)]
    struct Relayout {
        snapshot: a206_core::Snapshot,
        id: Uuid,
        steps: Vec<Option<Columns>>,
        step: Signal<usize>,
        drafts: crate::conversation::Drafts,
    }

    #[derive(Clone, Debug, PartialEq, Routable)]
    enum RelayoutRoute {
        #[route("/")]
        RelayoutView {},
    }

    #[component]
    fn RelayoutView() -> Element {
        let fixture = use_context::<Relayout>();
        let state = loaded_state(fixture.snapshot.clone());
        use_context_provider(|| state);
        use_context_provider(|| fixture.drafts);
        let split = fixture.steps[(fixture.step)()].clone();
        rsx! { Chat { id: fixture.id, split, message: None } }
    }

    fn render_thread(fixture: ThreadFixture) -> String {
        fn fixture_app() -> Element {
            rsx! { Router::<FixtureRoute> {} }
        }
        let mut dom = VirtualDom::new(fixture_app);
        dom.provide_root_context(fixture);
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn split_view_keeps_each_conversations_subagents_in_its_details() {
        let parent = a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat);
        let mut child = a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat);
        child.parent = Some(parent.id);
        let mut snapshot = crate::empty_snapshot();
        snapshot.threads = vec![parent.clone(), child.clone()];
        let html = render_thread(ThreadFixture {
            snapshot,
            id: parent.id,
            split: Columns::of([child.id]),
            message: None,
        });
        assert_eq!(html.matches(">Subagents<").count(), 2, "{html}");
        assert!(html.contains("Delegated from"), "{html}");
        assert!(html.contains("No subagents yet"), "{html}");
    }

    #[test]
    fn loaded_deep_links_select_exact_threads_and_restore_split_selector() {
        let first = a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat);
        let second = a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat);
        let mut snapshot = crate::empty_snapshot();
        snapshot.threads = vec![first.clone(), second.clone()];
        let fixture = ThreadFixture {
            snapshot,
            id: first.id,
            split: Columns::of([second.id]),
            message: None,
        };
        let html = render_thread(fixture.clone());
        assert_eq!(html.matches("aria-label=\"Message your agent\"").count(), 2);
        assert!(html.contains(&format!("Close {}", second.title)), "{html}");

        // A URL may never open the latest or first available thread as a fallback.
        let html = render_thread(ThreadFixture {
            id: Uuid::new_v4(),
            ..fixture.clone()
        });
        assert!(html.contains("Conversation unavailable"));
        assert!(!html.contains(&first.title));
        assert!(!html.contains(&second.title));
        let html = render_thread(ThreadFixture {
            split: Columns::of([Uuid::new_v4()]),
            ..fixture.clone()
        });
        assert!(html.contains("A conversation beside this one is unavailable"));
        assert_eq!(html.matches("aria-label=\"Message your agent\"").count(), 1);
        let html = render_thread(ThreadFixture {
            split: Columns::of([first.id]),
            ..fixture
        });
        assert_eq!(html.matches("aria-label=\"Message your agent\"").count(), 1);
    }

    #[test]
    fn up_to_four_conversations_open_side_by_side() {
        let threads: Vec<_> = (0..6)
            .map(|_| a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat))
            .collect();
        let mut snapshot = crate::empty_snapshot();
        snapshot.threads = threads.clone();
        let first = threads[0].id;
        let html = render_thread(ThreadFixture {
            snapshot: snapshot.clone(),
            id: first,
            split: Columns::of(threads[1..3].iter().map(|t| t.id)),
            message: None,
        });
        assert_eq!(html.matches("aria-label=\"Message your agent\"").count(), 3);
        // Each column beside the first can be closed, and one more added.
        assert!(html.contains(&format!("Close {}", threads[1].title)));
        assert!(html.contains(&format!("Close {}", threads[2].title)));
        assert!(html.contains("Add a column"));

        // A fourth fills the view: no more are offered, and extra ids are not shown.
        let html = render_thread(ThreadFixture {
            snapshot,
            id: first,
            split: Columns::of(threads[1..].iter().map(|t| t.id)),
            message: None,
        });
        assert_eq!(
            html.matches("aria-label=\"Message your agent\"").count(),
            Columns::MAX_VISIBLE
        );
        assert!(!html.contains("Add a column"));
        assert!(!html.contains(&format!("Close {}", threads[5].title)));
    }

    #[test]
    fn adding_and_closing_columns_keeps_unsent_drafts() {
        let threads: Vec<_> = (0..3)
            .map(|_| a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat))
            .collect();
        let [first, b, c] = [threads[0].id, threads[1].id, threads[2].id];
        let typed: std::collections::HashMap<Uuid, String> = threads
            .iter()
            .map(|t| (t.id, format!("unsent {}", Uuid::new_v4())))
            .collect();
        let mut snapshot = crate::empty_snapshot();
        snapshot.threads = threads;
        fn app() -> Element {
            rsx! { Router::<RelayoutRoute> {} }
        }
        let mut dom = VirtualDom::new(app);
        let (mut step, drafts) = dom.in_scope(ScopeId::ROOT, || {
            let drafts = typed
                .iter()
                .map(|(id, text)| {
                    let unsent = crate::conversation::Unsent {
                        text: text.clone(),
                        attachments: vec![],
                    };
                    (*id, unsent)
                })
                .collect();
            (Signal::new(0), Signal::new(drafts))
        });
        // Add a column, close the one before the last, then close split view.
        let steps = vec![
            Columns::of([b]),
            Columns::of([b, c]),
            Columns::of([c]),
            None,
        ];
        let visible = [
            vec![first, b],
            vec![first, b, c],
            vec![first, c],
            vec![first],
        ];
        dom.provide_root_context(Relayout {
            snapshot,
            id: first,
            steps: steps.clone(),
            step,
            drafts: crate::conversation::Drafts(drafts),
        });
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        for (index, shown) in visible.iter().enumerate() {
            if index > 0 {
                dom.in_scope(ScopeId::ROOT, || step.set(index));
                dom.render_immediate(&mut dioxus::core::NoOpMutations);
            }
            let html = dioxus_ssr::render(&dom);
            for id in shown {
                assert!(html.contains(&typed[id]), "step {index}: {html}");
            }
        }
        let kept = dom.in_scope(ScopeId::ROOT, || drafts.peek().len());
        assert_eq!(kept, typed.len());
    }

    #[test]
    fn columns_keep_order_without_repeats_and_read_old_split_links() {
        let [first, a, b] = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let columns = Columns::of([a, first, a, b]).expect("not empty");
        assert_eq!(columns.beside(first), [a, b]);
        assert_eq!(Columns::of([]), None);
        // An address written for one split conversation still opens it.
        assert_eq!(
            format!("/conversations/{first}?split={a}")
                .parse::<Route>()
                .unwrap(),
            Route::Chat {
                id: first,
                split: Columns::of([a]),
                message: None
            }
        );
        assert!(format!("{a},").parse::<Columns>().is_err());
    }

    #[test]
    fn a_linked_message_is_marked_and_a_missing_one_offers_the_whole_conversation() {
        let mut thread = a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat);
        let linked = a206_core::Message::new(
            a206_core::Role::User,
            Uuid::new_v4().to_string(),
            0,
            thread.backend,
        );
        let other = a206_core::Message::new(
            a206_core::Role::User,
            Uuid::new_v4().to_string(),
            0,
            thread.backend,
        );
        thread.messages = vec![linked.clone(), other];
        let mut snapshot = crate::empty_snapshot();
        snapshot.threads = vec![thread.clone()];
        let fixture = ThreadFixture {
            snapshot,
            id: thread.id,
            split: None,
            message: Some(linked.id),
        };
        let html = render_thread(fixture.clone());
        assert_eq!(html.matches("Linked message").count(), 1);
        let marker = html.find("Linked message").unwrap();
        let text = html.find(&linked.text).unwrap();
        assert!(marker < text, "the mark belongs to the linked message");
        assert!(!html.contains("Message unavailable"));

        let html = render_thread(ThreadFixture {
            message: Some(Uuid::new_v4()),
            ..fixture
        });
        assert!(html.contains("Message unavailable"));
        assert!(!html.contains("Linked message"));
        assert!(html.contains(&linked.text), "the conversation stays open");
        let whole = Route::Chat {
            id: thread.id,
            split: None,
            message: None,
        };
        assert!(html.contains(&format!("href=\"{whole}\"")));
    }

    #[test]
    fn a_running_native_conversation_takes_and_shows_steered_messages() {
        let mut thread = a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat);
        let waiting = Uuid::new_v4().to_string();
        thread.state = a206_core::RunState::Running;
        thread.steering.push(a206_core::Message::new(
            a206_core::Role::User,
            waiting.clone(),
            0,
            a206_core::CodingBackend::Native,
        ));
        let mut snapshot = crate::empty_snapshot();
        snapshot.threads = vec![thread.clone()];
        let fixture = ThreadFixture {
            snapshot,
            id: thread.id,
            split: None,
            message: None,
        };
        let html = render_thread(fixture.clone());
        assert!(html.contains("Add to the running response"), "{html}");
        assert!(html.contains(&waiting) && html.contains("Read before its next step"));
        assert!(!html.contains("readonly"), "{html}");
        // Another engine's run cannot take messages while it works.
        let mut external = fixture;
        external.snapshot.threads[0].backend = a206_core::CodingBackend::ALL
            .into_iter()
            .find(|b| *b != a206_core::CodingBackend::Native)
            .unwrap();
        let html = render_thread(external);
        assert!(!html.contains("Add to the running response"));
        assert!(html.contains("readonly"), "{html}");
    }

    #[derive(Clone, Debug, PartialEq, Routable)]
    enum MessengerRoute {
        #[route("/")]
        MessengerView {},
    }

    #[component]
    fn MessengerView() -> Element {
        let snapshot = use_context::<a206_core::Snapshot>();
        let state = loaded_state(snapshot);
        use_context_provider(|| state);
        rsx! { crate::messengers::Messengers {} }
    }

    #[test]
    fn messenger_page_offers_to_allow_only_what_was_refused() {
        use a206_core::{
            MessengerConfig, MessengerKind, MessengerState, MessengerStatus, RefusedChat,
        };
        let id = || Uuid::new_v4().simple().to_string();
        let (allowed, other, sender, stranger, credential) = (id(), id(), id(), id(), id());
        let mut config = MessengerConfig::new(format!("m-{}", id()), MessengerKind::Telegram);
        config.enabled = true;
        config.credential = Some(credential.clone());
        config.allowed_chats.insert(allowed.clone());
        let refused = |chat: &str, sender: Option<&str>| RefusedChat {
            chat: chat.to_owned(),
            sender: sender.map(str::to_owned),
            title: None,
            count: 1,
            last_at: 0,
        };
        let mut snapshot = crate::empty_snapshot();
        snapshot.messengers = vec![MessengerStatus {
            name: config.name.clone(),
            state: MessengerState::Failed,
            refused: vec![
                refused(&allowed, Some(&sender)),
                refused(&other, None),
                // A sender in a chat that is not allowed is not offered.
                refused(&other, Some(&stranger)),
            ],
            ..MessengerStatus::default()
        }];
        snapshot.settings.messengers = vec![config.clone()];
        fn app() -> Element {
            rsx! { Router::<MessengerRoute> {} }
        }
        let mut dom = VirtualDom::new(app);
        dom.provide_root_context(snapshot);
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains(&config.name) && html.contains(&credential));
        assert!(html.contains("Failed"));
        // One button each in the allow-list editors, plus one per refusal
        // that allowing would admit.
        assert_eq!(html.matches(">Allow chat<").count(), 2, "{html}");
        assert_eq!(html.matches(">Allow sender<").count(), 2, "{html}");
        assert!(html.contains(&stranger));
    }

    #[test]
    fn irc_messenger_cards_name_the_login_they_sign_in_with() {
        use a206_core::{MessengerConfig, MessengerKind};
        let id = || Uuid::new_v4().simple().to_string();
        let login = format!("bot{}", &id()[..8]);
        let mut config = MessengerConfig::new(format!("m-{}", id()), MessengerKind::Irc);
        config.login = Some(login.clone());
        config.endpoint = Some(format!("ircs://{}.test", id()));
        let mut snapshot = crate::empty_snapshot();
        snapshot.settings.messengers = vec![config];
        fn app() -> Element {
            rsx! { Router::<MessengerRoute> {} }
        }
        let mut dom = VirtualDom::new(app);
        dom.provide_root_context(snapshot);
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains(&format!("Signs in as {login}")), "{html}");
        // The login can be changed on the card, as credentials can.
        assert!(html.contains("Change login"), "{html}");
    }
}
