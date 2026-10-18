//! Shared user review of a gateway-host installation. No secret inputs.
use crate::{PageFrame, call, error::ErrorNotice};
use a206_core::{Command, ErrorKind, ErrorReport, Reply, imports::*};
use fc_dioxus::prelude::*;

fn reviewed(reply: Reply) -> Result<Review, ErrorReport> {
    match reply {
        Reply::ImportPreview(review) if review.version == REVIEW_VERSION => Ok(*review),
        Reply::ImportPreview(_) => Err(ErrorReport::new(
            ErrorKind::Protocol,
            "Gateway import review version is unsupported. Update the gateway and preview again.",
        )),
        Reply::Error(error) => Err(error),
        _ => Err(ErrorReport::new(
            ErrorKind::Protocol,
            "Gateway did not return an import preview.",
        )),
    }
}
fn applied(reply: Reply) -> Result<Applied, ErrorReport> {
    match reply {
        Reply::ImportApplied(result) => Ok(*result),
        Reply::Error(error) => Err(error),
        _ => Err(ErrorReport::new(
            ErrorKind::Protocol,
            "Gateway did not confirm the import. Refresh settings before retrying.",
        )),
    }
}

async fn finish_application(
    reply: Result<Reply, ErrorReport>,
    refresh: impl std::future::Future<Output = ()>,
) -> Result<Applied, ErrorReport> {
    let value = applied(reply?)?;
    // Publish confirmation only after the shared snapshot refresh has run.
    // Keep receipt failures in the confirmed partial result across the await.
    refresh.await;
    Ok(value)
}

#[component]
pub(crate) fn Imports() -> Element {
    let mut source = use_signal(String::new);
    let mut review = use_signal(|| None::<Review>);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut busy = use_signal(|| false);
    rsx! { PageFrame { kicker: "Settings", title: "Import", description: "Review settings from another installation before adding them.",
        Card { variant: CardVariant::Elevated,
            CardHeader { CardTitle { "RustyClaw source" } CardSubtitle { "Only RustyClaw is supported here. The path is on the gateway host, including when you connect remotely." } }
            CardBody { Field { label: rsx! { "Installation directory" }, hint: rsx! { "Enter an absolute path. Source files are read-only; nothing is activated during preview." },
                Input { value: ReadSignal::from(source), availability: if busy() { Availability::Disabled } else { Availability::Enabled }, placeholder: "/path/to/.rustyclaw", oninput: move |value| { source.set(value); review.set(None); failure.set(None); } }
            } }
            // The gateway validates its own host's path syntax. A web/TUI/
            // desktop client must not apply its platform's filesystem rules.
            CardFooter { Button { variant: ButtonVariant::Primary, availability: if busy() || source().trim().is_empty() { Availability::Disabled } else { Availability::Enabled }, onclick: move |_| {
                busy.set(true); failure.set(None); review.set(None);
                let source = source();
                spawn(async move {
                    match call(Command::PreviewImport { source: source.into() }).await.and_then(reviewed) {
                        Ok(result) => review.set(Some(result)), Err(error) => failure.set(Some(error)),
                    }
                    busy.set(false);
                });
            }, if busy() { "Reading source…" } else { "Preview import" } } }
        }
        if let Some(report) = failure() { ErrorNotice { report, title: "Import preview unavailable".to_string() } }
        if let Some(value) = review() { ReviewPanel { key: "{value.id}", review: value } }
        Alert { title: rsx! { "Credentials and other tools" }, "This screen imports selected connection settings and disabled messengers. It does not import vaults, plaintext credentials, grants, hooks, plugins or MCP servers. Password-protected RustyClaw vault migration is available through `agent206 import rustyclaw` on the trusted terminal. Other source adapters are still planned." }
    } }
}

#[component]
fn ReviewPanel(review: Review) -> Element {
    let state = use_context::<crate::State>();
    let selection = use_signal(Selection::default);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut busy = use_signal(|| false);
    let mut result = use_signal(|| None::<Applied>);
    let mut attempted = use_signal(|| false);
    let preview = &review.preview;
    let review_id = review.id;
    let enabled = !busy() && !attempted();
    let valid = selection().valid_for(preview);
    rsx! { Card {
        CardHeader { CardTitle { "Review import" } CardSubtitle { "{preview.source.display()}" } }
        CardBody { div { class: "a206-stack",
            p { "Choose each item to add. Existing accounts are kept. Messenger entries stay disabled and require a new vault credential before connecting." }
            p { class: "a206-muted", "Source labels, model text and credential references are omitted. Custom provider labels are generated. Providers have no credential reference; edit their settings with `agent206 settings --apply` before authenticated use." }
            Disclosure { summary: rsx! { "Source and review record" },
                p { "Configuration SHA-256: {preview.config_sha256.as_deref().unwrap_or(\"No configuration file\")}" }
                p { "Target settings revision: {review.settings_revision}" }
                p { "The gateway rereads the source and checks this revision before applying. A changed source or target requires a new preview." }
            }
            h3 { "Providers ({preview.providers.len()})" }
            if preview.providers.is_empty() { p { class: "a206-muted", "No provider settings found." } }
            for (index, provider) in preview.providers.iter().enumerate() {
                ProviderRow { key: "provider-{index}", provider: provider.clone(), index, selection, enabled }
            }
            h3 { "Messengers ({preview.messengers.len()})" }
            if preview.messengers.is_empty() { p { class: "a206-muted", "No messenger settings found." } }
            for (index, messenger) in preview.messengers.iter().enumerate() {
                MessengerRow { key: "messenger-{index}", messenger: messenger.clone(), index, selection, enabled }
            }
            if preview.vault.is_some() { Alert { title: rsx! { "Vault left unchanged" }, "An encrypted vault was found. Import it from the trusted terminal; this selection does not copy or unlock it." } }
            if !preview.inactive.is_empty() || !preview.unsupported.is_empty() {
                Disclosure { summary: rsx! { "Other data left behind" },
                    for item in &preview.inactive { p { "{inactive_label(item.category)}: {item.count}" } }
                    for item in &preview.unsupported { p { "{unsupported_label(*item)}" } }
                }
            }
            if let Some(report) = failure() { ErrorNotice { report, title: "Import not confirmed".to_string() } p { "Preview the source again before retrying." } }
            if let Some(applied) = result() {
                Alert { title: rsx! { "Settings imported" }, "Providers added: {applied.receipt.providers_added.len()}. Disabled messengers added: {applied.receipt.messengers_added.len()}. The vault was not changed." }
                if let Some(report) = applied.receipt_error { ErrorNotice { report, title: "Settings saved; import receipt failed".to_string() } }
                else { p { class: "a206-muted", "Receipt: imports/{applied.receipt.id}.toml on the gateway host." } }
            }
        } }
        CardFooter { Button { variant: ButtonVariant::Primary, availability: if enabled && valid { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| {
            busy.set(true); attempted.set(true); failure.set(None);
            let chosen = selection();
            spawn(async move {
                let response = call(Command::ApplyImport { review: review_id, selection: chosen }).await;
                match finish_application(response, crate::refresh(state)).await {
                    Ok(value) => result.set(Some(value)), Err(error) => failure.set(Some(error)),
                }
                busy.set(false);
            });
        }, if busy() { "Applying…" } else { "Import selected settings" } } }
    } }
}

#[component]
fn ProviderRow(
    provider: ProviderImport,
    index: usize,
    mut selection: Signal<Selection>,
    enabled: bool,
) -> Element {
    let checked = use_memo(move || {
        if selection.read().providers.contains(&index) {
            CheckState::On
        } else {
            CheckState::Off
        }
    });
    let available = provider.action == ProviderAction::Add;
    rsx! { div { class: "a206-stack",
        Checkbox { label: rsx! { "{provider.label}" }, checked: ReadSignal::from(checked), availability: if enabled && available { Availability::Enabled } else { Availability::Disabled },
            onchange: move |on| { let mut value = selection.write(); if on { value.providers.insert(index); } else { value.providers.remove(&index); } } }
        p { class: "a206-muted", "{provider.api.label()} · {provider.base_url}" }
        if !available { Badge { {provider_status(&provider.action)} } }
    } }
}

#[component]
fn MessengerRow(
    messenger: MessengerImport,
    index: usize,
    mut selection: Signal<Selection>,
    enabled: bool,
) -> Element {
    let checked = use_memo(move || {
        if selection.read().messengers.contains(&index) {
            CheckState::On
        } else {
            CheckState::Off
        }
    });
    let available = messenger.action == MessengerAction::Add;
    rsx! { div { class: "a206-stack",
        Checkbox { label: rsx! { "{messenger.name} · {messenger.source_kind}" }, checked: ReadSignal::from(checked), availability: if enabled && available { Availability::Enabled } else { Availability::Disabled },
            onchange: move |on| { let mut value = selection.write(); if on { value.messengers.insert(index); } else { value.messengers.remove(&index); } } }
        if available { Badge { "Will be imported disabled" } }
        if !available { Badge { {messenger_status(&messenger.action)} } }
        if let Some(endpoint) = &messenger.endpoint { p { class: "a206-muted", "{endpoint}" } }
        Disclosure { summary: rsx! { "Source restrictions: {messenger.allowed_chats.len()} chats · {messenger.allowed_users.len()} senders" },
            p { "Allowed chats: {messenger.allowed_chats.iter().cloned().collect::<Vec<_>>().join(\", \")}" }
            p { "Allowed senders: {messenger.allowed_users.iter().cloned().collect::<Vec<_>>().join(\", \")}" }
        }
        if messenger.token_left_behind { p { class: "a206-muted", "Plaintext source credentials are not copied. Imported entries need a vault credential before connecting; existing credentials stay unchanged." } }
    } }
}

fn provider_status(action: &ProviderAction) -> &'static str {
    match action {
        ProviderAction::Add => "Available",
        ProviderAction::AlreadyConfigured { .. } => "Already configured",
        ProviderAction::Unsupported { reason } => match reason {
            UnsupportedProvider::UnknownProvider => "Provider connector not supported",
            UnsupportedProvider::DeviceFlow => "Device-flow sign-in not imported",
            UnsupportedProvider::NoEndpoint => "No API endpoint provided",
            UnsupportedProvider::InvalidEndpoint => "API endpoint refused",
        },
    }
}
fn messenger_status(action: &MessengerAction) -> &'static str {
    match action {
        MessengerAction::Add => "Available",
        MessengerAction::AlreadyConfigured => "Already configured",
        MessengerAction::Unsupported { reason } => match reason {
            UnsupportedMessenger::UnknownKind => "Messenger not supported",
            UnsupportedMessenger::ExternalConfig => "Separate configuration not read",
            UnsupportedMessenger::InvalidChats => "Chat or sender restrictions cannot be preserved",
            UnsupportedMessenger::InvalidEndpoint => "Server endpoint refused",
            UnsupportedMessenger::PlainTextIrc => "Unencrypted IRC refused",
            UnsupportedMessenger::InvalidLogin => "IRC login refused",
            UnsupportedMessenger::NoSignalDaemon => {
                "Connect a user-managed signal-cli daemon separately"
            }
            UnsupportedMessenger::ThroughBeeper => "Connect this service through Beeper separately",
        },
    }
}
fn inactive_label(category: InactiveCategory) -> &'static str {
    match category {
        InactiveCategory::Messengers => "Messengers",
        InactiveCategory::McpServers => "MCP servers",
        InactiveCategory::ToolPermissions => "Tool permission rules",
        InactiveCategory::DisabledToolGroups => "Disabled tool groups",
        InactiveCategory::Services => "Services",
        InactiveCategory::Engines => "Engines",
        InactiveCategory::Sandbox => "Sandbox configuration",
        InactiveCategory::PlaintextToken => "Plaintext registry tokens",
        InactiveCategory::Skills => "Skills",
    }
}
fn unsupported_label(item: Unsupported) -> &'static str {
    match item {
        Unsupported::WorkspaceInstructions => "Workspace instructions",
        Unsupported::Conversations => "Conversation history",
        Unsupported::Memory => "Memory",
        Unsupported::SystemPrompt => "Messenger system prompt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unexpected_replies_preserve_failure_instead_of_claiming_an_import() {
        assert_eq!(reviewed(Reply::Ok).unwrap_err().kind, ErrorKind::Protocol);
        assert_eq!(applied(Reply::Ok).unwrap_err().kind, ErrorKind::Protocol);
        let failure = ErrorReport::new(ErrorKind::Conflict, "Source changed");
        assert_eq!(applied(Reply::Error(failure.clone())).unwrap_err(), failure);
    }
    #[test]
    fn an_unversioned_development_review_never_reaches_the_view() {
        let secret = uuid::Uuid::new_v4().to_string();
        let review = Review {
            version: 0,
            id: uuid::Uuid::new_v4(),
            settings_revision: 0,
            preview: Preview {
                version: PREVIEW_VERSION,
                tool: Tool::RustyClaw,
                source: secret.clone().into(),
                config_sha256: None,
                vault: None,
                providers: vec![],
                messengers: vec![],
                inactive: vec![],
                unsupported: vec![],
            },
        };
        let failure = reviewed(Reply::ImportPreview(Box::new(review))).unwrap_err();
        assert_eq!(failure.kind, ErrorKind::Protocol);
        assert!(!format!("{failure:?}").contains(&secret));
        assert!(failure.message.contains("Update the gateway"));
    }
    #[tokio::test]
    async fn confirmation_refreshes_shared_state_and_keeps_a_partial_receipt_error() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let refreshed = AtomicBool::new(false);
        let value = Applied {
            receipt: Receipt {
                version: PREVIEW_VERSION,
                id: uuid::Uuid::new_v4(),
                tool: Tool::RustyClaw,
                source: "/source".into(),
                config_sha256: None,
                applied_at: 0,
                providers_added: vec![uuid::Uuid::new_v4()],
                messengers_added: vec![],
                vault_imported: false,
            },
            receipt_error: Some(ErrorReport::new(ErrorKind::Storage, "Receipt write failed")),
        };
        let confirmed =
            finish_application(Ok(Reply::ImportApplied(Box::new(value.clone()))), async {
                refreshed.store(true, Ordering::SeqCst);
            })
            .await
            .unwrap();
        assert!(refreshed.load(Ordering::SeqCst));
        assert_eq!(confirmed, value);
        refreshed.store(false, Ordering::SeqCst);
        assert!(
            finish_application(Ok(Reply::Ok), async {
                refreshed.store(true, Ordering::SeqCst);
            })
            .await
            .is_err()
        );
        assert!(!refreshed.load(Ordering::SeqCst));
    }
}
