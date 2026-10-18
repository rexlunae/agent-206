//! Migration uses the same private broker session as enrollment. Event handlers
//! belong to the parent panel so removing this child cannot hide its outcome.
use super::*;
use a206_vault_channel::{IMPORT_REVIEW_VERSION, ImportReview};

fn reviewed(reply: Reply) -> Result<ImportReview, VaultError> {
    match reply {
        Reply::VaultImportReview(review) if review.version == IMPORT_REVIEW_VERSION => Ok(review),
        Reply::Error(error) => Err(error),
        _ => Err(unexpected()),
    }
}
fn can_apply(
    review: Option<&ImportReview>,
    password: &str,
    code: &str,
    needs_code: bool,
    busy: bool,
) -> bool {
    !busy
        && review.is_some_and(|r| r.version == IMPORT_REVIEW_VERSION)
        && !password.is_empty()
        && (!needs_code || !code.trim().is_empty())
}
fn retryable(error: &VaultError) -> bool {
    matches!(
        error.code,
        BrokerError::TotpRequired
            | BrokerError::TotpReplayed
            | BrokerError::AuthenticationFailed
            | BrokerError::ClockDrift
    )
}
impl Panel {
    pub(super) fn preview_import(mut self, source: String) {
        if !self.begin() {
            return;
        }
        self.import_review.set(None);
        self.import_totp.set(false);
        // Deliberately detached event task, owned by the parent Dioxus panel.
        // Unmount cancels waiting; the broker has at most one connection review.
        spawn(async move {
            match self
                .session()
                .call(Request::ReviewVaultImport {
                    version: IMPORT_REVIEW_VERSION,
                    source: source.trim().into(),
                })
                .await
                .and_then(reviewed)
            {
                Ok(review) => self.import_review.set(Some(review)),
                Err(error) => self.fail(
                    "vault_import_review",
                    "Could not review the source vault",
                    &error,
                ),
            }
            self.finish().await;
        });
    }
    pub(super) fn apply_import(mut self, password: WireSecret, code: Option<WireSecret>) {
        let Some(review) = self.import_review.peek().clone() else {
            return;
        };
        if !self.begin() {
            return;
        }
        // Parent-scope task survives removal of the migration child after a
        // status change. Panel unmount cancels waiting, not a sent broker write;
        // the next status is authoritative. No password is retained for retries.
        spawn(async move {
            match self
                .session()
                .call(Request::ApplyVaultImport {
                    version: IMPORT_REVIEW_VERSION,
                    review: review.id,
                    password,
                    totp: code,
                })
                .await
            {
                Ok(Reply::Ok) => self.after_unlock(true).await,
                Ok(_) => self.fail(
                    "vault_import",
                    "Vault import was not confirmed",
                    &unexpected(),
                ),
                Err(error) => {
                    if error.code == BrokerError::TotpRequired {
                        self.import_totp.set(true);
                    }
                    if !retryable(&error) {
                        self.import_review.set(None);
                        self.import_totp.set(false);
                    }
                    self.fail("vault_import", "Vault import was not confirmed", &error);
                }
            }
            self.finish().await;
        });
    }
    pub(super) fn cancel_import(mut self) {
        if !self.begin() {
            return;
        }
        self.import_review.set(None);
        self.import_totp.set(false);
        // Detached from the event handler; the parent panel owns this task.
        spawn(async move {
            match self
                .session()
                .call(Request::CancelVaultImport {
                    version: IMPORT_REVIEW_VERSION,
                })
                .await
            {
                Ok(Reply::Ok) => {}
                Ok(_) => self.fail(
                    "vault_import_cancel",
                    "Could not cancel vault review",
                    &unexpected(),
                ),
                Err(error) => self.fail(
                    "vault_import_cancel",
                    "Could not cancel vault review",
                    &error,
                ),
            }
            self.finish().await;
        });
    }
}

#[component]
pub(super) fn Migration(
    panel: Panel,
    onpreview: EventHandler<String>,
    onapply: EventHandler<(WireSecret, Option<WireSecret>)>,
    oncancel: EventHandler<()>,
) -> Element {
    let mut source = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut code = use_signal(String::new);
    let busy = (panel.busy)();
    let availability = if busy {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let review = (panel.import_review)();
    let needs_code = (panel.import_totp)();
    let enabled = can_apply(review.as_ref(), &password(), &code(), needs_code, busy);
    rsx! { Disclosure { summary: rsx! { "Import a RustyClaw vault" },
        p { "Import a whole password-protected vault before creating a new one. The source stays in place. Password, authenticator enrollment, credential policies and disabled flags are preserved. An existing vault is never replaced or merged. Key-file vaults are not supported yet." }
        if let Some(review) = review {
            DescriptionList {
                DescriptionItem { term: rsx! { "Source on broker host" }, code { "{review.source.display()}" } }
                DescriptionItem { term: rsx! { "Bytes" }, "{review.bytes}" }
                DescriptionItem { term: rsx! { "SHA-256" }, code { class: "a206-number", {review.sha256.iter().map(|b| format!("{b:02x}")).collect::<String>()} } }
            }
            p { class: "a206-muted", "This identifies the file; its password and any enrolled authenticator are verified on a private copy when imported. A source edit or lost connection requires another preview. Connections and tool grants are not imported here." }
            Field { label: rsx! { "Source vault password" }, Input { r#type: InputType::Password, autocomplete: "current-password", availability: availability.clone(), value: ReadSignal::from(password), oninput: move |v| password.set(v) } }
            Field { label: rsx! { "Source authenticator code" }, hint: rsx! { if needs_code { "Required. Enter the source password again and its current authenticator code." } else { "Enter the current code if the source vault has an authenticator." } },
                Input { availability: availability.clone(), autocomplete: "one-time-code", inputmode: "numeric", value: ReadSignal::from(code), oninput: move |v| code.set(v) }
            }
            div { class: "a206-row",
                Button { variant: ButtonVariant::Primary, availability: if enabled { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| {
                    if !can_apply(panel.import_review.peek().as_ref(), &password.peek(), &code.peek(), *panel.import_totp.peek(), *panel.busy.peek()) { return; }
                    let password = WireSecret::new(take(password));
                    let code = take(code);
                    onapply.call((password, (!code.trim().is_empty()).then(|| WireSecret::new(code))));
                }, "Import reviewed vault" }
                Button { variant: ButtonVariant::Ghost, availability: availability.clone(), onclick: move |_| { take(password); take(code); oncancel.call(()); }, "Cancel vault review" }
            }
        } else {
            Field { label: rsx! { "RustyClaw vault file" }, hint: rsx! { "Absolute file path on the broker host, usually credentials/secrets.json under your RustyClaw directory. This is not a browser upload." },
                Input { availability: availability.clone(), value: ReadSignal::from(source), oninput: move |v| source.set(v), placeholder: "Absolute path to the encrypted vault" }
            }
            Button { variant: ButtonVariant::Secondary, availability: if busy || source.read().trim().is_empty() { Availability::Disabled } else { Availability::Enabled }, onclick: move |_| { take(password); take(code); onpreview.call(source()); }, "Preview vault import" }
        }
    } }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_versions_and_apply_gates_fail_closed() {
        let mut review = ImportReview {
            version: IMPORT_REVIEW_VERSION,
            id: a206_core::Id::new_v4(),
            source: "/source".into(),
            bytes: 1,
            sha256: [0; 32],
        };
        assert!(!can_apply(None, "password", "", false, false));
        assert!(!can_apply(Some(&review), "", "", false, false));
        assert!(!can_apply(Some(&review), "password", "", true, false));
        assert!(!can_apply(Some(&review), "password", "code", true, true));
        assert!(can_apply(Some(&review), "password", "code", true, false));
        review.version = 0;
        assert!(!can_apply(Some(&review), "password", "code", true, false));
        assert!(reviewed(Reply::VaultImportReview(review)).is_err());
    }
    #[test]
    fn source_changes_are_not_authentication_retries() {
        let error = |code| VaultError {
            code,
            report: a206_core::ErrorReport::new(a206_core::ErrorKind::Conflict, "source refused"),
        };
        assert!(!retryable(&error(BrokerError::ImportSourceChanged)));
        assert!(!retryable(&error(BrokerError::ImportReviewUnavailable)));
        assert!(retryable(&error(BrokerError::TotpRequired)));
    }
}
