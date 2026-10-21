//! One conversion for the native and encrypted setup surfaces.
use crate::{BrokerError, Reply, Request, WireSecret};
use a206_core::{ErrorKind, ErrorReport};
use a206_vault_channel as wire;
/// At the broker's wire boundary, retain the crypto/protocol cause chain as a
/// structured report; never replace an authentication failure with prose alone.
pub(crate) fn channel_error(error: wire::Error) -> BrokerError {
    fn report(error: &(dyn std::error::Error + 'static), depth: usize) -> ErrorReport {
        let mut value = ErrorReport::new(ErrorKind::Protocol, error.to_string());
        if depth < a206_core::error::MAX_CHAIN {
            value.source = error
                .source()
                .map(|source| Box::new(report(source, depth + 1)));
        }
        value
    }
    tracing::warn!(
        error = &error as &dyn std::error::Error,
        "vault channel refused a message"
    );
    BrokerError::Channel(report(&error, 0).with_correlation(uuid::Uuid::new_v4()))
}
fn secret(value: wire::Secret) -> WireSecret {
    WireSecret::new(value.expose().to_owned())
}
fn text(value: wire::Secret) -> String {
    value.expose().to_owned()
}
/// Validate panel-only constraints before mapping to compatible native opcodes.
/// Native CLI destination normalization is an older, separate contract.
pub fn validate(value: &wire::Request) -> Result<(), BrokerError> {
    if let wire::Request::SetCredentialDestinations { destinations, .. } = value {
        for destination in destinations {
            if destination.len() > wire::MAX_CREDENTIAL_DESTINATION_BYTES
                || destination.chars().any(char::is_whitespace)
            {
                return Err(BrokerError::Invalid(crate::InvalidRequest::Destination));
            }
            let url = url::Url::parse(destination)
                .map_err(|_| BrokerError::Invalid(crate::InvalidRequest::Destination))?;
            if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some()
            {
                return Err(BrokerError::Invalid(crate::InvalidRequest::Destination));
            }
        }
    }
    Ok(())
}
pub fn request(value: wire::Request) -> Request {
    match value {
        wire::Request::CredentialPage {
            version,
            after,
            limit,
        } => Request::CredentialPage {
            version,
            after,
            limit,
        },
        wire::Request::AddApiKey {
            version,
            name,
            label,
            destinations,
            value,
        } => Request::AddApiKey(crate::ApiKeyInput {
            version,
            name,
            label,
            destinations,
            value: secret(value),
        }),
        wire::Request::Status => Request::Status,
        wire::Request::ReviewVaultImport { version, source } => {
            Request::ReviewVaultImport { version, source }
        }
        wire::Request::ApplyVaultImport {
            version,
            review,
            password,
            totp,
        } => Request::ApplyVaultImport {
            version,
            review,
            password: secret(password),
            totp: totp.map(secret),
        },
        wire::Request::CancelVaultImport { version } => Request::CancelVaultImport { version },
        wire::Request::Unlock {
            password,
            totp,
            create,
        } => Request::Unlock {
            password: secret(password),
            totp: totp.map(text),
            create,
        },
        wire::Request::Lock => Request::Lock,
        wire::Request::StepUp { password, totp } => Request::StepUp {
            password: password.map(secret),
            totp: totp.map(text),
        },
        wire::Request::BeginTotp { account } => Request::BeginTotp { account },
        wire::Request::ConfirmTotp { code } => Request::ConfirmTotp { code: text(code) },
        wire::Request::CancelTotp => Request::CancelTotp,
        wire::Request::PairGateway => Request::PairGateway,
        wire::Request::SetAgentAccess { enabled } => Request::SetAgentAccess { enabled },
        wire::Request::SetCredentialDisabled { name, disabled } => Request::UpdateCredential {
            name,
            policy: None,
            disabled: Some(disabled),
            destinations: None,
        },
        wire::Request::SetCredentialDestinations { name, destinations } => {
            Request::UpdateCredential {
                name,
                policy: None,
                disabled: None,
                destinations: Some(destinations),
            }
        }
    }
}
pub fn code(error: &BrokerError) -> wire::Code {
    match error {
        BrokerError::ImportReviewUnavailable => wire::Code::ImportReviewUnavailable,
        BrokerError::ImportSourceChanged => wire::Code::ImportSourceChanged,
        BrokerError::TotpRequired => wire::Code::TotpRequired,
        BrokerError::TotpReplayed => wire::Code::TotpReplayed,
        BrokerError::StepUpRequired => wire::Code::StepUpRequired,
        BrokerError::AuthenticationFailed => wire::Code::AuthenticationFailed,
        BrokerError::ClockDrift { .. } => wire::Code::ClockDrift,
        _ => wire::Code::Other,
    }
}
pub fn reply(result: Result<Reply, BrokerError>) -> wire::Reply {
    match result {
        Ok(Reply::Error(BrokerError::ImportFailure(report) | BrokerError::Channel(report)))
        | Err(BrokerError::ImportFailure(report) | BrokerError::Channel(report)) => {
            wire::Reply::Error(wire::Failure {
                code: wire::Code::Other,
                report,
            })
        }
        Ok(Reply::Ok) => wire::Reply::Ok,
        Ok(Reply::CredentialPage(page)) => wire::Reply::CredentialPage(page),
        Ok(Reply::VaultImportReview(review)) => wire::Reply::VaultImportReview(review),
        Ok(Reply::VaultImportApplied(applied)) => wire::Reply::VaultImportApplied(applied),
        Ok(Reply::Status(s)) => wire::Reply::Status(status(s)),
        Ok(Reply::TotpEnrollment {
            otpauth_url,
            manual_key,
            expires_at,
        }) => wire::Reply::TotpEnrollment {
            otpauth_url: wire::Secret::new(otpauth_url.expose().to_owned()),
            manual_key: wire::Secret::new(manual_key.expose().to_owned()),
            expires_at,
        },
        Ok(Reply::GatewayPairing { token }) => wire::Reply::GatewayPairing {
            token: wire::Secret::new(token.expose().to_owned()),
        },
        Ok(Reply::Error(error)) | Err(error) => {
            let kind = match &error {
                BrokerError::Locked | BrokerError::NoVault | BrokerError::GatewayNotPaired => {
                    ErrorKind::Unavailable
                }
                BrokerError::NoPendingEnrollment
                | BrokerError::AlreadyExists
                | BrokerError::VaultExists => ErrorKind::Conflict,
                BrokerError::ImportReviewUnavailable | BrokerError::ImportSourceChanged => {
                    ErrorKind::Conflict
                }
                BrokerError::Invalid(_) | BrokerError::SshKey(_) => ErrorKind::InvalidRequest,
                BrokerError::Channel(_) => ErrorKind::Protocol,
                BrokerError::NotFound => ErrorKind::NotFound,
                BrokerError::Vault(_) => ErrorKind::Storage,
                BrokerError::Upstream(_) => ErrorKind::Provider,
                _ => ErrorKind::PermissionDenied,
            };
            let mut report = ErrorReport::new(kind, error.to_string());
            // Preserve typed error causes across the encrypted boundary.
            fn causes(
                error: &(dyn std::error::Error + 'static),
                depth: usize,
            ) -> Option<ErrorReport> {
                if depth == a206_core::error::MAX_CHAIN {
                    return None;
                }
                error.source().map(|source| {
                    let mut report = ErrorReport::new(ErrorKind::Other, source.to_string());
                    report.source = causes(source, depth + 1).map(Box::new);
                    report
                })
            }
            report.source = causes(&error, 0).map(Box::new);
            wire::Reply::Error(wire::Failure {
                code: code(&error),
                report: report.with_correlation(uuid::Uuid::new_v4()),
            })
        }
        Ok(_) => wire::Reply::Error(wire::Failure {
            code: wire::Code::Other,
            report: ErrorReport::new(ErrorKind::Protocol, "unexpected vault setup reply"),
        }),
    }
}

/// Public setup facts shared by the native and encrypted surfaces.
pub fn status(s: crate::BrokerStatus) -> wire::Status {
    wire::Status {
        exists: s.exists,
        unlocked: s.unlocked,
        totp_enrolled: s.totp_enrolled,
        agent_access: s.agent_access,
        step_up_active: s.step_up_active,
        gateway_paired: s.gateway_paired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn channel_failures_preserve_root_kind_cause_and_correlation() {
        let report = ErrorReport::new(ErrorKind::Protocol, "metadata encoding failed")
            .caused_by(ErrorReport::new(ErrorKind::Storage, "synthetic cause"))
            .with_correlation(uuid::Uuid::new_v4());
        for result in [
            Err(BrokerError::Channel(report.clone())),
            Ok(Reply::Error(BrokerError::Channel(report.clone()))),
        ] {
            let wire::Reply::Error(failure) = reply(result) else {
                panic!()
            };
            assert_eq!(failure.report, report);
        }
    }
}
