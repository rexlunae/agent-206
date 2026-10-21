//! User-only metadata and API-key entry, through the private broker session.
use super::*;
use a206_vault_channel::{
    AccessPolicy, CREDENTIALS_SCHEMA_VERSION, CredentialKind, CredentialPage, MAX_CREDENTIAL_PAGE,
};
use zeroize::Zeroize;

pub(super) struct KeyInput {
    name: String,
    label: String,
    destinations: Vec<String>,
    value: WireSecret,
}
#[derive(Clone, PartialEq)]
pub(super) struct DestinationDraft {
    pub(super) name: String,
    pub(super) origins: String,
}
pub(super) fn destination_failure_title(error: &VaultError) -> &'static str {
    if error.report.kind == a206_core::ErrorKind::InvalidRequest {
        "Destinations rejected; correct the origins and retry"
    } else {
        "Destination change not confirmed; cancel the edit and refresh credentials before retrying"
    }
}
impl Panel {
    pub(super) fn read_credentials(mut self, after: Option<String>) {
        if *self.busy.peek() {
            return;
        }
        // Background metadata reads must preserve setup/import outcomes.
        self.busy.set(true);
        self.credential_read_failure.set(None);
        spawn(async move {
            let result = self
                .session()
                .call(Request::CredentialPage {
                    version: CREDENTIALS_SCHEMA_VERSION,
                    after,
                    limit: MAX_CREDENTIAL_PAGE,
                })
                .await;
            match result {
                Ok(Reply::CredentialPage(page)) if valid_page(&page) => {
                    self.credential_page.set(Some(page))
                }
                Ok(_) => self.credential_read_failure.set(Some(failure(
                    "vault_credentials",
                    "Could not read credentials",
                    &unexpected(),
                ))),
                Err(error) => self.credential_read_failure.set(Some(failure(
                    "vault_credentials",
                    "Could not read credentials",
                    &error,
                ))),
            }
            self.finish().await;
        });
    }
    pub(super) fn add_key(mut self, input: KeyInput) {
        if !self.begin() {
            return;
        }
        self.credential_failure.set(None);
        spawn(async move {
            match self
                .session()
                .call(Request::AddApiKey {
                    version: CREDENTIALS_SCHEMA_VERSION,
                    name: input.name,
                    label: input.label,
                    destinations: input.destinations,
                    value: input.value,
                })
                .await
            {
                Ok(Reply::Ok) => {
                    self.key_editor.set(false);
                    self.notice
                        .set(Some("API key saved. Each agent use requires approval."));
                    self.credential_page.set(None);
                }
                Ok(_) => self.credential_failure.set(Some(failure(
                    "vault_add_key",
                    "API key save was not confirmed; refresh credentials before retrying",
                    &unexpected(),
                ))),
                Err(error) => {
                    if broker_refused(&error, BrokerError::StepUpRequired) {
                        self.flow.set(Flow::StepUp(Task::AddCredential));
                    }
                    self.credential_failure.set(Some(failure(
                        "vault_add_key",
                        save_failure_title(&error),
                        &error,
                    )));
                }
            }
            self.finish().await;
        });
    }
}
fn save_failure_title(error: &VaultError) -> &'static str {
    if error.code == BrokerError::StepUpRequired
        || matches!(
            error.report.kind,
            a206_core::ErrorKind::InvalidRequest
                | a206_core::ErrorKind::Conflict
                | a206_core::ErrorKind::PermissionDenied
        )
    {
        "API key was not saved. Correct the fields and re-enter the key."
    } else {
        "Could not confirm API key save; refresh credentials before retrying"
    }
}
fn valid_page(page: &CredentialPage) -> bool {
    page.version == CREDENTIALS_SCHEMA_VERSION
        && page.entries.len() <= usize::from(MAX_CREDENTIAL_PAGE)
        && page.total >= page.entries.len()
        && a206_protocol::encode(page)
            .is_ok_and(|bytes| bytes.len() <= a206_vault_channel::MAX_CREDENTIAL_PAGE_BYTES)
        && page.entries.windows(2).all(|pair| pair[0].0 < pair[1].0)
        && page
            .next
            .as_ref()
            .is_none_or(|next| page.entries.last().is_some_and(|(name, _)| name == next))
}
fn policy(policy: &AccessPolicy) -> String {
    match policy {
        AccessPolicy::Always => "No per-use condition".into(),
        AccessPolicy::WithApproval => "Approval per use".into(),
        AccessPolicy::WithAuth => "Fresh authentication".into(),
        AccessPolicy::SkillOnly(scopes) if scopes.is_empty() => "Locked: no skills".into(),
        AccessPolicy::TriggerOnly(scopes) if scopes.is_empty() => "Locked: no triggers".into(),
        AccessPolicy::SkillOnly(scopes) => format!("Skills: {scopes:?}"),
        AccessPolicy::TriggerOnly(scopes) => format!("Triggers: {scopes:?}"),
    }
}
fn kind(kind: CredentialKind) -> &'static str {
    match kind {
        CredentialKind::ApiKey => "API key",
        CredentialKind::HttpPasskey => "HTTP passkey",
        CredentialKind::UsernamePassword => "Login",
        CredentialKind::SshKey => "SSH key",
        CredentialKind::Token => "Token",
        CredentialKind::FormAutofill => "Form autofill",
        CredentialKind::PaymentMethod => "Payment method",
        CredentialKind::SecureNote => "Secure note",
        CredentialKind::Other => "Other",
    }
}
#[component]
pub(super) fn Credentials(
    panel: Panel,
    onread: EventHandler<Option<String>>,
    onadd: EventHandler<KeyInput>,
    onchange: EventHandler<(String, bool)>,
    ondestinations: EventHandler<(String, Vec<String>)>,
) -> Element {
    let mut editing = panel.destination_editor;
    let mut deleting = panel.delete_credential;
    let editor_origins =
        use_memo(move || editing().map_or_else(String::new, |draft| draft.origins));
    let busy = (panel.busy)();
    let page = (panel.credential_page)();
    let failed = (panel.credential_read_failure)().is_some();
    use_effect(move || {
        if !(panel.busy)()
            && (panel.credential_page)().is_none()
            && (panel.credential_read_failure)().is_none()
        {
            onread.call(None);
        }
    });
    let changing = busy
        || (panel.flow)() != Flow::Idle
        || (panel.key_editor)()
        || editing().is_some()
        || deleting().is_some()
        || (panel.policy_editor)().is_some();
    let available = if changing {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let rows = page
        .as_ref()
        .map(|page| {
            page.entries
                .iter()
                .map(|(name, meta)| TableRow {
                    key: name.clone(),
                    selected: false,
                    cells: vec![
                        TableCell::rich(
                            name,
                            rsx! { strong { "{meta.label}" } br {} code { "{name}" } },
                        ),
                        TableCell::text(kind(meta.kind)),
                        TableCell::rich(name, rsx! { span { class: "a206-policy-identifier", "{policy(&meta.policy)}" } }),
                        TableCell::text(if meta.destinations.is_empty() {
                            "No permitted destinations".into()
                        } else {
                            meta.destinations.join(", ")
                        }),
                        TableCell::text(if meta.disabled { "Disabled" } else { "Enabled" }),
                        TableCell::rich(name, rsx! {
                            CredentialStateAction {
                                name: name.clone(), label: meta.label.clone(), disabled: meta.disabled,
                                busy: changing, onrequest: onchange,
                            }
                            Button {
                                variant: ButtonVariant::Ghost,
                                availability: if changing { Availability::Disabled } else { Availability::Enabled },
                                onclick: {
                                    let name = name.clone();
                                    let destinations = meta.destinations.clone();
                                    move |_| editing.set(Some(DestinationDraft { name: name.clone(), origins: destinations.join(" ") }))
                                },
                                "Edit destinations for {meta.label}"
                            }
                            Button { variant: ButtonVariant::Ghost,
                                availability: if changing { Availability::Disabled } else { Availability::Enabled },
                                onclick: {
                                    let name = name.clone();
                                    let policy = meta.policy.clone();
                                    move |_| panel.policy_editor.set(Some(super::credential_policy::PolicyDraft::new(name.clone(), policy.clone())))
                                }, "Edit policy for {meta.label}"
                            }
                            Button { variant: ButtonVariant::Ghost, availability: available.clone(),
                                onclick: { let name = name.clone(); move |_| deleting.set(Some(name.clone())) },
                                "Delete {meta.label}"
                            }
                        }),
                    ],
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Credentials" }
            if let Some(page) = &page { Badge { "{page.total}" } }
        }
        CardBody {
            p { class: "a206-muted", "Values are never displayed. Agent access, destination restrictions and per-use policies all apply." }
            if let Some(page) = &page {
                Table { caption: rsx! { "Stored credential policies" }, compact: true, busy,
                    columns: vec![TableColumn::new("Credential").row_header(), TableColumn::new("Kind"), TableColumn::new("Policy"), TableColumn::new("Destinations"), TableColumn::new("Status"), TableColumn::new("Action")], rows,
                    empty: rsx! { "No typed credentials on this page." },
                }
                if page.legacy_count > 0 { p { "{page.legacy_count} legacy entries are available through the trusted local CLI." } }
            }
            if (panel.key_editor)() {
                KeyForm { busy, needs_auth: !(panel.status)().is_some_and(|s| s.step_up_active) || (panel.flow)() != Flow::Idle,
                    onadd, oncancel: move |_| panel.key_editor.set(false), onauthenticate: move |_| panel.start(Task::AddCredential) }
            }
            if let Some(draft) = editing() {
                DestinationForm { key: "{draft.name}", name: draft.name, origins: ReadSignal::from(editor_origins), busy: busy || (panel.flow)() != Flow::Idle,
                    oninput: move |origins| { if let Some(draft) = editing.write().as_mut() { draft.origins = origins; } },
                    onsave: ondestinations,
                    oncancel: move |_| editing.set(None),
                }
            }
            if (panel.policy_editor)().is_some() {
                super::credential_policy::PolicyForm { draft: panel.policy_editor, busy: busy || (panel.flow)() != Flow::Idle,
                    onsave: move |(name, policy)| panel.start(Task::SetCredentialPolicy { name, policy }),
                    oncancel: move |_| panel.policy_editor.set(None),
                }
            }
            if let Some(name) = deleting() {
                DeleteConfirmation { name, busy: busy || (panel.flow)() != Flow::Idle,
                    ondelete: move |name| panel.start(Task::DeleteCredential { name }),
                    oncancel: move |_| deleting.set(None),
                }
            }
        }
        CardFooter {
            CredentialPaging { unavailable: busy || (panel.flow)() != Flow::Idle || (panel.key_editor)(), editing: editing().is_some() || (panel.policy_editor)().is_some() || deleting().is_some(), failed,
                next: page.as_ref().and_then(|p| p.next.clone()), onread }
            if page.is_some() && !(panel.key_editor)() && editing().is_none() && (panel.policy_editor)().is_none() && deleting().is_none() {
                Button { variant: ButtonVariant::Secondary, availability: available, onclick: move |_| panel.start(Task::AddCredential), "Add API key" }
            }
        }
    } }
}

#[component]
fn DeleteConfirmation(
    name: String,
    busy: bool,
    ondelete: EventHandler<String>,
    oncancel: EventHandler<()>,
) -> Element {
    let available = if busy {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    rsx! {
        h3 { "Delete this credential?" }
        code { class: "a206-policy-identifier", "{name:?}" }
        p { "Permanently removes this stored credential and its restrictions. This cannot be undone here. Applications using it may stop working; other credentials and global agent access are unchanged." }
        Button { variant: ButtonVariant::Primary, availability: available.clone(), onclick: move |_| ondelete.call(name.clone()), "Delete credential permanently" }
        Button { variant: ButtonVariant::Ghost, availability: available, onclick: move |_| oncancel.call(()), "Keep credential" }
    }
}

#[component]
fn CredentialPaging(
    unavailable: bool,
    editing: bool,
    failed: bool,
    next: Option<String>,
    onread: EventHandler<Option<String>>,
) -> Element {
    let available = if unavailable || editing {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    rsx! {
        Button { variant: ButtonVariant::Ghost, availability: available.clone(), onclick: move |_| onread.call(None), if failed { "Retry credentials" } else { "Refresh credentials" } }
        if let Some(next) = next {
            Button { variant: ButtonVariant::Ghost, availability: available, onclick: move |_| onread.call(Some(next.clone())), "Next page" }
        }
    }
}

#[component]
fn DestinationForm(
    name: String,
    origins: ReadSignal<String>,
    busy: bool,
    oninput: EventHandler<String>,
    onsave: EventHandler<(String, Vec<String>)>,
    oncancel: EventHandler<()>,
) -> Element {
    let available = if busy {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    rsx! {
        h3 { "Edit permitted destinations" }
        code { "{name}" }
        Field { label: rsx! { "New permitted destinations" },
            hint: rsx! { "Replaces the complete destination list. Separate exact origins with spaces, without paths, queries or fragments. Leave empty to prevent brokered use. Policy and enabled state stay unchanged." },
            Input { availability: available.clone(), value: origins, oninput }
        }
        Button { variant: ButtonVariant::Primary, availability: available.clone(),
            onclick: move |_| onsave.call((name.clone(), origins.read().split_whitespace().map(str::to_owned).collect())),
            "Save destinations"
        }
        Button { variant: ButtonVariant::Ghost, availability: available, onclick: move |_| oncancel.call(()), "Cancel destinations" }
    }
}

#[component]
fn CredentialStateAction(
    name: String,
    label: String,
    disabled: bool,
    busy: bool,
    onrequest: EventHandler<(String, bool)>,
) -> Element {
    rsx! {
        Button {
            variant: ButtonVariant::Ghost,
            availability: if busy { Availability::Disabled } else { Availability::Enabled },
            onclick: move |_| onrequest.call((name.clone(), !disabled)),
            if disabled { "Enable {label}" } else { "Disable {label}" }
        }
    }
}
#[component]
fn KeyForm(
    busy: bool,
    needs_auth: bool,
    onadd: EventHandler<KeyInput>,
    oncancel: EventHandler<()>,
    onauthenticate: EventHandler<()>,
) -> Element {
    let mut name = use_signal(String::new);
    let mut label = use_signal(String::new);
    let mut origins = use_signal(String::new);
    let mut value = use_signal(String::new);
    // Clear the input buffer on cancellation, lock or navigation as well as
    // submission. The form never retains a key for an authentication retry.
    use_drop(move || {
        if let Ok(mut buffer) = value.try_write() {
            buffer.zeroize();
        }
        // A disposed signal is already gone; no input buffer remains to clear.
    });
    let availability = if busy || needs_auth {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let ready = !busy
        && !needs_auth
        && !name.read().trim().is_empty()
        && !label.read().trim().is_empty()
        && !value.read().is_empty();
    rsx! {
        Field { label: rsx! { "Credential name" }, hint: rsx! { "A unique vault identifier. Existing credentials cannot be replaced here." },
            Input { availability: availability.clone(), value: ReadSignal::from(name), oninput: move |v| name.set(v) }
        }
        Field { label: rsx! { "Label" }, Input { availability: availability.clone(), value: ReadSignal::from(label), oninput: move |v| label.set(v) } }
        Field { label: rsx! { "Permitted origins" }, hint: rsx! { "Separate exact HTTPS origins with spaces. No paths or queries. Leave empty to prevent brokered use." },
            Input { availability: availability.clone(), value: ReadSignal::from(origins), oninput: move |v| origins.set(v) }
        }
        Field { label: rsx! { "API key" }, hint: rsx! { "Cleared after every submit. Re-enter it after correcting an error. Saved with approval required for every agent use." },
            Input { availability: availability.clone(), r#type: InputType::Password, value: ReadSignal::from(value), oninput: move |v| { let mut buffer = value.write(); buffer.zeroize(); *buffer = v; }, autocomplete: "off" }
        }
        if needs_auth { Button { variant: ButtonVariant::Secondary, availability: if busy { Availability::Disabled } else { Availability::Enabled }, onclick: move |_| onauthenticate.call(()), "Authenticate to continue" } }
        Button { variant: ButtonVariant::Primary, availability: if ready { Availability::Enabled } else { Availability::Disabled },
            onclick: move |_| {
                let input = KeyInput { name: name(), label: label(), destinations: origins.read().split_whitespace().map(str::to_owned).collect(), value: WireSecret::new(take(value)) };
                onadd.call(input);
            }, "Save API key" }
        Button { variant: ButtonVariant::Ghost, onclick: move |_| { value.write().zeroize(); oncancel.call(()); }, "Cancel API key" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deletion_confirmation_names_exact_target_and_disables_both_actions_while_busy() {
        fn root((name, busy): (String, bool)) -> Element {
            rsx! { DeleteConfirmation { name, busy, ondelete: move |_| {}, oncancel: move |_| {} } }
        }
        let name = format!("  {}  ", uuid::Uuid::new_v4());
        for busy in [false, true] {
            let mut dom = VirtualDom::new_with_props(root, (name.clone(), busy));
            dom.rebuild_in_place();
            let html = dioxus_ssr::render(&dom);
            assert!(html.contains(&name), "{html}");
            assert!(
                html.contains("Delete credential permanently") && html.contains("Keep credential"),
                "{html}"
            );
            assert_eq!(
                html.matches(" disabled=true").count(),
                if busy { 2 } else { 0 },
                "{html}"
            );
        }
    }
    #[test]
    fn scoped_policy_labels_quote_exact_identifiers_including_whitespace() {
        let identifier = format!("  {}  ", uuid::Uuid::new_v4());
        for policy_value in [
            AccessPolicy::SkillOnly(vec![identifier.clone()]),
            AccessPolicy::TriggerOnly(vec![identifier.clone()]),
        ] {
            assert!(policy(&policy_value).contains(&format!("{identifier:?}")));
        }
    }
    #[test]
    fn invalid_destinations_and_uncertain_writes_offer_different_recovery() {
        let error = |kind| VaultError {
            code: BrokerError::Other,
            report: ErrorReport::new(kind, "Not confirmed"),
        };
        assert!(
            destination_failure_title(&error(a206_core::ErrorKind::InvalidRequest))
                .contains("correct the origins")
        );
        for kind in [
            a206_core::ErrorKind::Unavailable,
            a206_core::ErrorKind::Protocol,
            a206_core::ErrorKind::Storage,
        ] {
            assert!(
                destination_failure_title(&error(kind)).contains("cancel the edit and refresh")
            );
        }
    }
    #[test]
    fn paging_waits_for_a_draft_to_be_saved_or_cancelled() {
        fn root((unavailable, editing): (bool, bool)) -> Element {
            rsx! { CredentialPaging { unavailable, editing, failed: false, next: Some(uuid::Uuid::new_v4().to_string()), onread: move |_| {} } }
        }
        for unavailable in [false, true] {
            for editing in [false, true] {
                let mut dom = VirtualDom::new_with_props(root, (unavailable, editing));
                dom.rebuild_in_place();
                let html = dioxus_ssr::render(&dom);
                assert!(
                    html.contains("Refresh credentials") && html.contains("Next page"),
                    "{html}"
                );
                assert_eq!(
                    html.matches(" disabled=true").count(),
                    if unavailable || editing { 2 } else { 0 },
                    "{html}"
                );
            }
        }
    }
    #[test]
    fn destination_editor_shows_current_origins_and_disables_controls_while_busy() {
        fn root((name, busy): (String, bool)) -> Element {
            let origins = use_signal(|| {
                "https://one.example.invalid ssh://two.example.invalid:22".to_string()
            });
            rsx! { DestinationForm {
                name, origins, busy, oninput: move |_| {},
                onsave: move |_| {}, oncancel: move |_| {},
            } }
        }
        let name = uuid::Uuid::new_v4().to_string();
        for busy in [false, true] {
            let mut dom = VirtualDom::new_with_props(root, (name.clone(), busy));
            dom.rebuild_in_place();
            let html = dioxus_ssr::render(&dom);
            assert!(html.contains(&name), "{html}");
            assert!(
                html.contains("https://one.example.invalid ssh://two.example.invalid:22"),
                "{html}"
            );
            assert_eq!(
                html.matches(" disabled=true").count(),
                if busy { 3 } else { 0 },
                "{html}"
            );
        }
    }
    #[test]
    fn credential_state_actions_name_the_credential_and_disable_while_busy() {
        fn root((disabled, busy, label): (bool, bool, String)) -> Element {
            rsx! { CredentialStateAction { name: "metadata-key".to_string(), label, disabled, busy, onrequest: move |_| {} } }
        }
        let label = uuid::Uuid::new_v4().to_string();
        for disabled in [false, true] {
            for busy in [false, true] {
                let mut dom = VirtualDom::new_with_props(root, (disabled, busy, label.clone()));
                dom.rebuild_in_place();
                let html = dioxus_ssr::render(&dom);
                let action = if disabled { "Enable" } else { "Disable" };
                assert!(html.contains(&format!("{action} {label}")), "{html}");
                assert_eq!(html.contains(" disabled=true"), busy, "{html}");
            }
        }
    }
    #[test]
    fn confirmed_rejections_and_uncertain_saves_have_different_recovery() {
        for kind in [
            a206_core::ErrorKind::InvalidRequest,
            a206_core::ErrorKind::Conflict,
            a206_core::ErrorKind::PermissionDenied,
        ] {
            let error = VaultError {
                code: BrokerError::Other,
                report: ErrorReport::new(kind, "Rejected"),
            };
            assert!(save_failure_title(&error).contains("Correct the fields"));
        }
        for kind in [
            a206_core::ErrorKind::Protocol,
            a206_core::ErrorKind::Unavailable,
            a206_core::ErrorKind::Storage,
        ] {
            let error = VaultError {
                code: BrokerError::Other,
                report: ErrorReport::new(kind, "Unconfirmed"),
            };
            assert!(save_failure_title(&error).contains("refresh credentials"));
        }
        let error = VaultError {
            code: BrokerError::StepUpRequired,
            report: ErrorReport::new(a206_core::ErrorKind::Other, "Authentication required"),
        };
        assert!(save_failure_title(&error).contains("re-enter the key"));
    }
    #[test]
    fn pages_reject_unknown_schema_bad_order_and_fabricated_cursors() {
        let mut page = CredentialPage {
            version: CREDENTIALS_SCHEMA_VERSION,
            entries: vec![],
            next: None,
            total: 0,
            legacy_count: 0,
        };
        assert!(valid_page(&page));
        page.next = Some(uuid::Uuid::new_v4().to_string());
        assert!(!valid_page(&page));
        page.next = None;
        page.version += 1;
        assert!(!valid_page(&page));
        page.version = CREDENTIALS_SCHEMA_VERSION;
        let meta = a206_vault_channel::CredentialMeta {
            label: uuid::Uuid::new_v4().to_string(),
            kind: CredentialKind::ApiKey,
            policy: AccessPolicy::WithApproval,
            description: None,
            disabled: false,
            destinations: vec![],
        };
        let first = format!("a-{}", uuid::Uuid::new_v4());
        let last = format!("z-{}", uuid::Uuid::new_v4());
        page.total = 2;
        page.entries = vec![(last.clone(), meta.clone()), (first.clone(), meta.clone())];
        assert!(!valid_page(&page));
        page.entries = vec![(first.clone(), meta.clone()), (last.clone(), meta)];
        assert!(valid_page(&page));
        page.next = Some(first);
        assert!(!valid_page(&page));
        page.next = Some(last);
        assert!(valid_page(&page));
    }
    #[test]
    fn empty_scopes_are_shown_locked_and_always_keeps_other_restrictions() {
        assert_eq!(
            policy(&AccessPolicy::SkillOnly(vec![])),
            "Locked: no skills"
        );
        assert_eq!(
            policy(&AccessPolicy::TriggerOnly(vec![])),
            "Locked: no triggers"
        );
        assert_eq!(policy(&AccessPolicy::Always), "No per-use condition");
        let name = uuid::Uuid::new_v4().to_string();
        assert!(policy(&AccessPolicy::SkillOnly(vec![name.clone()])).contains(&name));
    }
}
