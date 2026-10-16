//! Shared desktop/web/terminal application tree, composed with fc-dioxus.
#![forbid(unsafe_code)]
mod conversation;
mod pages;
mod route;
mod settings;
use a206_core::{Command, Mode, Reply, Settings, Snapshot};
use dioxus::router::Navigator;
use fc_dioxus::prelude::*;
use route::Route;
#[cfg(target_arch = "wasm32")]
use uuid::Uuid;
#[cfg(not(target_arch = "wasm32"))]
static CLIENT: std::sync::Mutex<Option<a206_runtime::client::Client>> = std::sync::Mutex::new(None);
#[cfg(not(target_arch = "wasm32"))]
static CONNECTION: std::sync::OnceLock<(std::path::PathBuf, Option<String>)> =
    std::sync::OnceLock::new();
#[cfg(not(target_arch = "wasm32"))]
pub fn configure(root: std::path::PathBuf, remote: Option<String>) {
    let _existing = CONNECTION.set((root.clone(), remote.clone()));
    if let Ok(mut client) = CLIENT.lock() {
        *client = Some(a206_runtime::client::Client::start(root, remote));
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn reconnect() {
    if let Some((root, remote)) = CONNECTION.get() {
        configure(root.clone(), remote.clone());
    }
}

#[derive(Clone, Copy)]
pub(crate) struct State {
    pub data: Signal<Snapshot>,
    pub navigator: Navigator,
    pub loaded: Signal<bool>,
    pub status: Signal<String>,
    pub connected: Signal<bool>,
    pub theme: Signal<ThemeChoice>,
}
fn empty_snapshot() -> Snapshot {
    Snapshot {
        settings: Settings::default(),
        threads: vec![],
        projects: vec![],
        catalogs: vec![],
        downloads: vec![],
        plugins: vec![],
        last_event: 0,
        managed_joshua_provider: None,
    }
}
#[cfg(not(target_arch = "wasm32"))]
async fn call(command: Command) -> Result<Reply, String> {
    let client = CLIENT
        .lock()
        .map_err(|_| "connection lock poisoned")?
        .clone()
        .ok_or("gateway is not connected")?;
    client.call(command).await.map_err(|e| e.to_string())
}
#[cfg(target_arch = "wasm32")]
async fn call(command: Command) -> Result<Reply, String> {
    let request = a206_protocol::Request {
        id: Uuid::new_v4(),
        body: command,
    };
    let origin = web_sys::window()
        .ok_or("browser window unavailable")?
        .location()
        .origin()
        .map_err(|_| "browser origin unavailable")?;
    let response = reqwest::Client::new()
        .post(format!("{origin}/api/command"))
        .header("Content-Type", "application/msgpack")
        .bearer_auth(web_session()?)
        .body(a206_protocol::encode(&request).map_err(|e| e.to_string())?)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("web gateway returned {}", response.status()));
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    let mut decoder = a206_protocol::Decoder::default();
    let mut replies = decoder
        .feed::<a206_protocol::Response<Reply>>(&bytes)
        .map_err(|e| e.to_string())?;
    decoder.finish().map_err(|e| e.to_string())?;
    if replies.len() != 1 {
        return Err("invalid gateway response".into());
    }
    let response = replies.remove(0);
    if response.id != request.id {
        return Err("gateway response id mismatch".into());
    }
    Ok(response.body)
}
#[cfg(target_arch = "wasm32")]
fn web_session() -> Result<String, String> {
    let window = web_sys::window().ok_or("browser unavailable")?;
    let storage = window
        .session_storage()
        .map_err(|_| "session storage unavailable")?
        .ok_or("session storage unavailable")?;
    let fragment = window
        .location()
        .hash()
        .map_err(|_| "session URL unavailable")?;
    if let Some(token) = fragment.strip_prefix("#a206=") {
        if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid browser session".into());
        }
        storage
            .set_item("a206.session", token)
            .map_err(|_| "session storage unavailable")?;
        let history = window
            .history()
            .map_err(|_| "browser history unavailable")?;
        let route = format!(
            "{}{}",
            window
                .location()
                .pathname()
                .map_err(|_| "session path unavailable")?,
            window
                .location()
                .search()
                .map_err(|_| "session query unavailable")?,
        );
        history
            .replace_state_with_url(
                &history.state().map_err(|_| "browser history unavailable")?,
                "",
                Some(&route),
            )
            .map_err(|_| "could not clear session URL")?;
    }
    storage
        .get_item("a206.session")
        .map_err(|_| "session storage unavailable")?
        .ok_or_else(|| "Open the authorization URL printed by the web gateway".into())
}
pub(crate) fn dispatch(mut state: State, command: Command) {
    let navigate = matches!(
        command,
        Command::CreateThread { .. } | Command::CreateSubagent { .. }
    );
    spawn(async move {
        let mut destination = None;
        match call(command).await {
            Ok(Reply::Error { message }) => state.status.set(message),
            Ok(Reply::Thread(thread)) => {
                if navigate {
                    destination = Some(Route::Chat {
                        id: thread.id,
                        split: None,
                    });
                }
                state.status.set(String::new());
            }
            Ok(Reply::Text(text)) => state.status.set(text),
            Ok(_) => state.status.set(String::new()),
            Err(error) => {
                state.status.set(error);
                state.connected.set(false);
            }
        }
        refresh(state).await;
        if let Some(route) = destination {
            state.navigator.push(route);
        }
    });
}
async fn refresh(mut state: State) {
    match call(Command::Snapshot).await {
        Ok(Reply::Snapshot(snapshot)) => {
            if *state.data.peek() != *snapshot {
                state.data.set(*snapshot);
            }
            state.loaded.set(true);
            state.connected.set(true);
        }
        Err(error) => {
            state.connected.set(false);
            state.status.set(error);
        }
        _ => {}
    }
}
fn theme_from(settings: &Settings) -> ThemeChoice {
    let design = Design::ALL
        .iter()
        .copied()
        .find(|d| d.as_str() == settings.theme_design)
        .unwrap_or(Design::Helix);
    let mut choice = ThemeChoice::from_design(design);
    if let Some(name) = &settings.theme_palette {
        choice.palette = Palette::ALL
            .iter()
            .copied()
            .find(|p| p.as_str() == name)
            .unwrap_or(design.palette());
    }
    choice.scheme = match settings.theme_scheme.as_str() {
        "light" => Scheme::Light,
        "dark" => Scheme::Dark,
        _ => Scheme::System,
    };
    choice.contrast = if settings.theme_contrast == "high" {
        Contrast::High
    } else {
        Contrast::System
    };
    choice
}
#[component]
pub fn App() -> Element {
    // Consume the bootstrap fragment before the router sees the initial location.
    #[cfg(target_arch = "wasm32")]
    use_hook(|| {
        let _ = web_session();
    });
    // Desktop supplies history itself; the terminal and SSR also need a history.
    #[cfg(not(target_arch = "wasm32"))]
    use_hook(|| {
        use dioxus::history::{History, MemoryHistory, provide_history_context};
        use std::rc::Rc;
        if try_consume_context::<Rc<dyn History>>().is_none() {
            provide_history_context(Rc::new(MemoryHistory::default()));
        }
    });
    let theme = use_signal(|| ThemeChoice::from_design(Design::Helix));
    use_context_provider(|| theme);
    let picked = theme();
    rsx! {
        document::Title { "Agent 206" }
        document::Stylesheet { href: asset!("/assets/app.css") }
        FutureCss { theme: picked.theme(false), design: picked.design, palette: picked.palette,
            Router::<Route> {}
        }
    }
}

/// Persistent route layout: navigation does not restart the gateway connection or polling.
#[component]
fn Shell() -> Element {
    let data = use_signal(empty_snapshot);
    let status = use_signal(String::new);
    let connected = use_signal(|| false);
    let loaded = use_signal(|| false);
    let mut theme = use_context::<Signal<ThemeChoice>>();
    let state = State {
        data,
        status,
        connected,
        loaded,
        theme,
        navigator: navigator(),
    };
    use_context_provider(|| state);
    use_future(move || async move {
        refresh(state).await;
        theme.set(theme_from(&state.data.peek().settings));
        loop {
            futures_timer::Delay::new(std::time::Duration::from_secs(2)).await;
            refresh(state).await;
        }
    });
    rsx! {
        div { class: "a206-shell a206-app",
            Header {}
            if !status().is_empty() { div { class: "a206-status", role: "status", "{status}" } }
            div { class: "a206-workspace",
                Split { label: name!("Workspace navigation width"), default_value: 18.0, min: 12.0, max: 32.0,
                    primary: rsx! { Sidebar {} },
                    secondary: rsx! { main { class: "a206-main", id: "workspace-main",
                        Outlet::<Route> {}
                    } }
                }
            }
            footer { class: "a206-statusbar",
                Badge { tone: if connected() { BadgeTone::Success } else { BadgeTone::Warning }, dot: true, if connected() { "Gateway connected" } else { "Gateway offline" } }
                span { "{data.read().threads.len()} conversations · {data.read().projects.len()} projects" }
                span { "Agent 206 / development preview" }
            }
        }
    }
}
#[component]
fn Header() -> Element {
    let state = use_context::<State>();
    let route = use_route::<Route>();
    rsx! { header { class: "a206-topbar",
        div { class: "a206-brand", img { src: asset!("/assets/agent206.svg"), alt: "", width: "34", height: "34" } "Agent 206" span { "YOUR WORK. YOUR AGENTS." } }
        div { class: "a206-row",
            IconButton { icon: IconName::ArrowStart, label: name!("Go back"), size: ButtonSize::Sm, variant: ButtonVariant::Ghost,
                availability: if state.navigator.can_go_back() { Availability::Enabled } else { Availability::Disabled },
                onclick: move |_| state.navigator.go_back() }
            IconButton { icon: IconName::ArrowEnd, label: name!("Go forward"), size: ButtonSize::Sm, variant: ButtonVariant::Ghost,
                availability: if state.navigator.can_go_forward() { Availability::Enabled } else { Availability::Disabled },
                onclick: move |_| state.navigator.go_forward() }
            Badge { tone: BadgeTone::Accent, "{route.label()}" }
            LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: Route::Appearance {}.to_string(), Icon { name: IconName::Settings } "Theme" }
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Primary, onclick: move |_| dispatch(state, Command::CreateThread { title: "New conversation".into(), mode: Mode::Chat }), "+ New chat" }
        }
    } }
}
#[component]
fn Sidebar() -> Element {
    let state = use_context::<State>();
    let route = use_route::<Route>();
    let snapshot = state.data.read();
    rsx! { NavSidebar { class: "a206-sidebar", label: name!("Workspace navigation"), title: rsx! { "Workspace" }, collapsible: false,
        div { class: "a206-nav-group",
            for item in [Route::Workspace {}, Route::Projects {}, Route::Conversations {}] {
                LinkButton { variant: ButtonVariant::Ghost, size: ButtonSize::Sm, class: if route == item { "a206-nav-active" } else { "" }, href: item.to_string(), Icon { name: item.icon(), size: IconSize::Sm } "{item.label()}" }
            }
        }
        div { class: "a206-nav-group",
            NavHeading { "Conversations" }
            if snapshot.threads.is_empty() { p { class: "a206-muted", "A fresh start." } }
            for thread in snapshot.threads.iter().rev().take(12) {
                { let id = thread.id; rsx! {
                    LinkButton { key: "{id}", size: ButtonSize::Sm, variant: ButtonVariant::Ghost, class: if matches!(route, Route::Chat { id: current, .. } if current == id) { "a206-nav-active" } else { "" }, href: Route::Chat { id, split: None }.to_string(),
                        Icon { name: if thread.parent.is_some() { IconName::ArrowEnd } else { IconName::Sparkles }, size: IconSize::Sm } span { class: "a206-nav-title", "{thread.title}" } Badge { tone: BadgeTone::Accent, "{thread.mode.label()}" }
                    }
                } }
            }
            if snapshot.threads.len() > 12 {
                LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: Route::Conversations {}.to_string(), "View all {snapshot.threads.len()} conversations" }
            }
        }
        div { class: "a206-nav-group",
            NavHeading { "Workbench" }
            for item in [Route::Documents {}, Route::Browser {}, Route::Downloads {}, Route::Extensions {}] {
                LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, class: if route == item { "a206-nav-active" } else { "" }, href: item.to_string(), Icon { name: item.icon(), size: IconSize::Sm } "{item.label()}" }
            }
        }
        div { class: "a206-sidebar-footer a206-nav-group",
            NavHeading { "Control center" }
            for item in [Route::Providers {}, Route::Joshua {}, Route::Prompts {}, Route::Security {}] {
                LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, class: if route == item { "a206-nav-active" } else { "" }, href: item.to_string(), Icon { name: item.icon(), size: IconSize::Sm } "{item.label()}" }
            }
        }
    } }
}
#[component]
pub(crate) fn PageFrame(
    kicker: String,
    title: String,
    description: String,
    children: Element,
) -> Element {
    rsx! { section { class: "a206-page", div { class: "a206-page-inner",
        header { p { class: "a206-eyebrow", "{kicker}" } h1 { "{title}" } p { class: "a206-muted", "{description}" } }
        {children}
    } } }
}
