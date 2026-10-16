//! The Extensions page: grants are a user operation in trusted chrome, and
//! panels are typed descriptions rendered here, never extension markup.
use crate::{PageFrame, State, call, dispatch, error::ErrorNotice};
use a206_core::{
    Capability, Command, ErrorReport, PluginSummary, Reply,
    extension::{Node, Tone, UiEvent, View},
};
use fc_dioxus::prelude::*;
use uuid::Uuid;

/// The panel being shown, and what reopens it after a button press.
#[derive(Clone, PartialEq)]
struct Open {
    extension: String,
    panel: String,
    title: String,
}

#[component]
pub fn Extensions() -> Element {
    let state = use_context::<State>();
    let project = use_signal(String::new);
    let open = use_signal(|| None::<Open>);
    let view = use_signal(|| None::<View>);
    let failure = use_signal(|| None::<ErrorReport>);
    // Each request bumps the generation; only the latest reply is shown, so a
    // slow earlier reply cannot replace a newer panel.
    let generation = use_signal(|| 0_u64);
    let show = move |target: Open, event: Option<UiEvent>| {
        let (mut open, mut view, mut failure, mut generation) = (open, view, failure, generation);
        let project = Uuid::parse_str(&project()).ok();
        let request = generation() + 1;
        generation.set(request);
        // Clear the old panel so its buttons cannot be pressed for the wrong one.
        view.set(None);
        failure.set(None);
        open.set(Some(target.clone()));
        spawn(async move {
            let command = Command::OpenPanel {
                extension: target.extension,
                panel: target.panel,
                project,
                event,
            };
            let reply = call(command).await;
            if generation() != request {
                return;
            }
            match reply {
                Ok(Reply::Panel(panel)) => view.set(Some(panel)),
                Ok(_) => {}
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    let plugins = state.data.read().plugins.clone();
    rsx! { PageFrame { kicker: "Extend your workspace", title: "Small tools. New possibilities.", description: "Compiled WASM extensions run in a separate, bounded process and use only what you grant.",
        Field { label: rsx! { "Project for extension panels" },
            NativeSelect { value: project(), onchange: { let mut project = project; move |v| project.set(v) },
                option { value: "", "No project" }
                for p in &state.data.read().projects { option { value: "{p.id}", "{p.name}" } }
            }
        }
        div { class: "a206-grid",
            for plugin in plugins {
                ExtensionCard { key: "{plugin.id}", plugin: plugin.clone(), onopen: move |target| show(target, None) }
            }
        }
        if let Some(target) = open() {
            Card { variant: CardVariant::Elevated,
                CardHeader { CardTitle { "{target.title}" } }
                CardBody {
                    if let Some(report) = failure() { ErrorNotice { report, title: "Extension failed".to_string() } }
                    if let Some(panel) = view() {
                        Nodes { nodes: panel.nodes, onpress: move |element| show(target.clone(), Some(UiEvent { element })) }
                    }
                }
            }
        }
    } }
}

/// The enable switch (`None`) or one capability's grant switch. Both are
/// user operations; the gateway rejects a grant the manifest did not request.
#[component]
fn ExtensionSwitch(extension: String, capability: Option<Capability>) -> Element {
    let state = use_context::<State>();
    let id = extension.clone();
    let checked = use_memo(move || {
        state
            .data
            .read()
            .plugins
            .iter()
            .find(|p| p.id == id)
            .is_some_and(|p| match capability {
                None => p.enabled,
                Some(capability) => p.granted.contains(&capability),
            })
    });
    let onchange = move |on: bool| {
        let command = match capability {
            None => Command::EnableExtension {
                extension: extension.clone(),
                enabled: on,
            },
            Some(capability) => {
                let Some(mut capabilities) = state
                    .data
                    .read()
                    .plugins
                    .iter()
                    .find(|p| p.id == extension)
                    .map(|p| p.granted.clone())
                else {
                    return;
                };
                if on {
                    capabilities.insert(capability);
                } else {
                    capabilities.remove(&capability);
                }
                Command::GrantExtension {
                    extension: extension.clone(),
                    capabilities,
                }
            }
        };
        dispatch(state, command);
    };
    match capability {
        None => {
            rsx! { SwitchRow { label: rsx! { "Enabled" }, checked: ReadSignal::from(checked), onchange } }
        }
        Some(capability) => rsx! { SwitchRow { label: rsx! { "{capability.label()}" },
        hint: rsx! { "Requested by the extension; off until you allow it." }, checked: ReadSignal::from(checked), onchange } },
    }
}

#[component]
fn ExtensionCard(plugin: PluginSummary, onopen: EventHandler<Open>) -> Element {
    let id = plugin.id.clone();
    let enabled = plugin.enabled;
    rsx! { Card {
        CardHeader {
            CardTitle { "{plugin.name}" }
            CardSubtitle { "{plugin.description}" }
        }
        CardBody {
            div { class: "a206-row",
                Badge { "Bundled WASM" }
                if !plugin.version.is_empty() { Badge { tone: BadgeTone::Neutral, outline: true, "Version {plugin.version}" } }
            }
            ExtensionSwitch { extension: id.clone(), capability: None }
            if plugin.capabilities.is_empty() {
                p { class: "a206-muted", "This extension requests no permissions." }
            } else {
                p { class: "a206-eyebrow", "Permissions" }
                for capability in plugin.capabilities.iter().copied() {
                    ExtensionSwitch { key: "{capability:?}", extension: id.clone(), capability: Some(capability) }
                }
            }
            if !plugin.tools.is_empty() {
                p { class: "a206-eyebrow", "Tools for agents" }
                List { divided: true,
                    for tool in &plugin.tools {
                        ListItem { key: "{tool.name}", headline: rsx! { code { "{tool.name}" } }, supporting: rsx! { "{tool.description}" } }
                    }
                }
            }
        }
        CardFooter {
            for panel in plugin.panels.clone() {
                {
                    let label = format!("Open {}", panel.title);
                    let target = Open { extension: id.clone(), panel: panel.id, title: panel.title };
                    rsx! { Button { key: "{target.panel}", variant: ButtonVariant::Secondary, availability: if enabled { Availability::Enabled } else { Availability::Disabled },
                        onclick: move |_| onopen.call(target.clone()), "{label}" } }
                }
            }
        }
    } }
}

/// Render panel nodes with fc-dioxus components. Text is always text.
#[component]
fn Nodes(nodes: Vec<Node>, onpress: EventHandler<String>) -> Element {
    rsx! { for (index, node) in nodes.into_iter().enumerate() {
        match node {
            Node::Heading { text } => rsx! { h3 { key: "{index}", "{text}" } },
            Node::Text { text } => rsx! { p { key: "{index}", "{text}" } },
            Node::Code { text } => rsx! { CodeBlock { key: "{index}", source: CodeSource::Plain(text) } },
            Node::Badge { text, tone } => rsx! { Badge { key: "{index}", tone: badge_tone(tone), "{text}" } },
            Node::Fields { rows } => rsx! { DescriptionList { key: "{index}",
                for (row, field) in rows.into_iter().enumerate() {
                    DescriptionItem { key: "{row}", term: rsx! { "{field.label}" }, "{field.value}" }
                }
            } },
            Node::List { items } => rsx! { List { key: "{index}", divided: true,
                for (row, item) in items.into_iter().enumerate() {
                    ListItem { key: "{row}", headline: rsx! { "{item}" } }
                }
            } },
            Node::Button { id, label } => rsx! { Button { key: "{index}", variant: ButtonVariant::Secondary, onclick: move |_| onpress.call(id.clone()), "{label}" } },
            Node::Section { title, nodes } => rsx! { section { key: "{index}", class: "a206-extension-section",
                if let Some(title) = title { h4 { "{title}" } }
                Nodes { nodes, onpress }
            } },
        }
    } }
}

fn badge_tone(tone: Tone) -> BadgeTone {
    match tone {
        Tone::Neutral => BadgeTone::Neutral,
        Tone::Accent => BadgeTone::Accent,
        Tone::Success => BadgeTone::Success,
        Tone::Warning => BadgeTone::Warning,
        Tone::Danger => BadgeTone::Danger,
    }
}
