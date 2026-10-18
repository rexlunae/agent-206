//! The user's agent profiles: listed on the Modes page, and offered when
//! creating a delegated conversation. Profiles are files in the gateway's
//! profile, so both read a fresh listing rather than the snapshot.
use crate::{State, call, dispatch, error::ErrorNotice};
use a206_core::{
    AgentProfile, AgentProfiles, Command, ErrorReport, Mode, ModelKey, ProviderConfig, Reply,
};
use fc_dioxus::prelude::*;
use std::collections::BTreeSet;
use uuid::Uuid;

async fn load() -> Result<AgentProfiles, ErrorReport> {
    decode_profiles(call(Command::AgentProfiles).await?)
}
fn decode_profiles(reply: Reply) -> Result<AgentProfiles, ErrorReport> {
    match reply {
        Reply::AgentProfiles(profiles) => Ok(profiles),
        Reply::Error(report) => Err(report),
        _ => Err(ErrorReport::new(
            a206_core::ErrorKind::Protocol,
            "gateway did not list agent profiles",
        )),
    }
}
fn selected<'a>(
    listed: &'a AgentProfiles,
    chosen: Option<&str>,
) -> Result<Option<&'a AgentProfile>, ErrorReport> {
    match chosen {
        None => Ok(None),
        Some(id) => listed.get(id).map(Some).ok_or_else(|| ErrorReport::new(
            a206_core::ErrorKind::NotFound, format!("Selected profile {id} is unavailable. Choose another profile or explicitly choose No profile."))),
    }
}

/// The mode and model a delegated conversation started from `profile` asks
/// for: the profile's mode (Plan when it names none) and the first of the
/// `permitted` models it names. `None` when it names none of them.
pub(crate) fn delegation(
    profile: Option<&AgentProfile>,
    permitted: &BTreeSet<ModelKey>,
    providers: &[ProviderConfig],
) -> Option<(Mode, ModelKey)> {
    let mode = profile.and_then(|p| p.mode).unwrap_or(Mode::Plan);
    let mut matching = permitted
        .iter()
        .filter(|key| profile.is_none_or(|p| p.names(key, providers)));
    let model = matching.next()?;
    if profile.is_some_and(|p| p.model.is_some()) && matching.next().is_some() {
        return None;
    }
    Some((mode, model.clone()))
}

/// The profiles on the Modes page, with any file that could not be used.
#[component]
pub(crate) fn AgentProfilesCard() -> Element {
    let mut profiles = use_signal(AgentProfiles::default);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let reload = move || {
        spawn(async move {
            match load().await {
                Ok(list) => {
                    failure.set(None);
                    profiles.set(list);
                }
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    use_hook(reload);
    let listed = profiles();
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Agent profiles" } CardSubtitle { "Presets a delegated conversation can start from. A profile only narrows what the delegating conversation could give." } }
        CardBody {
            if let Some(report) = failure() { ErrorNotice { report, title: "Agent profiles unavailable".to_string(), dismiss: move |_| failure.set(None) } }
            if listed.profiles.is_empty() && listed.problems.is_empty() {
                p { class: "a206-muted", "None yet. Add a TOML file, such as reviewer.toml, to the agent-profiles directory of the gateway's profile." }
            }
            for profile in listed.profiles {
                div { key: "{profile.id}", class: "a206-stack",
                    div { class: "a206-row", strong { "{profile.name}" } code { "{profile.id}" } }
                    if let Some(description) = &profile.description { p { class: "a206-muted", "{description}" } }
                    div { class: "a206-row",
                        Badge { tone: BadgeTone::Accent, {profile.mode.map(|m| m.label()).unwrap_or("Any mode")} }
                        Badge { {match (&profile.model, &profile.connection) {
                            (Some(model), Some(connection)) => format!("{model} on {connection}"),
                            (Some(model), None) => model.clone(),
                            _ => "Any permitted model".into(),
                        }} }
                        match &profile.capabilities {
                            None => rsx! { Badge { "Capabilities as requested" } },
                            Some(set) if set.is_empty() => rsx! { Badge { "No capabilities" } },
                            Some(set) => rsx! { for capability in set.iter().copied() { Badge { key: "{capability:?}", tone: BadgeTone::Info, "{capability.label()}" } } },
                        }
                    }
                    if !profile.instructions.is_empty() {
                        Disclosure { summary: rsx! { "Instructions" }, blockquote { class: "a206-quote", "{profile.instructions}" } }
                    }
                }
            }
            for problem in listed.problems {
                ErrorNotice { key: "{problem.file}", report: problem.error, title: format!("{} cannot be used", problem.file) }
            }
        }
        CardFooter { Button { variant: ButtonVariant::Secondary, onclick: move |_| reload(), "Refresh" } }
    } }
}

/// Creates a delegated conversation of `thread`, from a profile if chosen.
#[component]
pub(crate) fn Delegate(thread: Uuid) -> Element {
    let state = use_context::<State>();
    let mut profiles = use_signal(AgentProfiles::default);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut chosen = use_signal(|| None::<String>);
    use_hook(move || {
        spawn(async move {
            match load().await {
                Ok(list) => profiles.set(list),
                Err(report) => failure.set(Some(report)),
            }
        });
    });
    let snapshot = state.data.read();
    let Some(permitted) = snapshot
        .threads
        .iter()
        .find(|t| t.id == thread)
        .map(|t| t.subagent_models.clone())
    else {
        return rsx! {};
    };
    let listed = profiles();
    let selected_id = chosen();
    let (profile, selection_failure) = match selected(&listed, selected_id.as_deref()) {
        Ok(profile) => (profile.cloned(), None),
        Err(report) => (None, Some(report)),
    };
    let request = if selection_failure.is_none() {
        delegation(profile.as_ref(), &permitted, &snapshot.settings.providers)
    } else {
        None
    };
    let mut choices: Vec<SelectChoice> = std::iter::once(SelectChoice::new("", "No profile"))
        .chain(
            listed
                .profiles
                .iter()
                .map(|p| SelectChoice::new(p.id.clone(), p.name.clone())),
        )
        .collect();
    if selection_failure.is_some()
        && let Some(id) = &selected_id
    {
        choices.push(SelectChoice::new(id.clone(), format!("{id} (unavailable)")));
    }
    let title = profile
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "Delegated task".into());
    let id = selected_id;
    rsx! { div { class: "a206-stack",
        if let Some(report) = failure() { ErrorNotice { report, title: "Agent profiles unavailable".to_string(), dismiss: move |_| failure.set(None) } }
        if let Some(report) = selection_failure { ErrorNotice { report, title: "Selected profile unavailable".to_string() } }
        if !listed.profiles.is_empty() || id.is_some() {
            Field { label: rsx! { "Agent profile" },
                Select { label: name!("Agent profile for the delegated conversation"), value: ReadSignal::from(chosen), options: choices,
                    onchange: move |id: String| chosen.set((!id.is_empty()).then_some(id)) }
            }
        }
        if profile.is_some() && request.is_none() {
            p { class: "a206-muted", "This profile must match exactly one permitted model connection. Check its model and connection settings." }
        }
        Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, disabled: request.is_none(), onclick: move |_| {
            if let Some((mode, model)) = request.clone() {
                let command = match id.clone() {
                    Some(profile) => Command::CreateProfileSubagent { parent: thread, title: title.clone(), mode, model, requested: Default::default(), profile },
                    None => Command::CreateSubagent { parent: thread, title: title.clone(), mode, model, requested: Default::default() },
                };
                dispatch(state, command);
            }
        }, "Create delegated conversation" }
    } }
}

#[cfg(test)]
mod tests;
