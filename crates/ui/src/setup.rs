//! First-time gateway setup for the desktop and terminal clients: choose this
//! computer or an SSH gateway, review an unknown host key's fingerprint, and
//! save the choice before anything connects.
use crate::{PROFILE, PageFrame, SETUP_ERROR, State};
use a206_core::{GatewayConnection, GatewayTarget, HostKey};
use a206_runtime::connection::{self, ConnectionError};
use fc_dioxus::prelude::*;
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
enum Step {
    Choosing,
    Checking,
    ReviewKeys(Vec<HostKey>),
    Ready(String),
    Failed(Arc<ConnectionError>),
}

/// Run setup I/O on its own runtime: the UI executor is not necessarily Tokio.
/// The worker thread is detached deliberately; its result (or its loss) comes
/// back through the channel, and its processes are killed on drop.
async fn off_ui<T: Send + 'static>(
    work: impl Future<Output = Result<T, ConnectionError>> + Send + 'static,
) -> Result<T, Arc<ConnectionError>> {
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
        Ok(result) => result.map_err(Arc::new),
        Err(_) => Err(Arc::new(ConnectionError::SetupWorker { source: None })),
    }
}
/// The error and its causes, for display. The structured error was already traced.
fn describe(error: &ConnectionError) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
fn profile() -> Option<(PathBuf, Option<String>)> {
    PROFILE.lock().ok().and_then(|profile| profile.clone())
}
/// Check an SSH gateway; an untrusted host moves to fingerprint review.
async fn check(root: PathBuf, endpoint: String) -> Step {
    let probe = {
        let (root, endpoint) = (root.clone(), endpoint.clone());
        off_ui(async move { connection::check_ssh(&root, &endpoint).await }).await
    };
    match probe {
        Ok(version) => Step::Ready(version),
        Err(error) if matches!(*error, ConnectionError::UnknownHost { .. }) => {
            match off_ui(async move { connection::scan_host_keys(&endpoint).await }).await {
                Ok(keys) => Step::ReviewKeys(keys),
                Err(error) => Step::Failed(error),
            }
        }
        Err(error) => Step::Failed(error),
    }
}
fn finish(mut state: State, root: PathBuf, target: GatewayTarget) -> Result<(), ConnectionError> {
    connection::save(
        &root,
        &GatewayConnection {
            target: target.clone(),
        },
    )?;
    if let Ok(mut slot) = SETUP_ERROR.lock() {
        *slot = None;
    }
    // Drop everything the previous gateway reported, and invalidate its
    // in-flight refreshes, before the new one answers (or fails to).
    let generation = state.gateway_generation.peek().wrapping_add(1);
    state.gateway_generation.set(generation);
    crate::connect(root, &target);
    state.data.set(crate::empty_snapshot());
    state.loaded.set(false);
    state.gateway.set(None);
    state.connected.set(false);
    state.status.set(String::new());
    state.setup.set(false);
    Ok(())
}

#[component]
pub(crate) fn GatewaySetup() -> Element {
    let mut state = use_context::<State>();
    let profile = profile();
    let saved = profile
        .as_ref()
        .and_then(|(root, _)| connection::load(root).ok().flatten());
    let mut kind = use_signal(|| match &saved {
        Some(GatewayConnection {
            target: GatewayTarget::Ssh { .. },
        }) => "ssh".to_owned(),
        _ => "local".to_owned(),
    });
    let mut endpoint = use_signal(|| match &saved {
        Some(GatewayConnection {
            target: GatewayTarget::Ssh { endpoint },
        }) => endpoint.clone(),
        _ => String::new(),
    });
    let mut step = use_signal(|| match SETUP_ERROR.lock().ok().and_then(|e| e.clone()) {
        Some(error) => Step::Failed(error),
        None => Step::Choosing,
    });
    let radios = use_id();
    let Some((root, remote)) = profile else {
        return rsx! { Alert { title: rsx! { "Gateway setup unavailable" }, "This client was started without a profile." } };
    };
    // Leaving setup is only possible once a gateway is configured.
    let can_cancel = !crate::setup_required();
    let ssh = kind() == "ssh";
    let busy = matches!(step(), Step::Checking);
    let availability = if busy {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    let check_root = root.clone();
    let trust_root = root.clone();
    let save_root = root.clone();
    rsx! { PageFrame { kicker: "First-time setup", title: "Choose your gateway.", description: "The gateway runs your agents and holds their policy. Pick where it lives before this client connects to anything.",
        if let Some(remote) = remote {
            Alert { title: rsx! { "Started with --remote" }, "This session uses {remote}. Saving here sets the default for later sessions." }
        }
        Card { variant: CardVariant::Feature,
            CardHeader { CardTitle { "Gateway location" } }
            CardBody {
                CheckGroup { legend: rsx! { "Where should Agent 206 connect?" },
                    radios: RadioChoice { name: radios, value: Some(kind()), onchange: Some(EventHandler::new(move |value: String| { kind.set(value); step.set(Step::Choosing); })) },
                    Radio { value: "local", availability: availability.clone(), label: rsx! { "This computer" }, hint: rsx! { "Start a gateway here with this profile when needed." } }
                    Radio { value: "ssh", availability: availability.clone(), label: rsx! { "A remote gateway over SSH" }, hint: rsx! { "Use an installed agent206 on another machine through OpenSSH, with strict host key checking." } }
                }
                if ssh {
                    Field { label: rsx! { "SSH endpoint" }, hint: rsx! { "ssh://user@host or ssh://user@host:port. Your OpenSSH keys and agent are used; passwords are not stored." },
                        Input { value: ReadSignal::from(endpoint), placeholder: "ssh://user@host", oninput: move |v: String| { endpoint.set(v); step.set(Step::Choosing); } }
                    }
                }
                match step() {
                    Step::Choosing => rsx! {},
                    Step::Checking => rsx! { p { class: "a206-muted", role: "status", "Checking the connection…" } },
                    Step::Ready(version) => rsx! { Alert { tone: AlertTone::Success, title: rsx! { "Gateway reachable" }, "Host key verified. Remote reports: {version}" } },
                    Step::Failed(error) => rsx! { Alert { tone: AlertTone::Warning, title: rsx! { "Could not use this gateway" }, {describe(&error)} } },
                    Step::ReviewKeys(keys) => rsx! {
                        Alert { tone: AlertTone::Warning, title: rsx! { "Verify this host before trusting it" },
                            p { "This host is not in your known hosts. Compare a fingerprint with one obtained from the server's administrator or console (for example `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`). Trust it only if they match." }
                            dl { class: "a206-fact-grid",
                                for key in keys.iter() {
                                    dt { key: "{key.fingerprint}", "{key.algorithm}" }
                                    dd { code { "{key.fingerprint}" }
                                        // Trust only the key the user verified, never the whole scan.
                                        {
                                            let (root, key) = (trust_root.clone(), key.clone());
                                            rsx! { Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, availability: availability.clone(),
                                                onclick: move |_| {
                                                    let (root, endpoint, key) = (root.clone(), endpoint(), key.clone());
                                                    step.set(Step::Checking);
                                                    spawn(async move {
                                                        if let Err(error) = connection::trust_host_keys(&root, std::slice::from_ref(&key)) {
                                                            step.set(Step::Failed(Arc::new(error)));
                                                            return;
                                                        }
                                                        step.set(check(root, endpoint).await);
                                                    });
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
                            onclick: move |_| {
                                if let Err(error) = finish(state, save_root.clone(), GatewayTarget::Ssh { endpoint: endpoint() }) {
                                    step.set(Step::Failed(Arc::new(error)));
                                }
                            },
                            "Save and connect" }
                    } else {
                        Button { variant: ButtonVariant::Primary, availability: if endpoint().trim().is_empty() { Availability::Disabled } else { availability.clone() }, busy,
                            onclick: move |_| {
                                let (root, endpoint) = (check_root.clone(), endpoint().trim().to_owned());
                                step.set(Step::Checking);
                                spawn(async move { step.set(check(root, endpoint).await); });
                            },
                            "Check connection" }
                    }
                } else {
                    Button { variant: ButtonVariant::Primary, availability: availability.clone(),
                        onclick: move |_| {
                            if let Err(error) = finish(state, save_root.clone(), GatewayTarget::Local) {
                                step.set(Step::Failed(Arc::new(error)));
                            }
                        },
                        "Use this computer" }
                }
                if can_cancel {
                    Button { variant: ButtonVariant::Ghost, onclick: move |_| state.setup.set(false), "Keep current gateway" }
                }
            }
        }
        Alert { title: rsx! { "Separate from security setup" }, "Choosing a gateway does not enroll it. Vault encryption and TOTP enrollment are configured on the gateway itself." }
    } }
}
