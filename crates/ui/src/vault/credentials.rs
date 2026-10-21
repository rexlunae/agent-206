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
        AccessPolicy::SkillOnly(scopes) => format!("Skills: {}", scopes.join(", ")),
        AccessPolicy::TriggerOnly(scopes) => format!("Triggers: {}", scopes.join(", ")),
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
) -> Element {
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
    let available = if busy {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let changing = busy || (panel.flow)() != Flow::Idle || (panel.key_editor)();
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
                        TableCell::text(policy(&meta.policy)),
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
        }
        CardFooter {
            Button { variant: ButtonVariant::Ghost, availability: available.clone(), onclick: move |_| onread.call(None), if failed { "Retry credentials" } else { "Refresh credentials" } }
            if let Some(next) = page.as_ref().and_then(|p| p.next.clone()) {
                Button { variant: ButtonVariant::Ghost, availability: available.clone(), onclick: move |_| onread.call(Some(next.clone())), "Next page" }
            }
            if page.is_some() && !(panel.key_editor)() {
                Button { variant: ButtonVariant::Secondary, availability: available, onclick: move |_| panel.start(Task::AddCredential), "Add API key" }
            }
        }
    } }
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
