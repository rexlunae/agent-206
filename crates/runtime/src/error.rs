//! Conversion of runtime failures into protocol [`ErrorReport`]s.
//!
//! Code inside the runtime keeps typed errors (`anyhow::Error` with its source
//! chain). Where a failure is handled (a reply, a failed run or download) it is
//! captured once with [`report`], recorded with [`record`] and sent as the
//! report. Never stringify an error to carry it further.
use a206_core::{ErrorKind, ErrorReport};
use std::error::Error;

/// Capture an error and its cause chain with runtime classifications.
/// URLs in transport errors are redacted: they can carry signed tokens.
pub fn report(error: &anyhow::Error) -> ErrorReport {
    let mut report = ErrorReport::capture(error.as_ref(), classify);
    redact(&mut report, error.as_ref());
    report
}

/// Record a handled failure as a structured tracing event. The report, not the
/// raw error, is recorded so redaction applies to logs as well as clients.
pub fn record(operation: &'static str, report: &ErrorReport) {
    let error: &(dyn Error + 'static) = report;
    match report.kind {
        // Broken invariants and lost state are errors; the rest are expected outcomes.
        ErrorKind::Internal | ErrorKind::Storage => tracing::error!(
            operation,
            kind = ?report.kind,
            correlation = report.correlation.map(tracing::field::display),
            error,
            "operation failed"
        ),
        _ => tracing::warn!(
            operation,
            kind = ?report.kind,
            correlation = report.correlation.map(tracing::field::display),
            error,
            "operation failed"
        ),
    }
}

/// Capture and record in one step, for failures handled where they occur.
pub fn handle(operation: &'static str, error: &anyhow::Error) -> ErrorReport {
    let report = report(error);
    record(operation, &report);
    report
}

/// Capture and record a failure that belongs to a request, run or transfer,
/// tagging the report and the log event with that id.
pub fn correlated(
    operation: &'static str,
    correlation: uuid::Uuid,
    error: &anyhow::Error,
) -> ErrorReport {
    let report = report(error).with_correlation(correlation);
    record(operation, &report);
    report
}

/// An error for a lock whose holder panicked.
pub fn poisoned(lock: &str) -> anyhow::Error {
    ErrorReport::new(ErrorKind::Internal, format!("{lock} lock poisoned")).into()
}

/// An error for a missing resource.
pub fn not_found(message: impl Into<String>) -> anyhow::Error {
    ErrorReport::new(ErrorKind::NotFound, message).into()
}

fn classify(error: &(dyn Error + 'static)) -> Option<ErrorKind> {
    if error.is::<a206_protocol::Error>() {
        return Some(ErrorKind::Protocol);
    }
    if error.is::<tokio::time::error::Elapsed>() {
        return Some(ErrorKind::Timeout);
    }
    if error.is::<rusqlite::Error>() {
        return Some(ErrorKind::Storage);
    }
    if error.is::<rmp_serde::decode::Error>() || error.is::<rmp_serde::encode::Error>() {
        return Some(ErrorKind::Protocol);
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return Some(if error.is_timeout() {
            ErrorKind::Timeout
        } else if error.is_connect() {
            ErrorKind::Unavailable
        } else {
            ErrorKind::Provider
        });
    }
    None
}

fn redact(report: &mut ErrorReport, error: &(dyn Error + 'static)) {
    let urls: Vec<String> = std::iter::successors(Some(error), |&link| link.source())
        .filter_map(|link| link.downcast_ref::<reqwest::Error>()?.url())
        .map(|url| url.as_str().to_owned())
        .collect();
    // Outer links can quote an inner message, so every link is redacted.
    let mut link = Some(report);
    while let Some(entry) = link {
        for url in &urls {
            entry.message = entry.message.replace(url, "<redacted URL>");
        }
        link = entry.source.as_deref_mut();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn context_chain_survives_capture() {
        let detail = uuid::Uuid::new_v4().to_string();
        let error = Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            detail.clone(),
        ))
        .context("could not write the settings file")
        .unwrap_err();
        let report = report(&error);
        assert_eq!(report.kind, ErrorKind::PermissionDenied);
        assert_eq!(report.message, "could not write the settings file");
        assert_eq!(report.causes().next().unwrap().message, detail);
        assert_eq!(report.detail(), format!("{error:#}"));
    }

    #[test]
    fn explicit_kinds_survive_added_context() {
        let error = Err::<(), _>(not_found("thread not found"))
            .context("could not send the message")
            .unwrap_err();
        let report = report(&error);
        assert_eq!(report.kind, ErrorKind::NotFound);
        assert_eq!(report.chain().count(), 2);
        assert_eq!(super::report(&poisoned("state")).kind, ErrorKind::Internal);
    }

    #[tokio::test]
    async fn transport_errors_do_not_reveal_urls() {
        let secret = uuid::Uuid::new_v4().simple().to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = reqwest::get(format!("http://{address}/download?token={secret}"))
            .await
            .context("download failed")
            .unwrap_err();
        let report = report(&error);
        assert_eq!(report.kind, ErrorKind::Unavailable);
        assert!(!report.detail().contains(&secret), "{}", report.detail());
    }
}
