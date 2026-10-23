//! Appearance is a workspace action, including when requested through an old URL.
use crate::route::Route;
use dioxus::history::History;
use fc_dioxus::prelude::*;
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone)]
pub(crate) struct Open(pub Signal<bool>, Arc<AtomicUsize>);

fn is_appearance(path: &str) -> bool {
    matches!(path.parse::<Route>(), Ok(Route::Appearance {}))
}

/// Install the product's modal action before the shared router creates entries.
/// All ordinary navigation stays with the renderer's existing history provider.
pub(crate) fn use_navigation() {
    let mut open = use_signal(|| false);
    let host_navigation = use_hook(|| Arc::new(AtomicUsize::new(0)));
    let host_events = host_navigation.clone();
    let history = use_hook(move || {
        let inner = dioxus::history::history();
        if is_appearance(&inner.current_route()) {
            inner.replace(Route::Workspace {}.to_string());
            open.set(true);
        }
        let history = Rc::new(AppearanceNavigation {
            inner,
            open,
            host_navigation: host_events,
        }) as Rc<dyn History>;
        // Navigator may also be called from a root-scope command callback.
        // Keep its provider identical to the one consumed below the App.
        dioxus::dioxus_core::provide_root_context(history.clone());
        history
    });
    use_context_provider(|| Open(open, host_navigation));
    use_context_provider(|| history);
}

/// Browser Back/Forward can arrive directly from the renderer rather than from
/// Navigator. Dismiss the overlay when that changes the content route, too.
pub(crate) fn use_dismiss_on_host_navigation() {
    let state = use_context::<Open>();
    let mut open = state.0;
    let revision = state.1.load(Ordering::Relaxed);
    let previous = use_hook(|| Rc::new(Cell::new(0)));
    use_effect(use_reactive!(|revision| {
        if previous.replace(revision) != revision {
            open.set(false);
        }
    }));
}

struct AppearanceNavigation {
    inner: Rc<dyn History>,
    open: Signal<bool>,
    host_navigation: Arc<AtomicUsize>,
}
impl AppearanceNavigation {
    fn set_open(&self, value: bool) {
        if *self.open.peek() != value {
            let mut open = self.open;
            open.set(value);
        }
    }
}
impl History for AppearanceNavigation {
    fn current_route(&self) -> String {
        self.inner.current_route()
    }
    fn current_prefix(&self) -> Option<String> {
        self.inner.current_prefix()
    }
    fn can_go_back(&self) -> bool {
        self.inner.can_go_back()
    }
    fn can_go_forward(&self) -> bool {
        self.inner.can_go_forward()
    }
    fn go_back(&self) {
        self.set_open(false);
        self.inner.go_back();
    }
    fn go_forward(&self) {
        self.set_open(false);
        self.inner.go_forward();
    }
    fn push(&self, route: String) {
        let appearance = is_appearance(&route);
        self.set_open(appearance);
        if !appearance {
            self.inner.push(route);
        }
    }
    fn replace(&self, route: String) {
        let appearance = is_appearance(&route);
        self.set_open(appearance);
        if !appearance {
            self.inner.replace(route);
        }
    }
    fn external(&self, url: String) -> bool {
        self.inner.external(url)
    }
    fn updater(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        let revision = self.host_navigation.clone();
        self.inner.updater(Arc::new(move || {
            // The host callback can run outside a Dioxus runtime scope. Only
            // record the traversal here; the subscribed UI dismisses safely.
            revision.fetch_add(1, Ordering::Relaxed);
            callback();
        }));
    }
    fn include_prevent_default(&self) -> bool {
        self.inner.include_prevent_default()
    }
}
