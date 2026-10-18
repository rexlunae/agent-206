use crate::{PageFrame, State, dispatch};
use a206_core::{
    BackendConfig, CodingBackend, Command, Mode, ProviderApi, ProviderConfig, ProviderPreset,
};
use fc_dioxus::prelude::*;
use uuid::Uuid;
#[component]
pub fn Providers() -> Element {
    let state = use_context::<State>();
    let mut label = use_signal(String::new);
    let mut endpoint = use_signal(String::new);
    let mut preset = use_signal(|| None::<String>);
    let mut api = use_signal(|| ProviderApi::OpenAi.label().to_owned());
    let api_value = use_memo(move || Some(api()));
    let mut credential = use_signal(String::new);
    let presets = state.data.read().settings.provider_presets.effective();
    let selected = presets
        .iter()
        .find(|p| Some(&p.id) == preset.read().as_ref());
    let snapshot = state.data.read();
    rsx! { PageFrame { kicker: "Connections", title: "Intelligence, on your terms.", description: "Connect an API endpoint. Its available models and reported limits are discovered live.",
        Card { CardHeader { CardTitle { "Add a provider" } CardSubtitle { "Choose a connector. Models and account facts come from its API." } }
            CardBody { div { class: "a206-grid",
                Field { label: rsx! { "Provider" }, Select { label: name!("Provider connector"), value: ReadSignal::from(preset), placeholder: "Choose provider",
                    options: presets.iter().map(|p| SelectChoice::new(p.id.clone(), p.name.clone()).in_group(p.group.clone())).collect(),
                    onchange: move |value: String| { if let Some(p) = state.data.read().settings.provider_presets.effective().into_iter().find(|p| p.id == value) { preset.set(Some(value)); label.set(p.name); endpoint.set(p.base_url); api.set(p.api.label().into()); credential.set(String::new()); } }
                } }
                Field { label: rsx! { "Connection name" }, Input { value: ReadSignal::from(label), oninput: move |v| label.set(v), placeholder: "A name you recognize" } }
                Field { label: rsx! { "API base URL" }, hint: rsx! { "Include the API path, usually /v1. HTTPS or loopback HTTP." }, Input { r#type: InputType::Url, value: ReadSignal::from(endpoint), oninput: move |v| endpoint.set(v), placeholder: "Your API endpoint" } }
                Field { label: rsx! { "Protocol" }, Select { label: name!("Provider API protocol"), value: ReadSignal::from(api_value),
                    options: ProviderApi::ALL.into_iter().map(|a| SelectChoice::new(a.label(), a.label())).collect(), onchange: move |value| api.set(value)
                } }
                Field { label: rsx! { "Vault credential name" }, hint: rsx! { "A reference only. Never paste an API key here." }, Input { value: ReadSignal::from(credential), oninput: move |v| credential.set(v), placeholder: "Optional saved credential name" } }
            } }
            CardBody { p { class: "a206-muted", {selected.map(|p| p.setup.clone()).unwrap_or_else(|| "Select a provider, or enter a custom connection below. No accounts are activated by the preset library.".into())} } }
            CardFooter { Button { variant: ButtonVariant::Primary, availability: if label().trim().is_empty() || endpoint().trim().is_empty() { Availability::Disabled } else { Availability::Enabled }, onclick: move |_| {
                let mut settings = state.data.read().settings.clone();
                settings.providers.push(ProviderConfig { id: Uuid::new_v4(), label: label().trim().to_owned(), base_url: endpoint().trim().to_owned(), credential: (!credential().trim().is_empty()).then(|| credential().trim().to_owned()), api: ProviderApi::ALL.into_iter().find(|a| a.label() == api()).unwrap_or_default() });
                dispatch(state, Command::SaveSettings(Box::new(settings)));
            }, "Save connection" } }
        }
        for provider in &snapshot.settings.providers {
            { let provider_id = provider.id; let catalog = snapshot.catalogs.iter().find(|c| c.provider == provider_id);
              rsx! { Card { key: "{provider_id}",
                CardHeader { CardTitle { "{provider.label}" } CardSubtitle { "{provider.base_url}" } }
                CardBody {
                    div { class: "a206-row", Badge { "{provider.api.label()}" } if provider.credential.is_some() { Badge { "Vault broker required" } } }
                    if let Some(catalog) = catalog {
                        div { class: "a206-row", Badge { "{catalog.models.len()} models" } span { class: "a206-muted", "Last refreshed: {catalog.fetched_at} · expires: {catalog.expires_at}" } }
                        for model in &catalog.models {
                            div { class: "a206-row a206-between", span { "{model.name}" } span { class: "a206-number", {model.context_limit.map(|v| format!("{v} context")).unwrap_or_else(|| "Context not reported".into())} } }
                            p { class: "a206-number", {format!("Input: {} · Output: {}", model.input_cost_per_token.map(|v| format!("{v} / token")).unwrap_or_else(|| "not reported".into()), model.output_cost_per_token.map(|v| format!("{v} / token")).unwrap_or_else(|| "not reported".into()))} }
                        }
                        p { class: "a206-muted", "Account balance: {catalog.balance.map(|v| v.to_string()).unwrap_or_else(|| \"not reported by this API\".into())}" }
                    } else { p { class: "a206-muted", "No catalog fetched yet." } }
                }
                CardFooter {
                    Button { variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::RefreshCatalog { provider: provider_id }), "Refresh models" }
                    Button { variant: ButtonVariant::Ghost, onclick: move |_| { let mut settings = state.data.read().settings.clone(); settings.providers.retain(|p| p.id != provider_id); dispatch(state, Command::SaveSettings(Box::new(settings))); }, "Remove connection" }
                }
              } }
            }
        }
        PresetLibrary {}
        h2 { "Coding-agent backends" }
        p { class: "a206-muted", "Optional agent engines, independent of your model connections. Runtime files stay outside private application state." }
        for kind in [CodingBackend::Codex, CodingBackend::ClaudeAgent] {
            BackendEditor { key: "{kind.label()}", kind, initial: snapshot.settings.backends.iter().find(|b| b.kind == kind).cloned() }
        }
        Alert { title: rsx! { "Credentialed providers" }, "This preview connects to endpoints that do not require API credentials. Vault-backed provider authentication is not enabled yet." }
    } }
}
#[component]
pub fn Appearance() -> Element {
    let mut state = use_context::<State>();
    rsx! { PageFrame { kicker: "Make it yours", title: "A workspace that feels right.", description: "Choose a future-css design, palette, color scheme and contrast. Your choice applies across the workspace.",
        ThemePicker { value: ReadSignal::from(state.theme), onchange: move |choice: ThemeChoice| {
            state.theme.set(choice);
            let mut settings = state.data.read().settings.clone();
            settings.theme_design = choice.design.as_str().into(); settings.theme_palette = Some(choice.palette.as_str().into());
            settings.theme_scheme = choice.scheme.as_str().into(); settings.theme_contrast = choice.contrast.as_str().into();
            dispatch(state, Command::SaveSettings(Box::new(settings)));
        } }
    } }
}
#[component]
pub fn Prompts() -> Element {
    let state = use_context::<State>();
    rsx! { PageFrame { kicker: "Agent behavior", title: "The right mode for the work.", description: "Prompts guide behavior. Tool permissions are enforced separately at the gateway.",
        for mode in Mode::ALL { PromptEditor { key: "{mode.label()}", mode, initial: state.data.read().settings.prompts.iter().find(|p| p.mode == mode).map(|p| p.prompt.clone()).unwrap_or_default() } }
        crate::profiles::AgentProfilesCard {}
        ProgramDirectories {}
    } }
}
/// The directories, beyond the system's, whose programs sandboxed commands
/// may run. The gateway checks and canonicalizes each one when saved.
#[component]
fn ProgramDirectories() -> Element {
    let state = use_context::<State>();
    let mut entry = use_signal(String::new);
    let directories = state.data.read().settings.program_directories.clone();
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Command programs" } CardSubtitle { "Directories whose programs sandboxed commands may run, read-only, besides /usr/bin and /bin" } }
        CardBody {
            if directories.is_empty() {
                p { class: "a206-muted", "None. Add a toolchain or package manager prefix, such as a Homebrew directory, to make its programs available. Its bin directory goes on the command's PATH." }
            }
            for directory in directories {
                div { key: "{directory.display()}", class: "a206-row",
                    code { "{directory.display()}" }
                    Button { variant: ButtonVariant::Ghost, onclick: { let directory = directory.clone(); move |_| { let mut settings = state.data.read().settings.clone(); settings.program_directories.retain(|d| *d != directory); dispatch(state, Command::SaveSettings(Box::new(settings))); } }, "Remove" }
                }
            }
            Field { label: rsx! { "Directory" }, hint: rsx! { "An absolute path. Not your home, a writable project or private state; programs there can read what commands are granted." }, Input { value: ReadSignal::from(entry), oninput: move |v| entry.set(v), placeholder: "Absolute directory path" } }
        }
        CardFooter { Button { variant: ButtonVariant::Secondary, disabled: entry.read().trim().is_empty(), onclick: move |_| { let mut settings = state.data.read().settings.clone(); settings.program_directories.push(entry().trim().into()); dispatch(state, Command::SaveSettings(Box::new(settings))); }, "Add directory" } }
    } }
}
#[component]
fn PromptEditor(mode: Mode, initial: String) -> Element {
    let state = use_context::<State>();
    let mut prompt = use_signal(|| initial);
    rsx! { Card {
        CardHeader { CardTitle { "{mode.label()}" } CardSubtitle { {format!("Maximum tool surface: {}", mode.ceiling().iter().map(|c| format!("{c:?}")).collect::<Vec<_>>().join(", "))} } }
        CardBody { Field { label: rsx! { "System prompt" }, Textarea { value: ReadSignal::from(prompt), oninput: move |v| prompt.set(v), rows: "3" } } }
        CardFooter { Button { variant: ButtonVariant::Secondary, onclick: move |_| { let mut settings = state.data.read().settings.clone(); if let Some(entry) = settings.prompts.iter_mut().find(|p| p.mode == mode) { entry.prompt = prompt(); } dispatch(state, Command::SaveSettings(Box::new(settings))); }, "Save prompt" } }
    } }
}

#[component]
fn BackendEditor(kind: CodingBackend, initial: Option<BackendConfig>) -> Element {
    let state = use_context::<State>();
    let enabled = initial.is_some();
    let mut executable = use_signal(|| {
        initial
            .as_ref()
            .map(|c| c.executable.clone())
            .unwrap_or_default()
    });
    let mut root = use_signal(|| {
        initial
            .as_ref()
            .map(|c| c.runtime_root.clone())
            .unwrap_or_default()
    });
    let mut script = use_signal(|| {
        initial
            .as_ref()
            .and_then(|c| c.adapter_script.clone())
            .unwrap_or_default()
    });
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "{kind.label()}" } CardSubtitle { if kind == CodingBackend::Codex { "Codex app-server · isolated stdio" } else { "Claude Agent SDK · ACP adapter · isolated stdio" } } }
        CardBody { div { class: "a206-grid",
            Field { label: rsx! { if kind == CodingBackend::Codex { "Codex executable" } else { "Node executable" } }, Input { value: ReadSignal::from(executable), oninput: move |v| executable.set(v), placeholder: "Absolute executable path" } }
            Field { label: rsx! { "Runtime installation directory" }, hint: rsx! { "Contains the executable, libraries and adapter. Do not select your home, project or private state directory." }, Input { value: ReadSignal::from(root), oninput: move |v| root.set(v), placeholder: "Absolute installation directory" } }
            if kind == CodingBackend::ClaudeAgent {
                Field { label: rsx! { "Claude Agent ACP entry point" }, Input { value: ReadSignal::from(script), oninput: move |v| script.set(v), placeholder: "Absolute adapter JavaScript path" } }
            }
        }
        p { class: "a206-muted", if kind == CodingBackend::Codex { "Requires a provider with the Responses API. Uses the exact API-discovered model you select." } else { "Requires the Anthropic Messages API. Uses API-key authentication through the future broker; subscription login is not offered." } }
        p { class: "a206-muted", "Experimental · macOS isolation only · authenticated dispatch and external delegation remain disabled." }
        }
        CardFooter {
            Button { variant: ButtonVariant::Primary, onclick: move |_| {
                let mut settings = state.data.read().settings.clone();
                settings.backends.retain(|b| b.kind != kind);
                settings.backends.push(BackendConfig { kind, executable: executable(), runtime_root: root(), adapter_script: (kind == CodingBackend::ClaudeAgent).then(&*script) });
                dispatch(state, Command::SaveSettings(Box::new(settings)));
            }, if enabled { "Save runtime" } else { "Configure runtime" } }
            if enabled { Button { variant: ButtonVariant::Ghost, onclick: move |_| { let mut settings = state.data.read().settings.clone(); settings.backends.retain(|b| b.kind != kind); dispatch(state, Command::SaveSettings(Box::new(settings))); }, "Disconnect runtime" } }
        }
    } }
}

#[component]
fn PresetLibrary() -> Element {
    let state = use_context::<State>();
    let mut selected = use_signal(|| None::<String>);
    let mut editing = use_signal(|| false);
    let mut generation = use_signal(|| 0_u64);
    let builtins = use_memo(move || {
        if state.data.read().settings.provider_presets.include_builtins {
            CheckState::On
        } else {
            CheckState::Off
        }
    });
    let snapshot = state.data.read();
    let library = &snapshot.settings.provider_presets;
    let entries = library.all();
    let initial = entries
        .iter()
        .find(|p| Some(&p.id) == selected.read().as_ref())
        .cloned();
    // Reload form values after a saved override is restored or changed elsewhere.
    let editor_key = format!("{:?}:{initial:?}:{}", selected(), generation());
    let options = entries
        .iter()
        .map(|p| {
            SelectChoice::new(
                p.id.clone(),
                if library.disabled.contains(&p.id) {
                    format!("{} · hidden", p.name)
                } else {
                    p.name.clone()
                },
            )
            .in_group(p.group.clone())
        })
        .collect();
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Provider library" } CardSubtitle { "{library.effective().len()} visible presets · stored overrides never change existing accounts" } }
        CardBody {
            Checkbox { label: rsx! { "Include bundled providers" }, checked: ReadSignal::from(builtins), onchange: move |include| { let mut settings = state.data.read().settings.clone(); settings.provider_presets.include_builtins = include; dispatch(state, Command::SaveSettings(Box::new(settings))); } }
            div { class: "a206-row",
                Select { label: name!("Edit provider preset"), value: ReadSignal::from(selected), options, placeholder: "Choose a preset to edit", onchange: move |value| { selected.set(Some(value)); editing.set(true); } }
                Button { variant: ButtonVariant::Secondary, onclick: move |_| { selected.set(None); generation.set(generation().wrapping_add(1)); editing.set(true); }, "New preset" }
            }
            if editing() { PresetEditor { key: "{editor_key}", initial } }
        }
    } }
}
#[component]
fn PresetEditor(initial: Option<ProviderPreset>) -> Element {
    let state = use_context::<State>();
    let new_id = Uuid::new_v4().to_string();
    let original = initial.as_ref().map(|p| p.id.clone());
    let is_builtin = original
        .as_ref()
        .is_some_and(|id| ProviderPreset::builtins().iter().any(|p| &p.id == id));
    let hidden = original.as_ref().is_some_and(|id| {
        state
            .data
            .read()
            .settings
            .provider_presets
            .disabled
            .contains(id)
    });
    let mut id = use_signal(|| initial.as_ref().map(|p| p.id.clone()).unwrap_or(new_id));
    let mut name = use_signal(|| initial.as_ref().map(|p| p.name.clone()).unwrap_or_default());
    let mut group = use_signal(|| {
        initial
            .as_ref()
            .map(|p| p.group.clone())
            .unwrap_or_else(|| "Custom".into())
    });
    let mut url = use_signal(|| {
        initial
            .as_ref()
            .map(|p| p.base_url.clone())
            .unwrap_or_default()
    });
    let mut setup = use_signal(|| {
        initial
            .as_ref()
            .map(|p| p.setup.clone())
            .unwrap_or_default()
    });
    let mut api = use_signal(|| {
        initial
            .as_ref()
            .map(|p| p.api)
            .unwrap_or_default()
            .label()
            .to_owned()
    });
    let api_value = use_memo(move || Some(api()));
    let reset_id = original.clone();
    let hide_id = original.clone();
    rsx! { div { class: "a206-stack",
        Divider {}
        div { class: "a206-grid",
            Field { label: rsx! { "Preset ID" }, hint: rsx! { "Stable letters, digits, dots, hyphens or underscores. Reuse a bundled ID to override it." }, Input { value: ReadSignal::from(id), oninput: move |v| id.set(v), placeholder: "Stable identifier" } }
            Field { label: rsx! { "Provider name" }, Input { value: ReadSignal::from(name), oninput: move |v| name.set(v) } }
            Field { label: rsx! { "Group" }, Input { value: ReadSignal::from(group), oninput: move |v| group.set(v) } }
            Field { label: rsx! { "API base URL" }, hint: rsx! { "Leave blank to require a per-account endpoint. No credentials or query parameters." }, Input { r#type: InputType::Url, value: ReadSignal::from(url), oninput: move |v| url.set(v) } }
            Field { label: rsx! { "Protocol" }, Select { label: name!("Preset API protocol"), value: ReadSignal::from(api_value), options: ProviderApi::ALL.into_iter().map(|a| SelectChoice::new(a.label(), a.label())).collect(), onchange: move |v| api.set(v) } }
        }
        Field { label: rsx! { "Setup instructions · no secrets" }, Textarea { rows: "2", value: ReadSignal::from(setup), oninput: move |v| setup.set(v) } }
        div { class: "a206-row",
            Button { variant: ButtonVariant::Primary, onclick: move |_| {
                let mut settings = state.data.read().settings.clone();
                let entry = ProviderPreset { id: id().trim().to_owned(), name: name().trim().to_owned(), group: group().trim().to_owned(), api: ProviderApi::ALL.into_iter().find(|a| a.label() == api()).unwrap_or_default(), base_url: url().trim().to_owned(), setup: setup() };
                settings.provider_presets.entries.retain(|p| p.id != entry.id);
                settings.provider_presets.disabled.remove(&entry.id);
                settings.provider_presets.entries.push(entry);
                dispatch(state, Command::SaveSettings(Box::new(settings)));
            }, "Save preset" }
            if let Some(target) = hide_id { Button { variant: ButtonVariant::Ghost, onclick: move |_| { let mut settings = state.data.read().settings.clone(); if hidden { settings.provider_presets.disabled.remove(&target); } else { settings.provider_presets.disabled.insert(target.clone()); } dispatch(state, Command::SaveSettings(Box::new(settings))); }, if hidden { "Show preset" } else { "Hide preset" } } }
            if let Some(target) = reset_id { Button { variant: ButtonVariant::Ghost, onclick: move |_| { let mut settings = state.data.read().settings.clone(); settings.provider_presets.entries.retain(|p| p.id != target); settings.provider_presets.disabled.remove(&target); dispatch(state, Command::SaveSettings(Box::new(settings))); }, if is_builtin { "Restore bundled version" } else { "Delete custom preset" } } }
        }
    } }
}
