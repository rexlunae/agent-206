//! Owner-only TOML provenance after a confirmed copy. A failed receipt does
//! not undo the verified vault: report that partial outcome to the caller.
use a206_core::{ErrorKind, ErrorReport};
use a206_vault_channel::{IMPORT_REVIEW_VERSION, ImportApplied, ImportReview};
use serde::Serialize;
use std::{
    error::Error,
    io::Write,
    path::{Path, PathBuf},
};
#[derive(Debug, thiserror::Error)]
enum ReceiptError {
    #[error("could not create the vault receipt directory")]
    Directory(#[source] std::io::Error),
    #[error("could not encode vault import provenance")]
    Encode(#[source] toml::ser::Error),
    #[error("could not stage a vault receipt")]
    Stage(#[source] std::io::Error),
    #[error("could not write or sync a vault receipt")]
    Write(#[source] std::io::Error),
    #[error("could not publish a vault receipt")]
    Publish(#[source] tempfile::PersistError),
    #[error("could not sync the vault receipt directory")]
    Sync(#[source] std::io::Error),
}
#[derive(Serialize)]
struct Receipt<'a> {
    version: u16,
    id: a206_core::Id,
    source: &'a Path,
    source_bytes: u64,
    source_sha256: String,
    applied_at: u64,
}
fn write(root: &Path, review: &ImportReview) -> Result<PathBuf, ReceiptError> {
    use std::os::unix::fs::PermissionsExt;
    let directory = root.join("vault").join("imports");
    std::fs::create_dir_all(&directory).map_err(ReceiptError::Directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .map_err(ReceiptError::Directory)?;
    let text = toml::to_string(&Receipt {
        version: IMPORT_REVIEW_VERSION,
        id: review.id,
        source: &review.source,
        source_bytes: review.bytes,
        source_sha256: review
            .sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        applied_at: a206_vault::unix_now(),
    })
    .map_err(ReceiptError::Encode)?;
    let mut staged = tempfile::NamedTempFile::new_in(&directory).map_err(ReceiptError::Stage)?;
    staged
        .write_all(text.as_bytes())
        .map_err(ReceiptError::Write)?;
    staged.as_file().sync_all().map_err(ReceiptError::Write)?;
    let path = directory.join(format!("{}.toml", review.id));
    staged
        .persist_noclobber(&path)
        .map_err(ReceiptError::Publish)?;
    std::fs::File::open(&directory)
        .and_then(|d| d.sync_all())
        .map_err(ReceiptError::Sync)?;
    Ok(path)
}
pub(super) fn record(root: &Path, review: ImportReview) -> ImportApplied {
    let (receipt, receipt_error) = match write(root, &review) {
        Ok(path) => (Some(path), None),
        Err(error) => {
            tracing::warn!(error = &error as &dyn Error, import = %review.id, "vault import receipt failed");
            (
                None,
                Some(
                    ErrorReport::new(
                        ErrorKind::Storage,
                        "Vault imported, but receipt durability could not be confirmed.",
                    )
                    .caused_by(ErrorReport::capture(&error, |link| {
                        link.is::<ReceiptError>().then_some(ErrorKind::Storage)
                    }))
                    .with_correlation(review.id),
                ),
            )
        }
    };
    ImportApplied {
        version: IMPORT_REVIEW_VERSION,
        review,
        receipt,
        receipt_error,
    }
}
