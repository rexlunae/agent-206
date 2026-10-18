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
        self.credential_failure.set(None);
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
                Ok(_) => self.credential_failure.set(Some(failure(
                    "vault_credentials",
                    "Could not read credentials",
                    &unexpected(),
                ))),
                Err(error) => self.credential_failure.set(Some(failure(
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
        self.key_editor.set(false);
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
                    self.notice
                        .set(Some("API key saved. Each agent use requires approval."));
                    self.credential_page.set(None);
                }
                Ok(_) => self.credential_failure.set(Some(failure(
                    "vault_add_key",
                    "API key save was not confirmed; refresh credentials before retrying",
                    &unexpected(),
                ))),
                Err(error) => self.credential_failure.set(Some(failure(
                    "vault_add_key",
                    "Could not confirm API key save; refresh credentials before retrying",
                    &error,
                ))),
            }
            self.finish().await;
        });
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
) -> Element {
    let busy = (panel.busy)();
    let page = (panel.credential_page)();
    let failed = (panel.credential_failure)().is_some();
    use_effect(move || {
        if !(panel.busy)()
            && (panel.credential_page)().is_none()
            && (panel.credential_failure)().is_none()
        {
            onread.call(None);
        }
    });
    let available = if busy {
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
                        TableCell::text(policy(&meta.policy)),
                        TableCell::text(if meta.destinations.is_empty() {
                            "No permitted destinations".into()
                        } else {
                            meta.destinations.join(", ")
                        }),
                        TableCell::text(if meta.disabled { "Disabled" } else { "Enabled" }),
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
                    columns: vec![TableColumn::new("Credential").row_header(), TableColumn::new("Kind"), TableColumn::new("Policy"), TableColumn::new("Destinations"), TableColumn::new("Status")], rows,
                    empty: rsx! { "No typed credentials on this page." },
                }
                if page.legacy_count > 0 { p { "{page.legacy_count} legacy entries are available through the trusted local CLI." } }
            }
            if (panel.key_editor)() {
                KeyForm { busy, onadd, oncancel: move |_| panel.key_editor.set(false) }
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
fn KeyForm(busy: bool, onadd: EventHandler<KeyInput>, oncancel: EventHandler<()>) -> Element {
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
    let ready = !busy
        && !name.read().trim().is_empty()
        && !label.read().trim().is_empty()
        && !value.read().is_empty();
    rsx! {
        Field { label: rsx! { "Credential name" }, hint: rsx! { "A unique vault identifier. Existing credentials cannot be replaced here." },
            Input { value: ReadSignal::from(name), oninput: move |v| name.set(v) }
        }
        Field { label: rsx! { "Label" }, Input { value: ReadSignal::from(label), oninput: move |v| label.set(v) } }
        Field { label: rsx! { "Permitted origins" }, hint: rsx! { "Separate exact HTTPS origins with spaces. No paths or queries. Leave empty to prevent brokered use." },
            Input { value: ReadSignal::from(origins), oninput: move |v| origins.set(v) }
        }
        Field { label: rsx! { "API key" }, hint: rsx! { "Saved encrypted with approval required for every agent use." },
            Input { r#type: InputType::Password, value: ReadSignal::from(value), oninput: move |v| { let mut buffer = value.write(); buffer.zeroize(); *buffer = v; }, autocomplete: "off" }
        }
        Button { variant: ButtonVariant::Primary, availability: if ready { Availability::Enabled } else { Availability::Disabled },
            onclick: move |_| {
                let input = KeyInput { name: take(name), label: take(label), destinations: take(origins).split_whitespace().map(str::to_owned).collect(), value: WireSecret::new(take(value)) };
                onadd.call(input);
            }, "Save API key" }
        Button { variant: ButtonVariant::Ghost, onclick: move |_| { value.write().zeroize(); oncancel.call(()); }, "Cancel API key" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
