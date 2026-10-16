//! Shared, typed navigation. Thread identity and split view have a single source of truth.
use crate::pages::Gateway;
use crate::{
    PageFrame, Shell,
    conversation::Workspace as Chat,
    pages::{
        Browser, Conversations, Documents, Downloads, Extensions, Joshua, Overview as Workspace,
        Projects, Security,
    },
    settings::{Appearance, Prompts, Providers},
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
    #[route("/conversations/:id?:split")]
    Chat { id: Uuid, split: Option<Uuid> },
    #[route("/projects")]
    Projects {},
    #[route("/connections")]
    Providers {},
    #[route("/downloads")]
    Downloads {},
    #[route("/extensions")]
    Extensions {},
    #[route("/browser")]
    Browser {},
    #[route("/documents")]
    Documents {},
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
    #[route("/:..segments")]
    NotFound { segments: Vec<String> },
}

impl Route {
    pub(crate) fn icon(&self) -> IconName {
        match self {
            Self::Workspace {} | Self::NotFound { .. } => IconName::Home,
            Self::Chat { .. } | Self::Conversations {} => IconName::Sparkles,
            Self::Projects {} => IconName::Folder,
            Self::Providers {} => IconName::Link,
            Self::Downloads {} => IconName::Download,
            Self::Extensions {} => IconName::Plus,
            Self::Browser {} => IconName::Globe,
            Self::Documents {} => IconName::File,
            Self::Appearance {} => IconName::Settings,
            Self::Prompts {} => IconName::Edit,
            Self::Security {} => IconName::Lock,
            Self::Joshua {} | Self::Gateway {} => IconName::Terminal,
        }
    }
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Workspace {} => "Workspace",
            Self::Chat { .. } => "Conversation",
            Self::Conversations {} => "Conversations",
            Self::Projects {} => "Projects",
            Self::Providers {} => "Connections",
            Self::Downloads {} => "Downloads",
            Self::Extensions {} => "Extensions",
            Self::Browser {} => "Browser",
            Self::Documents {} => "Documents & git",
            Self::Appearance {} => "Appearance",
            Self::Prompts {} => "Agent modes",
            Self::Security {} => "Security",
            Self::Joshua {} => "Joshua",
            Self::Gateway {} => "Gateway",
            Self::NotFound { .. } => "Page not found",
        }
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
mod tests {
    use super::*;
    use dioxus::history::{History, MemoryHistory};
    use std::rc::Rc;

    #[test]
    fn conversation_addresses_round_trip_and_reject_malformed_identity() {
        let id = Uuid::new_v4();
        let split = Uuid::new_v4();
        for route in [
            Route::Chat { id, split: None },
            Route::Chat {
                id,
                split: Some(split),
            },
        ] {
            assert_eq!(route.to_string().parse::<Route>().unwrap(), route);
        }
        assert_eq!(
            format!("/conversations/{id}").parse::<Route>().unwrap(),
            Route::Chat { id, split: None }
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
        split: Option<Uuid>,
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
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Choose your gateway."));
        assert!(html.contains("This computer"));
        assert!(html.contains("A remote gateway over SSH"));
        assert!(html.contains("value=\"local\" checked"), "{html}");
        assert!(html.contains("Use this computer"));
        // A configured client may leave setup without changing its gateway.
        assert!(html.contains("Keep current gateway"));
    }

    #[component]
    fn ThreadView() -> Element {
        let fixture = use_context::<ThreadFixture>();
        let state = crate::State {
            data: use_signal(|| fixture.snapshot.clone()),
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
        };
        use_context_provider(|| state);
        rsx! { Chat { id: fixture.id, split: fixture.split } }
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
    fn loaded_deep_links_select_exact_threads_and_restore_split_selector() {
        let first = a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat);
        let second = a206_core::Thread::new(Uuid::new_v4().to_string(), a206_core::Mode::Chat);
        let mut snapshot = crate::empty_snapshot();
        snapshot.threads = vec![first.clone(), second.clone()];
        let fixture = ThreadFixture {
            snapshot,
            id: first.id,
            split: Some(second.id),
        };
        let html = render_thread(fixture.clone());
        assert_eq!(html.matches("aria-label=\"Message your agent\"").count(), 2);
        assert!(html.contains(&format!("value=\"{}\" selected", second.id)));

        // A URL may never open the latest or first available thread as a fallback.
        let html = render_thread(ThreadFixture {
            id: Uuid::new_v4(),
            ..fixture.clone()
        });
        assert!(html.contains("Conversation unavailable"));
        assert!(!html.contains(&first.title));
        assert!(!html.contains(&second.title));
        let html = render_thread(ThreadFixture {
            split: Some(Uuid::new_v4()),
            ..fixture.clone()
        });
        assert!(html.contains("Second conversation unavailable"));
        assert_eq!(html.matches("aria-label=\"Message your agent\"").count(), 1);
        let html = render_thread(ThreadFixture {
            split: Some(first.id),
            ..fixture
        });
        assert_eq!(html.matches("aria-label=\"Message your agent\"").count(), 1);
    }
}
