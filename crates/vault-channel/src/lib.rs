//! End-to-end, broker-authenticated vault setup. The HTTP/SSH bridge carries
//! only Noise messages; passwords and enrollment seeds terminate in the UI and
//! broker. The broker public key must be obtained through a trusted channel.
#![forbid(unsafe_code)]
use a206_core::ErrorReport;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const PATTERN: &str = "Noise_NK_25519_ChaChaPoly_SHA256";
const PROLOGUE: &[u8] = b"Agent206 vault setup v1";
pub const MAX_MESSAGE: usize = 65535;
pub const IMPORT_REVIEW_VERSION: u16 = 1;

/// Public identity of a source held privately by this broker connection.
/// This describes file bytes, not decrypted contents or format validity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReview {
    #[serde(default)]
    pub version: u16,
    pub id: a206_core::Id,
    pub source: std::path::PathBuf,
    pub bytes: u64,
    pub sha256: [u8; 32],
}
/// Confirmed copy, with receipt failure kept as an explicit partial result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportApplied {
    pub version: u16,
    pub review: ImportReview,
    pub receipt: Option<std::path::PathBuf>,
    pub receipt_error: Option<ErrorReport>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("vault channel authentication failed; reconnect and verify the broker key")]
    Noise(#[from] snow::Error),
    #[error("vault channel message exceeds its limit")]
    TooLarge,
    #[error("could not encode the vault channel message")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("could not decode the vault channel message")]
    Decode(#[from] rmp_serde::decode::Error),
}
fn builder() -> Result<snow::Builder<'static>, Error> {
    Ok(snow::Builder::new(PATTERN.parse()?).prologue(PROLOGUE)?)
}
/// Per-broker-lifetime key. Private material never leaves the broker.
pub struct Identity {
    private: Zeroizing<Vec<u8>>,
    pub public: Vec<u8>,
}
impl Identity {
    pub fn generate() -> Result<Self, Error> {
        let keys = builder()?.generate_keypair()?;
        Ok(Self {
            private: Zeroizing::new(keys.private),
            public: keys.public,
        })
    }
    pub fn accept(&self, first: &[u8]) -> Result<(Channel, Vec<u8>), Error> {
        bounded(first)?;
        let mut state = builder()?
            .local_private_key(&self.private)?
            .build_responder()?;
        let mut scratch = Zeroizing::new(vec![0; MAX_MESSAGE]);
        state.read_message(first, &mut scratch)?;
        let n = state.write_message(&[], &mut scratch)?;
        let reply = scratch[..n].to_vec();
        Ok((Channel(state.into_transport_mode()?), reply))
    }
}
pub struct Handshake(snow::HandshakeState);
impl Handshake {
    pub fn start(public: &[u8]) -> Result<(Self, Vec<u8>), Error> {
        let mut state = builder()?.remote_public_key(public)?.build_initiator()?;
        let mut first = vec![0; MAX_MESSAGE];
        let n = state.write_message(&[], &mut first)?;
        first.truncate(n);
        Ok((Self(state), first))
    }
    pub fn finish(mut self, response: &[u8]) -> Result<Channel, Error> {
        bounded(response)?;
        self.0
            .read_message(response, &mut Zeroizing::new(vec![0; MAX_MESSAGE]))?;
        Ok(Channel(self.0.into_transport_mode()?))
    }
}
/// Sequential transport nonces reject replay, reordering and cross-session
/// substitution. A lost HTTP response invalidates the session; mutations are
/// never automatically retried after an ambiguous result.
pub struct Channel(snow::TransportState);
impl Channel {
    pub fn seal<T: Serialize>(&mut self, value: &T) -> Result<Vec<u8>, Error> {
        let plain = Zeroizing::new(rmp_serde::to_vec_named(value)?);
        if plain.len() > MAX_MESSAGE - 16 {
            return Err(Error::TooLarge);
        }
        let mut out = vec![0; plain.len() + 16];
        let n = self.0.write_message(&plain, &mut out)?;
        out.truncate(n);
        Ok(out)
    }
    pub fn open<T: serde::de::DeserializeOwned>(&mut self, bytes: &[u8]) -> Result<T, Error> {
        bounded(bytes)?;
        let mut plain = Zeroizing::new(vec![0; bytes.len()]);
        let n = self.0.read_message(bytes, &mut plain)?;
        Ok(rmp_serde::from_slice(&plain[..n])?)
    }
}
fn bounded(bytes: &[u8]) -> Result<(), Error> {
    if bytes.len() > MAX_MESSAGE {
        Err(Error::TooLarge)
    } else {
        Ok(())
    }
}

/// The deliberately limited setup surface. No reveal, export, provider calls,
/// grants, shutdown, or arbitrary broker requests can travel through it.
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Status,
    ReviewVaultImport {
        version: u16,
        source: std::path::PathBuf,
    },
    ApplyVaultImport {
        version: u16,
        review: a206_core::Id,
        password: Secret,
        totp: Option<Secret>,
    },
    CancelVaultImport {
        version: u16,
    },
    Unlock {
        password: Secret,
        totp: Option<Secret>,
        create: bool,
    },
    Lock,
    StepUp {
        password: Option<Secret>,
        totp: Option<Secret>,
    },
    BeginTotp {
        account: String,
    },
    ConfirmTotp {
        code: Secret,
    },
    CancelTotp,
    PairGateway,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub exists: bool,
    pub unlocked: bool,
    pub totp_enrolled: Option<bool>,
    pub agent_access: Option<bool>,
    pub step_up_active: bool,
    pub gateway_paired: Option<bool>,
}
#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    VaultImportReview(ImportReview),
    VaultImportApplied(ImportApplied),
    Status(Status),
    TotpEnrollment {
        otpauth_url: Secret,
        manual_key: Secret,
        expires_at: u64,
    },
    GatewayPairing {
        token: Secret,
    },
    Error(Failure),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Code {
    Other,
    ImportReviewUnavailable,
    ImportSourceChanged,
    TotpRequired,
    TotpReplayed,
    StepUpRequired,
    AuthenticationFailed,
    ClockDrift,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{report}")]
pub struct Failure {
    pub code: Code,
    #[source]
    pub report: ErrorReport,
}
/// Secret-bearing data never implements a value-revealing Debug.
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(Zeroizing<String>);
impl Secret {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}
/// Authenticated HTTP relay. IDs identify private broker connections, not keys.
#[derive(Debug, Serialize, Deserialize)]
pub enum RelayRequest {
    Key,
    Open {
        #[serde(with = "serde_bytes")]
        first: Vec<u8>,
    },
    Exchange {
        session: String,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    Close {
        session: String,
    },
}
#[derive(Debug, Serialize, Deserialize)]
pub enum RelayReply {
    Key(Vec<u8>),
    Opened {
        session: String,
        #[serde(with = "serde_bytes")]
        response: Vec<u8>,
    },
    Message(#[serde(with = "serde_bytes")] Vec<u8>),
    Closed,
    Error(ErrorReport),
}
#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> (Channel, Channel) {
        let identity = Identity::generate().unwrap();
        let (hello, first) = Handshake::start(&identity.public).unwrap();
        let (broker, response) = identity.accept(&first).unwrap();
        (hello.finish(&response).unwrap(), broker)
    }
    #[test]
    fn authenticated_encryption_roundtrip_and_replay_denial() {
        let (mut ui, mut broker) = pair();
        let secret = "sensitive password";
        let ciphertext = ui
            .seal(&Request::Unlock {
                password: Secret::new(secret.into()),
                totp: None,
                create: true,
            })
            .unwrap();
        assert!(
            !ciphertext
                .windows(secret.len())
                .any(|w| w == secret.as_bytes())
        );
        assert!(matches!(
            broker.open::<Request>(&ciphertext).unwrap(),
            Request::Unlock { create: true, .. }
        ));
        assert!(broker.open::<Request>(&ciphertext).is_err());
        let reply = broker.seal(&Reply::Ok).unwrap();
        assert!(matches!(ui.open::<Reply>(&reply).unwrap(), Reply::Ok));
    }
    #[test]
    fn substituted_key_tampering_reordering_and_other_sessions_fail() {
        let identity = Identity::generate().unwrap();
        let impostor = Identity::generate().unwrap();
        let (_, first) = Handshake::start(&identity.public).unwrap();
        assert!(impostor.accept(&first).is_err());
        let (mut ui, mut broker) = pair();
        let first = ui.seal(&Request::Status).unwrap();
        let mut second = ui.seal(&Request::Lock).unwrap();
        assert!(broker.open::<Request>(&second).is_err());
        second[0] ^= 1;
        assert!(broker.open::<Request>(&second).is_err());
        let (_, mut other) = pair();
        assert!(other.open::<Request>(&first).is_err());
        assert!(matches!(
            broker.open::<Request>(&first).unwrap(),
            Request::Status
        ));
        assert!(ui.seal(&vec![0u8; MAX_MESSAGE]).is_err());
    }
}
