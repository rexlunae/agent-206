//! What the pre-install audit found, beside the text the user reviews.
use a206_core::{AuditFinding, AuditLocation, AuditReport};
use fc_dioxus::prelude::*;

fn place(finding: &AuditFinding) -> String {
    match &finding.location {
        AuditLocation::Line(line) => format!("Line {line}"),
        AuditLocation::File { path, line } => match line {
            Some(line) => format!("{path}, line {line}"),
            None => path.clone(),
        },
        AuditLocation::Field { item: None, field } => {
            let label = field.label();
            let mut chars = label.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
        AuditLocation::Field {
            item: Some(item),
            field,
        } => format!("{item}, {}", field.label()),
    }
}

/// Findings as a warning to check before approving; a clean report says what
/// the audit covers, since finding nothing does not make content safe.
#[component]
pub fn AuditFindings(report: AuditReport) -> Element {
    if report.is_clean() {
        return rsx! { p { class: "a206-muted", "The audit found none of the risky patterns it checks for. Review the text yourself as well." } };
    }
    rsx! {
        Alert { tone: AlertTone::Warning, title: rsx! { "Check before approving" },
            p { "The audit matched patterns that often appear in harmful instructions. They can be legitimate; read these places before you approve." }
            List { divided: true,
                for (index, finding) in report.findings.iter().enumerate() {
                    ListItem { key: "{index}", headline: rsx! { "{finding.rule.label()}" }, supporting: rsx! { "{place(finding)}" } }
                }
            }
            if report.more > 0 { p { class: "a206-muted", "And {report.more} more." } }
        }
    }
}
