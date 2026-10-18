//! Connection-owned reviews. Source identities never come from apply requests.
use super::*;
use a206_vault::{ImportIdentity, ReviewedImportError};
use a206_vault_channel::{IMPORT_REVIEW_VERSION, ImportReview};
use std::path::PathBuf;
mod receipt;

// New reviewed operations retain storage and partial-copy causes at the wire
// boundary. Authentication failures remain deliberately opaque.
fn failure(error: VaultError, id: uuid::Uuid) -> BrokerError {
    use a206_core::{ErrorKind, ErrorReport};
    if !matches!(
        error,
        VaultError::ImportSource(_) | VaultError::Io(_) | VaultError::ImportLeftInPlace(_)
    ) {
        return vault_error(error);
    }
    tracing::warn!(error = &error as &dyn std::error::Error, import = %id, "vault import failed");
    BrokerError::ImportFailure(
        ErrorReport::capture(&error, |link| {
            link.downcast_ref::<VaultError>().map(|error| match error {
                VaultError::ImportSource(_) => ErrorKind::InvalidRequest,
                _ => ErrorKind::Storage,
            })
        })
        .with_correlation(id),
    )
}

pub(super) fn version(session: &Session, version: u16) -> Result<(), BrokerError> {
    if session.version < REVIEWED_IMPORT_VERSION || version != IMPORT_REVIEW_VERSION {
        return Err(BrokerError::Invalid(InvalidRequest::UnsupportedVersion));
    }
    Ok(())
}
fn source(path: &Path) -> Result<(), BrokerError> {
    if !path.is_absolute() || path.as_os_str().len() > 4096 {
        return Err(BrokerError::Invalid(InvalidRequest::ImportSource));
    }
    Ok(())
}
pub(super) fn review(shared: &Shared, session: &mut Session, path: PathBuf) -> Handled {
    // A new attempt invalidates the previous review, even if reading fails.
    session.import_review = None;
    source(&path)?;
    let id = uuid::Uuid::new_v4();
    let identity = shared
        .vault()
        .review_import(&path)
        .map_err(|error| failure(error, id))?;
    let review = ImportReview {
        version: IMPORT_REVIEW_VERSION,
        id,
        source: path,
        bytes: identity.bytes,
        sha256: identity.sha256,
    };
    session.import_review = Some(review.clone());
    Ok(Outcome::Reply(Reply::VaultImportReview(review)))
}
pub(super) fn apply(
    shared: &Shared,
    session: &mut Session,
    path: &Path,
    password: &WireSecret,
    totp: Option<&str>,
    now: Instant,
    review: Option<ImportReview>,
) -> Handled {
    shared
        .throttle
        .check(&(), now)
        .map_err(|l| BrokerError::LockedOut {
            retry_after_secs: l.retry_after.as_secs().max(1),
        })?;
    source(path)?;
    let mut vault = shared.vault();
    let secret = Secret::new(password.expose().to_owned());
    let result = match &review {
        Some(review) => vault.import_reviewed(
            path,
            ImportIdentity {
                bytes: review.bytes,
                sha256: review.sha256,
            },
            &secret,
            totp,
            unix_now(),
        ),
        None => vault
            .import(path, &secret, totp, unix_now())
            .map_err(ReviewedImportError::from),
    };
    match result {
        Ok(ImportCheck::Imported) => {}
        Ok(ImportCheck::TotpRequired) => return Err(BrokerError::TotpRequired),
        Ok(ImportCheck::Totp(outcome)) => return Err(failed(shared, now, totp_error(outcome))),
        Err(ReviewedImportError::SourceChanged) => {
            session.import_review = None;
            return Err(BrokerError::ImportSourceChanged);
        }
        Err(ReviewedImportError::Vault(VaultError::Crypto)) => {
            return Err(failed(shared, now, BrokerError::AuthenticationFailed));
        }
        Err(ReviewedImportError::Vault(error)) => {
            session.import_review = None;
            return Err(match &review {
                Some(review) => failure(error, review.id),
                None => vault_error(error),
            });
        }
    }
    session.import_review = None;
    let generation = vault.generation();
    drop(vault);
    shared.throttle.success(&());
    session.step_up.grant(now, generation);
    match review {
        Some(review) => Ok(Outcome::Reply(Reply::VaultImportApplied(receipt::record(
            &shared.root,
            review,
        )))),
        None => ok(), // Legacy clients retain their original reply contract.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_copy_failure_keeps_cause_and_correlation_on_both_wires() {
        let id = uuid::Uuid::new_v4();
        let error = failure(
            VaultError::ImportLeftInPlace(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            )),
            id,
        );
        let bytes = a206_protocol::encode(&error).unwrap();
        let native: BrokerError = a206_protocol::Decoder::default()
            .feed(&bytes)
            .unwrap()
            .remove(0);
        assert_eq!(native, error);
        let BrokerError::ImportFailure(report) = &native else {
            panic!("missing import failure")
        };
        assert!(report.message.contains("imported vault is in place"));
        assert_eq!(report.kind, a206_core::ErrorKind::Storage);
        assert_eq!(report.correlation, Some(id));
        assert_eq!(
            report.source.as_ref().unwrap().kind,
            a206_core::ErrorKind::PermissionDenied
        );
        let a206_vault_channel::Reply::Error(encrypted) =
            crate::channel::reply(Err(native.clone()))
        else {
            panic!("missing encrypted failure")
        };
        assert_eq!(&encrypted.report, report);
    }
}
