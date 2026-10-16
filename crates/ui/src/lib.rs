//! Shared desktop/web/terminal application tree, composed with fc-dioxus.
#![forbid(unsafe_code)]
mod conversation;
mod pages;
mod settings;
use a206_core::{Command, Mode, Reply, Settings, Snapshot};
use fc_dioxus::prelude::*;
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

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Page {
    Workspace,
    Chat,
    Conversations,
    Projects,
    Providers,
    Downloads,
    Extensions,
    Browser,
    Documents,
    Appearance,
    Prompts,
    Security,
    Joshua,
}
impl Page {
    fn icon(self) -> IconName {
        match self {
            Self::Workspace => IconName::Home,
            Self::Chat | Self::Conversations => IconName::Sparkles,
            Self::Projects => IconName::Folder,
            Self::Providers => IconName::Link,
            Self::Downloads => IconName::Download,
            Self::Extensions => IconName::Plus,
            Self::Browser => IconName::Globe,
            Self::Documents => IconName::File,
            Self::Appearance => IconName::Settings,
            Self::Prompts => IconName::Edit,
            Self::Security => IconName::Lock,
            Self::Joshua => IconName::Terminal,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::Chat => "Conversation",
            Self::Conversations => "Conversations",
            Self::Projects => "Projects",
            Self::Providers => "Connections",
            Self::Downloads => "Downloads",
            Self::Extensions => "Extensions",
            Self::Browser => "Browser",
            Self::Documents => "Documents & git",
            Self::Appearance => "Appearance",
            Self::Prompts => "Agent modes",
            Self::Security => "Security",
            Self::Joshua => "Joshua",
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) struct State {
    pub data: Signal<Snapshot>,
    pub page: Signal<Page>,
    pub selected: Signal<Option<Uuid>>,
    pub secondary: Signal<Option<Uuid>>,
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
        window
            .history()
            .map_err(|_| "browser history unavailable")?
            .replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some("/"))
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
        match call(command).await {
            Ok(Reply::Error { message }) => state.status.set(message),
            Ok(Reply::Thread(thread)) => {
                if navigate {
                    state.selected.set(Some(thread.id));
                    state.page.set(Page::Chat);
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
    });
}
async fn refresh(mut state: State) {
    match call(Command::Snapshot).await {
        Ok(Reply::Snapshot(snapshot)) => {
            if *state.data.peek() != *snapshot {
                state.data.set(*snapshot);
            }
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
    let data = use_signal(empty_snapshot);
    let page = use_signal(|| Page::Workspace);
    let selected = use_signal(|| None);
    let secondary = use_signal(|| None);
    let status = use_signal(String::new);
    let connected = use_signal(|| false);
    let mut theme = use_signal(|| ThemeChoice::from_design(Design::Helix));
    let state = State {
        data,
        page,
        selected,
        secondary,
        status,
        connected,
        theme,
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
    let picked = theme();
    rsx! {
        document::Title { "Agent 206" }
        document::Stylesheet { href: asset!("/assets/app.css") }
        FutureCss { theme: picked.theme(false), design: picked.design, palette: picked.palette,
            div { class: "a206-shell a206-app",
                Header {}
                if !status().is_empty() { div { class: "a206-status", role: "status", "{status}" } }
                div { class: "a206-workspace",
                    Split { label: name!("Workspace navigation width"), default_value: 18.0, min: 12.0, max: 32.0,
                        primary: rsx! { Sidebar {} },
                        secondary: rsx! { main { class: "a206-main", id: "workspace-main",
                            match page() {
                                Page::Workspace => rsx! { pages::Overview {} },
                                Page::Chat => rsx! { conversation::Workspace {} },
                                Page::Conversations => rsx! { pages::Conversations {} },
                                Page::Providers => rsx! { settings::Providers {} },
                                Page::Appearance => rsx! { settings::Appearance {} },
                                Page::Prompts => rsx! { settings::Prompts {} },
                                Page::Projects => rsx! { pages::Projects {} },
                                Page::Downloads => rsx! { pages::Downloads {} },
                                Page::Extensions => rsx! { pages::Extensions {} },
                                Page::Browser => rsx! { pages::Browser {} },
                                Page::Documents => rsx! { pages::Documents {} },
                                Page::Security => rsx! { pages::Security {} },
                                Page::Joshua => rsx! { pages::Joshua {} },
                            }
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
}
#[component]
fn Header() -> Element {
    let mut state = use_context::<State>();
    rsx! { header { class: "a206-topbar",
        div { class: "a206-brand", img { src: asset!("/assets/agent206.svg"), alt: "", width: "34", height: "34" } "Agent 206" span { "YOUR WORK. YOUR AGENTS." } }
        div { class: "a206-row",
            Badge { tone: BadgeTone::Accent, "{state.page.read().label()}" }
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, onclick: move |_| state.page.set(Page::Appearance), Icon { name: IconName::Settings } "Theme" }
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Primary, onclick: move |_| dispatch(state, Command::CreateThread { title: "New conversation".into(), mode: Mode::Chat }), "+ New chat" }
        }
    } }
}
#[component]
fn Sidebar() -> Element {
    let mut state = use_context::<State>();
    let snapshot = state.data.read();
    rsx! { NavSidebar { class: "a206-sidebar", label: name!("Workspace navigation"), title: rsx! { "Workspace" }, collapsible: false,
        div { class: "a206-nav-group",
            for item in [Page::Workspace, Page::Projects, Page::Conversations] {
                Button { variant: ButtonVariant::Ghost, size: ButtonSize::Sm, class: if (state.page)() == item { "a206-nav-active" } else { "" }, onclick: move |_| state.page.set(item), Icon { name: item.icon(), size: IconSize::Sm } "{item.label()}" }
            }
        }
        div { class: "a206-nav-group",
            NavHeading { "Conversations" }
            if snapshot.threads.is_empty() { p { class: "a206-muted", "A fresh start." } }
            for thread in snapshot.threads.iter().rev().take(12) {
                { let id = thread.id; rsx! {
                    Button { key: "{id}", size: ButtonSize::Sm, variant: ButtonVariant::Ghost, pressed: (state.selected)() == Some(id), onclick: move |_| { state.selected.set(Some(id)); state.page.set(Page::Chat); },
                        Icon { name: if thread.parent.is_some() { IconName::ArrowEnd } else { IconName::Sparkles }, size: IconSize::Sm } span { class: "a206-nav-title", "{thread.title}" } Badge { tone: BadgeTone::Accent, "{thread.mode.label()}" }
                    }
                } }
            }
            if snapshot.threads.len() > 12 {
                Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, onclick: move |_| state.page.set(Page::Conversations), "View all {snapshot.threads.len()} conversations" }
            }
        }
        div { class: "a206-nav-group",
            NavHeading { "Workbench" }
            for item in [Page::Documents, Page::Browser, Page::Downloads, Page::Extensions] {
                Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, onclick: move |_| state.page.set(item), Icon { name: item.icon(), size: IconSize::Sm } "{item.label()}" }
            }
        }
        div { class: "a206-sidebar-footer a206-nav-group",
            NavHeading { "Control center" }
            for item in [Page::Providers, Page::Joshua, Page::Prompts, Page::Security] {
                Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, onclick: move |_| state.page.set(item), Icon { name: item.icon(), size: IconSize::Sm } "{item.label()}" }
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
