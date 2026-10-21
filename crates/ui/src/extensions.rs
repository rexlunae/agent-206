//! The Extensions page: grants are a user operation in trusted chrome, and
//! panels are typed descriptions rendered here, never extension markup.
use crate::{PageFrame, State, audit::AuditFindings, call, dispatch, error::ErrorNotice};
use a206_core::{
    Capability, Command, ErrorKind, ErrorReport, ExtensionProposal, MAX_EXTENSION_MANIFEST,
    MAX_EXTENSION_MODULE, PluginSource, PluginSummary, Reply,
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
    let proposals = state.data.read().extension_proposals.clone();
    rsx! { PageFrame { icon: IconName::Plus, kicker: "Workbench", title: "Extensions", description: "Install extensions, review their permissions and enable the tools you need.",
        Field { label: rsx! { "Project for extension panels" },
            NativeSelect { value: project(), onchange: { let mut project = project; move |v| project.set(v) },
                option { value: "", "No project" }
                for p in &state.data.read().projects { option { value: "{p.id}", "{p.name}" } }
            }
        }
        InstallExtension {}
        if !proposals.is_empty() {
            section { aria_label: "Prepared by agents",
                h2 { "Prepared by agents" }
                p { class: "a206-muted", "Review what each package asks for. Nothing here runs or has any permission; installing one adds it disabled, with nothing granted." }
                div { class: "a206-grid",
                    for proposal in proposals {
                        ProposalCard { key: "{proposal.digest}", proposal: proposal.clone() }
                    }
                }
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
        crate::skills::Skills {}
        crate::mcp::McpServers {}
    } }
}

/// Install a package the user picks: its `plugin.toml` and compiled module.
/// The gateway validates it again; it arrives disabled with nothing granted.
#[component]
fn InstallExtension() -> Element {
    let mut reading = use_signal(|| false);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut installed = use_signal(|| None::<String>);
    // A keyed fragment replaces the native input after each choice, so
    // choosing the same files again emits a fresh change event.
    let mut version = use_signal(|| 0_u64);
    rsx! { Card {
        CardHeader {
            CardTitle { "Install an extension" }
            CardSubtitle { "Choose its plugin.toml and compiled .wasm module together. It arrives disabled, with no permissions, until you review it below." }
        }
        CardBody {
            if let Some(report) = failure() { ErrorNotice { report, title: "Could not install the extension".to_string() } }
            if let Some(name) = installed() { p { class: "a206-muted", "Installed {name}. Review its permissions below before enabling it." } }
            for key in [version()] {
                input { key: "{key}", r#type: "file", multiple: true, accept: ".toml,.wasm", aria_label: "Choose an extension's plugin.toml and .wasm module", disabled: reading(),
                    onchange: move |event: FormEvent| {
                        let files = event.files();
                        if files.is_empty() { return; }
                        reading.set(true);
                        failure.set(None);
                        installed.set(None);
                        spawn(async move {
                            let result = async {
                                let command = package_command(&files).await?;
                                match call(command).await? {
                                    Reply::Extension(summary) => Ok(summary.name),
                                    _ => Err(ErrorReport::new(ErrorKind::Protocol, "The gateway answered with the wrong kind of result")),
                                }
                            }.await;
                            match result {
                                Ok(name) => installed.set(Some(name)),
                                Err(report) => failure.set(Some(report)),
                            }
                            reading.set(false);
                            version.set(version().wrapping_add(1));
                        });
                    }
                }
            }
        }
    } }
}

/// The install command for exactly one `.toml` manifest and one `.wasm` module.
async fn package_command(files: &[dioxus::html::FileData]) -> Result<Command, ErrorReport> {
    let rejected = |message: &str| ErrorReport::new(ErrorKind::InvalidRequest, message);
    let pick = |extension: &str| {
        let mut matching = files
            .iter()
            .filter(|f| f.name().to_ascii_lowercase().ends_with(extension));
        match (matching.next(), matching.next()) {
            (Some(file), None) => Some(file),
            _ => None,
        }
    };
    let (Some(manifest), Some(module), 2) = (pick(".toml"), pick(".wasm"), files.len()) else {
        return Err(rejected("Choose one plugin.toml and one .wasm module"));
    };
    let unreadable = |error: dioxus::CapturedError| {
        ErrorReport::new(ErrorKind::Other, "Could not read the chosen file")
            .caused_by(ErrorReport::capture(&**error, |_| None))
    };
    let manifest = crate::files::read_bounded(manifest, MAX_EXTENSION_MANIFEST)
        .await
        .map_err(unreadable)?
        .ok_or_else(|| rejected("The manifest is larger than 64 KiB"))?;
    let manifest = String::from_utf8(manifest).map_err(|error| {
        rejected("The manifest is not UTF-8 text").caused_by(ErrorReport::capture(&error, |_| None))
    })?;
    let module = crate::files::read_bounded(module, MAX_EXTENSION_MODULE)
        .await
        .map_err(unreadable)?
        .ok_or_else(|| rejected("The module is larger than 3 MiB"))?;
    Ok(Command::InstallExtension { manifest, module })
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
    let state = use_context::<State>();
    let mut confirm = use_signal(|| false);
    let id = plugin.id.clone();
    let enabled = plugin.enabled;
    rsx! { Card {
        CardHeader {
            CardTitle { "{plugin.name}" }
            CardSubtitle { "{plugin.description}" }
        }
        CardBody {
            div { class: "a206-row",
                PackageSource { source: plugin.source.clone() }
                if !plugin.version.is_empty() { Badge { tone: BadgeTone::Neutral, outline: true, "Version {plugin.version}" } }
            }
            p { class: "a206-muted", code { "{plugin.id}" } }
            ExtensionSwitch { extension: id.clone(), capability: None }
            if plugin.capabilities.is_empty() {
                p { class: "a206-muted", "This extension requests no permissions." }
            } else {
                p { class: "a206-eyebrow", "Permissions" }
                for capability in plugin.capabilities.iter().copied() {
                    ExtensionSwitch { key: "{capability:?}", extension: id.clone(), capability: Some(capability) }
                }
                if !plugin.hosts.is_empty() {
                    p { class: "a206-muted", "{network_destinations(&plugin.hosts)}" }
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
            // Bundled extensions ship with the application; packages from
            // anyone else get the audit beside the switches that enable them.
            if plugin.source != PluginSource::Bundled { AuditFindings { report: a206_audit::plugin(&plugin) } }
        }
        if confirm() {
            CardBody { Alert { title: rsx! { "Uninstall {plugin.name}?" }, "Its package and everything it stored are removed. Installing it again starts disabled, with nothing granted.",
                Button { variant: ButtonVariant::Danger, onclick: { let id = id.clone(); move |_| { confirm.set(false); dispatch(state, Command::UninstallExtension { extension: id.clone() }); } }, "Confirm uninstall" }
                Button { variant: ButtonVariant::Ghost, onclick: move |_| confirm.set(false), "Keep it" }
            } }
        }
        CardFooter {
            if matches!(plugin.source, PluginSource::Installed { .. }) {
                Button { variant: ButtonVariant::Ghost, onclick: move |_| confirm.set(true), "Uninstall" }
            }
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

/// A package an agent prepared. Its text is the package author's; the badges,
/// provenance and buttons are trusted chrome that a package cannot style.
#[component]
fn ProposalCard(proposal: ExtensionProposal) -> Element {
    let state = use_context::<State>();
    let summary = &proposal.summary;
    let (conversation, replaces) = {
        let data = state.data.read();
        let conversation = data
            .threads
            .iter()
            .find(|t| t.id == proposal.thread)
            .map(|t| t.title.clone());
        let replaces = data.plugins.iter().find(|p| p.id == summary.id).cloned();
        (conversation, replaces)
    };
    let digest = proposal.digest.clone();
    rsx! { Card { variant: CardVariant::Subtle,
        CardHeader {
            CardTitle { "{summary.name}" }
            CardSubtitle { "{summary.description}" }
        }
        CardBody {
            div { class: "a206-row",
                Prepared { digest: proposal.digest.clone() }
                if !summary.version.is_empty() { Badge { tone: BadgeTone::Neutral, outline: true, "Version {summary.version}" } }
            }
            p { class: "a206-muted", code { "{summary.id}" } }
            match conversation {
                Some(title) => rsx! { div { class: "a206-row", span { class: "a206-muted", "From the conversation" } LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: crate::Route::Chat { id: proposal.thread, split: None, message: None }.to_string(), "{title}" } } },
                None => rsx! { p { class: "a206-muted", "From a conversation that no longer exists." } },
            }
            if let Some(installed) = replaces {
                Alert { title: rsx! { "Replaces {installed.name} {installed.version}" }, "Installing this update disables the extension and removes every permission you granted it." }
            }
            if summary.capabilities.is_empty() {
                p { class: "a206-muted", "It requests no permissions." }
            } else {
                p { class: "a206-eyebrow", "Permissions it will ask for" }
                List { divided: true,
                    for capability in summary.capabilities.iter() {
                        ListItem { key: "{capability:?}", headline: rsx! { "{capability.label()}" } }
                    }
                }
                if !summary.hosts.is_empty() {
                    p { class: "a206-muted", "{network_destinations(&summary.hosts)}" }
                }
            }
            if !summary.tools.is_empty() {
                p { class: "a206-eyebrow", "Tools for agents" }
                List { divided: true,
                    for tool in &summary.tools {
                        ListItem { key: "{tool.name}", headline: rsx! { code { "{tool.name}" } }, supporting: rsx! { "{tool.description}" } }
                    }
                }
            }
            AuditFindings { report: a206_audit::plugin(summary) }
        }
        CardFooter {
            Button { variant: ButtonVariant::Primary, onclick: { let digest = digest.clone(); move |_| dispatch(state, Command::InstallProposal { digest: digest.clone() }) }, "Install" }
            Button { variant: ButtonVariant::Ghost, onclick: move |_| dispatch(state, Command::DismissProposal { digest: digest.clone() }), "Dismiss" }
        }
    } }
}

/// Marks a package as an agent's, with its digest, so it cannot pass for a
/// bundled or user-installed extension by its name.
#[component]
fn Prepared(digest: String) -> Element {
    rsx! {
        Badge { tone: BadgeTone::Warning, "Prepared by an agent" }
        Badge { tone: BadgeTone::Neutral, outline: true, title: "SHA-256 of the prepared package: {digest}", "Package {short(&digest)}" }
    }
}

/// The first characters of a package digest, enough to tell packages apart.
fn short(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

/// Where the package came from. An installed one is marked as the user's and
/// shows its digest, so it cannot pass for a bundled extension by its name.
#[component]
fn PackageSource(source: PluginSource) -> Element {
    match source {
        PluginSource::Bundled => rsx! { Badge { "Bundled WASM" } },
        PluginSource::Installed { digest } => rsx! {
            Badge { tone: BadgeTone::Warning, "Installed by you" }
            Badge { tone: BadgeTone::Neutral, outline: true, title: "SHA-256 of the installed package: {digest}", "Package {short(&digest)}" }
        },
    }
}

/// Where an extension's network permission may connect, as the user reads it.
fn network_destinations(hosts: &[String]) -> String {
    if hosts.iter().any(|host| host == "*") {
        "Network: any public HTTPS site; private and local addresses are always refused.".into()
    } else {
        format!("Network: only {}", hosts.join(", "))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: PluginSource) -> String {
        fn root(source: PluginSource) -> Element {
            rsx! { PackageSource { source } }
        }
        let mut dom = VirtualDom::new_with_props(root, source);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn installed_packages_are_marked_with_their_digest() {
        let digest = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let html = render(PluginSource::Installed {
            digest: digest.clone(),
        });
        assert!(html.contains("Installed by you"), "{html}");
        assert!(html.contains(&digest[..12]), "{html}");
        assert!(html.contains(&digest), "{html}");
        assert!(!html.contains("Bundled"), "{html}");
        let html = render(PluginSource::Bundled);
        assert!(!html.contains("Installed"), "{html}");
    }

    #[test]
    fn prepared_packages_are_marked_as_an_agents() {
        fn root(digest: String) -> Element {
            rsx! { Prepared { digest } }
        }
        let digest = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let mut dom = VirtualDom::new_with_props(root, digest.clone());
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Prepared by an agent"), "{html}");
        assert!(html.contains(&digest), "{html}");
        assert!(!html.contains("Installed by you"), "{html}");
    }
}
