//! The memory tools' failures. Each has a stable code; the guest turns it
//! into the ABI's `failed { code, message }` only at the export edge.
use crate::{
    paths::PathError,
    tools::{MAX_NOTE_BYTES, MAX_NOTE_FILE, MAX_QUERY_BYTES, MAX_QUERY_TERMS},
};
use a206_extension_sdk::{GuestError, HostFailure};

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error(transparent)]
    Path(#[from] PathError),
    /// An argument the SDK could not read as its declared kind.
    #[error(transparent)]
    Argument(GuestError),
    #[error("`{0}` is required")]
    Missing(&'static str),
    #[error("`{name}` must be from {min} to {max}")]
    OutOfRange {
        name: &'static str,
        min: i64,
        max: i64,
    },
    #[error("the query is longer than {MAX_QUERY_BYTES} bytes")]
    QueryTooLong,
    #[error("the query has no words to search for")]
    EmptyQuery,
    #[error("the query has more than {MAX_QUERY_TERMS} different words")]
    TooManyTerms,
    #[error("`{0}` does not exist, or is reached through a symbolic link")]
    NotFound(String),
    #[error("`{0}` is not UTF-8 text")]
    NotText(String),
    #[error("`{path}` has {total} lines; line {from} is past its end")]
    PastEnd { path: String, from: u64, total: u64 },
    #[error("the note text is empty")]
    EmptyNote,
    #[error("the note text is longer than {MAX_NOTE_BYTES} bytes")]
    NoteTooLong,
    #[error("`{0}` would grow past {MAX_NOTE_FILE} bytes; save to a new note")]
    NoteFull(String),
    #[error(
        "`{0}` is empty, so an append could not detect a change made meanwhile; save to a new note"
    )]
    EmptyFile(String),
    /// The host refused or failed a call; its own code is kept.
    #[error(transparent)]
    Host(#[from] HostFailure),
}

impl MemoryError {
    pub fn code(&self) -> &str {
        match self {
            Self::Path(_) => "not_memory_path",
            Self::Argument(error) => &error.code,
            Self::Missing(_)
            | Self::OutOfRange { .. }
            | Self::QueryTooLong
            | Self::EmptyQuery
            | Self::TooManyTerms
            | Self::EmptyNote
            | Self::NoteTooLong => "invalid_argument",
            Self::NotFound(_) => "not_found",
            Self::NotText(_) => "not_text",
            Self::PastEnd { .. } => "out_of_range",
            Self::NoteFull(_) => "note_full",
            Self::EmptyFile(_) => "empty_note_file",
            Self::Host(failure) => failure.code.as_str(),
        }
    }
}

impl From<MemoryError> for GuestError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::Argument(error) => error,
            MemoryError::Host(failure) => failure.into(),
            error => GuestError::new(error.code(), error.to_string()),
        }
    }
}
