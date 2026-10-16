//! Conversion of runtime failures into protocol [`ErrorReport`]s.
//!
//! Each runtime module keeps its own `thiserror` enum whose causes stay in
//! `source()`. Where a failure is handled (a reply, a failed run or download)
//! it is captured once with [`report`], recorded with [`record`] and sent as
//! the report. Never stringify an error to carry it further.
use a206_core::{ErrorKind, ErrorReport};
use std::error::Error;

/// A runtime error type that knows the client-facing kind of its failure.
/// `None` defers to the error's causes, for wrappers that add only context.
/// A `#[error(transparent)]` variant hides its inner error from the source
/// chain, so it must return [`classify`] of that inner error itself.
pub trait Classified {
    fn kind(&self) -> Option<ErrorKind>;
}

/// Fail with `error` unless `condition` holds.
pub(crate) fn check<E>(condition: bool, error: E) -> Result<(), E> {
    if condition { Ok(()) } else { Err(error) }
}

/// Capture an error and its cause chain with runtime classifications.
/// URLs in transport errors are redacted: they can carry signed tokens.
pub fn report(error: &(dyn Error + 'static)) -> ErrorReport {
    let mut report = ErrorReport::capture(error, classify);
    redact(&mut report, error);
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
pub fn handle(operation: &'static str, error: &(dyn Error + 'static)) -> ErrorReport {
    let report = report(error);
    record(operation, &report);
    report
}

/// Capture and record a failure that belongs to a request, run or transfer,
/// tagging the report and the log event with that id.
pub fn correlated(
    operation: &'static str,
    correlation: uuid::Uuid,
    error: &(dyn Error + 'static),
) -> ErrorReport {
    let report = report(error).with_correlation(correlation);
    record(operation, &report);
    report
}

/// The kind of one link of an error chain, if the runtime knows it.
pub(crate) fn classify(error: &(dyn Error + 'static)) -> Option<ErrorKind> {
    macro_rules! classified {
        ($($error:ty),* $(,)?) => {$(
            if let Some(error) = error.downcast_ref::<$error>() {
                return error.kind();
            }
        )*};
    }
    classified!(
        crate::backends::BackendError,
        crate::backends::proxy::ProxyError,
        crate::client::ClientError,
        crate::connection::ConnectionError,
        crate::control::ControlError,
        crate::gateway::GatewayError,
        crate::joshua::JoshuaError,
        crate::network::DownloadError,
        crate::process::ProcessError,
        crate::provider::EndpointError,
        crate::provider::PresetError,
        crate::provider::ProviderError,
        crate::service::ServiceError,
        crate::store::StoreError,
        crate::transport::FrameError,
        crate::transport::LaunchError,
        crate::transport::SshEndpointError,
        crate::vault::VaultError,
        a206_broker::BrokerError,
        a206_broker::ClientError,
        a206_extensions::ExtensionError,
        a206_protocol::Error,
        a206_sandbox::SandboxError,
        reqwest::Error,
    );
    if error.is::<tokio::time::error::Elapsed>() {
        return Some(ErrorKind::Timeout);
    }
    if error.is::<rusqlite::Error>() {
        return Some(ErrorKind::Storage);
    }
    if error.is::<rmp_serde::decode::Error>() || error.is::<rmp_serde::encode::Error>() {
        return Some(ErrorKind::Protocol);
    }
    None
}

impl Classified for a206_sandbox::SandboxError {
    fn kind(&self) -> Option<ErrorKind> {
        a206_sandbox::SandboxError::kind(self)
    }
}

impl Classified for a206_broker::BrokerError {
    fn kind(&self) -> Option<ErrorKind> {
        use a206_broker::{BrokerError as B, UpstreamFailure as U};
        Some(match self {
            B::Locked | B::NoVault | B::LockedOut { .. } | B::GatewayNotPaired => {
                ErrorKind::Unavailable
            }
            B::AuthenticationFailed
            | B::TotpRequired
            | B::ClockDrift { .. }
            | B::TotpReplayed
            | B::StepUpRequired
            | B::Forbidden
            | B::Denied(_) => ErrorKind::PermissionDenied,
            B::NoPendingEnrollment | B::AlreadyExists | B::VaultExists => ErrorKind::Conflict,
            B::NotFound => ErrorKind::NotFound,
            B::Invalid(_) => ErrorKind::InvalidRequest,
            B::Vault(_) => ErrorKind::Storage,
            B::Upstream(U::Timeout) => ErrorKind::Timeout,
            B::Upstream(U::Connect) => ErrorKind::Unavailable,
            B::Upstream(_) => ErrorKind::Provider,
        })
    }
}
impl Classified for a206_broker::ClientError {
    fn kind(&self) -> Option<ErrorKind> {
        use a206_broker::ClientError as C;
        match self {
            C::Io(_) | C::Closed => Some(ErrorKind::Unavailable),
            C::Protocol(_) | C::Stream(_) => Some(ErrorKind::Protocol),
            // The provider layer falls back before this can reach a client.
            C::StreamUnsupported => Some(ErrorKind::Unavailable),
            // `transparent` hides the reply from the chain, so classify it here.
            C::Broker(reply) => reply.kind(),
        }
    }
}
impl Classified for a206_extensions::ExtensionError {
    fn kind(&self) -> Option<ErrorKind> {
        use a206_extensions::ExtensionError as X;
        Some(match self {
            X::Manifest(_) | X::Abi(_) | X::Id | X::Entry | X::TooLarge | X::Module(_) => {
                ErrorKind::InvalidRequest
            }
            // The extension broke its ABI contract.
            X::MissingRun(_)
            | X::MissingMemory
            | X::OutputBounds(_)
            | X::OutputTooLarge
            | X::Output(_)
            | X::TooManyFields => ErrorKind::Protocol,
            X::Runtime(_) => return None,
        })
    }
}
impl Classified for a206_protocol::Error {
    fn kind(&self) -> Option<ErrorKind> {
        Some(ErrorKind::Protocol)
    }
}
impl Classified for reqwest::Error {
    fn kind(&self) -> Option<ErrorKind> {
        Some(if self.is_timeout() {
            ErrorKind::Timeout
        } else if self.is_connect() {
            ErrorKind::Unavailable
        } else {
            ErrorKind::Provider
        })
    }
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

    /// A caller's context around a cause, as runtime modules add it.
    #[derive(Debug, thiserror::Error)]
    enum Context {
        #[error("could not write the settings file")]
        Write(#[source] std::io::Error),
        #[error("download failed")]
        Download(#[source] reqwest::Error),
    }

    #[test]
    fn context_chain_survives_capture() {
        let detail = uuid::Uuid::new_v4().to_string();
        let error = Context::Write(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            detail.clone(),
        ));
        let report = report(&error);
        assert_eq!(report.kind, ErrorKind::PermissionDenied);
        assert_eq!(report.message, "could not write the settings file");
        assert_eq!(report.causes().next().unwrap().message, detail);
        assert_eq!(
            report.detail(),
            format!("could not write the settings file: {detail}")
        );
    }

    #[test]
    fn explicit_kinds_survive_added_context() {
        use crate::{gateway::GatewayError, store::StoreError};
        let wrapped = report(&GatewayError::Receipt(StoreError::ThreadNotFound));
        assert_eq!(wrapped.kind, ErrorKind::NotFound);
        assert_eq!(wrapped.chain().count(), 2);
        // A transparent wrapper is not a link of its own, but keeps the inner kind.
        let transparent = report(&GatewayError::Store(StoreError::ThreadNotFound));
        assert_eq!(transparent.kind, ErrorKind::NotFound);
        assert_eq!(transparent.chain().count(), 1);
        let poisoned = report(&GatewayError::Poisoned { lock: "state" });
        assert_eq!(poisoned.kind, ErrorKind::Internal);
    }

    #[test]
    fn missing_paths_keep_their_io_kind() {
        use std::io::{Error, ErrorKind as Io};
        for (io, kind) in [
            (Io::NotFound, ErrorKind::NotFound),
            (Io::PermissionDenied, ErrorKind::PermissionDenied),
        ] {
            let backend = crate::backends::BackendError::Missing {
                what: "backend executable",
                source: Error::from(io),
            };
            assert_eq!(report(&backend).kind, kind);
            let model = crate::joshua::JoshuaError::MissingModel(Error::from(io));
            assert_eq!(report(&model).kind, kind);
        }
    }

    #[test]
    fn broker_failures_keep_their_kind() {
        let denied = crate::provider::ProviderError::Broker(a206_broker::ClientError::from(
            a206_broker::BrokerError::StepUpRequired,
        ));
        assert_eq!(report(&denied).kind, ErrorKind::PermissionDenied);
        for (failure, kind) in [
            (a206_broker::UpstreamFailure::Timeout, ErrorKind::Timeout),
            (
                a206_broker::UpstreamFailure::Connect,
                ErrorKind::Unavailable,
            ),
            (a206_broker::UpstreamFailure::Transfer, ErrorKind::Provider),
        ] {
            let upstream =
                a206_broker::ClientError::from(a206_broker::BrokerError::Upstream(failure));
            assert_eq!(report(&upstream).kind, kind);
        }
        let offline =
            a206_broker::ClientError::from(std::io::Error::from(std::io::ErrorKind::NotFound));
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
        let error = Context::Write(std::io::Error::other(cause.clone()));
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
        let error = Context::Download(
            reqwest::get(format!("http://{address}/download?token={secret}"))
                .await
                .unwrap_err(),
        );
        let report = report(&error);
        assert_eq!(report.kind, ErrorKind::Unavailable);
        assert!(!report.detail().contains(&secret), "{}", report.detail());
    }
}
