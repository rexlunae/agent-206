//! Skills installed in the gateway's profile, reviewed and approved here.
use crate::{State, audit::AuditFindings, call, error::ErrorNotice};
use a206_core::{Command, ErrorReport, Reply, SkillSummary, SkillText};
use fc_dioxus::prelude::*;

#[derive(Default)]
struct Review {
    generation: u64,
    selected: Option<String>,
    text: Option<SkillText>,
}
impl Review {
    fn open(&mut self, name: String) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.selected = Some(name);
        self.text = None;
        self.generation
    }
    fn close(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.selected = None;
        self.text = None;
    }
    fn current(&self, generation: u64) -> bool {
        self.selected.is_some() && self.generation == generation
    }
    fn receive(&mut self, generation: u64, text: SkillText) {
        if self.current(generation) && self.selected.as_deref() == Some(&text.name) {
            self.text = Some(text);
        }
    }
    fn toggled(&mut self, name: &str, enabled: bool, digest: Option<&str>) {
        if self.selected.as_deref() == Some(name)
            && (!enabled
                || self
                    .text
                    .as_ref()
                    .is_some_and(|text| Some(text.digest.as_str()) == digest))
        {
            self.close();
        }
    }
}

#[component]
pub fn Skills() -> Element {
    let state = use_context::<State>();
    let mut skills = use_signal(Vec::<SkillSummary>::new);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut review = use_signal(Review::default);
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
                name: name.clone(),
                enabled,
                digest: digest.clone(),
            })
            .await
            {
                Ok(_) => {
                    review.write().toggled(&name, enabled, digest.as_deref());
                    reload();
                }
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    let mut open = move |name: String| {
        let generation = review.write().open(name.clone());
        spawn(async move {
            let result = call(Command::ReadSkill { name }).await;
            if !review.peek().current(generation) {
                return;
            }
            match result {
                Ok(Reply::Skill(text)) => review.write().receive(generation, text),
                Ok(_) => {}
                Err(report) => {
                    review.write().close();
                    failure.set(Some(report));
                }
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
                        if let Some(report) = skill.failure.clone() { ErrorNotice { report, title: "This skill cannot be read".to_string() } } else { AuditFindings { report: skill.audit.clone() } }
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
        if review.read().selected.is_some() && review.read().text.is_none() {
            p { class: "a206-muted", "Loading skill review…" }
        }
        if let Some(reviewed) = review.read().text.clone() {
            Card { variant: CardVariant::Elevated,
                CardHeader { CardTitle { "{reviewed.name}" } CardSubtitle { "SKILL.md as it was read for this review" } }
                CardBody { pre { class: "a206-message-text", "{reviewed.text}" } }
                CardFooter {
                    Button { variant: ButtonVariant::Primary, onclick: { let reviewed = reviewed.clone(); move |_| toggle(reviewed.name.clone(), true, Some(reviewed.digest.clone())) }, "Approve this text and enable" }
                    Button { variant: ButtonVariant::Ghost, onclick: move |_| review.write().close(), "Close" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(name: &str, digest: &str) -> SkillText {
        SkillText {
            name: name.into(),
            text: name.into(),
            digest: digest.into(),
        }
    }
    #[test]
    fn unrelated_disable_keeps_the_open_review() {
        let mut review = Review::default();
        let id = review.open("notes".into());
        review.receive(id, text("notes", "first"));
        review.toggled("calendar", false, None);
        assert_eq!(review.text, Some(text("notes", "first")));
        review.toggled("notes", false, None);
        assert!(review.text.is_none());
    }
    #[test]
    fn late_replies_and_old_approvals_do_not_replace_the_current_review() {
        let mut review = Review::default();
        let first = review.open("notes".into());
        let second = review.open("calendar".into());
        review.receive(second, text("calendar", "second"));
        review.receive(first, text("notes", "first"));
        assert_eq!(review.text, Some(text("calendar", "second")));
        review.toggled("calendar", true, Some("old"));
        assert!(review.text.is_some());
        review.close();
        review.receive(second, text("calendar", "second"));
        assert!(review.text.is_none());
    }
}
