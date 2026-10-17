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
#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;
    pub type Target = std::path::PathBuf;
    #[derive(Clone)]
    pub struct Session(a206_runtime::vault::Session);
    fn failure(error: a206_runtime::vault::VaultError) -> Failure {
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
            let result = self
                .0
                .call(a206_broker::channel::request(request))
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
        pub async fn pair_gateway(&self) -> Result<(), Failure> {
            self.0.pair_gateway().await.map_err(failure)
        }
        pub async fn pair_if_running(&self) -> Result<bool, Failure> {
            self.0.pair_if_running().await.map_err(failure)
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
        match crate::web_request("/api/vault", request)
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
                let RelayReply::Opened { session, response } =
                    relay(RelayRequest::Open { first }).await?
                else {
                    return Err(unexpected());
                };
                active = Some(Active {
                    id: session,
                    channel: handshake.finish(&response).map_err(transport)?,
                });
            }
            let mut active = active.ok_or_else(unexpected)?;
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
