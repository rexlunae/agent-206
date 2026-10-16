//! Versioned MessagePack messages between trusted clients, the gateway and the
//! vault broker. Secret-bearing fields use [`WireSecret`], which zeroizes and
//! never prints its value.
use a206_vault::{AccessPolicy, CredentialMeta, CredentialValue, Secret};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

pub const BROKER_VERSION: u16 = 1;
/// Largest provider request or response body the broker carries. The rest of
/// the 4 MiB frame is reserved for the envelope, so a body at this limit still
/// fits in one frame.
pub const MAX_PROVIDER_BODY: usize = a206_protocol::MAX_FRAME - 64 * 1024;

pub struct WireSecret(Zeroizing<String>);
impl WireSecret {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    fn secret(&self) -> Secret {
        Secret::new(self.0.to_string())
    }
    fn from_secret(secret: &Secret) -> Self {
        Self::new(secret.expose(str::to_owned))
    }
}
impl std::fmt::Debug for WireSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl Serialize for WireSecret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for WireSecret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Self::new)
    }
}

/// Who a connection acts for. The gateway role can only ask the broker to
/// perform destination-bound requests; it never receives a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Gateway,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum WireValue {
    Single(WireSecret),
    UserPass {
        username: WireSecret,
        password: WireSecret,
    },
    SshKeyPair {
        private_key: WireSecret,
        public_key: WireSecret,
    },
    FormFields(BTreeMap<String, WireSecret>),
    PaymentCard {
        cardholder: WireSecret,
        number: WireSecret,
        expiry: WireSecret,
        cvv: WireSecret,
        extra: BTreeMap<String, WireSecret>,
    },
}

fn to_secrets(map: &BTreeMap<String, WireSecret>) -> BTreeMap<String, Secret> {
    map.iter().map(|(k, v)| (k.clone(), v.secret())).collect()
}
fn to_wire(map: &BTreeMap<String, Secret>) -> BTreeMap<String, WireSecret> {
    map.iter()
        .map(|(k, v)| (k.clone(), WireSecret::from_secret(v)))
        .collect()
}

impl From<&WireValue> for CredentialValue {
    fn from(value: &WireValue) -> Self {
        match value {
            WireValue::Single(v) => Self::Single(v.secret()),
            WireValue::UserPass { username, password } => Self::UserPass {
                username: username.secret(),
                password: password.secret(),
            },
            WireValue::SshKeyPair {
                private_key,
                public_key,
            } => Self::SshKeyPair {
                private_key: private_key.secret(),
                public_key: public_key.secret(),
            },
            WireValue::FormFields(f) => Self::FormFields(to_secrets(f)),
            WireValue::PaymentCard {
                cardholder,
                number,
                expiry,
                cvv,
                extra,
            } => Self::PaymentCard {
                cardholder: cardholder.secret(),
                number: number.secret(),
                expiry: expiry.secret(),
                cvv: cvv.secret(),
                extra: to_secrets(extra),
            },
        }
    }
}
impl From<&CredentialValue> for WireValue {
    fn from(value: &CredentialValue) -> Self {
        let w = WireSecret::from_secret;
        match value {
            CredentialValue::Single(v) => Self::Single(w(v)),
            CredentialValue::UserPass { username, password } => Self::UserPass {
                username: w(username),
                password: w(password),
            },
            CredentialValue::SshKeyPair {
                private_key,
                public_key,
            } => Self::SshKeyPair {
                private_key: w(private_key),
                public_key: w(public_key),
            },
            CredentialValue::FormFields(f) => Self::FormFields(to_wire(f)),
            CredentialValue::PaymentCard {
                cardholder,
                number,
                expiry,
                cvv,
                extra,
            } => Self::PaymentCard {
                cardholder: w(cardholder),
                number: w(number),
                expiry: w(expiry),
                cvv: w(cvv),
                extra: to_wire(extra),
            },
        }
    }
}

/// How the broker attaches a token. Header names are fixed here, never
/// supplied by the gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthScheme {
    /// `Authorization: Bearer …`
    Bearer,
    /// `x-api-key: …` (Anthropic)
    ApiKeyHeader,
    /// `x-goog-api-key: …` (Gemini)
    GoogleApiKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    Get,
    Post,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProviderCall {
    pub credential: String,
    pub method: HttpMethod,
    pub url: String,
    pub auth: AuthScheme,
    /// The only extra header the broker forwards (Anthropic's API version).
    pub anthropic_version: Option<String>,
    /// JSON request body, sent with `content-type: application/json`.
    #[serde(with = "serde_bytes")]
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// Must be the first message on every connection.
    Hello {
        role: Role,
        version: u16,
    },
    Status,
    /// `create` permits creating a vault when none exists.
    Unlock {
        password: WireSecret,
        totp: Option<String>,
        create: bool,
    },
    Lock,
    Shutdown,
    /// Re-verify the user for this connection: the authenticator code when
    /// enrolled, otherwise the vault password.
    StepUp {
        password: Option<WireSecret>,
        totp: Option<String>,
    },
    BeginTotp {
        account: String,
    },
    ConfirmTotp {
        code: String,
    },
    /// Needs both factors: the vault password and a current code.
    RemoveTotp {
        password: WireSecret,
        code: String,
    },
    Credentials,
    /// Convert a raw entry from before typed credentials into a typed one
    /// named `name`, which may differ from the legacy key.
    AdoptLegacy {
        legacy: String,
        name: String,
        meta: CredentialMeta,
    },
    PutCredential {
        name: String,
        meta: CredentialMeta,
        value: WireValue,
        /// Overwrite an existing credential of this name; otherwise refuse.
        replace: bool,
    },
    UpdateCredential {
        name: String,
        policy: Option<AccessPolicy>,
        disabled: Option<bool>,
        destinations: Option<Vec<String>>,
    },
    DeleteCredential {
        name: String,
    },
    SetAgentAccess {
        enabled: bool,
    },
    Reveal {
        name: String,
    },
    Provider(ProviderCall),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerStatus {
    pub exists: bool,
    pub unlocked: bool,
    /// Unknown while locked: enrollment lives inside the encrypted vault.
    pub totp_enrolled: Option<bool>,
    pub agent_access: Option<bool>,
    pub step_up_active: bool,
}

/// Structured broker failures. Clients render these with `Display`; nothing
/// is flattened to a string before it reaches the client or tracing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum BrokerError {
    #[error("the vault is locked")]
    Locked,
    #[error("no vault exists yet; create one first")]
    NoVault,
    #[error("too many failed attempts; retry in {retry_after_secs} seconds")]
    LockedOut { retry_after_secs: u64 },
    #[error("authentication failed")]
    AuthenticationFailed,
    #[error("an authenticator code is required")]
    TotpRequired,
    #[error("authenticator code rejected: clocks differ by about {seconds} seconds")]
    ClockDrift { seconds: u64 },
    #[error("authenticator code was already used")]
    TotpReplayed,
    #[error("confirm it is you first (step-up authentication)")]
    StepUpRequired,
    #[error("this connection's role cannot make that request")]
    Forbidden,
    #[error("no two-factor enrollment is pending on this connection")]
    NoPendingEnrollment,
    #[error("credential use denied")]
    Denied(#[source] a206_vault::Denial),
    #[error("credential not found")]
    NotFound,
    #[error("a credential with that name already exists")]
    AlreadyExists,
    #[error("invalid request")]
    Invalid(#[source] InvalidRequest),
    #[error("vault storage failed")]
    Vault(#[source] VaultFailure),
    #[error("provider request failed")]
    Upstream(#[source] UpstreamFailure),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum InvalidRequest {
    #[error("unsupported broker protocol version")]
    UnsupportedVersion,
    #[error("the connection already identified itself")]
    AlreadyGreeted,
    #[error("credential name is empty, reserved, too long or has control characters")]
    Name,
    #[error("credential value does not match its kind")]
    KindMismatch,
    #[error("destinations must be HTTPS (or loopback HTTP) URLs without embedded credentials")]
    Destination,
    #[error("at most 16 destinations may be bound")]
    TooManyDestinations,
    #[error("provider URLs must be HTTPS (or loopback HTTP) without credentials or fragments")]
    ProviderUrl,
    #[error("the provider request body is too large for the vault broker")]
    ProviderBody,
    #[error("the vault password is required")]
    PasswordRequired,
    #[error("two-factor authentication is already enrolled")]
    TotpEnrolled,
    #[error("two-factor authentication is not enrolled")]
    TotpNotEnrolled,
    #[error("the two-factor enrollment expired; start again")]
    EnrollmentExpired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum VaultFailure {
    #[error("stored vault data is corrupted")]
    Corrupted,
    #[error("vault encryption failed")]
    Crypto,
    #[error("the vault file could not be written; it has been locked")]
    Storage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum UpstreamFailure {
    #[error("the provider did not respond in time")]
    Timeout,
    #[error("could not connect to the provider")]
    Connect,
    #[error("the provider connection failed during transfer")]
    Transfer,
    #[error("the provider response is too large for the vault broker")]
    TooLarge,
    #[error("the stored credential is not a valid header value")]
    InvalidHeader,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    Status(BrokerStatus),
    TotpEnrollment {
        otpauth_url: WireSecret,
        manual_key: WireSecret,
        expires_at: u64,
    },
    /// Typed credential metadata, plus names of raw entries written before
    /// typed credentials existed (adoptable with `AdoptLegacy`). No values.
    Credentials {
        typed: Vec<(String, CredentialMeta)>,
        legacy: Vec<String>,
    },
    Value(WireValue),
    Provider {
        status: u16,
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    Error(BrokerError),
}
