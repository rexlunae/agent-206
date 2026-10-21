//! The Gateway page's health checks: the same report as `agent206 doctor`.
//! Desktop and terminal clients run the checks themselves; the web client
//! asks its bridge, which checks the profile, gateway and vault broker on its
//! own computer. Running the checks starts nothing and changes nothing; each
//! repair they offer is made only when its button is pressed, as
//! `agent206 doctor --repair` asks first.
use crate::error::{ErrorNotice, record};
use a206_core::{
    Command, ErrorReport,
    doctor::{DoctorReport, Health as Level, Repair},
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
fn profile() -> Result<(std::path::PathBuf, Option<String>), ErrorReport> {
    crate::PROFILE
        .lock()
        .ok()
        .and_then(|profile| profile.clone())
        .ok_or_else(|| {
            ErrorReport::new(
                a206_core::ErrorKind::Unavailable,
                "this client was started without a profile",
            )
        })
}
#[cfg(not(target_arch = "wasm32"))]
async fn check() -> Result<DoctorReport, ErrorReport> {
    let (root, remote) = profile()?;
    // The gateway this client's commands go to, so a refresh the report
    // offers reaches the gateway it was found on, not one another client
    // saved since.
    let active = crate::TARGET
        .lock()
        .ok()
        .and_then(|active| active.clone())
        .map(|(target, _)| target);
    a206_runtime::doctor::check_detached(root, active, remote)
        .await
        .map_err(|error| a206_runtime::error::report(&error))
}
#[cfg(target_arch = "wasm32")]
async fn check() -> Result<DoctorReport, ErrorReport> {
    crate::web_request("/api/health", ()).await
}

/// Make one offered repair. A model-list refresh is the gateway command the
/// Connections page sends; permission repairs change files on the computer
/// that ran the checks.
async fn apply(fix: Repair) -> Result<(), ErrorReport> {
    if let Repair::RefreshCatalog { provider, .. } = fix {
        return crate::call(Command::RefreshCatalog { provider })
            .await
            .map(drop);
    }
    let result = permissions(fix).await;
    if let Err(error) = &result {
        record("doctor repair", error);
    }
    result
}
#[cfg(not(target_arch = "wasm32"))]
async fn permissions(fix: Repair) -> Result<(), ErrorReport> {
    let (root, remote) = profile()?;
    a206_runtime::doctor::repair_detached(root, remote, fix)
        .await
        .map_err(|error| a206_runtime::error::report(&error))?
        .map_err(|error| a206_runtime::error::report(&error))
}
#[cfg(target_arch = "wasm32")]
async fn permissions(fix: Repair) -> Result<(), ErrorReport> {
    crate::web_request::<_, Result<(), ErrorReport>>("/api/repair", fix).await?
}

#[component]
pub(crate) fn HealthChecks() -> Element {
    let mut report = use_signal(|| None::<Result<DoctorReport, ErrorReport>>);
    let mut running = use_signal(|| false);
    let mut repairing = use_signal(|| None::<Repair>);
    let mut failed = use_signal(|| None::<(Repair, ErrorReport)>);
    let mut refresh = move || {
        running.set(true);
        failed.set(None);
        spawn(async move {
            let result = check().await;
            if let Err(error) = &result {
                record("doctor", error);
            }
            report.set(Some(result));
            running.set(false);
        });
    };
    let mut make = move |fix: Repair| {
        repairing.set(Some(fix.clone()));
        failed.set(None);
        spawn(async move {
            match apply(fix.clone()).await {
                Ok(()) => refresh(),
                Err(error) => failed.set(Some((fix, error))),
            }
            repairing.set(None);
        });
    };
    let busy = running() || repairing.read().is_some();
    rsx! { Card { variant: CardVariant::Subtle,
        CardHeader { CardTitle { span { class: "a206-icon-label", Icon { name: IconName::Info, size: IconSize::Sm } "Health checks" } } }
        CardBody {
            match &*report.read() {
                None => rsx! { p { class: "a206-muted", if cfg!(target_arch = "wasm32") { "Check the web server's profile, gateway, vault broker and provider accounts. Nothing is started or changed." } else { "Check the profile, gateway, vault broker and provider accounts. Nothing is started or changed." } } },
                Some(Err(error)) => rsx! { ErrorNotice { report: error.clone(), title: "Health checks could not run".to_string() } },
                Some(Ok(report)) => rsx! {
                    ul { class: "a206-stack", aria_label: "Health check results",
                        for check in report.checks.iter() {
                            li { class: "a206-row",
                                Badge { tone: tone(check.health),
                                    Icon { size: IconSize::Sm, name: match check.health {
                                        Level::Ok => IconName::Check,
                                        Level::Warning => IconName::Warning,
                                        Level::Unknown => IconName::Help,
                                        Level::Problem => IconName::Error,
                                    } }
                                    "{label(check.health)}"
                                }
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
                    if !report.repairs.is_empty() {
                        h3 { "Repairs" }
                        p { class: "a206-muted", "Each repair only removes group and other access, or fetches a model list from the provider's API. It is made when you press its button." }
                        ul { class: "a206-stack", aria_label: "Available repairs",
                            for fix in report.repairs.iter().cloned() {
                                li { class: "a206-row",
                                    span { "{fix}" }
                                    Button {
                                        variant: ButtonVariant::Secondary,
                                        busy: repairing.read().as_ref() == Some(&fix),
                                        availability: if busy { Availability::Disabled } else { Availability::Enabled },
                                        aria_label: format!("Repair: {fix}"),
                                        onclick: { let fix = fix.clone(); move |_| make(fix.clone()) },
                                        Icon { name: IconName::Settings, size: IconSize::Sm } "Repair"
                                    }
                                }
                            }
                        }
                    }
                    if let Some((fix, error)) = &*failed.read() {
                        ErrorNotice { report: error.clone(), title: format!("Could not {fix}") }
                    }
                },
            }
        }
        CardFooter {
            Button { variant: ButtonVariant::Secondary, busy: running(), availability: if busy { Availability::Disabled } else { Availability::Enabled }, onclick: move |_| refresh(), if !running() { Icon { name: IconName::Refresh, size: IconSize::Sm } } if running() { "Checking…" } else { "Run health checks" } }
        }
    } }
}
