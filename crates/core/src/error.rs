//! Structured errors that cross the gateway protocol.
//!
//! Errors stay typed inside a process. At a boundary (a gateway reply, a
//! persisted record, a client view) they become an [`ErrorReport`]: a kind a
//! client can act on, the outermost message, and the cause chain. A report is
//! itself a [`std::error::Error`] whose `source()` walks that chain, so clients
//! can record it through `tracing` or wrap it in `anyhow` without flattening it.
use serde::{Deserialize, Deserializer, Serialize};
use std::{error::Error, fmt};

/// Longest cause chain kept in a report; deeper causes are summarized.
pub const MAX_CHAIN: usize = 16;
/// Longest message kept for one link of a report.
pub const MAX_MESSAGE: usize = 4096;

/// What a client can do about a failure. Unknown kinds from newer peers
/// decode as [`ErrorKind::Other`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorKind {
    /// The request was malformed or failed validation.
    InvalidRequest,
    /// A referenced resource does not exist.
    NotFound,
    /// The request conflicts with current state or an earlier request.
    Conflict,
    /// Policy denied the operation.
    PermissionDenied,
    /// The gateway or a dependency is not reachable or not accepting work.
    Unavailable,
    /// An operation did not finish in time; its outcome may be unknown.
    Timeout,
    /// The operation was cancelled.
    Cancelled,
    /// A peer sent an invalid or unsupported frame.
    Protocol,
    /// A model provider or other remote API failed.
    Provider,
    /// Durable state could not be read or written.
    Storage,
    /// An internal invariant failed.
    Internal,
    /// Not classified.
    #[default]
    #[serde(other)]
    Other,
}
impl ErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid request",
            Self::NotFound => "Not found",
            Self::Conflict => "Conflict",
            Self::PermissionDenied => "Permission denied",
            Self::Unavailable => "Unavailable",
            Self::Timeout => "Timed out",
            Self::Cancelled => "Cancelled",
            Self::Protocol => "Protocol error",
            Self::Provider => "Provider error",
            Self::Storage => "Storage error",
            Self::Internal => "Internal error",
            Self::Other => "Error",
        }
    }
}

/// A serializable error with its cause chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorReport {
    pub kind: ErrorKind,
    pub message: String,
    pub source: Option<Box<ErrorReport>>,
}

impl ErrorReport {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: bounded(message.into()),
            source: None,
        }
    }
    /// Attach a cause below the innermost link of this report.
    pub fn caused_by(mut self, cause: ErrorReport) -> Self {
        let mut link = &mut self;
        let mut depth = 1;
        while link.source.is_some() && depth < MAX_CHAIN {
            link = link.source.as_mut().expect("checked above");
            depth += 1;
        }
        if depth < MAX_CHAIN {
            link.source = Some(Box::new(cause));
        }
        self.truncate();
        self
    }
    /// Capture an error and its `source()` chain. `classify` names the kind of
    /// each link; the report's own kind is the outermost classified link.
    pub fn capture(
        error: &(dyn Error + 'static),
        classify: impl Fn(&(dyn Error + 'static)) -> Option<ErrorKind>,
    ) -> Self {
        let classify =
            |link: &(dyn Error + 'static)| classify(link).or_else(|| classify_core(link));
        let mut links = Vec::new();
        let mut next = Some(error);
        while let Some(link) = next {
            if links.len() == MAX_CHAIN {
                links.push(Self::new(ErrorKind::Other, "further causes omitted"));
                break;
            }
            links.push(Self::new(
                classify(link).unwrap_or_default(),
                link.to_string(),
            ));
            next = link.source();
        }
        let kind = links
            .iter()
            .map(|link| link.kind)
            .find(|kind| *kind != ErrorKind::Other)
            .unwrap_or_default();
        let mut report = links
            .into_iter()
            .rev()
            .reduce(|cause, mut link| {
                link.source = Some(Box::new(cause));
                link
            })
            .unwrap_or_else(|| Self::new(ErrorKind::Other, "unknown error"));
        report.kind = kind;
        report
    }
    /// This report followed by each cause, outermost first.
    pub fn chain(&self) -> impl Iterator<Item = &ErrorReport> {
        std::iter::successors(Some(self), |link| link.source.as_deref())
    }
    /// The causes below the outermost message.
    pub fn causes(&self) -> impl Iterator<Item = &ErrorReport> {
        self.chain().skip(1)
    }
    /// Messages joined like `anyhow`'s alternate format, for plain-text sinks.
    pub fn detail(&self) -> String {
        self.chain()
            .map(|link| link.message.as_str())
            .collect::<Vec<_>>()
            .join(": ")
    }
    fn truncate(&mut self) {
        let mut link = self;
        for _ in 1..MAX_CHAIN {
            match link.source {
                Some(ref mut next) => link = next,
                None => return,
            }
        }
        link.source = None;
    }
}
impl fmt::Display for ErrorReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            f.write_str(&self.detail())
        } else {
            f.write_str(&self.message)
        }
    }
}
impl Error for ErrorReport {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}
impl From<crate::PolicyError> for ErrorReport {
    fn from(error: crate::PolicyError) -> Self {
        Self::capture(&error, |_| None)
    }
}

/// Accepts reports and the plain-string errors written by earlier versions.
impl<'de> Deserialize<'de> for ErrorReport {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Legacy(String),
            Report {
                #[serde(default)]
                kind: ErrorKind,
                message: String,
                #[serde(default)]
                source: Option<Box<ErrorReport>>,
            },
        }
        let mut report = match Wire::deserialize(deserializer)? {
            Wire::Legacy(message) => Self::new(ErrorKind::Other, message),
            Wire::Report {
                kind,
                message,
                source,
            } => Self {
                kind,
                message: bounded(message),
                source,
            },
        };
        report.truncate();
        Ok(report)
    }
}

fn classify_core(error: &(dyn Error + 'static)) -> Option<ErrorKind> {
    if let Some(report) = error.downcast_ref::<ErrorReport>() {
        return Some(report.kind).filter(|kind| *kind != ErrorKind::Other);
    }
    if error.is::<crate::PolicyError>() {
        return Some(ErrorKind::PermissionDenied);
    }
    let io = error.downcast_ref::<std::io::Error>()?;
    use std::io::ErrorKind as Io;
    Some(match io.kind() {
        Io::NotFound => ErrorKind::NotFound,
        Io::PermissionDenied => ErrorKind::PermissionDenied,
        Io::TimedOut => ErrorKind::Timeout,
        Io::ConnectionRefused
        | Io::ConnectionReset
        | Io::ConnectionAborted
        | Io::NotConnected
        | Io::BrokenPipe
        | Io::UnexpectedEof => ErrorKind::Unavailable,
        Io::InvalidInput | Io::InvalidData => ErrorKind::InvalidRequest,
        Io::AlreadyExists => ErrorKind::Conflict,
        _ => return None,
    })
}

fn bounded(mut message: String) -> String {
    if message.len() > MAX_MESSAGE {
        let mut end = MAX_MESSAGE;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Outer(std::io::Error);
    impl fmt::Display for Outer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("could not open the workspace")
        }
    }
    impl Error for Outer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn capture_keeps_the_cause_chain_and_classifies_it() {
        let cause = uuid::Uuid::new_v4().to_string();
        let error = Outer(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            cause.clone(),
        ));
        let report = ErrorReport::capture(&error, |_| None);
        assert_eq!(report.kind, ErrorKind::NotFound);
        assert_eq!(report.message, error.to_string());
        let causes: Vec<_> = report.causes().collect();
        assert_eq!(causes.len(), 1);
        assert_eq!(causes[0].message, cause);
        assert_eq!(causes[0].kind, ErrorKind::NotFound);
        assert_eq!(format!("{report:#}"), format!("{error}: {cause}"));
        // The report is itself an error chain for tracing and anyhow.
        let source = Error::source(&report).expect("cause");
        assert_eq!(source.to_string(), cause);
    }

    #[test]
    fn explicit_classification_wins_and_policy_is_denial() {
        let report = ErrorReport::capture(&Outer(std::io::Error::other("x")), |link| {
            link.is::<Outer>().then_some(ErrorKind::Storage)
        });
        assert_eq!(report.kind, ErrorKind::Storage);
        let denied: ErrorReport = crate::PolicyError::ModelDenied.into();
        assert_eq!(denied.kind, ErrorKind::PermissionDenied);
    }

    #[test]
    fn chains_and_messages_are_bounded() {
        let mut report = ErrorReport::new(ErrorKind::Other, "x".repeat(MAX_MESSAGE * 2));
        assert!(report.message.len() <= MAX_MESSAGE + '…'.len_utf8());
        for _ in 0..MAX_CHAIN * 2 {
            report = report.caused_by(ErrorReport::new(ErrorKind::Other, "cause"));
        }
        assert_eq!(report.chain().count(), MAX_CHAIN);
    }

    #[test]
    fn decodes_reports_legacy_strings_and_unknown_kinds() {
        let report = ErrorReport::new(ErrorKind::Conflict, uuid::Uuid::new_v4().to_string())
            .caused_by(ErrorReport::new(ErrorKind::Storage, "disk"));
        let bytes = rmp_serde::to_vec_named(&report).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<ErrorReport>(&bytes).unwrap(),
            report
        );

        let legacy = uuid::Uuid::new_v4().to_string();
        let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
        let decoded: ErrorReport = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!((decoded.kind, decoded.message), (ErrorKind::Other, legacy));

        #[derive(Serialize)]
        struct Future {
            kind: &'static str,
            message: &'static str,
        }
        let bytes = rmp_serde::to_vec_named(&Future {
            kind: "SomethingNew",
            message: "later",
        })
        .unwrap();
        let decoded: ErrorReport = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded.kind, ErrorKind::Other);
    }
}
