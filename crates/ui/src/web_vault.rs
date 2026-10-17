//! The Security page's vault card in the web client. The vault lives with the
//! web server, and its password and enrollment key never pass through a
//! browser, so this card shows the vault's state and can lock it; unlocking,
//! creating, enrolling and pairing stay with the desktop and terminal apps.
use crate::error::{ErrorNotice, record};
use a206_core::{ErrorReport, VaultReply, VaultRequest, VaultState};
use fc_dioxus::prelude::*;

fn known(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "Yes",
        Some(false) => "No",
        None => "Unknown while locked",
    }
}

/// What the bridge said, or why it could not say.
#[derive(Clone, PartialEq)]
enum Shown {
    Checking,
    State(VaultState),
    Remote(String),
}

async fn ask(request: VaultRequest) -> Result<VaultReply, ErrorReport> {
    crate::web_request("/api/vault", request).await
}

#[component]
pub(crate) fn WebVault() -> Element {
    let mut shown = use_signal(|| Shown::Checking);
    let mut failure = use_signal(|| None::<(&'static str, ErrorReport)>);
    let mut notice = use_signal(|| None::<&'static str>);
    let mut busy = use_signal(|| false);
    let mut run = move |request: VaultRequest| {
        if *busy.peek() {
            return;
        }
        busy.set(true);
        failure.set(None);
        notice.set(None);
        spawn(async move {
            let title = match request {
                VaultRequest::Status => "Could not read the vault state",
                VaultRequest::Lock => "Could not lock the vault",
            };
            match ask(request).await {
                Ok(VaultReply::State(state)) => {
                    if request == VaultRequest::Lock {
                        notice.set(Some("Vault locked."));
                    }
                    shown.set(Shown::State(state));
                }
                Ok(VaultReply::Remote { endpoint }) => shown.set(Shown::Remote(endpoint)),
                Ok(VaultReply::Error(report)) | Err(report) => {
                    record("vault", &report);
                    failure.set(Some((title, report)));
                }
            }
            busy.set(false);
        });
    };
    // Reads only through `peek`, so this runs once, after the first render.
    use_effect(move || run(VaultRequest::Status));
    let busy_now = busy();
    let availability = if busy_now {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    if let Shown::Remote(endpoint) = shown() {
        return rsx! { Card { variant: CardVariant::Subtle,
            CardHeader { CardTitle { "Vault" } Badge { tone: BadgeTone::Neutral, "Remote gateway" } }
            CardBody {
                p { "This web server uses the gateway at {endpoint}. Its vault broker runs on that host." }
                p { class: "a206-muted", "Run `agent206 vault unlock` and `agent206 vault totp enroll` in a terminal on that host." }
            }
        } };
    }
    let state = match shown() {
        Shown::State(state) => Some(state),
        _ => None,
    };
    let (label, tone) = match &state {
        None => ("Checking", BadgeTone::Neutral),
        Some(s) if !s.exists => ("No vault", BadgeTone::Warning),
        Some(s) if !s.unlocked => ("Locked", BadgeTone::Warning),
        Some(_) => ("Unlocked", BadgeTone::Success),
    };
    let unlocked = state.as_ref().is_some_and(|s| s.unlocked);
    rsx! { Card { variant: CardVariant::Feature,
        CardHeader { CardTitle { "Vault" } Badge { tone, dot: true, "{label}" } }
        CardBody {
            if let Some(state) = &state {
                DescriptionList {
                    DescriptionItem { term: rsx! { "Encrypted vault" }, if state.exists { "Created" } else { "Not created yet" } }
                    DescriptionItem { term: rsx! { "Two-factor authentication" }, "{known(state.totp_enrolled)}" }
                    DescriptionItem { term: rsx! { "Agent credential access" }, "{known(state.agent_access)}" }
                    DescriptionItem { term: rsx! { "Gateway paired" }, "{known(state.gateway_paired)}" }
                }
            } else if failure().is_none() {
                p { class: "a206-muted", role: "status", "Checking the vault…" }
            }
            if let Some(message) = notice() {
                Alert { tone: AlertTone::Success, urgency: Urgency::Polite, "{message}" }
            }
            if let Some((title, report)) = failure() {
                ErrorNotice { report, title: title.to_string() }
            }
            p { class: "a206-muted", "Create, unlock and pair the vault, and enroll two-factor authentication, from the desktop or terminal app on the web server's computer, or with `agent206 vault` there. The browser never handles the vault password." }
            if unlocked && crate::sign_in::network() {
                p { class: "a206-muted", "Locking also stops new sign-ins from other devices until the vault is unlocked on that computer." }
            }
        }
        CardFooter {
            if unlocked {
                Button { variant: ButtonVariant::Primary, availability: availability.clone(), busy: busy_now,
                    onclick: move |_| run(VaultRequest::Lock), "Lock vault" }
            }
            Button { variant: ButtonVariant::Ghost, availability, onclick: move |_| run(VaultRequest::Status), "Check again" }
        }
    } }
}
