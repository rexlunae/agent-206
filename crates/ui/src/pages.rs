pub use crate::extensions::Extensions;
use crate::health::HealthChecks;
use crate::{PageFrame, State, call, dispatch, error::ErrorNotice, route::Route};
use a206_core::{Command, DownloadState, ErrorReport, Mode, Reply};
use fc_dioxus::prelude::*;
use uuid::Uuid;
#[component]
pub fn Overview() -> Element {
    let state = use_context::<State>();
    let now = crate::messengers::use_catalog_clock(state)();
    let snapshot = state.data.read();
    rsx! { section { class: "a206-page", div { class: "a206-page-inner",
        Card { variant: CardVariant::Feature, as_: CardTag::Div, class: "a206-hero",
            div { class: "a206-hero-copy",
                h1 { "Workspace" }
                p { class: "a206-muted", "Conversations, projects, and local services." }
                div { class: "a206-row",
                    Button { variant: ButtonVariant::Primary, onclick: move |_| dispatch(state, Command::CreateThread { title: "New conversation".into(), mode: Mode::Chat }), "Start a conversation" }
                    LinkButton { variant: ButtonVariant::Secondary, href: Route::Projects {}.to_string(), "Open a project" }
                    LinkButton { variant: ButtonVariant::Secondary, href: Route::Gateway {}.to_string(), "Manage gateway" }
                }
            }
            LinkButton { variant: ButtonVariant::Ghost, href: Route::Running {}.to_string(),
                Badge { tone: BadgeTone::Accent, "{snapshot.threads.iter().filter(|t| t.state == a206_core::RunState::Running).count()} active responses" } }
        }
        if crate::welcome::unfinished(&snapshot, now) {
            Card { variant: CardVariant::Subtle, CardBody { div { class: "a206-row a206-between",
                div { strong { "Setup incomplete" } p { class: "a206-muted", "Open the setup checklist to see the remaining steps." } }
                LinkButton { variant: ButtonVariant::Primary, href: Route::Welcome {}.to_string(), "Get started" }
            } } }
        }
        div { class: "a206-grid",
            Metric { label: name!("Conversations"), title: rsx! { "Conversations" }, value: Some(snapshot.threads.len() as f64), display: snapshot.threads.len().to_string(), meta: rsx! {} }
            Metric { label: name!("Projects"), title: rsx! { "Projects" }, value: Some(snapshot.projects.len() as f64), display: snapshot.projects.len().to_string(), meta: rsx! { "Coordinator conversations" } }
            Metric { label: name!("Configured providers"), title: rsx! { "Providers" }, value: Some(snapshot.settings.providers.len() as f64), display: snapshot.settings.providers.len().to_string(), meta: rsx! { "Configured connections" } }
        }
        Card { variant: CardVariant::Elevated,
            CardHeader { CardTitle { "Recent conversations" } }
            CardBody {
                ConversationList { limit: 8 }
            }
            CardFooter { LinkButton { variant: ButtonVariant::Ghost, href: Route::Conversations {}.to_string(), "All conversations ({snapshot.threads.len()})" } }
        }
        h2 { "Shortcuts" }
        div { class: "a206-grid",
            for (title, body, target) in [
                ("Connections", "Configure providers and refresh available models.", Route::Providers {}),
                ("Projects", "Create a project or open its coordinator.", Route::Projects {}),
                ("Appearance", "Set the theme, color scheme and contrast.", Route::Appearance {}),
            ] {
                Card { variant: CardVariant::Elevated, CardHeader { CardTitle { "{title}" } }
                    CardBody { p { class: "a206-muted", "{body}" } }
                    CardFooter { LinkButton { variant: ButtonVariant::Ghost, href: target.to_string(), "Open {title}" } }
                }
            }
        }
        Card { variant: CardVariant::Subtle, CardBody { div { class: "a206-row a206-between", div { strong { "Tool permissions" } p { class: "a206-muted", "Agents begin without tool grants. Modes and delegated tasks can narrow access further." } } LinkButton { variant: ButtonVariant::Secondary, href: Route::Security {}.to_string(), "Security details" } } } }
    } } }
}
#[component]
pub fn Conversations() -> Element {
    let mut query = use_signal(String::new);
    rsx! { PageFrame { kicker: "Workspace", title: "Conversations", description: "Find and reopen any conversation, including project coordinators and delegated work.",
        Card {
            CardBody {
                Field { label: rsx! { "Search conversation titles" }, Input { value: ReadSignal::from(query), oninput: move |value| query.set(value), placeholder: "Search by title" } }
                ConversationList { query: query() }
            }
        }
        Card { CardHeader { CardTitle { "Search messages" } } CardBody { crate::search::MessageSearch {} } }
    } }
}
#[component]
fn ConversationList(
    #[props(default)] query: String,
    #[props(default)] limit: Option<usize>,
) -> Element {
    let state = use_context::<State>();
    let snapshot = state.data.read();
    let query = query.trim().to_lowercase();
    let threads: Vec<_> = snapshot
        .threads
        .iter()
        .rev()
        .filter(|thread| thread.title.to_lowercase().contains(&query))
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    rsx! { div { class: "a206-nav-group",
        if threads.is_empty() { p { class: "a206-muted", if snapshot.threads.is_empty() { "Start a conversation to begin." } else { "No conversations match this title." } } }
        for thread in threads {
            { let id = thread.id;
              let model = thread.model.as_ref().and_then(|key| snapshot.catalogs.iter().flat_map(|c| &c.models).find(|m| &m.key == key)).map(|m| m.name.as_str()).unwrap_or("Choose a model");
              rsx! { LinkButton { key: "{id}", class: "a206-recent-thread", size: ButtonSize::Sm, variant: ButtonVariant::Ghost,
                  href: Route::Chat { id, split: None, message: None }.to_string(),
                  Icon { name: IconName::Sparkles, size: IconSize::Sm }
                  span { class: "a206-nav-title", strong { "{thread.title}" } span { class: "a206-muted", "{model} · {thread.messages.len()} messages" } }
                  Badge { tone: BadgeTone::Accent, "{thread.mode.label()}" }
                  Badge { tone: if thread.state == a206_core::RunState::Failed { BadgeTone::Danger } else { BadgeTone::Neutral }, "{thread.state:?}" }
              } }
            }
        }
    } }
}
#[component]
pub fn Projects() -> Element {
    let state = use_context::<State>();
    let mut name = use_signal(String::new);
    let mut root = use_signal(String::new);
    rsx! { PageFrame { kicker: "Workspace", title: "Projects", description: "Create a project with a workspace directory and coordinator conversation.",
        Card { CardBody { div { class: "a206-grid",
            Field { label: rsx! { "Project name" }, Input { value: ReadSignal::from(name), oninput: move |v| name.set(v) } }
            Field { label: rsx! { "Workspace directory" }, Input { value: ReadSignal::from(root), oninput: move |v| root.set(v), placeholder: "Absolute path on the gateway host" } }
        } } CardFooter { Button { variant: ButtonVariant::Primary, onclick: move |_| dispatch(state, Command::CreateProject { name: name(), root: root() }), "Create project" } } }
        div { class: "a206-grid", for project in &state.data.read().projects {
            { let manager = project.manager; rsx! { Card {
                CardHeader { CardTitle { "{project.name}" } CardSubtitle { "{project.root}" } }
                CardFooter { LinkButton { variant: ButtonVariant::Secondary, href: Route::Chat { id: manager, split: None, message: None }.to_string(), "Open coordinator" } }
            } } }
        } }
    } }
}
#[component]
pub fn Downloads() -> Element {
    let state = use_context::<State>();
    let mut url = use_signal(String::new);
    let mut name = use_signal(String::new);
    let mut hash = use_signal(String::new);
    let mut models_directory = use_signal(|| false);
    rsx! { PageFrame { kicker: "Workbench", title: "Downloads", description: "Start, monitor or cancel downloads. Supply a SHA-256 hash to verify the file.",
        Card { CardBody { div { class: "a206-grid",
            Field { label: rsx! { "Download URL" }, Input { value: ReadSignal::from(url), oninput: move |v| url.set(v), r#type: InputType::Url } }
            Field { label: rsx! { "Filename" }, Input { value: ReadSignal::from(name), oninput: move |v| name.set(v) } }
            Field { label: rsx! { "Expected SHA-256 (optional)" }, Input { value: ReadSignal::from(hash), oninput: move |v| hash.set(v) } }
        } Checkbox { label: rsx! { "Save in ~/models for Joshua" }, onchange: move |v| models_directory.set(v) } } CardFooter { Button { variant: ButtonVariant::Primary, onclick: move |_| dispatch(state, Command::Download { url: url(), name: name(), sha256: (!hash().is_empty()).then_some(hash()), models_directory: models_directory() }), "Start download" } } }
        if state.data.read().downloads.is_empty() { EmptyState { compact: true, title: rsx! { "No downloads" }, body: rsx! { "Your downloads will appear here." } } }
        for download in &state.data.read().downloads {
            { let id = download.id; let running = matches!(download.state, DownloadState::Queued | DownloadState::Running);
              rsx! { Transfer { key: "{id}", label: name!("Download progress"), title: rsx! { "{download.name}" },
                  meta: rsx! { "{download.received} bytes received" },
                  progress: download.total.filter(|t| *t > 0).map(|total| ProgressValue::Of { value: download.received as f64, max: total as f64 }),
                  status: rsx! { Badge { "{download.state:?}" } },
                  actions: rsx! { if running { Button { variant: ButtonVariant::Ghost, onclick: move |_| dispatch(state, Command::CancelDownload { id }), "Cancel" } } },
                  if let Some(error) = download.failure_report() { ErrorNotice { report: error, title: "Download failed".to_string() } }
              } }
            }
        }
    } }
}
#[component]
pub fn Browser() -> Element {
    let mut address = use_signal(String::new);
    let mut source = use_signal(String::new);
    rsx! { PageFrame { kicker: "Workbench", title: "Browser preview", description: "Preview a development server in a sandboxed browser surface.",
        div { class: "a206-row", div { class: "a206-grow", Field { label: rsx! { "Preview URL" }, Input { value: ReadSignal::from(address), oninput: move |v| address.set(v), r#type: InputType::Url, placeholder: "Enter your development server URL" } } }
            Button { variant: ButtonVariant::Primary, onclick: move |_| source.set(address()), "Open preview" }
        }
        if !source().is_empty() { Webview { key: "{source}", class: "a206-browser", title: name!("Development browser"), src: source(), sandbox: "allow-scripts allow-forms" } }
        Alert { title: rsx! { "Isolated preview" }, "This preview has no vault credentials. Browser automation and approved credential injection require the isolated browser broker, which is not enabled in this build." }
    } }
}
#[component]
pub fn Documents() -> Element {
    let state = use_context::<State>();
    let mut project = use_signal(String::new);
    let mut path = use_signal(String::new);
    let mut output = use_signal(String::new);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let read = move |command| {
        spawn(async move {
            match call(command).await {
                Ok(Reply::Text(text)) => {
                    failure.set(None);
                    output.set(text);
                }
                Err(e) => {
                    output.set(String::new());
                    failure.set(Some(e));
                }
                _ => {}
            }
        });
    };
    rsx! { PageFrame { kicker: "Workbench", title: "Documents & Git", description: "Read a project file or inspect its Git status.",
        div { class: "a206-grid",
            Field { label: rsx! { "Project" }, NativeSelect { value: project(), onchange: move |v| project.set(v), option { value: "", "Select a project…" } for p in &state.data.read().projects { option { value: "{p.id}", "{p.name}" } } } }
            Field { label: rsx! { "Document path" }, Input { value: ReadSignal::from(path), oninput: move |v| path.set(v), placeholder: "Path relative to your project" } }
        }
        div { class: "a206-row",
            Button { variant: ButtonVariant::Primary, onclick: move |_| if let Ok(project) = Uuid::parse_str(&project()) { read(Command::ReadDocument { project, path: path() }); }, "Read document" }
            Button { variant: ButtonVariant::Secondary, onclick: move |_| if let Ok(project) = Uuid::parse_str(&project()) { read(Command::GitStatus { project }); }, "Git status" }
        }
        if let Some(report) = failure() { ErrorNotice { report } }
        Card { CardBody { if output().is_empty() { EmptyState { compact: true, title: rsx! { "No text to display" }, body: rsx! { "Select a project to read a document or inspect Git status. Empty files return no text." } } } else { pre { class: "a206-document", "{output}" } } } }
    } }
}
#[component]
pub fn Security() -> Element {
    rsx! { PageFrame { kicker: "Settings", title: "Security", description: "Manage vault authentication and gateway access.",
        div { class: "a206-grid",
            Card { CardHeader { CardTitle { "Tool access" } } CardBody { p { "New conversations have no tool permissions. Subagents cannot receive more access than their parent or mode allows. Extensions require explicit grants." } } }
            Card { CardHeader { CardTitle { "Encrypted vault" } } CardBody { p { "Credentials stay encrypted when locked. Unlock the vault and pair the gateway before using provider credentials." } } }
            Card { CardHeader { CardTitle { "Verified SSH" } } CardBody { p { "Remote connections use your OpenSSH configuration and verified host keys. Gateway setup shows an unknown host's fingerprint and trusts it only after you confirm it." } } }
        }
        VaultControls {}
        GatewayControls {}
        Alert { title: rsx! { "Credential management" }, "Use `agent206 vault` on the gateway host to add credentials, change access policies or remove two-factor authentication. Agent-requested credential use is unavailable." }
    } }
}
/// Native clients run vault setup against their broker; the web client reads
/// and locks the web server's vault through its bridge.
#[component]
pub(crate) fn VaultControls() -> Element {
    #[cfg(not(target_arch = "wasm32"))]
    {
        rsx! { crate::vault::VaultSection {} }
    }
    #[cfg(target_arch = "wasm32")]
    {
        rsx! { crate::web_vault::WebVault {} }
    }
}
#[component]
pub fn Joshua() -> Element {
    let mut state = use_context::<State>();
    let mut label = use_signal(String::new);
    let mut model = use_signal(String::new);
    let mut port = use_signal(String::new);
    let mut remote = use_signal(String::new);
    let mut service_status =
        use_signal(|| "Check status to inspect the managed process.".to_owned());
    rsx! { PageFrame { kicker: "Services", title: "Joshua", description: "Run a model from ~/models locally or over SSH.",
        Card { CardHeader { CardTitle { "Managed inference" } } CardBody { div { class: "a206-grid",
            Field { label: rsx! { "Connection name" }, Input { value: ReadSignal::from(label), oninput: move |v| label.set(v) } }
            Field { label: rsx! { "Model filename in ~/models" }, Input { value: ReadSignal::from(model), oninput: move |v| model.set(v) } }
            Field { label: rsx! { "API port" }, Input { value: ReadSignal::from(port), oninput: move |v| port.set(v), r#type: InputType::Number } }
            Field { label: rsx! { "SSH endpoint (optional)" }, Input { value: ReadSignal::from(remote), oninput: move |v| remote.set(v), placeholder: "ssh://user@host" } }
        } p { class: "a206-muted", "{service_status}" } }
        CardFooter {
            Button { variant: ButtonVariant::Primary, onclick: move |_| { if let Ok(port) = port().parse() { dispatch(state, Command::StartJoshua { label: label(), model_file: model(), port, remote: (!remote().is_empty()).then_some(remote()) }); } else { state.status.set("Enter a valid API port".into()); } }, "Start Joshua" }
            Button { variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::StopJoshua), "Stop managed process" }
            Button { variant: ButtonVariant::Ghost, onclick: move |_| { spawn(async move { if let Ok(Reply::Text(text)) = call(Command::JoshuaStatus).await { service_status.set(text); } }); }, "Check status" }
        } }
        div { class: "a206-row",
            LinkButton { variant: ButtonVariant::Secondary, href: Route::Downloads {}.to_string(), "Download a model" }
            LinkButton { variant: ButtonVariant::Secondary, href: Route::Providers {}.to_string(), "Discover models" }
        }
        Alert { title: rsx! { "Service ownership" }, "Start requires an installed joshua executable. Stop affects only the process started here. To switch models, stop it and start the selected file. Remote release installation and updating are not implemented yet." }
    } }
}

#[component]
pub fn Gateway() -> Element {
    let state = use_context::<State>();
    let report = state.gateway.read();
    let info = report.as_ref().and_then(|report| report.info.as_ref());
    rsx! { PageFrame { kicker: "Services", title: "Gateway", description: "Manage the gateway shared by your clients. Starting a process and completing its security setup are separate steps.",
        GatewayControls {}
        Card { variant: CardVariant::Elevated,
            CardHeader { CardTitle { "Connection details" } }
            CardBody {
                p { class: "a206-muted", {report.as_ref().map(|report| report.target.clone()).unwrap_or_else(|| "Checking configured connection…".into())} }
                if let Some(info) = info {
                    dl { class: "a206-fact-grid",
                        dt { "Process" } dd { "{info.pid}" }
                        dt { "Version / protocol" } dd { "{info.version} / {info.protocol}" }
                        dt { "Uptime" } dd { "{info.uptime_seconds} seconds" }
                        dt { "Data directory" } dd { "{info.data_dir}" }
                        dt { "Active work" } dd { "{info.active_runs} agent runs · {info.active_downloads} downloads" }
                        dt { "Accepting work" } dd { match info.accepting_work { Some(true) => "Yes", Some(false) => "No · shutdown cleanup", None => "Not reported" } }
                        dt { "Vault pairing" } dd { match info.vault_paired { Some(true) => "Holds a pairing (void once the vault locks)", Some(false) => "Not paired", None => "Not reported" } }
                    }
                }
                if let Some(error) = report.as_ref().and_then(|report| report.failure_report()) {
                    ErrorNotice { report: error, title: "Connection needs attention".to_string() }
                }
            }
            CardFooter { Button { variant: ButtonVariant::Secondary, onclick: move |_| { let mut setup = state.setup; setup.set(true); }, "Change gateway" } }
        }
        HealthChecks {}
        Card { variant: CardVariant::Subtle, CardHeader { CardTitle { "Security setup" } }
            CardBody { p { "The separate vault broker holds API keys and performs authenticated provider requests, so the gateway never sees them. Create, unlock and pair it, and enroll two-factor authentication, from the Security page of the desktop or terminal app or with `agent206 vault`; add credentials with `agent206 vault add`. A running gateway is not proof that the vault is set up." } }
            CardFooter { LinkButton { href: Route::Security {}.to_string(), variant: ButtonVariant::Secondary, "Open vault and security" } }
        }
        Alert { title: rsx! { "Shared service" }, "Stop and restart interrupt work for every connected client. Owned agent runs, downloads and the local Joshua child are cleaned up before stop is acknowledged. Start is always an explicit action after a stop; reconnect only attaches to an existing gateway." }
    } }
}
#[component]
fn GatewayControls() -> Element {
    use a206_core::{GatewayAction, GatewayPhase};
    let state = use_context::<State>();
    let busy = (state.gateway_busy)();
    let running = state
        .gateway
        .read()
        .as_ref()
        .is_some_and(|report| report.phase == GatewayPhase::Running);
    let mut confirm = use_signal(|| None::<GatewayAction>);
    let stopped = state
        .gateway
        .read()
        .as_ref()
        .is_some_and(|report| report.phase == GatewayPhase::Stopped);
    let label = match state.gateway.read().as_ref().map(|report| report.phase) {
        Some(GatewayPhase::Running) => "Running",
        Some(GatewayPhase::Stopped) => "Stopped",
        Some(GatewayPhase::Offline) => "Offline",
        None => "Checking connection",
    };
    let availability = if busy.is_none() {
        Availability::Enabled
    } else {
        Availability::Disabled
    };
    rsx! { Card { variant: CardVariant::Feature,
        CardHeader { CardTitle { "Gateway controls" }
            Badge { tone: if running { BadgeTone::Success } else { BadgeTone::Warning }, dot: true,
                if let Some(action) = busy { "{action:?} in progress" } else { "{label}" }
            }
        }
        CardBody { p { class: "a206-gateway-copy", if running { "Connected to the configured gateway." } else { "Start the gateway, or reconnect if another client has started it." } } }
        CardFooter {
            Button { variant: ButtonVariant::Primary, availability: if !running { availability.clone() } else { Availability::Disabled }, busy: busy == Some(GatewayAction::Start),
                onclick: move |_| crate::control_gateway(state, GatewayAction::Start), "Start gateway" }
            Button { variant: ButtonVariant::Secondary, availability: if !stopped { availability.clone() } else { Availability::Disabled },
                onclick: move |_| confirm.set(Some(GatewayAction::Stop)), "Stop gateway" }
            Button { variant: ButtonVariant::Secondary, availability: if !stopped { availability.clone() } else { Availability::Disabled },
                onclick: move |_| confirm.set(Some(GatewayAction::Restart)), "Restart gateway" }
            Button { variant: ButtonVariant::Secondary, availability,
                onclick: move |_| crate::control_gateway(state, GatewayAction::Reconnect), "Reconnect" }
        }
        if let Some(action) = confirm() {
            CardBody { Alert { title: rsx! { "Interrupt shared work?" }, "This affects all clients, active agent runs, downloads and managed local inference.",
                Button { variant: ButtonVariant::Danger, onclick: move |_| { confirm.set(None); crate::control_gateway(state, action); }, if action == GatewayAction::Stop { "Confirm stop" } else { "Confirm restart" } }
                Button { variant: ButtonVariant::Ghost, onclick: move |_| confirm.set(None), "Keep running" }
            } }
        }
    } }
}
