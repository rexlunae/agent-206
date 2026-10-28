//! The work queue tools' failures. Each has a stable code; the guest turns
//! it into the ABI's `failed { code, message }` only at the export edge.
use crate::{
    queue::{MAX_ENTRIES, MAX_FIELD, QUEUE},
    tools::MAX_FILE,
};
use a206_extension_sdk::{GuestError, HostFailure};

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// An argument the SDK could not read as its declared kind.
    #[error(transparent)]
    Argument(GuestError),
    #[error("`{0}` is required")]
    Missing(&'static str),
    #[error("give either `id` or `area`, not both")]
    IdAndArea,
    #[error(
        "`{0}` is not a work ID: an uppercase letter, then 2 to 63 uppercase letters, digits or hyphens, such as API-012"
    )]
    InvalidId(String),
    #[error("`{0}` is not a work area: uppercase letters, digits and hyphens, such as API")]
    InvalidArea(String),
    #[error("`{0}` is empty")]
    EmptyField(&'static str),
    #[error("`{0}` must be one line")]
    MultiLine(&'static str),
    #[error("`{0}` is longer than {MAX_FIELD} bytes")]
    LongField(&'static str),
    #[error("`{0}` has more than {MAX_ENTRIES} entries")]
    TooManyEntries(&'static str),
    #[error("`{0}` needs at least one entry")]
    EmptyChecklist(&'static str),
    #[error("`{0}` must be a positive number")]
    Number(&'static str),
    #[error("work item {0} already exists")]
    Duplicate(String),
    #[error("{QUEUE} does not exist; record an item with work_record first")]
    NoQueue,
    #[error("there is no work item {0}")]
    NoItem(String),
    #[error("work item {id} has no `{group}` checklist")]
    NoGroup { id: String, group: String },
    #[error("work item {id} has {count} `{group}` tasks; number them from 1")]
    NoTask {
        id: String,
        group: String,
        count: usize,
    },
    #[error("task {number} of `{group}` in {id} is already checked")]
    AlreadyChecked {
        id: String,
        group: String,
        number: usize,
    },
    #[error(
        "an evidence box is checked only with its result: give `result`, such as the test run and its outcome"
    )]
    EvidenceNeedsResult,
    #[error("work item {id} still has unchecked tasks: {}", open.join("; "))]
    Incomplete { id: String, open: Vec<String> },
    #[error(
        "`{0}` is not a changelog: give CHANGELOG.md or a relative path ending in /CHANGELOG.md"
    )]
    NotChangelog(String),
    #[error("`{0}` is not UTF-8 text")]
    NotText(String),
    #[error("`{0}` is larger than {MAX_FILE} bytes; archive closed items (or old releases) first")]
    TooLarge(String),
    #[error(
        "`{0}` is empty, so a change could not detect an edit made meanwhile; give it a heading first"
    )]
    EmptyFile(String),
    /// The host refused or failed a call; its own code is kept.
    #[error(transparent)]
    Host(#[from] HostFailure),
}

impl QueueError {
    pub fn code(&self) -> &str {
        match self {
            Self::Argument(error) => &error.code,
            Self::Missing(_)
            | Self::IdAndArea
            | Self::InvalidId(_)
            | Self::InvalidArea(_)
            | Self::EmptyField(_)
            | Self::MultiLine(_)
            | Self::LongField(_)
            | Self::TooManyEntries(_)
            | Self::EmptyChecklist(_)
            | Self::Number(_)
            | Self::NotChangelog(_) => "invalid_argument",
            Self::Duplicate(_) => "duplicate",
            Self::NoQueue | Self::NoItem(_) | Self::NoGroup { .. } | Self::NoTask { .. } => {
                "not_found"
            }
            Self::AlreadyChecked { .. } => "already_checked",
            Self::EvidenceNeedsResult => "evidence_needs_result",
            Self::Incomplete { .. } => "incomplete",
            Self::NotText(_) => "not_text",
            Self::TooLarge(_) => "too_large",
            Self::EmptyFile(_) => "empty_file",
            Self::Host(failure) => failure.code.as_str(),
        }
    }
}

impl From<QueueError> for GuestError {
    fn from(error: QueueError) -> Self {
        match error {
            QueueError::Argument(error) => error,
            QueueError::Host(failure) => failure.into(),
            error => GuestError::new(error.code(), error.to_string()),
        }
    }
}
