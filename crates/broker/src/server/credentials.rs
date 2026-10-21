//! User-only metadata and fixed-policy key creation. No value is read for a page.
use super::*;
use a206_vault_channel::{
    CREDENTIALS_SCHEMA_VERSION, CredentialPage, MAX_API_KEY_BYTES, MAX_CREDENTIAL_PAGE,
    MAX_CREDENTIAL_PAGE_BYTES,
};

fn version(session: &Session, schema: u16) -> Result<(), BrokerError> {
    if session.version < CREDENTIALS_VERSION || schema != CREDENTIALS_SCHEMA_VERSION {
        return Err(BrokerError::Invalid(InvalidRequest::UnsupportedVersion));
    }
    Ok(())
}

/// Keep an updated entry readable by the bounded metadata panel. Reserve the
/// cursor/count encoding too, so it fits even when it is not the last entry.
pub(super) fn validate_updated_metadata(
    name: &str,
    meta: CredentialMeta,
) -> Result<(), BrokerError> {
    let page = CredentialPage {
        version: CREDENTIALS_SCHEMA_VERSION,
        entries: vec![(name.to_owned(), meta)],
        next: Some(name.to_owned()),
        total: usize::MAX,
        legacy_count: usize::MAX,
    };
    match a206_protocol::encode(&page) {
        Ok(bytes) if bytes.len() <= MAX_CREDENTIAL_PAGE_BYTES => Ok(()),
        Ok(_) | Err(a206_protocol::Error::TooLarge) => {
            Err(BrokerError::Vault(VaultFailure::MetadataTooLarge))
        }
        Err(error) => Err(BrokerError::Channel(
            a206_core::ErrorReport::capture(&error, |_| Some(a206_core::ErrorKind::Protocol))
                .with_correlation(uuid::Uuid::new_v4()),
        )),
    }
}

pub(super) fn page(
    shared: &Shared,
    session: &Session,
    schema: u16,
    after: Option<String>,
    limit: u16,
) -> Handled {
    version(session, schema)?;
    if limit == 0
        || limit > MAX_CREDENTIAL_PAGE
        || after
            .as_ref()
            .is_some_and(|name| name.len() > MAX_CREDENTIAL_PAGE_BYTES)
    {
        return Err(BrokerError::Invalid(InvalidRequest::CredentialPage));
    }
    let vault = shared.vault();
    let mut entries = vault.credentials().map_err(vault_error)?;
    let legacy_count = vault.names().map_err(vault_error)?.len();
    drop(vault);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let total = entries.len();
    entries.retain(|(name, _)| after.as_ref().is_none_or(|after| name > after));
    let more = entries.len() > usize::from(limit);
    entries.truncate(usize::from(limit));
    let mut page = CredentialPage {
        version: CREDENTIALS_SCHEMA_VERSION,
        next: if more {
            entries.last().map(|(name, _)| name.clone())
        } else {
            None
        },
        entries,
        total,
        legacy_count,
    };
    loop {
        // This shared page is also sealed in Noise; its payload must fit that
        // tighter bound. Shrink only by whole entries, never a policy or scope.
        match a206_protocol::encode(&page) {
            Ok(bytes) if bytes.len() <= MAX_CREDENTIAL_PAGE_BYTES => {
                return Ok(Outcome::Reply(Reply::CredentialPage(page)));
            }
            Ok(_) | Err(a206_protocol::Error::TooLarge) => {
                if page.entries.len() <= 1 {
                    return Err(BrokerError::Vault(VaultFailure::MetadataTooLarge));
                }
                page.entries.pop();
                page.next = page.entries.last().map(|(name, _)| name.clone());
            }
            Err(a206_protocol::Error::Encode(error)) => {
                return Err(crate::channel::channel_error(
                    a206_vault_channel::Error::Encode(error),
                ));
            }
            Err(error) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "credential metadata encoding failed"
                );
                return Err(BrokerError::Channel(
                    a206_core::ErrorReport::capture(&error, |_| {
                        Some(a206_core::ErrorKind::Protocol)
                    })
                    .with_correlation(uuid::Uuid::new_v4()),
                ));
            }
        }
    }
}

pub(super) fn add(shared: &Shared, session: &Session, input: ApiKeyInput, now: Instant) -> Handled {
    version(session, input.version)?;
    if input.value.expose().is_empty() || input.value.expose().len() > MAX_API_KEY_BYTES {
        return Err(BrokerError::Invalid(InvalidRequest::ApiKeyValue));
    }
    if input.label.trim().is_empty()
        || input.label.len() > a206_vault_channel::MAX_CREDENTIAL_LABEL_BYTES
        || input.label.chars().any(char::is_control)
    {
        return Err(BrokerError::Invalid(InvalidRequest::CredentialLabel));
    }
    // A path/query must not be mistaken for a narrower credential grant.
    // Existing normalization and scheme/host checks remain authoritative.
    for destination in &input.destinations {
        if destination.len() > a206_vault_channel::MAX_CREDENTIAL_DESTINATION_BYTES {
            return Err(BrokerError::Invalid(InvalidRequest::Destination));
        }
        let url = url::Url::parse(destination)
            .map_err(|_| BrokerError::Invalid(InvalidRequest::Destination))?;
        // Generic credential bindings also support SSH/IRC. API-key setup
        // stays restricted to HTTPS and loopback HTTP regardless of those
        // other credential families.
        allowed_scheme(&url).map_err(|_| BrokerError::Invalid(InvalidRequest::Destination))?;
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(BrokerError::Invalid(InvalidRequest::Destination));
        }
    }
    let meta = CredentialMeta {
        label: input.label,
        kind: a206_vault::CredentialKind::ApiKey,
        policy: a206_vault::AccessPolicy::WithApproval,
        description: None,
        disabled: false,
        destinations: input.destinations,
    };
    put(
        shared,
        session,
        input.name,
        meta,
        WireValue::Single(input.value),
        false,
        now,
    )
}

pub(super) fn put(
    shared: &Shared,
    session: &Session,
    name: String,
    mut meta: CredentialMeta,
    value: WireValue,
    replace: bool,
    now: Instant,
) -> Handled {
    check_name(&name)?;
    meta.destinations = origins(meta.destinations)?;
    let mut vault = shared.vault();
    require_step_up(session, vault.generation(), now)?;
    // Check and write under one guard: neither concurrent adds nor a retry
    // after a lost acknowledgement can replace an existing credential.
    if !replace {
        match vault.credential(&name) {
            Ok(_) => return Err(BrokerError::AlreadyExists),
            Err(VaultError::NotFound) => {}
            Err(error) => return Err(vault_error(error)),
        }
    }
    vault
        .put_credential(&name, &meta, (&value).into())
        .map_err(vault_error)?;
    ok()
}
