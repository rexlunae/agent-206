//! The user's agent teams: listed on the Modes page, and started from a
//! conversation's details. Teams and profiles are files in the gateway's
//! profile, so both read a fresh listing rather than the snapshot.
use crate::{State, call, dispatch, error::ErrorNotice};
use a206_core::{AgentProfiles, AgentTeams, Command, ErrorKind, ErrorReport, Reply, teams::start};
use fc_dioxus::prelude::*;
use uuid::Uuid;

async fn load() -> Result<AgentTeams, ErrorReport> {
    decode(call(Command::AgentTeams).await?)
}
fn decode(reply: Reply) -> Result<AgentTeams, ErrorReport> {
    match reply {
        Reply::AgentTeams(teams) => Ok(teams),
        Reply::Error(report) => Err(report),
        _ => Err(ErrorReport::new(
            ErrorKind::Protocol,
            "gateway did not list agent teams",
        )),
    }
}
async fn load_profiles() -> Result<AgentProfiles, ErrorReport> {
    match call(Command::AgentProfiles).await? {
        Reply::AgentProfiles(profiles) => Ok(profiles),
        Reply::Error(report) => Err(report),
        _ => Err(ErrorReport::new(
            ErrorKind::Protocol,
            "gateway did not list agent profiles",
        )),
    }
}

/// The teams on the Modes page, with any file that could not be used.
#[component]
pub(crate) fn AgentTeamsCard() -> Element {
    let mut teams = use_signal(AgentTeams::default);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let reload = move || {
        spawn(async move {
            match load().await {
                Ok(list) => {
                    failure.set(None);
                    teams.set(list);
                }
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    use_hook(reload);
    let listed = teams();
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Agent teams" } CardSubtitle { "A coordinator profile that hands tasks to member profiles. Starting a team never gives more than the conversation it starts from." } }
        CardBody {
            if let Some(report) = failure() { ErrorNotice { report, title: "Agent teams unavailable".to_string(), dismiss: move |_| failure.set(None) } }
            if listed.teams.is_empty() && listed.problems.is_empty() {
                p { class: "a206-muted", "None yet. Add a TOML file, such as research.toml, to the agent-teams directory of the gateway's profile." }
            }
            for team in listed.teams {
                div { key: "{team.id}", class: "a206-stack",
                    div { class: "a206-row", strong { "{team.name}" } code { "{team.id}" } }
                    if let Some(description) = &team.description { p { class: "a206-muted", "{description}" } }
                    div { class: "a206-row",
                        Badge { tone: BadgeTone::Accent, "Coordinator: {team.coordinator}" }
                        for member in team.members.iter() { Badge { key: "{member}", tone: BadgeTone::Info, "{member}" } }
                    }
                    if !team.instructions.is_empty() {
                        Disclosure { summary: rsx! { "Shared instructions" }, blockquote { class: "a206-quote", "{team.instructions}" } }
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

/// Starts a team on `thread`, which must not already belong to one.
#[component]
pub(crate) fn StartTeam(thread: Uuid) -> Element {
    let state = use_context::<State>();
    let mut teams = use_signal(AgentTeams::default);
    let mut profiles = use_signal(AgentProfiles::default);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut chosen = use_signal(|| None::<String>);
    use_hook(move || {
        spawn(async move {
            match load().await {
                Ok(list) => teams.set(list),
                Err(report) => failure.set(Some(report)),
            }
            match load_profiles().await {
                Ok(list) => profiles.set(list),
                Err(report) => failure.set(Some(report)),
            }
        });
    });
    let snapshot = state.data.read();
    let Some(parent) = snapshot.threads.iter().find(|t| t.id == thread) else {
        return rsx! {};
    };
    let listed = teams();
    if parent.team.is_some() || (listed.teams.is_empty() && failure().is_none()) {
        return rsx! {};
    }
    let team = chosen().and_then(|id| listed.get(&id).cloned());
    let request = team
        .as_ref()
        .and_then(|t| start(t, parent, &profiles(), &snapshot.settings.providers));
    let choices: Vec<SelectChoice> = std::iter::once(SelectChoice::new("", "Choose a team"))
        .chain(
            listed
                .teams
                .iter()
                .map(|t| SelectChoice::new(t.id.clone(), t.name.clone())),
        )
        .collect();
    rsx! { div { class: "a206-stack",
        if let Some(report) = failure() { ErrorNotice { report, title: "Agent teams unavailable".to_string(), dismiss: move |_| failure.set(None) } }
        Field { label: rsx! { "Agent team" },
            Select { label: name!("Agent team to start from this conversation"), value: ReadSignal::from(chosen), options: choices,
                onchange: move |id: String| chosen.set((!id.is_empty()).then_some(id)) }
        }
        if team.is_some() && request.is_none() {
            p { class: "a206-muted", "This team's coordinator profile is unavailable, or does not match exactly one of this conversation's subagent models." }
        }
        Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, disabled: request.is_none(), onclick: move |_| {
            if let Some(command) = request.clone() {
                dispatch(state, command);
            }
        }, "Start team" }
    } }
}

#[cfg(test)]
mod tests;
