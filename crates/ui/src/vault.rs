//! Vault setup on the Security page of the desktop and terminal clients:
//! create, unlock and lock the vault, pair the local gateway and enroll an
//! authenticator. Every request goes to the separate vault broker, which
//! enforces its own throttling and step-up rules; this panel holds no policy.
//!
//! Passwords and codes live only in their input's value until submitted and
//! are cleared on submit. A password kept for the authenticator-code retry is
//! held as a zeroizing [`WireSecret`], never rendered. The enrollment key is
//! shown as text content so it can be copied into an authenticator app.
use crate::error::{ErrorNotice, record};
use crate::welcome::{VaultFacts, VaultProgress, VaultSeen};
use a206_broker::{BrokerError, BrokerStatus, ClientError, Reply, Request, WireSecret};
use a206_core::{ErrorReport, GatewayTarget};
use a206_runtime::vault::{Session, VaultError};
use fc_dioxus::prelude::*;
use std::path::PathBuf;

/// The account name shown in the authenticator app, as the CLI uses by default.
const TOTP_ACCOUNT: &str = "agent206";

/// Where this client's vault controls apply.
#[derive(Clone, PartialEq)]
pub(crate) enum Placement {
    /// This computer's profile: its broker serves the local gateway.
    Local(PathBuf),
    /// The gateway runs elsewhere, with its own vault broker.
    Remote(String),
    /// The saved gateway setup could not be read.
    Unreadable(ErrorReport),
    /// The client was started without a profile.
    NoProfile,
}
pub(crate) fn placement() -> Placement {
    let Some((root, remote)) = crate::PROFILE.lock().ok().and_then(|p| p.clone()) else {
        return Placement::NoProfile;
    };
    match a206_runtime::connection::resolve(&root, remote) {
        Ok(Some(GatewayTarget::Ssh { endpoint })) => Placement::Remote(endpoint),
        // Without a chosen gateway, setup is shown instead of this page; a
        // local vault is still this profile's vault.
        Ok(Some(GatewayTarget::Local) | None) => Placement::Local(root),
        Err(error) => Placement::Unreadable(a206_runtime::error::report(&error)),
    }
}

/// The vault actions the broker's state makes possible.
#[derive(Debug, Default, PartialEq, Eq)]
struct Offer {
    create: bool,
    unlock: bool,
    lock: bool,
    pair: bool,
    enroll: bool,
}
fn offer(status: &BrokerStatus) -> Offer {
    Offer {
        create: !status.exists,
        unlock: status.exists && !status.unlocked,
        lock: status.unlocked,
        pair: status.unlocked,
        // Enrollment is known only once unlocked: it lives inside the vault.
        enroll: status.unlocked && status.totp_enrolled == Some(false),
    }
}
fn known(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "Yes",
        Some(false) => "No",
        None => "Unknown while locked",
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Task {
    Pair,
    Enroll,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flow {
    Idle,
    /// The vault asked for an authenticator code; the password is held.
    UnlockCode,
    /// The task needs fresh authentication on this connection first.
    StepUp(Task),
    /// An enrollment was started; the app's first code confirms it.
    Confirm,
}
struct Enrollment {
    otpauth_url: WireSecret,
    manual_key: WireSecret,
}

/// A failure to show, with a title naming what went wrong for the user.
#[derive(Clone, Debug, PartialEq)]
struct Failure {
    title: String,
    report: ErrorReport,
}

#[derive(Clone, Copy)]
struct Panel {
    session: Signal<Session>,
    status: Signal<Option<BrokerStatus>>,
    failure: Signal<Option<Failure>>,
    /// A secondary failure that did not undo the main action (pairing after unlock).
    warning: Signal<Option<Failure>>,
    notice: Signal<Option<&'static str>>,
    busy: Signal<bool>,
    flow: Signal<Flow>,
    held: Signal<Option<WireSecret>>,
    enrollment: Signal<Option<Enrollment>>,
    /// The Get started checklist, when this panel is on that page.
    progress: Option<VaultProgress>,
}
fn broker_refused(error: &VaultError, expected: BrokerError) -> bool {
    matches!(error, VaultError::Broker(ClientError::Broker(refusal)) if *refusal == expected)
}
fn failure(operation: &'static str, title: &str, error: &VaultError) -> Failure {
    let report = a206_runtime::error::report(error);
    record(operation, &report);
    Failure {
        title: title.to_owned(),
        report,
    }
}
/// Show a status failure without hiding the action failure that preceded it.
fn keep_both(primary: &mut Option<Failure>, secondary: &mut Option<Failure>, status: Failure) {
    if primary.is_none() {
        *primary = Some(status);
    } else if secondary.is_none() {
        *secondary = Some(status);
    }
    // Both slots taken: the status failure is already recorded by `failure`.
}
impl Panel {
    fn session(&self) -> Session {
        self.session.peek().clone()
    }
    /// Start an action unless one is running; clears the previous outcome.
    fn begin(mut self) -> bool {
        if *self.busy.peek() {
            return false;
        }
        self.busy.set(true);
        self.failure.set(None);
        self.warning.set(None);
        self.notice.set(None);
        true
    }
    fn fail(mut self, operation: &'static str, title: &str, error: &VaultError) {
        self.failure.set(Some(failure(operation, title, error)));
    }
    async fn finish(mut self) {
        self.refresh().await;
        self.busy.set(false);
    }
    fn report(self, seen: VaultSeen) {
        if let Some(progress) = self.progress {
            progress.report(seen);
        }
    }
    async fn refresh(mut self) {
        match self.session().status().await {
            Ok(status) => {
                self.report(VaultSeen::Facts(VaultFacts {
                    exists: status.exists,
                    unlocked: status.unlocked,
                    totp_enrolled: status.totp_enrolled,
                    paired: status.gateway_paired,
                }));
                self.status.set(Some(status));
            }
            Err(error) => {
                self.report(VaultSeen::Unreadable);
                self.status.set(None);
                let status = failure("vault_status", "Could not read the vault state", &error);
                let (mut primary, mut secondary) = (self.failure.write(), self.warning.write());
                keep_both(&mut primary, &mut secondary, status);
            }
        }
    }
    fn check(self) {
        if self.begin() {
            spawn(self.finish());
        }
    }
    fn unlock(mut self, password: WireSecret, totp: Option<String>, create: bool) {
        if !self.begin() {
            return;
        }
        spawn(async move {
            let request = Request::Unlock {
                password: WireSecret::new(password.expose().to_owned()),
                totp,
                create,
            };
            match self.session().call(request).await {
                Ok(_) => {
                    self.held.set(None);
                    self.flow.set(Flow::Idle);
                    // This connection holds the unlock's step-up, so it can pair now.
                    match self.session().pair_if_running().await {
                        Ok(true) => self.notice.set(Some("Vault unlocked and gateway paired.")),
                        Ok(false) => self.notice.set(Some(
                            "Vault unlocked. Start the gateway, then pair it here so it can use credentials.",
                        )),
                        Err(error) => {
                            self.notice.set(Some("Vault unlocked."));
                            self.warning.set(Some(failure(
                                "vault_pair",
                                "The vault is unlocked, but the gateway could not be paired",
                                &error,
                            )));
                        }
                    }
                }
                Err(error) if !create && broker_refused(&error, BrokerError::TotpRequired) => {
                    self.held.set(Some(password));
                    self.flow.set(Flow::UnlockCode);
                }
                Err(error) => {
                    self.held.set(None);
                    self.flow.set(Flow::Idle);
                    let title = if create {
                        "Could not create the vault"
                    } else {
                        "Could not unlock the vault"
                    };
                    self.fail("vault_unlock", title, &error);
                }
            }
            self.finish().await;
        });
    }
    fn lock(mut self) {
        if !self.begin() {
            return;
        }
        spawn(async move {
            match self.session().call(Request::Lock).await {
                Ok(_) => {
                    self.flow.set(Flow::Idle);
                    self.enrollment.set(None);
                    self.notice.set(Some("Vault locked."));
                }
                Err(error) => self.fail("vault_lock", "Could not lock the vault", &error),
            }
            self.finish().await;
        });
    }
    /// Run `task`, first asking for fresh authentication unless this
    /// connection already holds a step-up.
    fn start(mut self, task: Task) {
        let active = self
            .status
            .peek()
            .as_ref()
            .is_some_and(|status| status.step_up_active);
        if !active {
            self.failure.set(None);
            self.notice.set(None);
            self.flow.set(Flow::StepUp(task));
            return;
        }
        if self.begin() {
            spawn(async move {
                self.perform(task).await;
                self.finish().await;
            });
        }
    }
    fn step_up(self, task: Task, password: Option<WireSecret>, totp: Option<String>) {
        if !self.begin() {
            return;
        }
        spawn(async move {
            match self
                .session()
                .call(Request::StepUp { password, totp })
                .await
            {
                Ok(_) => self.perform(task).await,
                // Each code works once; the one just used to unlock is refused.
                Err(error) if broker_refused(&error, BrokerError::TotpReplayed) => self.fail(
                    "vault_step_up",
                    "That code was already used; enter the next one from your app",
                    &error,
                ),
                Err(error) => self.fail("vault_step_up", "Could not confirm it is you", &error),
            }
            self.finish().await;
        });
    }
    async fn perform(mut self, task: Task) {
        match task {
            Task::Pair => match self.session().pair_gateway().await {
                Ok(()) => {
                    self.flow.set(Flow::Idle);
                    self.notice.set(Some("Gateway paired with the vault."));
                }
                // The step-up this panel saw has expired: ask for it again.
                Err(error) if broker_refused(&error, BrokerError::StepUpRequired) => {
                    self.flow.set(Flow::StepUp(task));
                }
                Err(error) => {
                    self.flow.set(Flow::Idle);
                    self.fail("vault_pair", "The gateway could not be paired", &error);
                }
            },
            Task::Enroll => {
                let account = TOTP_ACCOUNT.to_owned();
                match self.session().call(Request::BeginTotp { account }).await {
                    Ok(Reply::TotpEnrollment {
                        otpauth_url,
                        manual_key,
                        ..
                    }) => {
                        self.enrollment.set(Some(Enrollment {
                            otpauth_url,
                            manual_key,
                        }));
                        self.flow.set(Flow::Confirm);
                    }
                    Ok(_) => {
                        self.flow.set(Flow::Idle);
                        self.fail(
                            "vault_totp_begin",
                            "Could not start enrollment",
                            &VaultError::UnexpectedReply,
                        );
                    }
                    Err(error) if broker_refused(&error, BrokerError::StepUpRequired) => {
                        self.flow.set(Flow::StepUp(task));
                    }
                    Err(error) => {
                        self.flow.set(Flow::Idle);
                        self.fail("vault_totp_begin", "Could not start enrollment", &error);
                    }
                }
            }
        }
    }
    fn confirm(mut self, code: String) {
        if !self.begin() {
            return;
        }
        spawn(async move {
            match self.session().call(Request::ConfirmTotp { code }).await {
                Ok(_) => {
                    self.enrollment.set(None);
                    self.flow.set(Flow::Idle);
                    self.notice.set(Some("Two-factor authentication enrolled."));
                }
                // A mistyped or expired code can be retried with the next one.
                Err(error)
                    if broker_refused(&error, BrokerError::AuthenticationFailed)
                        || matches!(
                            error,
                            VaultError::Broker(ClientError::Broker(BrokerError::ClockDrift { .. }))
                        ) =>
                {
                    self.fail(
                        "vault_totp_confirm",
                        "That code did not match; try the next one",
                        &error,
                    )
                }
                Err(error) => {
                    self.enrollment.set(None);
                    self.flow.set(Flow::Idle);
                    self.fail("vault_totp_confirm", "Enrollment did not finish", &error);
                }
            }
            self.finish().await;
        });
    }
    fn take_held(mut self) -> Option<WireSecret> {
        self.held.write().take()
    }
    fn cancel(mut self) {
        self.held.set(None);
        self.enrollment.set(None);
        self.flow.set(Flow::Idle);
    }
}

/// The Security page's vault section for this client.
#[component]
pub(crate) fn VaultSection() -> Element {
    rsx! { VaultPlacement { placement: placement() } }
}
#[component]
fn VaultPlacement(placement: Placement) -> Element {
    match placement {
        Placement::Local(root) => rsx! { VaultPanel { root } },
        Placement::Remote(endpoint) => rsx! {
            Card { variant: CardVariant::Subtle,
                CardHeader { CardTitle { "Vault" } Badge { tone: BadgeTone::Neutral, "Remote gateway" } }
                CardBody {
                    p { "This client uses the gateway at {endpoint}. Its vault broker runs on that host, and vault controls for a remote gateway are not available here yet." }
                    p { class: "a206-muted", "Run `agent206 vault init`, `agent206 vault unlock` and `agent206 vault totp enroll` in a terminal on that host." }
                }
            }
        },
        Placement::Unreadable(report) => rsx! {
            ErrorNotice { report, title: "Vault controls need a readable gateway setup".to_string() }
        },
        Placement::NoProfile => rsx! {
            Alert { title: rsx! { "Vault controls unavailable" }, "This client was started without a profile." }
        },
    }
}

/// Each submit takes the typed secret out of its field before anything else.
fn take(mut field: Signal<String>) -> String {
    let value = field();
    field.set(String::new());
    value
}
#[component]
fn VaultPanel(root: PathBuf) -> Element {
    let panel = Panel {
        session: use_signal(|| Session::open(root.clone())),
        status: use_signal(|| None),
        failure: use_signal(|| None),
        warning: use_signal(|| None),
        notice: use_signal(|| None),
        busy: use_signal(|| false),
        flow: use_signal(|| Flow::Idle),
        held: use_signal(|| None),
        enrollment: use_signal(|| None),
        progress: VaultProgress::listener(),
    };
    // Reads only through `peek`, so this runs once, after the first render.
    use_effect(move || panel.check());
    // On the Get started page, keep the checklist current with changes other
    // clients make. Skipped while an action or a multi-step flow is running.
    use_future(move || async move {
        if panel.progress.is_none() {
            return;
        }
        loop {
            futures_timer::Delay::new(crate::welcome::VAULT_POLL).await;
            if !*panel.busy.peek() && *panel.flow.peek() == Flow::Idle {
                panel.refresh().await;
            }
        }
    });
    let mut password = use_signal(String::new);
    let mut confirm = use_signal(String::new);
    let mut code = use_signal(String::new);
    let mut mismatch = use_signal(|| None::<&'static str>);
    let busy = (panel.busy)();
    let availability = if busy {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let status = (panel.status)();
    let flow = (panel.flow)();
    let enrolled = status
        .as_ref()
        .is_some_and(|s| s.totp_enrolled == Some(true));
    let offer = status.as_ref().map(offer).unwrap_or_default();
    let (label, tone) = match &status {
        None => ("Checking", BadgeTone::Neutral),
        Some(s) if !s.exists => ("No vault", BadgeTone::Warning),
        Some(s) if !s.unlocked => ("Locked", BadgeTone::Warning),
        Some(_) => ("Unlocked", BadgeTone::Success),
    };
    rsx! { Card { variant: CardVariant::Feature,
        CardHeader { CardTitle { "Vault" } Badge { tone, dot: true, "{label}" } }
        CardBody {
            if let Some(status) = &status {
                DescriptionList {
                    DescriptionItem { term: rsx! { "Encrypted vault" }, if status.exists { "Created" } else { "Not created yet" } }
                    DescriptionItem { term: rsx! { "Two-factor authentication" }, "{known(status.totp_enrolled)}" }
                    DescriptionItem { term: rsx! { "Agent credential access" }, "{known(status.agent_access)}" }
                }
            } else if (panel.failure)().is_none() {
                p { class: "a206-muted", role: "status", "Checking the vault…" }
            }
            if let Some(message) = (panel.notice)() {
                Alert { tone: AlertTone::Success, urgency: Urgency::Polite, "{message}" }
            }
            if let Some(Failure { title, report }) = (panel.failure)() {
                ErrorNotice { report, title }
            }
            if let Some(Failure { title, report }) = (panel.warning)() {
                ErrorNotice { report, title }
            }
            match flow {
                Flow::UnlockCode => rsx! {
                    Field { label: rsx! { "Authenticator code" }, hint: rsx! { "This vault has two-factor authentication. Enter the current code from your app." },
                        Input { r#type: InputType::Text, value: ReadSignal::from(code), oninput: move |v| code.set(v), autocomplete: "one-time-code", inputmode: "numeric" }
                    }
                },
                Flow::StepUp(task) => rsx! {
                    p { if task == Task::Pair { "Confirm it is you before pairing the gateway." } else { "Confirm it is you before enrolling an authenticator." } }
                    if enrolled {
                        Field { label: rsx! { "Authenticator code" },
                            Input { value: ReadSignal::from(code), oninput: move |v| code.set(v), autocomplete: "one-time-code", inputmode: "numeric" }
                        }
                    } else {
                        Field { label: rsx! { "Vault password" },
                            Input { r#type: InputType::Password, value: ReadSignal::from(password), oninput: move |v| password.set(v), autocomplete: "current-password" }
                        }
                    }
                },
                Flow::Confirm => rsx! {
                    if let Some(enrollment) = panel.enrollment.read().as_ref() {
                        p { "Add this account to your authenticator app with the setup key, or open the setup link on a device that has the app. Then enter the first code it shows." }
                        DescriptionList { layout: DlLayout::Stacked,
                            DescriptionItem { term: rsx! { "Setup key" }, code { class: "a206-secret-text", "{enrollment.manual_key.expose()}" } }
                            DescriptionItem { term: rsx! { "Setup link" }, code { class: "a206-secret-text", "{enrollment.otpauth_url.expose()}" } }
                        }
                    }
                    Field { label: rsx! { "Code from the app" },
                        Input { value: ReadSignal::from(code), oninput: move |v| code.set(v), autocomplete: "one-time-code", inputmode: "numeric" }
                    }
                },
                Flow::Idle => rsx! {
                    if offer.create {
                        Field { label: rsx! { "New vault password" }, hint: rsx! { "It encrypts every credential. It cannot be recovered if lost." },
                            Input { r#type: InputType::Password, value: ReadSignal::from(password), oninput: move |v| { password.set(v); mismatch.set(None); }, autocomplete: "new-password" }
                        }
                        Field { label: rsx! { "Repeat password" }, error: mismatch().map(|m| rsx! { "{m}" }),
                            Input { r#type: InputType::Password, invalid: mismatch().is_some(), value: ReadSignal::from(confirm), oninput: move |v| { confirm.set(v); mismatch.set(None); }, autocomplete: "new-password" }
                        }
                    }
                    if offer.unlock {
                        Field { label: rsx! { "Vault password" },
                            Input { r#type: InputType::Password, value: ReadSignal::from(password), oninput: move |v| password.set(v), autocomplete: "current-password" }
                        }
                    }
                },
            }
        }
        CardFooter {
            match flow {
                Flow::UnlockCode => rsx! {
                    Button { variant: ButtonVariant::Primary, availability: availability.clone(), busy,
                        onclick: move |_| {
                            let code = take(code);
                            // The password was held only for this retry.
                            match panel.take_held() {
                                Some(password) => panel.unlock(password, Some(code), false),
                                None => panel.cancel(),
                            }
                        },
                        "Unlock" }
                    Button { variant: ButtonVariant::Ghost, availability: availability.clone(), onclick: move |_| panel.cancel(), "Cancel" }
                },
                Flow::StepUp(task) => rsx! {
                    Button { variant: ButtonVariant::Primary, availability: availability.clone(), busy,
                        onclick: move |_| {
                            if enrolled {
                                panel.step_up(task, None, Some(take(code)));
                            } else {
                                panel.step_up(task, Some(WireSecret::new(take(password))), None);
                            }
                        },
                        "Continue" }
                    Button { variant: ButtonVariant::Ghost, availability: availability.clone(), onclick: move |_| panel.cancel(), "Cancel" }
                },
                Flow::Confirm => rsx! {
                    Button { variant: ButtonVariant::Primary, availability: availability.clone(), busy,
                        onclick: move |_| panel.confirm(take(code)), "Confirm enrollment" }
                    Button { variant: ButtonVariant::Ghost, availability: availability.clone(), onclick: move |_| panel.cancel(), "Cancel" }
                },
                Flow::Idle => rsx! {
                    if offer.create {
                        Button { variant: ButtonVariant::Primary, availability: availability.clone(), busy,
                            onclick: move |_| {
                                let (first, second) = (take(password), take(confirm));
                                if first.is_empty() {
                                    mismatch.set(Some("Enter a password."));
                                } else if first != second {
                                    mismatch.set(Some("The passwords do not match."));
                                } else {
                                    panel.unlock(WireSecret::new(first), None, true);
                                }
                            },
                            "Create vault" }
                    }
                    if offer.unlock {
                        Button { variant: ButtonVariant::Primary, availability: availability.clone(), busy,
                            onclick: move |_| panel.unlock(WireSecret::new(take(password)), None, false), "Unlock vault" }
                    }
                    if offer.pair {
                        Button { variant: ButtonVariant::Secondary, availability: availability.clone(), onclick: move |_| panel.start(Task::Pair), "Pair gateway" }
                    }
                    if offer.enroll {
                        Button { variant: ButtonVariant::Secondary, availability: availability.clone(), onclick: move |_| panel.start(Task::Enroll), "Set up two-factor" }
                    }
                    if offer.lock {
                        Button { variant: ButtonVariant::Secondary, availability: availability.clone(), onclick: move |_| panel.lock(), "Lock vault" }
                    }
                    Button { variant: ButtonVariant::Ghost, availability: availability.clone(), onclick: move |_| panel.check(), "Check again" }
                },
            }
        }
    } }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(exists: bool, unlocked: bool, totp: Option<bool>) -> BrokerStatus {
        BrokerStatus {
            exists,
            unlocked,
            totp_enrolled: totp,
            agent_access: unlocked.then_some(false),
            step_up_active: false,
            gateway_paired: unlocked.then_some(false),
        }
    }

    #[test]
    fn a_status_failure_never_hides_the_action_failure() {
        let shown = |title: &str| Failure {
            title: title.to_owned(),
            report: a206_runtime::error::report(&VaultError::GatewayClosed),
        };
        let (mut primary, mut secondary) = (Some(shown("action")), None);
        keep_both(&mut primary, &mut secondary, shown("status"));
        assert_eq!(primary, Some(shown("action")));
        assert_eq!(secondary, Some(shown("status")));
        let (mut primary, mut secondary) = (None, None);
        keep_both(&mut primary, &mut secondary, shown("status"));
        assert_eq!(primary, Some(shown("status")));
        assert_eq!(secondary, None);
    }

    #[test]
    fn offers_only_what_the_vault_state_allows() {
        assert_eq!(
            offer(&status(false, false, None)),
            Offer {
                create: true,
                ..Offer::default()
            }
        );
        assert_eq!(
            offer(&status(true, false, None)),
            Offer {
                unlock: true,
                ..Offer::default()
            }
        );
        let unlocked = Offer {
            lock: true,
            pair: true,
            ..Offer::default()
        };
        assert_eq!(
            offer(&status(true, true, Some(false))),
            Offer {
                enroll: true,
                ..unlocked
            }
        );
        // Enrolled vaults offer no second enrollment.
        assert_eq!(
            offer(&status(true, true, Some(true))),
            Offer {
                lock: true,
                pair: true,
                ..Offer::default()
            }
        );
    }

    #[test]
    fn remote_gateway_shows_a_notice_without_controls() {
        fn root(endpoint: String) -> Element {
            rsx! { VaultPlacement { placement: Placement::Remote(endpoint) } }
        }
        let endpoint = format!("ssh://{}@example.invalid", uuid::Uuid::new_v4().simple());
        let mut dom = VirtualDom::new_with_props(root, endpoint.clone());
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains(&endpoint), "{html}");
        assert!(html.contains("not available here yet"), "{html}");
        assert!(!html.contains("<button"), "{html}");
        assert!(!html.contains("<input"), "{html}");
    }
}
