//! The Gateway page's health checks: the same read-only report as
//! `agent206 doctor`. Desktop and terminal clients run the checks themselves;
//! the web client asks its bridge, which checks the profile, gateway and vault
//! broker on its own computer. Running the checks starts nothing and changes
//! nothing.
use crate::error::{ErrorNotice, record};
use a206_core::{
    ErrorReport,
    doctor::{DoctorReport, Health as Level},
};
use fc_dioxus::prelude::*;

fn tone(level: Level) -> BadgeTone {
    match level {
        Level::Ok => BadgeTone::Success,
        Level::Warning => BadgeTone::Warning,
        Level::Unknown => BadgeTone::Neutral,
        Level::Problem => BadgeTone::Danger,
    }
}
fn label(level: Level) -> &'static str {
    match level {
        Level::Ok => "OK",
        Level::Warning => "Check",
        Level::Unknown => "Unknown",
        Level::Problem => "Fix",
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn check() -> Result<DoctorReport, ErrorReport> {
    let Some((root, remote)) = crate::PROFILE.lock().ok().and_then(|p| p.clone()) else {
        return Err(ErrorReport::new(
            a206_core::ErrorKind::Unavailable,
            "this client was started without a profile",
        ));
    };
    a206_runtime::doctor::check_detached(root, remote)
        .await
        .map_err(|error| a206_runtime::error::report(&error))
}
#[cfg(target_arch = "wasm32")]
async fn check() -> Result<DoctorReport, ErrorReport> {
    crate::web_request("/api/health", ()).await
}

#[component]
pub(crate) fn HealthChecks() -> Element {
    let mut report = use_signal(|| None::<Result<DoctorReport, ErrorReport>>);
    let mut running = use_signal(|| false);
    let run = move |_| {
        running.set(true);
        spawn(async move {
            let result = check().await;
            if let Err(error) = &result {
                record("doctor", error);
            }
            report.set(Some(result));
            running.set(false);
        });
    };
    rsx! { Card { variant: CardVariant::Subtle,
        CardHeader { CardTitle { "Health checks" } }
        CardBody {
            match &*report.read() {
                None => rsx! { p { class: "a206-muted", if cfg!(target_arch = "wasm32") { "Check the web server's profile, gateway, vault broker and provider accounts. Nothing is started or changed." } else { "Check the profile, gateway, vault broker and provider accounts. Nothing is started or changed." } } },
                Some(Err(error)) => rsx! { ErrorNotice { report: error.clone(), title: "Health checks could not run".to_string() } },
                Some(Ok(report)) => rsx! {
                    ul { class: "a206-stack", aria_label: "Health check results",
                        for check in report.checks.iter() {
                            li { class: "a206-row",
                                Badge { tone: tone(check.health), dot: true, "{label(check.health)}" }
                                span { "{check.finding}" }
                                if let Some(recovery) = check.finding.recovery().filter(|_| check.health != Level::Ok) {
                                    code { class: "a206-muted", "{recovery}" }
                                }
                            }
                            if let Some(error) = &check.error {
                                ErrorNotice { report: error.clone(), title: check.finding.to_string() }
                            }
                        }
                    }
                },
            }
        }
        CardFooter {
            Button { variant: ButtonVariant::Secondary, busy: running(), availability: if running() { Availability::Disabled } else { Availability::Enabled }, onclick: run, if running() { "Checking…" } else { "Run health checks" } }
        }
    } }
}
