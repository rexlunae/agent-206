//! Skills installed in the gateway's profile, reviewed and approved here.
use crate::{State, call, error::ErrorNotice};
use a206_core::{Command, ErrorReport, Reply, SkillSummary};
use fc_dioxus::prelude::*;

#[component]
pub fn Skills() -> Element {
    let state = use_context::<State>();
    let mut skills = use_signal(Vec::<SkillSummary>::new);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut review = use_signal(|| None::<(String, String)>);
    let reload = move || {
        spawn(async move {
            match call(Command::Skills).await {
                Ok(Reply::Skills(list)) => {
                    failure.set(None);
                    skills.set(list);
                }
                Ok(_) => {}
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    // Settings events follow approvals made here or from the CLI.
    let revision = state.data.read().settings.revision;
    use_effect(use_reactive!(|revision| {
        let _ = revision;
        reload();
    }));
    let toggle = move |name: String, enabled: bool| {
        spawn(async move {
            match call(Command::SetSkillEnabled { name, enabled }).await {
                Ok(_) => reload(),
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    let open = move |name: String| {
        spawn(async move {
            match call(Command::ReadSkill { name: name.clone() }).await {
                Ok(Reply::Text(text)) => review.set(Some((name, text))),
                Ok(_) => {}
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    rsx! {
        h2 { "Skills" }
        p { class: "a206-muted", "Instruction folders in your profile's skills directory. Review one before enabling it; any later edit turns it off until you approve it again. Skills never add permissions." }
        if let Some(report) = failure() { ErrorNotice { report, title: "Skills unavailable".to_string(), dismiss: move |_| failure.set(None) } }
        if skills.read().is_empty() { EmptyState { compact: true, title: rsx! { "No skills installed" }, body: rsx! { "Copy a skill folder containing SKILL.md into the skills directory of the gateway's profile." } } }
        div { class: "a206-grid",
            for skill in skills() {
                Card { key: "{skill.name}",
                    CardHeader { CardTitle { {format!("{}{}", skill.emoji.clone().map(|e| e + " ").unwrap_or_default(), skill.name)} } CardSubtitle { {skill.description.clone().unwrap_or_default()} } }
                    CardBody {
                        div { class: "a206-row",
                            Badge { if skill.enabled { "Enabled" } else if skill.changed { "Changed since approval" } else { "Off" } }
                        }
                        for unmet in &skill.unmet { p { class: "a206-muted", "Not available here: {unmet}" } }
                        if !skill.expects_env.is_empty() { p { class: "a206-muted", {format!("Expects environment variables tools do not receive: {}", skill.expects_env.join(", "))} } }
                        if let Some(report) = skill.failure.clone() { ErrorNotice { report, title: "This skill cannot be read".to_string() } }
                    }
                    CardFooter {
                        if skill.failure.is_none() {
                            Button { variant: ButtonVariant::Secondary, onclick: { let name = skill.name.clone(); move |_| open(name.clone()) }, "Review" }
                            Button { variant: if skill.enabled { ButtonVariant::Ghost } else { ButtonVariant::Primary }, onclick: { let name = skill.name.clone(); let enabled = !skill.enabled; move |_| toggle(name.clone(), enabled) }, if skill.enabled { "Disable" } else { "Approve and enable" } }
                        } else if skill.changed {
                            Button { variant: ButtonVariant::Ghost, onclick: { let name = skill.name.clone(); move |_| toggle(name.clone(), false) }, "Disable" }
                        }
                    }
                }
            }
        }
        if let Some((name, text)) = review() {
            Card { variant: CardVariant::Elevated,
                CardHeader { CardTitle { "{name}" } CardSubtitle { "SKILL.md as it is now" } }
                CardBody { pre { class: "a206-message-text", "{text}" } }
                CardFooter { Button { variant: ButtonVariant::Ghost, onclick: move |_| review.set(None), "Close" } }
            }
        }
    }
}
