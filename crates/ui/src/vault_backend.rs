//! Renderer adapters for the same broker-owned setup state machine.
use a206_vault_channel::{Code, Failure, Reply, Request, Status};
pub(crate) fn unexpected() -> Failure {
    Failure {
        code: Code::Other,
        report: a206_core::ErrorReport::new(
            a206_core::ErrorKind::Protocol,
            "unexpected vault setup reply",
        ),
    }
}
fn checked(reply: Reply) -> Result<Reply, Failure> {
    match reply {
        Reply::Error(error) => Err(error),
        reply => Ok(reply),
    }
}
// Keep generic setup compatible with v10, while declining migration before
// serializing a reviewed request or its secrets for an unsupported connection.
#[cfg(any(target_arch = "wasm32", test))]
fn reviewed_supported(version: u16, request: &Request) -> Result<(), Failure> {
    if version < a206_vault_channel::TOTP_REMOVAL_BROKER_VERSION
        && matches!(request, Request::RemoveTotp { .. })
    {
        return Err(Failure {
            code: Code::Other,
            report: a206_core::ErrorReport::new(
                a206_core::ErrorKind::Unavailable,
                "This broker cannot remove an authenticator through this panel. Stop the old vault broker and retry with the updated application.",
            ),
        });
    }
    if version < a206_vault_channel::CREDENTIAL_POLICY_BROKER_VERSION
        && matches!(request, Request::SetCredentialPolicy { .. })
    {
        return Err(Failure {
            code: Code::Other,
            report: a206_core::ErrorReport::new(
                a206_core::ErrorKind::Unavailable,
                "This broker cannot edit credential policies through this panel. Stop the old vault broker and retry with the updated application.",
            ),
        });
    }
    if version < a206_vault_channel::CREDENTIAL_DESTINATIONS_BROKER_VERSION
        && matches!(request, Request::SetCredentialDestinations { .. })
    {
        return Err(Failure {
            code: Code::Other,
            report: a206_core::ErrorReport::new(
                a206_core::ErrorKind::Unavailable,
                "This broker cannot edit credential destinations through this panel. Stop the old vault broker and retry with the updated application.",
            ),
        });
    }
    if version < a206_vault_channel::CREDENTIAL_STATE_BROKER_VERSION
        && matches!(request, Request::SetCredentialDisabled { .. })
    {
        return Err(Failure {
            code: Code::Other,
            report: a206_core::ErrorReport::new(
                a206_core::ErrorKind::Unavailable,
                "This broker cannot enable or disable credentials through this panel. Stop the old vault broker and retry with the updated application.",
            ),
        });
    }
    if version < a206_vault_channel::AGENT_ACCESS_BROKER_VERSION
        && matches!(request, Request::SetAgentAccess { .. })
    {
        return Err(Failure {
            code: Code::Other,
            report: a206_core::ErrorReport::new(
                a206_core::ErrorKind::Unavailable,
                "This broker cannot change agent access through this panel. Stop the old vault broker and retry with the updated application.",
            ),
        });
    }
    if version < a206_vault_channel::CREDENTIALS_BROKER_VERSION
        && matches!(
            request,
            Request::CredentialPage { .. } | Request::AddApiKey { .. }
        )
    {
        return Err(Failure {
            code: Code::Other,
            report: a206_core::ErrorReport::new(
                a206_core::ErrorKind::Unavailable,
                "This broker cannot manage credentials through this panel. Stop the old vault broker and retry with the updated application.",
            ),
        });
    }
    if version < a206_vault_channel::REVIEWED_IMPORT_BROKER_VERSION
        && matches!(
            request,
            Request::ReviewVaultImport { .. }
                | Request::ApplyVaultImport { .. }
                | Request::CancelVaultImport { .. }
        )
    {
        return Err(Failure {
            code: Code::Other,
            report: a206_core::ErrorReport::new(
                a206_core::ErrorKind::Unavailable,
                "This broker cannot review vault imports. Stop the old vault broker and retry with the updated application.",
            ),
        });
    }
    Ok(())
}
#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;
    pub type Target = std::path::PathBuf;
    #[derive(Clone)]
    pub struct Session(a206_runtime::vault::Session);
    fn failure(error: a206_runtime::vault::VaultError) -> Failure {
        if let a206_runtime::vault::VaultError::Broker(a206_broker::ClientError::Broker(
            a206_broker::BrokerError::ImportFailure(report)
            | a206_broker::BrokerError::Channel(report),
        )) = &error
        {
            return Failure {
                code: Code::Other,
                report: report.clone(),
            };
        }
        let code = match &error {
            a206_runtime::vault::VaultError::Broker(a206_broker::ClientError::Broker(error)) => {
                a206_broker::channel::code(error)
            }
            _ => Code::Other,
        };
        Failure {
            code,
            report: a206_runtime::error::report(&error),
        }
    }
    impl Session {
        pub fn open(root: Target) -> Self {
            Self(a206_runtime::vault::Session::open(root))
        }
        pub async fn call(&self, request: Request) -> Result<Reply, Failure> {
            if let Err(error) = a206_broker::channel::validate(&request) {
                return checked(a206_broker::channel::reply(Err(error)));
            }
            // The native opcode already exists. This editor specifically needs
            // v21's atomic metadata-size check, not a new native request variant.
            let minimum = if matches!(request, Request::SetCredentialPolicy { .. }) {
                a206_vault_channel::CREDENTIAL_POLICY_BROKER_VERSION
            } else {
                a206_broker::MIN_BROKER_VERSION
            };
            let result = self
                .0
                .call_with_minimum(a206_broker::channel::request(request), minimum)
                .await
                .map_err(failure)?;
            checked(a206_broker::channel::reply(Ok(result)))
        }
        pub async fn status(&self) -> Result<Status, Failure> {
            match self.call(Request::Status).await? {
                Reply::Status(s) => Ok(s),
                _ => Err(unexpected()),
            }
        }
        /// Observe a running broker without undoing a deliberate stop.
        pub async fn status_if_running(&self) -> Result<Option<Status>, Failure> {
            self.0
                .status_if_running()
                .await
                .map(|value| value.map(a206_broker::channel::status))
                .map_err(failure)
        }
        pub async fn pair_gateway(&self) -> Result<(), Failure> {
            self.0.pair_gateway().await.map_err(failure)
        }
        pub async fn pair_if_running(&self) -> Result<bool, Failure> {
            self.0.pair_if_running().await.map_err(failure)
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn native_structured_failures_keep_the_logged_reference_at_the_display_root() {
            let report = a206_core::ErrorReport::new(a206_core::ErrorKind::Storage, "partial copy")
                .with_correlation(uuid::Uuid::new_v4())
                .caused_by(a206_core::ErrorReport::new(
                    a206_core::ErrorKind::PermissionDenied,
                    "denied",
                ));
            for error in [
                a206_broker::BrokerError::ImportFailure(report.clone()),
                a206_broker::BrokerError::Channel(report.clone()),
            ] {
                let error = a206_runtime::vault::VaultError::Broker(
                    a206_broker::ClientError::Broker(error),
                );
                assert_eq!(failure(error).report, report);
            }
        }
    }
}
#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use a206_vault_channel::{Channel, Handshake, RelayReply, RelayRequest};
    use std::{cell::RefCell, rc::Rc};
    pub type Target = Vec<u8>;
    struct Active {
        id: String,
        channel: Channel,
        broker_version: u16,
    }
    #[derive(Clone)]
    pub struct Session {
        key: Rc<Vec<u8>>,
        active: Rc<RefCell<Option<Active>>>,
    }
    fn transport(error: impl std::error::Error + 'static) -> Failure {
        Failure {
            code: Code::Other,
            report: crate::captured(a206_core::ErrorKind::Protocol)(&error),
        }
    }
    async fn relay(request: RelayRequest) -> Result<RelayReply, Failure> {
        match crate::web_request("/api/vault/channel", request)
            .await
            .map_err(|report| Failure {
                code: Code::Other,
                report,
            })? {
            RelayReply::Error(report) => Err(Failure {
                code: Code::Other,
                report,
            }),
            reply => Ok(reply),
        }
    }
    impl Session {
        pub fn open(key: Target) -> Self {
            Self {
                key: Rc::new(key),
                active: Rc::default(),
            }
        }
        pub async fn call(&self, request: Request) -> Result<Reply, Failure> {
            // The panel serializes operations. Taking the state also invalidates
            // the channel if this future is cancelled after sending a mutation.
            let mut active = self.active.borrow_mut().take();
            if active.is_none() {
                let (handshake, first) = Handshake::start(&self.key).map_err(transport)?;
                let RelayReply::Opened {
                    session,
                    response,
                    broker_version,
                } = relay(RelayRequest::Open { first }).await?
                else {
                    return Err(unexpected());
                };
                active = Some(Active {
                    id: session,
                    channel: handshake.finish(&response).map_err(transport)?,
                    broker_version,
                });
            }
            let mut active = active.ok_or_else(unexpected)?;
            if let Err(error) = reviewed_supported(active.broker_version, &request) {
                *self.active.borrow_mut() = Some(active);
                return Err(error);
            }
            let bytes = active.channel.seal(&request).map_err(transport)?;
            let RelayReply::Message(bytes) = relay(RelayRequest::Exchange {
                session: active.id.clone(),
                bytes,
            })
            .await?
            else {
                return Err(unexpected());
            };
            let reply = active.channel.open::<Reply>(&bytes).map_err(transport)?;
            *self.active.borrow_mut() = Some(active);
            checked(reply)
        }
        pub async fn status(&self) -> Result<Status, Failure> {
            match self.call(Request::Status).await? {
                Reply::Status(s) => Ok(s),
                _ => Err(unexpected()),
            }
        }
        pub async fn pair_gateway(&self) -> Result<(), Failure> {
            let Reply::GatewayPairing { token } = self.call(Request::PairGateway).await? else {
                return Err(unexpected());
            };
            match crate::request(a206_core::Command::PairBroker {
                pairing: a206_core::BrokerPairing::new(token.expose().to_owned()),
            })
            .await
            .map_err(|report| Failure {
                code: Code::Other,
                report,
            })? {
                a206_core::Reply::Ok => Ok(()),
                a206_core::Reply::Error(report) => Err(Failure {
                    code: Code::Other,
                    report,
                }),
                _ => Err(unexpected()),
            }
        }
        pub async fn pair_if_running(&self) -> Result<bool, Failure> {
            // Inspect through the existing controller; status never starts a deliberately stopped gateway.
            let report: a206_core::GatewayReport =
                crate::web_request("/api/gateway", a206_core::GatewayAction::Status)
                    .await
                    .map_err(|report| Failure {
                        code: Code::Other,
                        report,
                    })?;
            if !matches!(report.phase, a206_core::GatewayPhase::Running) {
                return Ok(false);
            }
            self.pair_gateway().await.map(|()| true)
        }
    }
    impl Drop for Active {
        fn drop(&mut self) {
            let id = self.id.clone();
            // Deliberately detached: closing the UI drops its broker socket and
            // pending enrollment; failure is harmless because the relay expires.
            dioxus::prelude::spawn(async move {
                if let Err(error) = relay(RelayRequest::Close { session: id }).await {
                    tracing::warn!(
                        error = &error as &dyn std::error::Error,
                        "vault channel cleanup failed"
                    );
                }
            });
        }
    }
}
pub(crate) use imp::{Session, Target};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authenticator_removal_refuses_old_web_peers_before_sealing() {
        for version in 0..=23 {
            let request = Request::RemoveTotp {
                password: a206_vault_channel::Secret::new(uuid::Uuid::new_v4().to_string()),
                code: a206_vault_channel::Secret::new(uuid::Uuid::new_v4().to_string()),
            };
            assert_eq!(reviewed_supported(version, &request).is_ok(), version >= 22);
            assert!(reviewed_supported(version, &Request::CancelTotp).is_ok());
        }
    }
    #[test]
    fn policy_changes_require_v21_without_raising_existing_operation_minimums() {
        for version in 0..=22 {
            assert_eq!(
                reviewed_supported(
                    version,
                    &Request::SetCredentialPolicy {
                        name: uuid::Uuid::new_v4().to_string(),
                        policy: a206_vault_channel::AccessPolicy::WithApproval
                    }
                )
                .is_ok(),
                version >= 21
            );
            assert_eq!(
                reviewed_supported(
                    version,
                    &Request::SetCredentialDestinations {
                        name: uuid::Uuid::new_v4().to_string(),
                        destinations: vec![]
                    }
                )
                .is_ok(),
                version >= 20
            );
        }
    }
    #[test]
    fn destination_edits_refuse_old_web_peers_without_raising_older_minimums() {
        for version in 0..=21 {
            assert_eq!(
                reviewed_supported(
                    version,
                    &Request::SetCredentialDestinations {
                        name: uuid::Uuid::new_v4().to_string(),
                        destinations: vec![],
                    }
                )
                .is_ok(),
                version >= 20
            );
            assert_eq!(
                reviewed_supported(
                    version,
                    &Request::SetCredentialDisabled {
                        name: uuid::Uuid::new_v4().to_string(),
                        disabled: true,
                    }
                )
                .is_ok(),
                version >= 19
            );
            assert_eq!(
                reviewed_supported(version, &Request::SetAgentAccess { enabled: false }).is_ok(),
                version >= 18
            );
        }
    }
    #[test]
    fn credential_state_changes_require_v19_but_keep_existing_features() {
        for version in 0..=20 {
            for disabled in [true, false] {
                assert_eq!(
                    reviewed_supported(
                        version,
                        &Request::SetCredentialDisabled {
                            name: uuid::Uuid::new_v4().to_string(),
                            disabled,
                        }
                    )
                    .is_ok(),
                    version >= 19
                );
            }
            assert_eq!(
                reviewed_supported(version, &Request::SetAgentAccess { enabled: false }).is_ok(),
                version >= 18
            );
        }
    }
    #[test]
    fn agent_access_changes_refuse_old_brokers_before_sending() {
        for version in 0..=19 {
            for enabled in [true, false] {
                assert_eq!(
                    reviewed_supported(version, &Request::SetAgentAccess { enabled }).is_ok(),
                    version >= 18
                );
            }
            assert!(reviewed_supported(version, &Request::Status).is_ok());
        }
    }
    #[test]
    fn credentials_require_v17_before_any_secret_is_serialized() {
        for version in 0..=18 {
            for request in [
                Request::CredentialPage {
                    version: 1,
                    after: None,
                    limit: 5,
                },
                Request::AddApiKey {
                    version: 1,
                    name: uuid::Uuid::new_v4().to_string(),
                    label: "Key".into(),
                    destinations: vec![],
                    value: a206_vault_channel::Secret::new(uuid::Uuid::new_v4().to_string()),
                },
            ] {
                assert_eq!(reviewed_supported(version, &request).is_ok(), version >= 17);
            }
            assert!(reviewed_supported(version, &Request::Status).is_ok());
        }
    }
    #[test]
    fn old_or_unknown_relays_keep_setup_but_never_send_reviewed_imports() {
        for version in [0, 8, 9, 10] {
            assert!(reviewed_supported(version, &Request::Status).is_ok());
            assert!(reviewed_supported(version, &Request::Lock).is_ok());
            let error = reviewed_supported(
                version,
                &Request::ReviewVaultImport {
                    version: a206_vault_channel::IMPORT_REVIEW_VERSION,
                    source: "/source".into(),
                },
            )
            .unwrap_err();
            assert!(error.report.message.contains("Stop the old vault broker"));
            assert!(
                reviewed_supported(
                    version,
                    &Request::ApplyVaultImport {
                        version: a206_vault_channel::IMPORT_REVIEW_VERSION,
                        review: uuid::Uuid::new_v4(),
                        password: a206_vault_channel::Secret::new(uuid::Uuid::new_v4().to_string()),
                        totp: None,
                    }
                )
                .is_err()
            );
            assert!(
                reviewed_supported(
                    version,
                    &Request::CancelVaultImport {
                        version: a206_vault_channel::IMPORT_REVIEW_VERSION
                    }
                )
                .is_err()
            );
        }
        assert!(
            reviewed_supported(
                11,
                &Request::ReviewVaultImport {
                    version: a206_vault_channel::IMPORT_REVIEW_VERSION,
                    source: "/source".into()
                }
            )
            .is_ok()
        );
    }
}
