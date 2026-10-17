//! MCP servers: user-configured tool servers the gateway runs in isolation.
use crate::{State, dispatch, error::ErrorNotice};
use a206_core::{
    Capability, Command, McpServerConfig, McpServerState, McpServerStatus, ToolPolicy,
};
use fc_dioxus::prelude::*;

/// Authority an MCP server entry may hold, with the words shown for it.
const AUTHORITY: [(Capability, &str); 4] = [
    (Capability::Network, "Network access"),
    (Capability::ReadWorkspace, "Read its project"),
    (Capability::WriteWorkspace, "Change its project"),
    (Capability::Execute, "Run system commands"),
];

#[component]
pub fn McpServers() -> Element {
    let state = use_context::<State>();
    let snapshot = state.data.read();
    rsx! {
        h2 { "MCP servers" }
        p { class: "a206-muted", "Tool servers you install yourself. Each runs isolated with only the authority you give it, and a conversation can use it only when its own permissions include all of that authority." }
        for server in &snapshot.settings.mcp_servers {
            McpServerCard { key: "{server.name}", config: server.clone(), status: snapshot.mcp_servers.iter().find(|s| s.name == server.name).cloned().unwrap_or_default() }
        }
        AddMcpServer {}
    }
}

/// Save settings with the server entries changed by `edit`.
fn save_servers(state: State, edit: impl FnOnce(&mut Vec<McpServerConfig>)) {
    let mut settings = state.data.read().settings.clone();
    edit(&mut settings.mcp_servers);
    dispatch(state, Command::SaveSettings(settings));
}

#[component]
fn McpServerCard(config: McpServerConfig, status: McpServerStatus) -> Element {
    let state = use_context::<State>();
    let name = config.name.clone();
    // Reactive on the prop, so a change saved elsewhere reaches this control.
    let enabled = config.enabled;
    let enabled = use_memo(use_reactive!(|enabled| if enabled {
        CheckState::On
    } else {
        CheckState::Off
    }));
    let authority = AUTHORITY
        .iter()
        .filter(|(c, _)| config.capabilities.contains(c))
        .map(|(_, label)| *label)
        .collect::<Vec<_>>();
    let running = matches!(
        status.state,
        McpServerState::Connected | McpServerState::Connecting
    );
    rsx! { Card {
        CardHeader { CardTitle { "{config.name}" } CardSubtitle { "{config.executable}" } }
        CardBody {
            div { class: "a206-row",
                Badge { "{status.state.label()}" }
                if let Some(server) = &status.server { span { class: "a206-muted", "Reports itself as {server}" } }
            }
            p { class: "a206-muted", if authority.is_empty() { "No network, project or command access." } else { {format!("Authority: {}", authority.join(", "))} } }
            Checkbox { label: rsx! { "Enabled" }, checked: ReadSignal::from(enabled), onchange: { let name = name.clone(); move |on: bool| save_servers(state, |servers| { for s in servers.iter_mut().filter(|s| s.name == name) { s.enabled = on; } }) } }
            if let Some(report) = status.failure.clone() { ErrorNotice { report, title: "MCP server failed".to_string() } }
            if status.tools.is_empty() {
                p { class: "a206-muted", if status.state == McpServerState::Connected { "This server offers no tools." } else { "Connect the server to see its tools." } }
            }
            for tool in &status.tools {
                ToolRow { key: "{tool.name}", server: config.name.clone(), tool: tool.name.clone(), description: tool.description.clone().unwrap_or_default(), policy: config.policy(&tool.name) }
            }
        }
        CardFooter {
            if running {
                Button { variant: ButtonVariant::Secondary, onclick: { let name = name.clone(); move |_| dispatch(state, Command::DisconnectMcpServer { name: name.clone() }) }, "Disconnect" }
            } else {
                Button { variant: ButtonVariant::Secondary, availability: if config.enabled { Availability::Enabled } else { Availability::Disabled }, onclick: { let name = name.clone(); move |_| dispatch(state, Command::ConnectMcpServer { name: name.clone() }) }, "Connect" }
            }
            Button { variant: ButtonVariant::Ghost, onclick: { let name = name.clone(); move |_| save_servers(state, |servers| servers.retain(|s| s.name != name)) }, "Remove server" }
        }
    } }
}

#[component]
fn ToolRow(server: String, tool: String, description: String, policy: ToolPolicy) -> Element {
    let state = use_context::<State>();
    let value = use_memo(use_reactive!(|policy| Some(policy.label().to_owned())));
    rsx! { div { class: "a206-row a206-between",
        div { strong { "{tool}" } if !description.is_empty() { p { class: "a206-muted", "{description}" } } }
        Select { label: name!("Policy for this tool"), value: ReadSignal::from(value),
            options: ToolPolicy::ALL.into_iter().map(|p| SelectChoice::new(p.label(), p.label())).collect(),
            onchange: move |label: String| {
                let Some(policy) = ToolPolicy::ALL.into_iter().find(|p| p.label() == label) else { return };
                let tool = tool.clone();
                save_servers(state, |servers| { for s in servers.iter_mut().filter(|s| s.name == server) { s.tools.insert(tool.clone(), policy); } });
            }
        }
    } }
}

#[component]
fn AddMcpServer() -> Element {
    let state = use_context::<State>();
    let mut name = use_signal(String::new);
    let mut executable = use_signal(String::new);
    let mut root = use_signal(String::new);
    let mut args = use_signal(String::new);
    let mut project = use_signal(|| None::<String>);
    let mut authority = use_signal(std::collections::BTreeSet::<Capability>::new);
    let projects = state.data.read().projects.clone();
    let workspace = authority.read().contains(&Capability::ReadWorkspace);
    let ready = !name().trim().is_empty()
        && !executable().trim().is_empty()
        && !root().trim().is_empty()
        && (!workspace || project().is_some());
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Add an MCP server" } CardSubtitle { "A local program that speaks MCP over stdio. It stays off until you enable it." } }
        CardBody { div { class: "a206-grid",
            Field { label: rsx! { "Name" }, hint: rsx! { "Lowercase letters, digits and hyphens." }, Input { value: ReadSignal::from(name), oninput: move |v| name.set(v), placeholder: "files" } }
            Field { label: rsx! { "Server executable" }, Input { value: ReadSignal::from(executable), oninput: move |v| executable.set(v), placeholder: "Absolute executable path" } }
            Field { label: rsx! { "Installation directory" }, hint: rsx! { "Contains the executable and everything it loads. Not your home, project or private state directory." }, Input { value: ReadSignal::from(root), oninput: move |v| root.set(v), placeholder: "Absolute installation directory" } }
            Field { label: rsx! { "Arguments" }, hint: rsx! { "One per line. Never put credentials here." }, Textarea { value: ReadSignal::from(args), oninput: move |v| args.set(v), rows: "2" } }
        }
        for (capability, label) in AUTHORITY {
            Checkbox { key: "{label}", label: rsx! { "{label}" }, onchange: move |on: bool| { let mut set = authority.write(); if on { set.insert(capability); if capability == Capability::WriteWorkspace { set.insert(Capability::ReadWorkspace); } } else { set.remove(&capability); if capability == Capability::ReadWorkspace { set.remove(&Capability::WriteWorkspace); } } } }
        }
        if workspace {
            Field { label: rsx! { "Project" }, Select { label: name!("Project this server may use"), value: ReadSignal::from(project), placeholder: "Choose a project",
                options: projects.iter().map(|p| SelectChoice::new(p.id.to_string(), p.name.clone())).collect(),
                onchange: move |id: String| project.set(Some(id))
            } }
        }
        }
        CardFooter { Button { variant: ButtonVariant::Primary, availability: if ready { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| {
            let capabilities = authority();
            let entry = McpServerConfig {
                name: name().trim().to_owned(),
                enabled: false,
                executable: executable().trim().to_owned(),
                runtime_root: root().trim().to_owned(),
                args: args().lines().map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned).collect(),
                env: Default::default(),
                project: capabilities.contains(&Capability::ReadWorkspace).then(|| project().and_then(|id| id.parse().ok())).flatten(),
                capabilities,
                default_policy: ToolPolicy::Ask,
                tools: Default::default(),
                timeout_seconds: None,
            };
            save_servers(state, |servers| servers.push(entry));
        }, "Add server" } }
    } }
}
