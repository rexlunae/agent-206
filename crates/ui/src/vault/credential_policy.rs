//! Nonsecret policy drafts. The broker, never this editor, authorizes a use.
use super::*;
use a206_vault_channel::AccessPolicy;

#[derive(Clone, Copy, PartialEq)]
enum PolicyKind {
    Always,
    Approval,
    Auth,
    Skills,
    Triggers,
}
impl PolicyKind {
    const ALL: [Self; 5] = [
        Self::Always,
        Self::Approval,
        Self::Auth,
        Self::Skills,
        Self::Triggers,
    ];
    fn key(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Approval => "approval",
            Self::Auth => "auth",
            Self::Skills => "skills",
            Self::Triggers => "triggers",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Always => "No per-use condition",
            Self::Approval => "Approval per use",
            Self::Auth => "Fresh authentication",
            Self::Skills => "Listed skills only",
            Self::Triggers => "Listed triggers only",
        }
    }
    fn parse(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }
}
#[derive(Clone, PartialEq)]
pub(super) struct PolicyDraft {
    pub(super) name: String,
    kind: PolicyKind,
    skills: Vec<String>,
    triggers: Vec<String>,
}
impl PolicyDraft {
    pub(super) fn new(name: String, policy: AccessPolicy) -> Self {
        let (kind, skills, triggers) = match policy {
            AccessPolicy::Always => (PolicyKind::Always, vec![], vec![]),
            AccessPolicy::WithApproval => (PolicyKind::Approval, vec![], vec![]),
            AccessPolicy::WithAuth => (PolicyKind::Auth, vec![], vec![]),
            AccessPolicy::SkillOnly(scopes) => (PolicyKind::Skills, scopes, vec![]),
            AccessPolicy::TriggerOnly(scopes) => (PolicyKind::Triggers, vec![], scopes),
        };
        Self {
            name,
            kind,
            skills,
            triggers,
        }
    }
    fn policy(&self) -> AccessPolicy {
        match self.kind {
            PolicyKind::Always => AccessPolicy::Always,
            PolicyKind::Approval => AccessPolicy::WithApproval,
            PolicyKind::Auth => AccessPolicy::WithAuth,
            PolicyKind::Skills => AccessPolicy::SkillOnly(self.skills.clone()),
            PolicyKind::Triggers => AccessPolicy::TriggerOnly(self.triggers.clone()),
        }
    }
    fn scopes(&self) -> &[String] {
        match self.kind {
            PolicyKind::Skills => &self.skills,
            PolicyKind::Triggers => &self.triggers,
            _ => &[],
        }
    }
    fn scopes_mut(&mut self) -> Option<&mut Vec<String>> {
        match self.kind {
            PolicyKind::Skills => Some(&mut self.skills),
            PolicyKind::Triggers => Some(&mut self.triggers),
            _ => None,
        }
    }
}

#[component]
pub(super) fn PolicyForm(
    mut draft: Signal<Option<PolicyDraft>>,
    busy: bool,
    onsave: EventHandler<(String, AccessPolicy)>,
    oncancel: EventHandler<()>,
) -> Element {
    let selected = use_memo(move || draft().map(|d| d.kind.key().to_string()));
    let Some(current) = draft() else {
        return rsx! {};
    };
    let available = if busy {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let scoped = matches!(current.kind, PolicyKind::Skills | PolicyKind::Triggers);
    let oversized = scoped && current.scopes().len() > a206_vault_channel::MAX_POLICY_SCOPES;
    rsx! {
        h3 { "Edit credential policy" }
        code { class: "a206-policy-identifier", "{current.name:?}" }
        p { class: "a206-muted", "Changes only the condition for agent use. Global agent access, enabled state and permitted destinations still apply. Saving a policy does not approve any use or authenticate an agent." }
        Field { label: rsx! { "New policy" },
            Select { label: name!("New policy"), value: ReadSignal::from(selected), availability: available.clone(),
                options: PolicyKind::ALL.into_iter().map(|kind| SelectChoice::new(kind.key(), kind.label())).collect(),
                onchange: move |key: String| {
                    if let Some(kind) = PolicyKind::parse(&key)
                        && let Some(draft) = draft.write().as_mut() { draft.kind = kind;
                    }
                }
            }
        }
        if oversized {
            p { "This stored scope list exceeds the panel's 64-identifier edit limit. Use the trusted vault CLI to edit it. Its identifiers are retained unchanged; choosing a different policy explicitly replaces this rule." }
        } else if scoped {
            p { "Only the exact identifiers listed below are allowed. An empty list permits no skills or triggers. Identifiers are case-sensitive and are not trimmed or split." }
            for index in 0..current.scopes().len() {
                ScopeRow { key: "{current.kind.key()}-{index}", draft, index, busy }
            }
            Button { variant: ButtonVariant::Secondary, availability: if busy || current.scopes().len() >= a206_vault_channel::MAX_POLICY_SCOPES { Availability::Disabled } else { Availability::Enabled },
                onclick: move |_| { if let Some(scopes) = draft.write().as_mut().and_then(PolicyDraft::scopes_mut) { scopes.push(String::new()); } },
                "Add permitted identifier"
            }
        }
        Button { variant: ButtonVariant::Primary, availability: if busy || oversized { Availability::Disabled } else { Availability::Enabled },
            onclick: move |_| { if let Some(current) = draft() { onsave.call((current.name.clone(), current.policy())); } },
            "Save policy"
        }
        Button { variant: ButtonVariant::Ghost, availability: available, onclick: move |_| oncancel.call(()), "Cancel policy" }
    }
}
#[component]
pub(super) fn PolicySummary(policy: AccessPolicy) -> Element {
    let draft = PolicyDraft::new(String::new(), policy);
    rsx! {
        p { "New policy: {draft.kind.label()}" }
        if matches!(draft.kind, PolicyKind::Skills | PolicyKind::Triggers) {
            if draft.scopes().is_empty() { p { "No identifiers: this policy permits no uses." } }
            for identifier in draft.scopes() { p { code { class: "a206-policy-identifier", "{identifier:?}" } } }
        }
    }
}
#[component]
fn ScopeRow(mut draft: Signal<Option<PolicyDraft>>, index: usize, busy: bool) -> Element {
    let value = use_memo(move || {
        draft()
            .and_then(|d| d.scopes().get(index).cloned())
            .unwrap_or_default()
    });
    let available = if busy {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    rsx! {
        Field { label: rsx! { "Permitted identifier {index + 1}" },
            Input { availability: available.clone(), value: ReadSignal::from(value),
                oninput: move |value| {
                    if let Some(scopes) = draft.write().as_mut().and_then(PolicyDraft::scopes_mut)
                        && let Some(scope) = scopes.get_mut(index) { *scope = value;
                    }
                }
            }
        }
        Button { variant: ButtonVariant::Ghost, availability: available,
            onclick: move |_| {
                if let Some(scopes) = draft.write().as_mut().and_then(PolicyDraft::scopes_mut)
                    && index < scopes.len() { scopes.remove(index);
                }
            }, "Remove identifier {index + 1}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_forms_keep_named_scopes_visible_and_gate_saving_while_busy() {
        fn root((name, policy, busy): (String, AccessPolicy, bool)) -> Element {
            let draft = use_signal(move || Some(PolicyDraft::new(name, policy)));
            rsx! { PolicyForm { draft, busy, onsave: move |_| {}, oncancel: move |_| {} } }
        }
        let name = uuid::Uuid::new_v4().to_string();
        let scope = format!(" {} exact scope ", uuid::Uuid::new_v4());
        for busy in [false, true] {
            let mut dom = VirtualDom::new_with_props(
                root,
                (
                    name.clone(),
                    AccessPolicy::SkillOnly(vec![scope.clone()]),
                    busy,
                ),
            );
            dom.rebuild_in_place();
            let html = dioxus_ssr::render(&dom);
            assert!(html.contains(&name) && html.contains(&scope), "{html}");
            assert!(
                html.contains("Save policy") && html.contains("Remove identifier 1"),
                "{html}"
            );
            assert_eq!(html.contains(" disabled=true"), busy, "{html}");
        }
    }
    #[test]
    fn every_policy_round_trips_without_changing_scopes_and_switching_keeps_each_draft() {
        let identifier = format!(" {} with spaces ", uuid::Uuid::new_v4());
        for policy in [
            AccessPolicy::Always,
            AccessPolicy::WithApproval,
            AccessPolicy::WithAuth,
            AccessPolicy::SkillOnly(vec![]),
            AccessPolicy::TriggerOnly(vec![]),
            AccessPolicy::SkillOnly(vec![identifier.clone()]),
            AccessPolicy::TriggerOnly(vec![identifier.clone()]),
        ] {
            let mut draft = PolicyDraft::new(uuid::Uuid::new_v4().to_string(), policy.clone());
            assert_eq!(draft.policy(), policy);
            let kind = draft.kind;
            draft.kind = PolicyKind::Always;
            draft.kind = kind;
            assert_eq!(draft.policy(), policy);
        }
        assert!(PolicyKind::parse(&uuid::Uuid::new_v4().to_string()).is_none());
    }
    #[test]
    fn oversized_legacy_scopes_are_retained_without_mounting_unbounded_inputs() {
        fn root(scopes: Vec<String>) -> Element {
            let draft = use_signal(move || {
                Some(PolicyDraft::new(
                    "legacy-scope-entry".into(),
                    AccessPolicy::SkillOnly(scopes),
                ))
            });
            rsx! { PolicyForm { draft, busy: false, onsave: move |_| {}, oncancel: move |_| {} } }
        }
        let scopes =
            vec![uuid::Uuid::new_v4().to_string(); a206_vault_channel::MAX_POLICY_SCOPES + 1];
        assert_eq!(
            PolicyDraft::new(String::new(), AccessPolicy::SkillOnly(scopes.clone())).policy(),
            AccessPolicy::SkillOnly(scopes.clone())
        );
        let mut dom = VirtualDom::new_with_props(root, scopes);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Use the trusted vault CLI"), "{html}");
        assert!(!html.contains("Permitted identifier 1"), "{html}");
        assert!(html.contains(" disabled=true"), "{html}");
    }
}
