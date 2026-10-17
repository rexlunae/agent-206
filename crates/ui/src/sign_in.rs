//! Sign-in for a web server opened to other devices. The browser sends a code
//! from the authenticator enrolled in the server's vault and keeps the session
//! bearer it gets back; a loopback server authorizes browsers with its
//! one-time link instead, so this screen never shows there.
use crate::{PageFrame, State, error::ErrorNotice};
use a206_core::{ErrorKind, ErrorReport, SignInReply, SignInRequest};
use fc_dioxus::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether this server signs browsers in with authenticator codes.
static NETWORK: AtomicBool = AtomicBool::new(false);

pub(crate) fn network() -> bool {
    NETWORK.load(Ordering::Relaxed)
}

/// Ask the server how browsers sign in. A server that cannot say (an older
/// build, or one not reachable yet) is treated as loopback; the next request
/// to it reports the actual problem.
pub(crate) async fn detect() {
    if let Ok(SignInReply::Methods { totp }) =
        crate::web_post::<_, SignInReply>("/api/sign-in", SignInRequest::Methods, None).await
    {
        NETWORK.store(totp, Ordering::Relaxed);
    }
}

/// True while this browser needs to sign in before anything else.
pub(crate) fn required() -> bool {
    network() && crate::web_session().is_err()
}

fn refused(reply: SignInReply) -> ErrorReport {
    match reply {
        SignInReply::Error(report) => report,
        _ => ErrorReport::new(ErrorKind::Protocol, "unexpected sign-in reply"),
    }
}

async fn submit(code: String) -> Result<(), ErrorReport> {
    match crate::web_post("/api/sign-in", SignInRequest::Totp { code }, None).await? {
        SignInReply::SignedIn { session } => crate::store_web_session(&session)
            .map_err(|message| ErrorReport::new(ErrorKind::Internal, message)),
        reply => Err(refused(reply)),
    }
}

/// End this browser's session on the server, then forget it here.
pub(crate) async fn sign_out() -> Result<(), ErrorReport> {
    let session = crate::web_session()
        .map_err(|message| ErrorReport::new(ErrorKind::PermissionDenied, message))?;
    let reply = crate::web_post(
        "/api/sign-in",
        SignInRequest::SignOut,
        Some(session.clone()),
    )
    .await;
    crate::forget_web_session(&session);
    match reply? {
        SignInReply::SignedOut => Ok(()),
        reply => Err(refused(reply)),
    }
}

#[component]
pub(crate) fn SignIn() -> Element {
    let state = use_context::<State>();
    let mut code = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut send = move || {
        if busy() || code().trim().is_empty() {
            return;
        }
        busy.set(true);
        failure.set(None);
        let entered = code().trim().to_owned();
        spawn(async move {
            match submit(entered).await {
                Ok(()) => {
                    code.set(String::new());
                    let mut sign_in = state.sign_in;
                    sign_in.set(false);
                }
                Err(report) => {
                    crate::error::record("sign in", &report);
                    failure.set(Some(report));
                }
            }
            busy.set(false);
        });
    };
    rsx! { PageFrame { kicker: "Sign in", title: "Sign in to Agent 206.", description: "This web server is open to other devices. Enter the current code from the authenticator app enrolled in its vault.",
        form { onsubmit: move |event| { event.prevent_default(); send(); },
            Card { variant: CardVariant::Feature,
                CardHeader { CardTitle { "Authenticator code" } }
                CardBody {
                    Field { label: rsx! { "Code" }, hint: rsx! { "Six digits from your authenticator app. The vault on the web server's computer must be unlocked." },
                        Input { value: ReadSignal::from(code), autocomplete: "one-time-code", inputmode: "numeric",
                            availability: if busy() { Availability::Disabled } else { Availability::Enabled },
                            oninput: move |value: String| code.set(value) }
                    }
                    if let Some(report) = failure() {
                        ErrorNotice { report, title: "Could not sign in".to_string() }
                    }
                }
                CardFooter {
                    Button { variant: ButtonVariant::Primary, r#type: ButtonType::Submit, busy: busy(),
                        availability: if code().trim().is_empty() { Availability::Disabled } else { Availability::Enabled },
                        "Sign in" }
                }
            }
        }
        Alert { title: rsx! { "A session lasts 12 hours" }, "Signing out, or restarting the web server, ends it sooner. Each code works once." }
    } }
}

/// Ends this browser's session; shown only by servers that sign browsers in.
#[component]
pub(crate) fn SignOut() -> Element {
    let state = use_context::<State>();
    let mut busy = use_signal(|| false);
    // Subscribe before checking `network()`, which is not a signal: the
    // shell signs in or connects right after it learns the server's kind.
    let signing_in = (state.sign_in)();
    let _connected = (state.connected)();
    if !network() || signing_in {
        return rsx! {};
    }
    rsx! { Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, busy: busy(),
    onclick: move |_| {
        busy.set(true);
        spawn(async move {
            let mut state = state;
            // Forgotten here either way; a failure is still shown.
            if let Err(report) = sign_out().await {
                crate::error::record("sign out", &report);
                state.error.set(Some(report));
            }
            state.data.set(crate::empty_snapshot());
            state.loaded.set(false);
            state.connected.set(false);
            state.sign_in.set(true);
            busy.set(false);
        });
    },
    "Sign out" } }
}
