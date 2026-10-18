//! Connection-owned reviews. Source identities never come from apply requests.
use super::*;
use a206_vault::{ImportIdentity, ReviewedImportError};
use a206_vault_channel::{IMPORT_REVIEW_VERSION, ImportReview};
use std::path::PathBuf;
mod receipt;

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
    let identity = shared.vault().review_import(&path).map_err(vault_error)?;
    let review = ImportReview {
        version: IMPORT_REVIEW_VERSION,
        id: uuid::Uuid::new_v4(),
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
            return Err(vault_error(error));
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
