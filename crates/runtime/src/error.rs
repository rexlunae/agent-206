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
    if let Some(error) = error.downcast_ref::<a206_broker::BrokerError>() {
        use a206_broker::BrokerError as B;
        return Some(match error {
            B::Locked | B::NoVault | B::LockedOut { .. } => ErrorKind::Unavailable,
            B::AuthenticationFailed
            | B::TotpRequired
            | B::ClockDrift { .. }
            | B::TotpReplayed
            | B::StepUpRequired
            | B::Forbidden
            | B::Denied(_) => ErrorKind::PermissionDenied,
            B::NoPendingEnrollment | B::AlreadyExists => ErrorKind::Conflict,
            B::NotFound => ErrorKind::NotFound,
            B::Invalid(_) => ErrorKind::InvalidRequest,
            B::Vault(_) => ErrorKind::Storage,
            B::Upstream(_) => ErrorKind::Provider,
        });
    }
    if let Some(error) = error.downcast_ref::<a206_broker::ClientError>() {
        use a206_broker::ClientError as C;
        return match error {
            C::Io(_) | C::Closed => Some(ErrorKind::Unavailable),
            C::Protocol(_) => Some(ErrorKind::Protocol),
            // `transparent` hides the reply from the chain, so classify it here.
            C::Broker(reply) => classify(reply),
        };
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

    #[test]
    fn broker_failures_keep_their_kind() {
        let denied = anyhow::Error::from(a206_broker::ClientError::from(
            a206_broker::BrokerError::StepUpRequired,
        ))
        .context("could not use the credential");
        assert_eq!(report(&denied).kind, ErrorKind::PermissionDenied);
        let offline = anyhow::Error::from(a206_broker::ClientError::from(std::io::Error::from(
            std::io::ErrorKind::NotFound,
        )));
        let offline = report(&offline);
        assert_eq!(offline.kind, ErrorKind::Unavailable);
        assert_eq!(offline.chain().count(), 2, "{offline:#}");
    }

    #[test]
    fn recorded_events_keep_the_cause_chain() {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let cause = uuid::Uuid::new_v4().to_string();
        let correlation = uuid::Uuid::new_v4();
        let error = Err::<(), _>(std::io::Error::other(cause.clone()))
            .context("could not write the settings file")
            .unwrap_err();
        tracing::subscriber::with_default(subscriber, || {
            correlated("save_settings", correlation, &error);
        });
        let logged = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(
            logged.contains("could not write the settings file"),
            "{logged}"
        );
        assert!(logged.contains(&cause), "{logged}");
        assert!(logged.contains(&correlation.to_string()), "{logged}");
    }

    #[tokio::test]
    async fn rejected_download_urls_do_not_reveal_their_query() {
        let secret = uuid::Uuid::new_v4().simple().to_string();
        for source in [
            format!("https://exa mple.com/model?token={secret}"),
            format!("https://[::1/model?token={secret}"),
            format!("http://example.com/model?token={secret}"),
            format!("https://user@example.com/model?token={secret}"),
            format!("https://example.com:8443/model?token={secret}"),
        ] {
            let error = crate::network::public_get(&source).await.unwrap_err();
            let report = report(&error);
            assert!(!report.detail().contains(&secret), "{}", report.detail());
        }
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
