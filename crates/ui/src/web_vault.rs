//! Public vault status and locking before broker-key verification. This card
//! never requests secret values; the separate verified channel owns creation,
//! unlocking, enrollment and pairing through the shared Security panel.
use crate::error::{ErrorNotice, record};
use crate::welcome::{VaultFacts, VaultProgress, VaultSeen};
use a206_core::{ErrorReport, VaultReply, VaultRequest, VaultState};
use fc_dioxus::prelude::*;

fn known(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "Yes",
        Some(false) => "No",
        None => "Unknown while locked",
    }
}

/// The checklist's view of the vault.
fn facts(state: &VaultState) -> VaultFacts {
    VaultFacts {
        exists: state.exists,
        unlocked: state.unlocked,
        totp_enrolled: state.totp_enrolled,
        paired: state.gateway_paired,
    }
}

/// What the bridge said, or why it could not say.
#[derive(Clone, PartialEq)]
enum Shown {
    Checking,
    State(VaultState),
    /// Locked just now, but its state could not be read afterwards.
    Locked,
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
    let progress = VaultProgress::listener();
    let report = move |seen| {
        if let Some(progress) = progress {
            progress.report(seen);
        }
    };
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
            let reply = ask(request).await;
            match &reply {
                Ok(
                    VaultReply::State(state)
                    | VaultReply::Locked {
                        state: Some(state), ..
                    },
                ) => {
                    report(VaultSeen::Facts(facts(state)));
                }
                // Locked just now: it exists, and nothing inside it is known.
                Ok(VaultReply::Locked { state: None, .. }) => {
                    report(VaultSeen::Facts(VaultFacts {
                        exists: true,
                        unlocked: false,
                        totp_enrolled: None,
                        paired: None,
                    }))
                }
                Ok(VaultReply::Error(_)) | Err(_) => report(VaultSeen::Unreadable),
                Ok(VaultReply::Remote { .. }) => report(VaultSeen::Remote),
            }
            match reply {
                Ok(VaultReply::State(state)) => shown.set(Shown::State(state)),
                Ok(VaultReply::Locked { state, refresh }) => {
                    notice.set(Some("Vault locked."));
                    shown.set(state.map_or(Shown::Locked, Shown::State));
                    if let Some(report) = refresh {
                        record("vault", &report);
                        failure.set(Some((
                            "Could not read the vault state after locking it",
                            report,
                        )));
                    }
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
    // On the Get started page, keep the checklist current with changes other
    // clients make, without clearing what the card is showing. A failed
    // re-read is recorded and shown as unknown; Check again displays it.
    use_future(move || async move {
        if progress.is_none() {
            return;
        }
        loop {
            futures_timer::Delay::new(crate::welcome::VAULT_POLL).await;
            if *busy.peek() {
                continue;
            }
            match ask(VaultRequest::Status).await {
                Ok(VaultReply::State(state)) => {
                    report(VaultSeen::Facts(facts(&state)));
                    shown.set(Shown::State(state));
                }
                Ok(VaultReply::Remote { endpoint }) => {
                    report(VaultSeen::Remote);
                    shown.set(Shown::Remote(endpoint));
                }
                Ok(VaultReply::Locked { .. }) => {}
                Ok(VaultReply::Error(error)) | Err(error) => {
                    record("vault", &error);
                    report(VaultSeen::Unreadable);
                }
            }
        }
    });
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
                if let Some((title, report)) = failure() {
                    ErrorNotice { report, title: title.to_string() }
                }
            }
            CardFooter {
                Button { variant: ButtonVariant::Ghost, availability, onclick: move |_| run(VaultRequest::Status), "Check again" }
            }
        } };
    }
    let locked = shown() == Shown::Locked;
    let state = match shown() {
        Shown::State(state) => Some(state),
        _ => None,
    };
    let (label, tone) = match &state {
        None if locked => ("Locked", BadgeTone::Warning),
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
                    DescriptionItem { term: rsx! { "Vault broker" }, if state.broker_running { "Running" } else { "Not running" } }
                    DescriptionItem { term: rsx! { "Encrypted vault" }, if state.exists { "Created" } else { "Not created yet" } }
                    DescriptionItem { term: rsx! { "Two-factor authentication" }, "{known(state.totp_enrolled)}" }
                    DescriptionItem { term: rsx! { "Agent credential access" }, "{known(state.agent_access)}" }
                }
            } else if failure().is_none() && !locked {
                p { class: "a206-muted", role: "status", "Checking the vault…" }
            }
            if let Some(message) = notice() {
                Alert { tone: AlertTone::Success, urgency: Urgency::Polite, "{message}" }
            }
            if let Some((title, report)) = failure() {
                ErrorNotice { report, title: title.to_string() }
            }
            p { class: "a206-muted", "Verify the broker public key below to create, unlock and pair the vault or enroll two-factor authentication through an encrypted connection. You can also use the desktop client, or run `agent206 vault` in a trusted terminal on the gateway host. Vault controls are unavailable in the TUI." }
            if unlocked && crate::sign_in::network() {
                p { class: "a206-muted", "Locking also stops new sign-ins from other devices until the vault is unlocked." }
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
