//! Client-side handling of structured gateway errors.
use a206_core::ErrorReport;
use fc_dioxus::prelude::*;

/// Record a failure the client is about to show, keeping its cause chain.
pub(crate) fn record(operation: &'static str, report: &ErrorReport) {
    let error: &(dyn std::error::Error + 'static) = report;
    tracing::warn!(operation, kind = ?report.kind, error, "request failed");
}

/// A failure with its kind and causes; causes stay collapsed until asked for.
#[component]
pub(crate) fn ErrorNotice(
    report: ErrorReport,
    #[props(default)] title: Option<String>,
    #[props(default)] dismiss: Option<EventHandler<()>>,
) -> Element {
    let causes: Vec<_> = report.causes().map(|cause| cause.message.clone()).collect();
    let title = title.unwrap_or_else(|| report.kind.label().to_owned());
    rsx! {
        Alert {
            tone: AlertTone::Danger,
            urgency: Urgency::Polite,
            class: "a206-error",
            title: rsx! { "{title}" },
            dismiss: dismiss.map(|ondismiss| Dismiss { label: name!("Dismiss error"), ondismiss }),
            actions: if causes.is_empty() { None } else { Some(rsx! {
                Disclosure { summary: rsx! { "Details" },
                    ol { class: "a206-error-causes",
                        for (index, cause) in causes.iter().enumerate() {
                            li { key: "{index}", "{cause}" }
                        }
                    }
                }
            }) },
            "{report.message}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::ErrorKind;

    fn render(report: ErrorReport) -> String {
        fn root(report: ErrorReport) -> Element {
            rsx! { ErrorNotice { report } }
        }
        let mut dom = VirtualDom::new_with_props(root, report);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn shows_the_kind_message_and_every_cause() {
        let message = uuid::Uuid::new_v4().to_string();
        let cause = uuid::Uuid::new_v4().to_string();
        let html = render(
            ErrorReport::new(ErrorKind::NotFound, message.clone())
                .caused_by(ErrorReport::new(ErrorKind::Other, cause.clone())),
        );
        assert!(html.contains(ErrorKind::NotFound.label()), "{html}");
        assert!(html.contains(&message), "{html}");
        assert!(html.contains(&cause), "{html}");
        assert!(html.contains("<details"), "{html}");
    }

    #[test]
    fn omits_details_without_causes() {
        let html = render(ErrorReport::new(ErrorKind::Other, "x"));
        assert!(!html.contains("<details"), "{html}");
    }
}
