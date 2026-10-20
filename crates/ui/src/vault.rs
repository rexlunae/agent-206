//! Vault setup on the Security page of the desktop and web clients:
//! create, unlock and lock the vault, pair the local gateway and enroll an
//! authenticator. Every request goes to the separate vault broker, which
//! enforces its own throttling and step-up rules; this panel holds no policy.
//!
//! Passwords and codes live only in their input's value until submitted and
//! are cleared on submit. A password kept for the authenticator-code retry is
//! held as a zeroizing [`WireSecret`], never rendered. The enrollment key is
//! shown as text content so it can be copied into an authenticator app.
//! Terminal clients do not mount the panel: the pinned terminal renderer does
//! not provide safe password input (dioxus-tui#6).
use crate::error::{ErrorNotice, record};
use crate::vault_backend::{Session, Target, unexpected};
use crate::welcome::{VaultFacts, VaultProgress, VaultSeen};
use a206_core::ErrorReport;
#[cfg(not(target_arch = "wasm32"))]
use a206_core::GatewayTarget;
use a206_vault_channel::{
    Code as BrokerError, Failure as VaultError, Reply, Request, Secret as WireSecret,
    Status as BrokerStatus,
};
use fc_dioxus::prelude::*;
mod credentials;
mod migration;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

/// The account name shown in the authenticator app, as the CLI uses by default.
const TOTP_ACCOUNT: &str = "agent206";

/// Where this client's vault controls apply.
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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
    AddCredential,
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
    graphic: Result<crate::totp_enrollment::Graphic, ErrorReport>,
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
    /// The latest background re-read's failure, until a read succeeds.
    poll_failure: Signal<Option<Failure>>,
    notice: Signal<Option<&'static str>>,
    busy: Signal<bool>,
    flow: Signal<Flow>,
    held: Signal<Option<WireSecret>>,
    enrollment: Signal<Option<Enrollment>>,
    import_review: Signal<Option<a206_vault_channel::ImportReview>>,
    import_totp: Signal<bool>,
    import_result: Signal<Option<a206_vault_channel::ImportApplied>>,
    credential_page: Signal<Option<a206_vault_channel::CredentialPage>>,
    credential_failure: Signal<Option<Failure>>,
    credential_read_failure: Signal<Option<Failure>>,
    key_editor: Signal<bool>,
    /// The Get started checklist, when this panel is on that page.
    progress: Option<VaultProgress>,
}
impl PartialEq for Panel {
    fn eq(&self, other: &Self) -> bool {
        // Each panel owns one stable set of signals. Children subscribe to
        // their values; identity distinguishes panels and gateway remounts.
        self.session == other.session
    }
}
/// A stopped broker holds no unlocked vault, so its vault reads as locked.
#[cfg(not(target_arch = "wasm32"))]
fn stopped(root: &Path) -> BrokerStatus {
    BrokerStatus {
        exists: a206_broker::vault_path(root).exists(),
        unlocked: false,
        totp_enrolled: None,
        agent_access: None,
        step_up_active: false,
        gateway_paired: None,
    }
}
fn broker_refused(error: &VaultError, expected: BrokerError) -> bool {
    error.code == expected
}
fn failure(operation: &'static str, title: &str, error: &VaultError) -> Failure {
    let report = error.report.clone();
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
        self.import_result.set(None);
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
    fn show(mut self, status: BrokerStatus) {
        if !status.unlocked {
            self.forget_credentials();
        }
        self.report(VaultSeen::Facts(VaultFacts {
            exists: status.exists,
            unlocked: status.unlocked,
            totp_enrolled: status.totp_enrolled,
            paired: status.gateway_paired,
        }));
        self.poll_failure.set(None);
        self.status.set(Some(status));
    }
    fn forget_credentials(mut self) {
        self.credential_page.set(None);
        // A page error belongs to the previous lock/connection state. Keep
        // mutation failures separately so an uncertain save is never hidden.
        self.credential_read_failure.set(None);
        self.key_editor.set(false);
    }
    /// The Get started page's background re-read. It never starts a broker,
    /// so a deliberate `vault stop` stays stopped, and its failure is shown
    /// apart from action failures and cleared by the next good read.
    #[cfg(not(target_arch = "wasm32"))]
    async fn poll(mut self, root: &Path) {
        match self.session().status_if_running().await {
            Ok(found) => self.show(found.unwrap_or_else(|| stopped(root))),
            Err(error) => {
                // Offer no action on a state that could not be confirmed.
                self.report(VaultSeen::Unreadable);
                self.status.set(None);
                self.forget_credentials();
                self.poll_failure.set(Some(failure(
                    "vault_status",
                    "Could not re-read the vault state",
                    &error,
                )));
            }
        }
    }
    async fn refresh(mut self) {
        match self.session().status().await {
            Ok(status) => self.show(status),
            Err(error) => {
                self.report(VaultSeen::Unreadable);
                self.status.set(None);
                self.forget_credentials();
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
                totp: totp.map(WireSecret::new),
                create,
            };
            match self.session().call(request).await {
                Ok(_) => {
                    self.held.set(None);
                    self.flow.set(Flow::Idle);
                    self.after_unlock(false).await;
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
    async fn after_unlock(mut self, imported: bool) {
        self.import_review.set(None);
        self.import_totp.set(false);
        // Both unlock and import grant a fresh step-up on this connection.
        match self.session().pair_if_running().await {
            Ok(true) => self.notice.set(Some(if imported {
                "Vault imported and gateway paired."
            } else {
                "Vault unlocked and gateway paired."
            })),
            Ok(false) => self.notice.set(Some(if imported {
                "Vault imported and unlocked. Start the gateway, then pair it here."
            } else {
                "Vault unlocked. Start the gateway, then pair it here so it can use credentials."
            })),
            Err(error) => {
                self.notice.set(Some(if imported {
                    "Vault imported and unlocked."
                } else {
                    "Vault unlocked."
                }));
                self.warning.set(Some(failure(
                    "vault_pair",
                    "The vault is unlocked, but the gateway could not be paired",
                    &error,
                )));
            }
        }
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
                .call(Request::StepUp {
                    password,
                    totp: totp.map(WireSecret::new),
                })
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
            Task::AddCredential => {
                self.flow.set(Flow::Idle);
                self.key_editor.set(true);
            }
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
                            graphic: crate::totp_enrollment::Graphic::encode(otpauth_url.expose()),
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
                            &unexpected(),
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
            match self
                .session()
                .call(Request::ConfirmTotp {
                    code: WireSecret::new(code),
                })
                .await
            {
                Ok(_) => {
                    self.enrollment.set(None);
                    self.flow.set(Flow::Idle);
                    self.notice.set(Some("Two-factor authentication enrolled."));
                }
                // A mistyped or expired code can be retried with the next one.
                Err(error)
                    if broker_refused(&error, BrokerError::AuthenticationFailed)
                        || error.code == BrokerError::ClockDrift =>
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
        let pending = self.enrollment.peek().is_some();
        self.held.set(None);
        self.enrollment.set(None);
        self.flow.set(Flow::Idle);
        // Also discard the broker-held pending seed, rather than only hiding it.
        if pending && self.begin() {
            spawn(async move {
                if let Err(error) = self.session().call(Request::CancelTotp).await {
                    self.fail("vault_totp_cancel", "Could not cancel enrollment", &error);
                }
                self.finish().await;
            });
        }
    }
}

/// The Security page's vault section for this client.
#[cfg(not(target_arch = "wasm32"))]
#[component]
pub(crate) fn VaultSection() -> Element {
    rsx! { VaultPlacement { placement: placement(), terminal: crate::terminal_mode() } }
}
#[cfg(not(target_arch = "wasm32"))]
#[component]
fn VaultPlacement(placement: Placement, #[props(default)] terminal: bool) -> Element {
    // Refuse before mounting VaultPanel, including its broker session and all
    // password, authenticator, import and credential-entry state. Masking the
    // painted text alone would not fix the renderer's value-attribute handling.
    if terminal {
        return rsx! {
            Card { variant: CardVariant::Subtle,
                CardHeader { CardTitle { "Vault" } Badge { tone: BadgeTone::Warning, "Unavailable in TUI" } }
                CardBody {
                    p { "Vault controls are unavailable in the TUI because its input controls cannot keep passwords and API keys private." }
                    p { "Use the desktop client, or run `agent206 vault --help` for vault commands in a trusted terminal on the gateway host." }
                }
            }
        };
    }
    match placement {
        Placement::Local(root) => rsx! { VaultPanel { target: root } },
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
    std::mem::take(&mut *field.write())
}
#[component]
fn VaultPanel(target: Target) -> Element {
    let panel = Panel {
        session: use_signal(|| Session::open(target.clone())),
        status: use_signal(|| None),
        failure: use_signal(|| None),
        warning: use_signal(|| None),
        poll_failure: use_signal(|| None),
        notice: use_signal(|| None),
        busy: use_signal(|| false),
        flow: use_signal(|| Flow::Idle),
        held: use_signal(|| None),
        enrollment: use_signal(|| None),
        import_review: use_signal(|| None),
        import_totp: use_signal(|| false),
        import_result: use_signal(|| None),
        credential_page: use_signal(|| None),
        credential_failure: use_signal(|| None),
        credential_read_failure: use_signal(|| None),
        key_editor: use_signal(|| false),
        progress: VaultProgress::listener(),
    };
    // Reads only through `peek`, so this runs once, after the first render.
    use_effect(move || panel.check());
    // Gateway stop/restart and locks from another client revoke pairings.
    // Refresh without erasing an action's outcome, and serialize with submits
    // because the encrypted channel uses ordered nonces. A failed connection
    // stops polling until the user explicitly retries.
    #[cfg(not(target_arch = "wasm32"))]
    let poll_target = target.clone();
    use_future(move || {
        #[cfg(not(target_arch = "wasm32"))]
        let target = poll_target.clone();
        async move {
            let mut panel = panel;
            loop {
                futures_timer::Delay::new(crate::welcome::VAULT_POLL).await;
                if !*panel.busy.peek() && panel.status.peek().is_some() {
                    panel.busy.set(true);
                    #[cfg(not(target_arch = "wasm32"))]
                    panel.poll(&target).await;
                    #[cfg(target_arch = "wasm32")]
                    panel.refresh().await;
                    panel.busy.set(false);
                }
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
                    DescriptionItem { term: rsx! { "Gateway pairing" }, "{known(status.gateway_paired)}" }
                }
                if status.unlocked && status.totp_enrolled == Some(false) {
                    Alert { tone: AlertTone::Warning, title: rsx! { "Verify an authenticator next" }, "Your vault is encrypted. Set up two-factor authentication and confirm a code before considering enrollment complete. Agent credential access stays off by default." }
                }
                if status.unlocked && status.totp_enrolled == Some(true) && status.gateway_paired == Some(true) {
                    Alert { tone: AlertTone::Success, title: rsx! { "Vault enrollment verified" }, "Encryption is active, an authenticator is enrolled, and the gateway holds a current pairing. Lock and unlock once to check your recovery workflow. Restarting the gateway requires pairing it again." }
                }
                if !status.exists && flow == Flow::Idle {
                    migration::Migration { panel,
                        onpreview: move |source| { take(password); take(confirm); take(code); panel.preview_import(source); },
                        onapply: move |(source_password, source_code)| { take(password); take(confirm); take(code); panel.apply_import(source_password, source_code); },
                        oncancel: move |_| panel.cancel_import(),
                    }
                } else if status.unlocked && status.totp_enrolled == Some(true) {
                    Alert { title: rsx! { "Optional: bring over your connections" },
                        p { "Preview RustyClaw provider and messenger settings separately. Choose each item before importing." }
                        Link { to: crate::Route::Imports {}, "Review connection settings" }
                        p { "Migration does not activate foreign hooks, extensions, permissions, or model choices." }
                    }
                }
            } else if (panel.failure)().is_none() && (panel.poll_failure)().is_none() {
                p { class: "a206-muted", role: "status", "Checking the vault…" }
            }
            if let Some(message) = (panel.notice)() {
                Alert { tone: AlertTone::Success, urgency: Urgency::Polite, "{message}" }
            }
            if let Some(applied) = (panel.import_result)() {
                if let Some(path) = applied.receipt { p { class: "a206-muted", "Vault import receipt: {path.display()}" } }
                if let Some(report) = applied.receipt_error { ErrorNotice { report, title: "Vault imported; receipt not confirmed".to_string() } }
            }
            if let Some(Failure { title, report }) = (panel.failure)() {
                ErrorNotice { report, title }
            }
            if let Some(Failure { title, report }) = (panel.warning)() {
                ErrorNotice { report, title }
            }
            if let Some(Failure { title, report }) = (panel.poll_failure)() {
                ErrorNotice { report, title }
            }
            match flow {
                Flow::UnlockCode => rsx! {
                    Field { label: rsx! { "Authenticator code" }, hint: rsx! { "This vault has two-factor authentication. Enter the current code from your app." },
                        Input { r#type: InputType::Text, value: ReadSignal::from(code), oninput: move |v| code.set(v), autocomplete: "one-time-code", inputmode: "numeric" }
                    }
                },
                Flow::StepUp(task) => rsx! {
                    p { match task { Task::Pair => "Confirm it is you before pairing the gateway.", Task::Enroll => "Confirm it is you before enrolling an authenticator.", Task::AddCredential => "Confirm it is you before adding an API key." } }
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
                        {crate::totp_enrollment::view(enrollment.otpauth_url.expose(), enrollment.manual_key.expose(), &enrollment.graphic)}
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
                    Button { variant: ButtonVariant::Ghost, availability: availability.clone(), onclick: move |_| { take(password); take(confirm); take(code); panel.cancel(); }, "Cancel" }
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
                    Button { variant: ButtonVariant::Ghost, availability: availability.clone(), onclick: move |_| { take(password); take(confirm); take(code); panel.cancel(); }, "Cancel" }
                },
                Flow::Confirm => rsx! {
                    Button { variant: ButtonVariant::Primary, availability: availability.clone(), busy,
                        onclick: move |_| panel.confirm(take(code)), "Confirm enrollment" }
                    Button { variant: ButtonVariant::Ghost, availability: availability.clone(), onclick: move |_| { take(password); take(confirm); take(code); panel.cancel(); }, "Cancel" }
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
    }
    if let Some(Failure { title, report }) = (panel.credential_failure)() {
        ErrorNotice { report, title }
    }
    if let Some(Failure { title, report }) = (panel.credential_read_failure)() {
        ErrorNotice { report, title }
    }
    if status.as_ref().is_some_and(|s| s.unlocked) {
        credentials::Credentials { panel,
            onread: move |after| panel.read_credentials(after),
            onadd: move |input| panel.add_key(input),
        }
    }
    }
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
            report: unexpected().report,
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

    #[test]
    fn terminal_never_mounts_vault_inputs_or_actions() {
        fn root(placement: Placement) -> Element {
            rsx! { VaultPlacement { placement, terminal: true } }
        }
        // A local target must be refused before a broker session is opened;
        // remote and missing-profile states must not expose forms either.
        for placement in [
            Placement::Local(std::env::temp_dir().join(uuid::Uuid::new_v4().to_string())),
            Placement::Remote(format!("ssh://{}@example.invalid", uuid::Uuid::new_v4())),
            Placement::NoProfile,
        ] {
            let mut dom = VirtualDom::new_with_props(root, placement);
            dom.rebuild_in_place();
            let html = dioxus_ssr::render(&dom);
            assert!(html.contains("Unavailable in TUI"), "{html}");
            assert!(html.contains("agent206 vault --help"), "{html}");
            assert!(!html.contains("<input"), "{html}");
            assert!(!html.contains("<button"), "{html}");
            assert!(!html.contains("<form"), "{html}");
        }
    }
}

/// A public key entered from the gateway's trusted terminal authenticates the
/// broker independently of the HTTP relay. Never trust an API-returned key alone.
#[cfg(target_arch = "wasm32")]
#[component]
pub(crate) fn VaultSection() -> Element {
    let mut verified = use_signal(|| None::<Vec<u8>>);
    let mut key = use_signal(String::new);
    let mut problem = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    if let Some(target) = verified() {
        return rsx! {
            Alert { tone: AlertTone::Success, title: rsx! { "Encrypted vault connection" },
                "The broker key is verified for this panel. Passwords and authenticator setup travel encrypted directly to the vault broker."
            }
            VaultPanel { target }
            Button { variant: ButtonVariant::Ghost, onclick: move |_| verified.set(None), "Disconnect vault controls" }
        };
    }
    rsx! {
        // Public status/lock remains available before the independent key
        // verification. No password or authenticator seed uses that API.
        crate::web_vault::WebVault {}
        Card { variant: CardVariant::Feature,
            CardHeader { CardTitle { "Connect securely to your vault" } Badge { tone: BadgeTone::Warning, "Verification required" } }
            CardBody {
                p { "Before entering a password, verify the vault broker using a trusted terminal on the computer running your gateway." }
                p { "Run this command with the same data directory used by the web server:" }
                code { "agent206 --data-dir <profile-directory> vault channel-key" }
                Field { label: rsx! { "Broker public key" }, hint: rsx! { "Paste the 64 hexadecimal characters printed in that terminal. This public key is safe to copy; never paste a password here." },
                    Input { value: ReadSignal::from(key), oninput: move |value: String| { key.set(value); problem.set(None); }, autocomplete: "off" }
                }
                if let Some(message) = problem() { Alert { tone: AlertTone::Danger, "{message}" } }
            }
            CardFooter {
                Button { variant: ButtonVariant::Primary, busy: busy(), availability: if busy() { Availability::Disabled } else { Availability::Enabled },
                    onclick: move |_| {
                        let value = key().trim().to_owned();
                        let parsed = parse_public_key(&value);
                        let Some(public) = parsed else { problem.set(Some("Enter the complete 64-character hexadecimal public key.".into())); return; };
                        busy.set(true);
                        spawn(async move {
                            // Verify possession before displaying secret-bearing inputs.
                            let session = Session::open(public.clone());
                            match session.status().await {
                                Ok(_) => { key.set(String::new()); verified.set(Some(public)); }
                                Err(error) => problem.set(Some(error.report.detail())),
                            }
                            busy.set(false);
                        });
                    }, "Verify and open vault setup" }
            }
        }
    }
}
#[cfg(any(target_arch = "wasm32", test))]
fn parse_public_key(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 || !value.is_ascii() {
        return None;
    }
    (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod key_tests {
    use super::parse_public_key;
    #[test]
    fn public_keys_are_exactly_32_bytes_and_never_accept_secret_like_input() {
        assert_eq!(parse_public_key(&"a1".repeat(32)), Some(vec![0xa1; 32]));
        for invalid in [
            "a1".repeat(31),
            "a1".repeat(33),
            "x1".repeat(32),
            "é".repeat(32),
            "password".into(),
        ] {
            assert!(parse_public_key(&invalid).is_none());
        }
    }
}
