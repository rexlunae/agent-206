//! First-time gateway setup for every client: choose this computer or an SSH
//! gateway, review an unknown host key's fingerprint, and save the choice
//! before anything connects. Desktop and terminal clients run SSH themselves;
//! the web client asks its bridge, which runs SSH and owns known_hosts, so the
//! browser only ever sees fingerprints.
use crate::{PageFrame, State, error::ErrorNotice};
use a206_core::{ErrorReport, GatewayProbe, GatewayTarget, HostKey, SetupStatus};
use fc_dioxus::prelude::*;

pub(crate) use backend::status;

#[cfg(not(target_arch = "wasm32"))]
mod backend {
    use crate::{PROFILE, SETUP_ERROR, TARGET};
    use a206_core::{
        ErrorKind, ErrorReport, GatewayConnection, GatewayProbe, GatewayTarget, SetupStatus,
    };
    use a206_runtime::connection::{self, ConnectionError};
    use std::path::PathBuf;

    /// The runtime already traced the failure where it happened; this only
    /// captures it for display.
    fn report(error: &ConnectionError) -> ErrorReport {
        a206_runtime::error::report(error)
    }
    fn profile() -> Result<(PathBuf, Option<String>), ErrorReport> {
        PROFILE
            .lock()
            .ok()
            .and_then(|profile| profile.clone())
            .ok_or_else(|| {
                ErrorReport::new(
                    ErrorKind::Internal,
                    "this client was started without a profile",
                )
            })
    }
    /// Run setup I/O on its own runtime: the UI executor is not necessarily Tokio.
    /// The worker thread is detached deliberately; its result (or its loss) comes
    /// back through the channel, and its processes are killed on drop.
    async fn off_ui<T: Send + 'static>(
        work: impl Future<Output = Result<T, ConnectionError>> + Send + 'static,
    ) -> Result<T, ErrorReport> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|source| ConnectionError::SetupWorker {
                    source: Some(source),
                })
                .and_then(|runtime| runtime.block_on(work));
            // The receiver is gone only if the setup screen was closed; nothing to report.
            let _delivery = sender.send(result);
        });
        match receiver.await {
            Ok(result) => result.map_err(|error| report(&error)),
            Err(_) => Err(report(&ConnectionError::SetupWorker { source: None })),
        }
    }
    pub(crate) async fn status() -> Result<SetupStatus, ErrorReport> {
        let (_, pinned) = profile()?;
        // Report the gateway in use, not a file another client may have rewritten.
        let active = TARGET.lock().ok().and_then(|target| target.clone());
        let problem = SETUP_ERROR.lock().ok().and_then(|problem| problem.clone());
        Ok(SetupStatus {
            pinned: pinned.is_some() && active.is_some(),
            active,
            problem,
        })
    }
    pub(super) async fn probe(endpoint: String) -> Result<GatewayProbe, ErrorReport> {
        let (root, _) = profile()?;
        off_ui(async move { connection::probe(&root, &endpoint).await }).await
    }
    pub(super) async fn trust(
        endpoint: String,
        fingerprint: String,
    ) -> Result<GatewayProbe, ErrorReport> {
        let (root, _) = profile()?;
        off_ui(
            async move { connection::trust_fingerprints(&root, &endpoint, &[fingerprint]).await },
        )
        .await
    }
    pub(super) async fn save(target: GatewayTarget) -> Result<(), ErrorReport> {
        let (root, _) = profile()?;
        let check = (root.clone(), target.clone());
        off_ui(async move { connection::verify(&check.0, &check.1).await }).await?;
        connection::save(
            &root,
            &GatewayConnection {
                target: target.clone(),
            },
        )
        .map_err(|error| report(&error))?;
        if let Ok(mut slot) = SETUP_ERROR.lock() {
            *slot = None;
        }
        // The controller owns its own threads, so it is created here on the UI
        // thread rather than on the short-lived setup worker.
        crate::connect(root, &target);
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mod backend {
    use a206_core::{
        ErrorKind, ErrorReport, GatewayProbe, GatewayTarget, SetupReply, SetupRequest, SetupStatus,
    };

    async fn ask(request: SetupRequest) -> Result<SetupReply, ErrorReport> {
        match crate::web_request("/api/setup", request).await? {
            SetupReply::Error(report) => Err(report),
            reply => Ok(reply),
        }
    }
    fn unexpected(reply: SetupReply) -> ErrorReport {
        ErrorReport::new(
            ErrorKind::Protocol,
            format!("the web bridge answered setup with {reply:?}"),
        )
    }
    pub(crate) async fn status() -> Result<SetupStatus, ErrorReport> {
        match ask(SetupRequest::Status).await? {
            SetupReply::Status(status) => Ok(status),
            reply => Err(unexpected(reply)),
        }
    }
    pub(super) async fn probe(endpoint: String) -> Result<GatewayProbe, ErrorReport> {
        match ask(SetupRequest::Probe { endpoint }).await? {
            SetupReply::Probe(probe) => Ok(probe),
            reply => Err(unexpected(reply)),
        }
    }
    pub(super) async fn trust(
        endpoint: String,
        fingerprint: String,
    ) -> Result<GatewayProbe, ErrorReport> {
        match ask(SetupRequest::Trust {
            endpoint,
            fingerprint,
        })
        .await?
        {
            SetupReply::Probe(probe) => Ok(probe),
            reply => Err(unexpected(reply)),
        }
    }
    pub(super) async fn save(target: GatewayTarget) -> Result<(), ErrorReport> {
        match ask(SetupRequest::Save { target }).await? {
            SetupReply::Saved => Ok(()),
            reply => Err(unexpected(reply)),
        }
    }
}

#[derive(Clone, PartialEq)]
enum Step {
    Choosing,
    Checking,
    ReviewKeys(Vec<HostKey>),
    Ready(String),
    Failed(ErrorReport),
}
impl Step {
    fn from(result: Result<GatewayProbe, ErrorReport>) -> Self {
        match result {
            Ok(GatewayProbe::Reachable { version }) => Self::Ready(version),
            Ok(GatewayProbe::Unverified { keys }) => Self::ReviewKeys(keys),
            Err(report) => Self::Failed(report),
        }
    }
}

/// Switch the workspace to the gateway just saved: drop everything the previous
/// gateway reported, and invalidate its in-flight refreshes, before the new one
/// answers (or fails to).
async fn finish(mut state: State, target: GatewayTarget) -> Result<(), ErrorReport> {
    backend::save(target).await?;
    let generation = state.gateway_generation.peek().wrapping_add(1);
    state.gateway_generation.set(generation);
    state.data.set(crate::empty_snapshot());
    state.loaded.set(false);
    state.gateway.set(None);
    state.connected.set(false);
    state.outage.set(None);
    state.status.set(String::new());
    state.error.set(None);
    state.setup.set(false);
    Ok(())
}

#[component]
pub(crate) fn GatewaySetup() -> Element {
    let status = use_resource(status);
    match &*status.read() {
        None => {
            rsx! { PageFrame { kicker: "First-time setup", title: "Choose your gateway.", description: "Checking how this client is set up…" } }
        }
        Some(Err(report)) => rsx! {
            PageFrame { kicker: "First-time setup", title: "Choose your gateway.", description: "Gateway setup could not start.",
                ErrorNotice { report: report.clone(), title: "Gateway setup unavailable".to_string() }
            }
        },
        Some(Ok(current)) => rsx! { SetupForm { current: current.clone() } },
    }
}

#[component]
fn SetupForm(current: SetupStatus) -> Element {
    let state = use_context::<State>();
    let saved_ssh = match &current.active {
        Some(GatewayTarget::Ssh { endpoint }) => Some(endpoint.clone()),
        _ => None,
    };
    let mut kind = use_signal(|| if saved_ssh.is_some() { "ssh" } else { "local" }.to_owned());
    let mut endpoint = use_signal(|| saved_ssh.clone().unwrap_or_default());
    let mut step = use_signal(|| match &current.problem {
        Some(problem) => Step::Failed(problem.clone()),
        None => Step::Choosing,
    });
    let radios = use_id();
    // Leaving setup is only possible once a gateway is configured.
    let can_cancel = !current.required();
    let ssh = kind() == "ssh";
    let busy = matches!(step(), Step::Checking);
    let availability = if busy {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let mut save = move |target: GatewayTarget| {
        step.set(Step::Checking);
        spawn(async move {
            if let Err(report) = finish(state, target).await {
                step.set(Step::Failed(report));
            }
        });
    };
    rsx! { PageFrame { kicker: "First-time setup", title: "Choose your gateway.", description: "The gateway runs your agents and holds their policy. Pick where it lives before this client connects to anything.",
        if let (true, Some(GatewayTarget::Ssh { endpoint })) = (current.pinned, current.active.clone()) {
            Alert { title: rsx! { "Started with --remote" }, "This session uses {endpoint}. Saving here replaces it and sets the default for later sessions." }
        }
        if cfg!(target_arch = "wasm32") {
            Alert { title: rsx! { "Set up through the web server" }, "The Agent 206 web server makes the SSH connection, with its own OpenSSH keys and known hosts. This browser never receives a key." }
        }
        Card { variant: CardVariant::Feature,
            CardHeader { CardTitle { "Gateway location" } }
            CardBody {
                CheckGroup { legend: rsx! { "Where should Agent 206 connect?" },
                    radios: RadioChoice { name: radios, value: Some(kind()), onchange: Some(EventHandler::new(move |value: String| { kind.set(value); step.set(Step::Choosing); })) },
                    Radio { value: "local", availability: availability.clone(), label: rsx! { if cfg!(target_arch = "wasm32") { "The computer running the web server" } else { "This computer" } }, hint: rsx! { "Start a gateway there with this profile when needed." } }
                    Radio { value: "ssh", availability: availability.clone(), label: rsx! { "A remote gateway over SSH" }, hint: rsx! { "Use an installed agent206 on another machine through OpenSSH, with strict host key checking." } }
                }
                if ssh {
                    Field { label: rsx! { "SSH endpoint" }, hint: rsx! { "ssh://user@host or ssh://user@host:port. OpenSSH keys and agent are used; passwords are not stored." },
                        Input { value: ReadSignal::from(endpoint), placeholder: "ssh://user@host", oninput: move |v: String| { endpoint.set(v); step.set(Step::Choosing); } }
                    }
                }
                match step() {
                    Step::Choosing => rsx! {},
                    Step::Checking => rsx! { p { class: "a206-muted", role: "status", "Checking the connection…" } },
                    Step::Ready(version) => rsx! { Alert { tone: AlertTone::Success, title: rsx! { "Gateway reachable" }, "Host key verified. Remote reports: {version}" } },
                    Step::Failed(report) => rsx! { ErrorNotice { report, title: "Could not use this gateway".to_string() } },
                    Step::ReviewKeys(keys) => rsx! {
                        Alert { tone: AlertTone::Warning, title: rsx! { "Verify this host before trusting it" },
                            p { "This host is not in your known hosts. Compare a fingerprint with one obtained from the server's administrator or console (for example `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`). Trust it only if they match." }
                            dl { class: "a206-fact-grid",
                                for key in keys.iter() {
                                    dt { key: "{key.fingerprint}", "{key.algorithm}" }
                                    dd { code { "{key.fingerprint}" }
                                        // Trust only the key the user verified, never the whole scan.
                                        {
                                            let fingerprint = key.fingerprint.clone();
                                            rsx! { Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, availability: availability.clone(),
                                                onclick: move |_| {
                                                    let (endpoint, fingerprint) = (endpoint().trim().to_owned(), fingerprint.clone());
                                                    step.set(Step::Checking);
                                                    spawn(async move { step.set(Step::from(backend::trust(endpoint, fingerprint).await)); });
                                                },
                                                "This fingerprint matches" } }
                                        }
                                    }
                                }
                            }
                        }
                    },
                }
            }
            CardFooter {
                if ssh {
                    if let Step::ReviewKeys(_) = step() {
                        Button { variant: ButtonVariant::Ghost, availability: availability.clone(), onclick: move |_| step.set(Step::Choosing), "None match" }
                    } else if let Step::Ready(_) = step() {
                        Button { variant: ButtonVariant::Primary, availability: availability.clone(),
                            onclick: move |_| save(GatewayTarget::Ssh { endpoint: endpoint().trim().to_owned() }),
                            "Save and connect" }
                    } else {
                        Button { variant: ButtonVariant::Primary, availability: if endpoint().trim().is_empty() { Availability::Disabled } else { availability.clone() }, busy,
                            onclick: move |_| {
                                let endpoint = endpoint().trim().to_owned();
                                step.set(Step::Checking);
                                spawn(async move { step.set(Step::from(backend::probe(endpoint).await)); });
                            },
                            "Check connection" }
                    }
                } else {
                    Button { variant: ButtonVariant::Primary, availability: availability.clone(), busy,
                        onclick: move |_| save(GatewayTarget::Local),
                        if cfg!(target_arch = "wasm32") { "Use the web server's computer" } else { "Use this computer" } }
                }
                if can_cancel {
                    Button { variant: ButtonVariant::Ghost, onclick: move |_| { let mut setup = state.setup; setup.set(false); }, "Keep current gateway" }
                }
            }
        }
        Alert { title: rsx! { "Separate from security setup" }, "Choosing a gateway does not enroll it. Vault encryption and TOTP enrollment are configured on the gateway itself." }
    } }
}
