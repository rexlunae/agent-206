//! Shared desktop/web/terminal application tree, composed with fc-dioxus.
#![forbid(unsafe_code)]
mod conversation;
mod error;
mod extensions;
mod health;
mod mcp;
mod messengers;
mod pages;
mod prompts;
mod route;
mod search;
mod settings;
mod skills;
mod tools;
#[cfg(target_arch = "wasm32")]
use a206_core::Ignore;
mod setup;
#[cfg(target_arch = "wasm32")]
mod sign_in;
#[cfg(target_arch = "wasm32")]
use sign_in::SignOut;
#[cfg(not(target_arch = "wasm32"))]
mod vault;
#[cfg(target_arch = "wasm32")]
mod web_vault;
use a206_core::{
    Command, ErrorKind, ErrorReport, GatewayAction, GatewayPhase, GatewayReport, Mode, Reply,
    Settings, Snapshot,
};
use dioxus::router::Navigator;
use error::ErrorNotice;
use fc_dioxus::prelude::*;
use route::Route;
#[cfg(target_arch = "wasm32")]
use uuid::Uuid;
#[cfg(not(target_arch = "wasm32"))]
static CLIENT: std::sync::Mutex<Option<a206_runtime::control::GatewayController>> =
    std::sync::Mutex::new(None);
/// Profile root and any `--remote` override, kept for first-time setup.
#[cfg(not(target_arch = "wasm32"))]
static PROFILE: std::sync::Mutex<Option<(std::path::PathBuf, Option<String>)>> =
    std::sync::Mutex::new(None);
/// A saved setup that could not be loaded; the setup screen shows it.
#[cfg(not(target_arch = "wasm32"))]
static SETUP_ERROR: std::sync::Mutex<Option<ErrorReport>> = std::sync::Mutex::new(None);
/// Connect only to an explicit `--remote` or a saved setup. Without either, the
/// desktop and terminal clients open first-time setup instead of launching a gateway.
#[cfg(not(target_arch = "wasm32"))]
pub fn configure(root: std::path::PathBuf, remote: Option<String>) {
    if let Ok(mut profile) = PROFILE.lock() {
        *profile = Some((root.clone(), remote.clone()));
    }
    let pinned = remote.is_some();
    match a206_runtime::connection::resolve(&root, remote) {
        Ok(Some(target)) => connect(root, &target, pinned),
        Ok(None) => {}
        Err(error) => {
            if let Ok(mut slot) = SETUP_ERROR.lock() {
                // Traced where it occurred; kept structured for the setup screen.
                *slot = Some(a206_runtime::error::report(&error));
            }
        }
    }
}
/// The target `CLIENT` talks to, and whether it came from `--remote`, for the
/// setup screen.
#[cfg(not(target_arch = "wasm32"))]
static TARGET: std::sync::Mutex<Option<(a206_core::GatewayTarget, bool)>> =
    std::sync::Mutex::new(None);
#[cfg(not(target_arch = "wasm32"))]
fn connect(root: std::path::PathBuf, target: &a206_core::GatewayTarget, pinned: bool) {
    if let Ok(mut client) = CLIENT.lock() {
        *client = Some(a206_runtime::control::GatewayController::new(
            root,
            target.remote().map(str::to_owned),
        ));
    }
    if let Ok(mut active) = TARGET.lock() {
        *active = Some((target.clone(), pinned));
    }
}
/// True until this client has a gateway to talk to.
fn setup_required() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        CLIENT
            .lock()
            .map(|client| client.is_none())
            .unwrap_or(false)
    }
    #[cfg(target_arch = "wasm32")]
    {
        // The bridge is asked once the shell mounts; see `Shell`.
        false
    }
}

#[derive(Clone, Copy)]
pub(crate) struct State {
    pub data: Signal<Snapshot>,
    pub navigator: Navigator,
    pub loaded: Signal<bool>,
    pub status: Signal<String>,
    pub error: Signal<Option<ErrorReport>>,
    pub outage: Signal<Option<ErrorReport>>,
    pub connected: Signal<bool>,
    pub gateway: Signal<Option<GatewayReport>>,
    pub gateway_busy: Signal<Option<GatewayAction>>,
    pub gateway_generation: Signal<u64>,
    pub theme: Signal<ThemeChoice>,
    /// First-time (or changed) gateway setup is showing instead of the workspace.
    pub setup: Signal<bool>,
    /// A server opened to other devices is asking this browser to sign in.
    #[cfg(target_arch = "wasm32")]
    pub sign_in: Signal<bool>,
}
fn empty_snapshot() -> Snapshot {
    Snapshot {
        settings: Settings::default(),
        threads: vec![],
        projects: vec![],
        catalogs: vec![],
        downloads: vec![],
        plugins: vec![],
        messengers: vec![],
        last_event: 0,
        managed_joshua_provider: None,
        mcp_servers: vec![],
    }
}
/// Send a command; a gateway-reported failure arrives as `Err` with its causes.
async fn call(command: Command) -> Result<Reply, ErrorReport> {
    let operation = command.name();
    let result = request(command).await.and_then(Reply::into_result);
    if let Err(report) = &result {
        error::record(operation, report);
    }
    result
}
#[cfg(not(target_arch = "wasm32"))]
fn controller() -> Result<a206_runtime::control::GatewayController, ErrorReport> {
    CLIENT
        .lock()
        .map_err(|_| ErrorReport::new(ErrorKind::Internal, "connection lock poisoned"))?
        .clone()
        .ok_or_else(|| ErrorReport::new(ErrorKind::Unavailable, "gateway is not configured"))
}
#[cfg(not(target_arch = "wasm32"))]
async fn request(command: Command) -> Result<Reply, ErrorReport> {
    controller()?
        .call(command)
        .await
        .map_err(|error| a206_runtime::error::report(&error))
}
#[cfg(target_arch = "wasm32")]
async fn request(command: Command) -> Result<Reply, ErrorReport> {
    web_request("/api/command", command).await
}
#[cfg(target_arch = "wasm32")]
fn unavailable(message: impl Into<String>) -> ErrorReport {
    ErrorReport::new(ErrorKind::Unavailable, message)
}
#[cfg(target_arch = "wasm32")]
fn captured(kind: ErrorKind) -> impl Fn(&(dyn std::error::Error + 'static)) -> ErrorReport {
    move |error| ErrorReport::capture(error, |_| Some(kind))
}
#[cfg(target_arch = "wasm32")]
pub(crate) async fn web_request<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    path: &str,
    body: T,
) -> Result<R, ErrorReport> {
    let bearer =
        web_session().map_err(|message| ErrorReport::new(ErrorKind::PermissionDenied, message))?;
    web_post(path, body, Some(bearer)).await
}
/// Post one MessagePack request to the bridge, with this browser's bearer
/// when it has one (sign-in needs none).
#[cfg(target_arch = "wasm32")]
pub(crate) async fn web_post<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    path: &str,
    body: T,
    bearer: Option<String>,
) -> Result<R, ErrorReport> {
    let request = a206_protocol::Request {
        id: Uuid::new_v4(),
        body,
    };
    let origin = web_sys::window()
        .ok_or_else(|| unavailable("browser window unavailable"))?
        .location()
        .origin()
        .map_err(|_| unavailable("browser origin unavailable"))?;
    let mut post = reqwest::Client::new()
        .post(format!("{origin}{path}"))
        .header("Content-Type", "application/msgpack");
    if let Some(bearer) = &bearer {
        post = post.bearer_auth(bearer);
    }
    let response = post
        .body(a206_protocol::encode(&request).map_err(|e| captured(ErrorKind::Protocol)(&e))?)
        .send()
        .await
        .map_err(|e| captured(ErrorKind::Unavailable)(&e))?;
    // A failed gateway control carries its structured report as the body.
    if response.status().as_u16() == 409 {
        return Err(decode_response::<ErrorReport>(&request.id, response).await?);
    }
    if !response.status().is_success() {
        if matches!(response.status().as_u16(), 404 | 405) {
            return Err(ErrorReport::new(
                ErrorKind::Protocol,
                "This web server does not provide the current app controls. Restart the Agent 206 web server from the current build, then open its authorization URL.",
            ));
        }
        if matches!(response.status().as_u16(), 401 | 403) {
            // A restarted bridge issues a new bearer and sessions expire; drop
            // the stale one so the next authorization or sign-in replaces it.
            if let Some(bearer) = &bearer {
                forget_web_session(bearer);
            }
            return Err(ErrorReport::new(
                ErrorKind::PermissionDenied,
                if sign_in::network() {
                    "Your sign-in has ended. Sign in again with a code from your authenticator app."
                } else {
                    "This browser is not authorized for the running web bridge. Open the authorization URL printed by `agent206 web`; restart it for a new link if that one was already used."
                },
            ));
        }
        return Err(unavailable(format!(
            "web gateway returned {}",
            response.status()
        )));
    }
    decode_response(&request.id, response).await
}
#[cfg(target_arch = "wasm32")]
async fn decode_response<R: serde::de::DeserializeOwned>(
    id: &Uuid,
    response: reqwest::Response,
) -> Result<R, ErrorReport> {
    let bytes = response
        .bytes()
        .await
        .map_err(|e| captured(ErrorKind::Unavailable)(&e))?;
    let mut decoder = a206_protocol::Decoder::default();
    let mut replies = decoder
        .feed::<a206_protocol::Response<R>>(&bytes)
        .map_err(|e| captured(ErrorKind::Protocol)(&e))?;
    decoder
        .finish()
        .map_err(|e| captured(ErrorKind::Protocol)(&e))?;
    if replies.len() != 1 {
        return Err(ErrorReport::new(
            ErrorKind::Protocol,
            "invalid gateway response",
        ));
    }
    let response = replies.remove(0);
    if response.id != *id {
        return Err(ErrorReport::new(
            ErrorKind::Protocol,
            "gateway response id mismatch",
        ));
    }
    Ok(response.body)
}
pub(crate) async fn control(action: GatewayAction) -> Result<GatewayReport, ErrorReport> {
    #[cfg(target_arch = "wasm32")]
    let result = web_request("/api/gateway", action).await;
    #[cfg(not(target_arch = "wasm32"))]
    let result = match controller() {
        Ok(client) => client
            .control(action)
            .await
            .map_err(|error| a206_runtime::error::report(&error)),
        Err(report) => Err(report),
    };
    if let Err(report) = &result {
        error::record(action.name(), report);
    }
    result
}
pub(crate) fn control_gateway(mut state: State, action: GatewayAction) {
    if state.gateway_busy.peek().is_some() {
        return;
    }
    state.gateway_busy.set(Some(action));
    let generation = state.gateway_generation.peek().wrapping_add(1);
    state.gateway_generation.set(generation);
    spawn(async move {
        match control(action).await {
            Ok(report) => {
                state.connected.set(report.phase == GatewayPhase::Running);
                state.error.set(report.failure_report());
                state.gateway.set(Some(report));
            }
            Err(error) => {
                state.connected.set(false);
                state.error.set(Some(error));
            }
        }
        state.gateway_busy.set(None);
        refresh(state).await;
    });
}
/// The bridge bearer is shared by every tab of this exact origin (scheme, host and
/// port) and dies with the bridge process, so it lives in origin-scoped local
/// storage: per-tab session storage stranded new tabs because the bootstrap link
/// is one-use.
#[cfg(target_arch = "wasm32")]
fn web_storage() -> Result<web_sys::Storage, String> {
    web_sys::window()
        .ok_or("browser unavailable")?
        .local_storage()
        .map_err(|_| "browser storage unavailable")?
        .ok_or_else(|| "browser storage unavailable".into())
}
/// Forget a rejected bearer, unless another tab has already stored a newer one.
#[cfg(target_arch = "wasm32")]
fn forget_web_session(rejected: &str) {
    if let Ok(storage) = web_storage()
        && storage.get_item("a206.session").ok().flatten().as_deref() == Some(rejected)
    {
        // If storage refuses, the stale bearer is rejected again and replaced
        // by the next authorization link; nothing more can be done here.
        storage.remove_item("a206.session").ignore();
    }
}
#[cfg(target_arch = "wasm32")]
fn web_session() -> Result<String, String> {
    let window = web_sys::window().ok_or("browser unavailable")?;
    let storage = web_storage()?;
    let fragment = window
        .location()
        .hash()
        .map_err(|_| "session URL unavailable")?;
    if let Some(token) = fragment.strip_prefix("#a206=") {
        store_web_session(token)?;
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
        .map_err(|_| "browser storage unavailable")?
        .ok_or_else(|| {
            if sign_in::network() {
                "Sign in with a code from your authenticator app.".into()
            } else {
                "Open the authorization URL printed by `agent206 web` to connect this browser."
                    .into()
            }
        })
}
/// Keep a bearer from the authorization link or a sign-in.
#[cfg(target_arch = "wasm32")]
fn store_web_session(token: &str) -> Result<(), String> {
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid browser session".into());
    }
    web_storage()?
        .set_item("a206.session", token)
        .map_err(|_| "browser storage unavailable".into())
}
pub(crate) fn dispatch(mut state: State, command: Command) {
    let navigate = matches!(
        command,
        Command::CreateThread { .. } | Command::CreateSubagent { .. }
    );
    spawn(async move {
        let mut destination = None;
        let unreachable = |report: &ErrorReport| report.kind == ErrorKind::Unavailable;
        match call(command).await {
            Ok(Reply::Thread(thread)) => {
                if navigate {
                    destination = Some(Route::Chat {
                        id: thread.id,
                        split: None,
                    });
                }
                state.status.set(String::new());
                state.error.set(None);
            }
            Ok(Reply::Text(text)) => {
                state.status.set(text);
                state.error.set(None);
            }
            Ok(_) => {
                state.status.set(String::new());
                state.error.set(None);
            }
            Err(error) => {
                // A rejected command does not mean the gateway went away.
                if unreachable(&error) {
                    state.connected.set(false);
                }
                state.status.set(String::new());
                state.error.set(Some(error));
            }
        }
        refresh(state).await;
        if let Some(route) = destination {
            state.navigator.push(route);
        }
    });
}
async fn refresh(mut state: State) {
    if state.gateway_busy.peek().is_some() {
        return;
    }
    let generation = *state.gateway_generation.peek();
    let report = control(GatewayAction::Status).await;
    if generation != *state.gateway_generation.peek() {
        return;
    }
    match report {
        Ok(report) => {
            let running = report.phase == GatewayPhase::Running;
            // The controller answered; its probe's failure, if any, is the outage.
            // A running gateway or a deliberate stop clears it. Command errors
            // stay until dismissed.
            set_outage(state, report.failure_report());
            state.connected.set(running);
            state.gateway.set(Some(report));
            if !running {
                return;
            }
        }
        Err(error) => {
            state.connected.set(false);
            set_outage(state, Some(error));
            return;
        }
    }
    if state.gateway_busy.peek().is_some() {
        return;
    }
    let snapshot = call(Command::Snapshot).await;
    if generation != *state.gateway_generation.peek() {
        return;
    }
    match snapshot {
        Ok(Reply::Snapshot(snapshot)) => {
            if *state.data.peek() != *snapshot {
                state.data.set(*snapshot);
            }
            state.loaded.set(true);
            state.connected.set(true);
        }
        Err(error) => {
            state.connected.set(false);
            set_outage(state, Some(error));
        }
        _ => {}
    }
}
/// Connection failures found by polling, kept apart from command failures so a
/// later successful poll clears only what polling reported.
fn set_outage(mut state: State, outage: Option<ErrorReport>) {
    if *state.outage.peek() != outage {
        state.outage.set(outage);
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
        // Captures a fresh authorization fragment; failures surface on the first request.
        web_session().ignore();
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
    let error = use_signal(|| None);
    let outage = use_signal(|| None);
    let connected = use_signal(|| false);
    let loaded = use_signal(|| false);
    let gateway = use_signal(|| None);
    let gateway_busy = use_signal(|| None);
    let gateway_generation = use_signal(|| 0);
    let setup = use_signal(setup_required);
    #[cfg(target_arch = "wasm32")]
    let sign_in = use_signal(|| false);
    let mut theme = use_context::<Signal<ThemeChoice>>();
    let state = State {
        data,
        status,
        error,
        outage,
        connected,
        gateway,
        gateway_busy,
        gateway_generation,
        loaded,
        theme,
        setup,
        #[cfg(target_arch = "wasm32")]
        sign_in,
        navigator: navigator(),
    };
    use_context_provider(|| state);
    use_future(move || async move {
        // The web bridge may not have a gateway yet; it is asked once this
        // browser may use it, before any gateway is polled.
        #[cfg(target_arch = "wasm32")]
        let mut asked = false;
        let mut themed = false;
        loop {
            #[cfg(target_arch = "wasm32")]
            let signing_in = {
                sign_in::detect().await;
                let mut sign_in = state.sign_in;
                if sign_in::required() && !*sign_in.peek() {
                    sign_in.set(true);
                }
                let signing_in = *sign_in.peek();
                // If the bridge cannot be asked, polling reports why, and it
                // is asked again next time.
                if !signing_in
                    && !asked
                    && let Ok(status) = setup::status().await
                {
                    asked = true;
                    if status.required() {
                        let mut setup = state.setup;
                        setup.set(true);
                    }
                }
                signing_in
            };
            #[cfg(not(target_arch = "wasm32"))]
            let signing_in = false;
            // Never poll (or launch) a gateway before the user has chosen one.
            if !signing_in && !*state.setup.peek() {
                refresh(state).await;
                // Apply each gateway's theme once its first snapshot arrives
                // (switching gateways resets `loaded`).
                let loaded = *state.loaded.peek();
                if loaded && !themed {
                    theme.set(theme_from(&state.data.peek().settings));
                }
                themed = loaded;
            } else {
                // Whatever is chosen next gets its own theme on its first snapshot.
                themed = false;
            }
            futures_timer::Delay::new(std::time::Duration::from_secs(2)).await;
        }
    });
    // The sign-in screen replaces the workspace until this browser signs in.
    #[cfg(target_arch = "wasm32")]
    let signing_in = sign_in();
    #[cfg(not(target_arch = "wasm32"))]
    let signing_in = false;
    rsx! {
        div { class: "a206-shell a206-app",
            Header {}
            if !status().is_empty() { div { class: "a206-status", role: "status", "{status}" } }
            if let Some(report) = state.outage.read().clone() {
                div { class: "a206-status", ErrorNotice { report, title: "Gateway unreachable".to_string() } }
            }
            if let Some(report) = state.error.read().clone() {
                div { class: "a206-status", ErrorNotice { report, dismiss: move |_| { let mut error = state.error; error.set(None); } } }
            }
            div { class: "a206-workspace",
                Split { label: name!("Workspace navigation width"), default_value: 18.0, min: 12.0, max: 32.0,
                    primary: rsx! { Sidebar {} },
                    secondary: rsx! { main { class: "a206-main", id: "workspace-main",
                        if signing_in { SignInGate {} } else if setup() { GatewaySetupGate {} } else { Outlet::<Route> {} }
                    } }
                }
            }
            footer { class: "a206-statusbar",
                LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: Route::Gateway {}.to_string(),
                    Badge { tone: if connected() { BadgeTone::Success } else { BadgeTone::Warning }, dot: true, if connected() { "Gateway connected" } else { "Gateway offline" } }
                }
                span { "{data.read().threads.len()} conversations · {data.read().projects.len()} projects" }
                span { "Agent 206 / development preview" }
            }
        }
    }
}
#[component]
fn SignInGate() -> Element {
    #[cfg(target_arch = "wasm32")]
    {
        rsx! { sign_in::SignIn {} }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        rsx! {}
    }
}
/// Signing out is offered only by web servers that sign browsers in.
#[cfg(not(target_arch = "wasm32"))]
#[component]
fn SignOut() -> Element {
    rsx! {}
}
#[component]
fn GatewaySetupGate() -> Element {
    rsx! { setup::GatewaySetup {} }
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
            LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: Route::Gateway {}.to_string(), Icon { name: Route::Gateway {}.icon() } "Gateway" }
            LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: Route::Appearance {}.to_string(), Icon { name: IconName::Settings } "Theme" }
            SignOut {}
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Primary, availability: if (state.connected)() && state.gateway_busy.read().is_none() { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| dispatch(state, Command::CreateThread { title: "New conversation".into(), mode: Mode::Chat }), "+ New chat" }
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
            for item in [Route::Gateway {}, Route::Providers {}, Route::Messengers {}, Route::Joshua {}, Route::Prompts {}, Route::Security {}] {
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
