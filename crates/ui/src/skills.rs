//! Skills installed in the gateway's profile, reviewed and approved here.
use crate::{State, call, error::ErrorNotice};
use a206_core::{Command, ErrorReport, Reply, SkillSummary, SkillText};
use fc_dioxus::prelude::*;

#[component]
pub fn Skills() -> Element {
    let state = use_context::<State>();
    let mut skills = use_signal(Vec::<SkillSummary>::new);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut review = use_signal(|| None::<SkillText>);
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
    // Enabling pins the digest of the text shown in the review card, so an
    // edit made after the review is never approved by this click.
    let toggle = move |name: String, enabled: bool, digest: Option<String>| {
        spawn(async move {
            match call(Command::SetSkillEnabled {
                name,
                enabled,
                digest,
            })
            .await
            {
                Ok(_) => {
                    review.set(None);
                    reload();
                }
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    let open = move |name: String| {
        spawn(async move {
            match call(Command::ReadSkill { name }).await {
                Ok(Reply::Skill(text)) => review.set(Some(text)),
                Ok(_) => {}
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    rsx! {
        h2 { "Skills" }
        p { class: "a206-muted", "Instruction folders in your profile's skills directory. Review one before enabling it; any later edit turns it off until you approve it again. Skills never add permissions." }
        // Installing or editing a skill changes files, not settings, so the
        // list is re-read on request.
        div { class: "a206-row", Button { variant: ButtonVariant::Secondary, onclick: move |_| reload(), "Refresh" } }
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
                            Button { variant: if skill.enabled { ButtonVariant::Secondary } else { ButtonVariant::Primary }, onclick: { let name = skill.name.clone(); move |_| open(name.clone()) }, "Review" }
                            if skill.enabled || skill.changed { Button { variant: ButtonVariant::Ghost, onclick: { let name = skill.name.clone(); move |_| toggle(name.clone(), false, None) }, "Disable" } }
                        } else if skill.changed {
                            Button { variant: ButtonVariant::Ghost, onclick: { let name = skill.name.clone(); move |_| toggle(name.clone(), false, None) }, "Disable" }
                        }
                    }
                }
            }
        }
        if let Some(reviewed) = review() {
            Card { variant: CardVariant::Elevated,
                CardHeader { CardTitle { "{reviewed.name}" } CardSubtitle { "SKILL.md as it was read for this review" } }
                CardBody { pre { class: "a206-message-text", "{reviewed.text}" } }
                CardFooter {
                    Button { variant: ButtonVariant::Primary, onclick: { let reviewed = reviewed.clone(); move |_| toggle(reviewed.name.clone(), true, Some(reviewed.digest.clone())) }, "Approve this text and enable" }
                    Button { variant: ButtonVariant::Ghost, onclick: move |_| review.set(None), "Close" }
                }
            }
        }
    }
}
