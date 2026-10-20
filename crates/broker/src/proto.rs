//! Versioned MessagePack messages between trusted clients, the gateway and the
//! vault broker. Secret-bearing fields use [`WireSecret`], which zeroizes and
//! never prints its value.
use a206_vault::{AccessPolicy, CredentialMeta, CredentialValue, Secret};
pub use a206_vault_channel::IMPORT_REVIEW_VERSION as IMPORT_REVIEW_SCHEMA_VERSION;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

/// Bump on any change to [`Request`] or [`Reply`]: a broker outlives the CLI
/// that started it, so an upgraded client may reach an older broker.
/// `Request::Hello`, `Request::Shutdown` (accepted before `Hello`) and
/// `Reply::Ok`/`Reply::Error` must stay compatible across versions; variants
/// are encoded by name.
///
/// Version 4 adds [`Request::ProviderStream`], version 5
/// [`Request::ImportVault`], version 6 [`AuthScheme::TelegramBot`] and
/// version 7 [`HttpMethod::Put`], version 8 [`Request::SocketOpen`] with
/// [`AuthScheme::DiscordBot`], version 9 [`SocketScheme::IrcSasl`], and version
/// 10 encrypted setup channels and TOTP cancellation, 11 reviewed vault
/// imports, 12 [`AuthScheme::WebhookUrl`], 13 [`Request::GitReceivePack`], 14
/// [`Request::GitUploadPack`], 15 [`AuthScheme::SubscriptionToken`], 16
/// [`Request::SshAgent`], and 17 bounded credential metadata pages and
/// fixed-policy API-key creation. A broker accepts greetings from
/// [`MIN_BROKER_VERSION`] up to this version, and a client falls back to the
/// newest version an older broker accepts (see `Client::version`), so streams
/// are only requested from brokers that understand them.
pub const BROKER_VERSION: u16 = 17;
pub use a206_vault_channel::CREDENTIALS_BROKER_VERSION as CREDENTIALS_VERSION;
/// First version with connection-held source reviews and guarded vault import.
pub use a206_vault_channel::REVIEWED_IMPORT_BROKER_VERSION as REVIEWED_IMPORT_VERSION;
/// The oldest protocol version this broker still serves and this client still
/// speaks. Versions 3 to 17 differ only by the requests, roles and schemes they add.
pub const MIN_BROKER_VERSION: u16 = 3;
/// The first version that serves [`Request::ProviderStream`].
pub const STREAM_VERSION: u16 = 4;
/// The first version that serves [`Request::ImportVault`].
pub const IMPORT_VERSION: u16 = 5;
/// The first version that attaches messenger credentials ([`AuthScheme::TelegramBot`]).
pub const MESSENGER_VERSION: u16 = 6;
/// The first version that sends [`HttpMethod::Put`] (Matrix sends messages
/// with it).
pub const PUT_VERSION: u16 = 7;
/// The first version that opens WebSocket sessions ([`Request::SocketOpen`])
/// and attaches Discord bot tokens ([`AuthScheme::DiscordBot`]).
pub const SOCKET_VERSION: u16 = 8;
/// The first version that opens IRC sessions ([`SocketScheme::IrcSasl`]).
pub const IRC_VERSION: u16 = 9;
/// First version with authenticated encrypted setup and explicit TOTP cancellation.
pub const SETUP_CHANNEL_VERSION: u16 = 10;
/// The first version that sends to webhook URLs held in the vault
/// ([`AuthScheme::WebhookUrl`]).
pub const WEBHOOK_VERSION: u16 = 12;
/// The first version that pushes over git's smart HTTP protocol
/// ([`Request::GitReceivePack`]).
pub const GIT_VERSION: u16 = 13;
/// The first version that fetches over git's smart HTTP protocol
/// ([`Request::GitUploadPack`]).
pub const GIT_FETCH_VERSION: u16 = 14;
/// The first version that attaches web search keys
/// ([`AuthScheme::SubscriptionToken`]).
pub const SEARCH_VERSION: u16 = 15;
/// The first version that signs SSH authentication with vault-held keys
/// ([`Request::SshAgent`]).
pub const SSH_AGENT_VERSION: u16 = 16;
/// Largest data the broker signs for SSH authentication: a session
/// identifier and a user authentication request are far smaller.
pub const MAX_SSH_SIGN_DATA: usize = 16 * 1024;
/// Longest IRC line the gateway may send, without its CR LF (RFC 1459's
/// 512 bytes, less the line ending).
pub const MAX_IRC_LINE: usize = 510;
/// Largest text message relayed in either direction of a socket session.
/// Larger upstream messages end the session; the rest of the frame is
/// reserved for the envelope.
pub const MAX_SOCKET_MESSAGE: usize = 2 * 1024 * 1024;
/// Largest response body slice carried by one [`Reply::ProviderChunk`]. Small
/// frames keep a stream responsive and bound what either side buffers.
pub const MAX_STREAM_CHUNK: usize = 64 * 1024;
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
/// perform destination-bound requests, after presenting a pairing a trusted
/// user client minted for the current unlock; it never receives a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Gateway,
    /// Carries only authenticated ciphertext for the browser setup surface.
    Relay,
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
    /// `/bot…` prefixed to the request path (Telegram Bot API). Since
    /// [`MESSENGER_VERSION`].
    TelegramBot,
    /// `Authorization: Bot …` (Discord). Since [`SOCKET_VERSION`].
    DiscordBot,
    /// The credential is a whole webhook URL, whose path and query carry its
    /// key (Slack, Discord, Google Chat and Teams incoming webhooks). The
    /// call names only the URL's origin, with an empty path and no query; the
    /// broker sends to the stored URL, which must be on that same origin.
    /// Since [`WEBHOOK_VERSION`].
    WebhookUrl,
    /// `X-Subscription-Token: …` with `Accept: application/json` (Brave
    /// Search). Since [`SEARCH_VERSION`].
    SubscriptionToken,
}

/// How the broker opens an authenticated WebSocket session. The service is
/// first asked, with the credential, where to connect; the socket URL it
/// answers with (which may itself carry a session ticket) never leaves the
/// broker. Since [`SOCKET_VERSION`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SocketScheme {
    /// `GET` the call's URL (Discord's `/gateway/bot`) with
    /// [`AuthScheme::DiscordBot`] and connect to the `url` it names, with
    /// Discord's version and JSON encoding. The service's first message
    /// (Hello) is relayed, then the broker sends the Identify message that
    /// carries the token, with these gateway intents.
    DiscordGateway { intents: u64 },
    /// `POST` the call's URL (Slack's `apps.connections.open`) with the
    /// app-level token as a bearer, and connect to the `url` it names.
    SlackSocketMode,
    /// Connect to the IRC server the call's URL names (`ircs://host:port`,
    /// or `irc://` on loopback), over TLS unless on loopback. The broker
    /// requests the `sasl` and `account-tag` capabilities (and
    /// `message-tags` when offered), registers with `login` as nickname,
    /// and signs in to that services account with SASL PLAIN, the credential
    /// being its password. Lines are relayed only once sign-in succeeded:
    /// each frame is one line without its line ending, and each
    /// `SocketSend` is one line of at most [`MAX_IRC_LINE`] bytes that the
    /// broker ends with CR LF. A refusal has status 0 and one of the codes
    /// `sasl_unavailable`, `account_tag_unavailable`, `sasl_failed`,
    /// `nick_in_use` or `registration_failed`. Since [`IRC_VERSION`].
    IrcSasl { login: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SocketCall {
    pub credential: String,
    /// The HTTPS (or loopback HTTP) URL that names the socket, or the IRC
    /// server for [`SocketScheme::IrcSasl`]. The credential must be bound to
    /// its origin (for IRC, `ircs://host:port`).
    pub url: String,
    pub scheme: SocketScheme,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    Get,
    Post,
    /// Since [`PUT_VERSION`].
    Put,
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

/// One round of a push or fetch over git's smart HTTP protocol, to the
/// `git-receive-pack` or `git-upload-pack` service of a repository. The
/// credential, a token, is sent as the password of HTTP Basic
/// authentication; forges each expect a particular username with it, which
/// is not secret. Since [`GIT_VERSION`] for pushes, [`GIT_FETCH_VERSION`]
/// for fetches.
#[derive(Debug, Serialize, Deserialize)]
pub struct GitCall {
    pub credential: String,
    pub username: String,
    /// The repository URL as a git remote names it: HTTPS (or loopback
    /// HTTP), without credentials, a query or a fragment. The credential
    /// must be bound to its origin.
    pub repository: String,
    /// `None` asks for the repository's references
    /// (`GET <repository>/info/refs?service=<service>`); `Some` sends the
    /// service its request (`POST <repository>/<service>`): a push's update
    /// commands and pack, or a fetch's wants and haves. A successful reply is the
    /// service's own answer; any other content type is refused as
    /// [`UpstreamFailure::NotGit`].
    #[serde(with = "serde_bytes")]
    pub request: Option<Vec<u8>>,
}

/// One use of an SSH key held in the vault, for an SSH client the gateway
/// runs (an SSH agent's work). Since [`SSH_AGENT_VERSION`].
#[derive(Debug, Serialize, Deserialize)]
pub struct SshAgentCall {
    pub credential: String,
    /// The `ssh://host:port` server the client is connecting to; the key
    /// must be bound to it.
    pub destination: String,
    /// `None` asks only for the public key; `Some` asks for a signature of
    /// what the client signs to authenticate with that key (a session
    /// identifier and user authentication request), and of nothing else.
    #[serde(with = "serde_bytes")]
    pub sign: Option<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiKeyInput {
    pub version: u16,
    pub name: String,
    pub label: String,
    pub destinations: Vec<String>,
    pub value: WireSecret,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    CredentialPage {
        version: u16,
        after: Option<String>,
        limit: u16,
    },
    AddApiKey(ApiKeyInput),
    ReviewVaultImport {
        version: u16,
        source: std::path::PathBuf,
    },
    ApplyVaultImport {
        version: u16,
        review: uuid::Uuid,
        password: WireSecret,
        totp: Option<WireSecret>,
    },
    CancelVaultImport {
        version: u16,
    },
    /// Must be the first message on every connection.
    Hello {
        role: Role,
        version: u16,
    },
    Status,
    ChannelKey,
    OpenChannel {
        #[serde(with = "serde_bytes")]
        first: Vec<u8>,
    },
    ChannelMessage {
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    /// Adopt an existing password-protected SecureStore vault (such as
    /// RustyClaw's) as this profile's vault when none exists yet. The broker
    /// copies the encrypted file and opens only its copy; the source is never
    /// written. `totp` is required when the source has two-factor enrolled.
    /// On success the vault is unlocked, as after `Unlock`.
    ImportVault {
        source: std::path::PathBuf,
        password: WireSecret,
        totp: Option<String>,
    },
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
    CancelTotp,
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
    /// Mint the gateway pairing for this unlock (user role, step-up). It
    /// replaces any earlier pairing; locking the vault voids it.
    PairGateway,
    /// Gateway role: present the pairing before any `Provider` call.
    GatewayAuth {
        token: WireSecret,
    },
    Provider(ProviderCall),
    /// Gateway role, version 4: like `Provider`, but the successful response
    /// body is relayed as it arrives. The broker answers with one
    /// `ProviderHead`, then `ProviderChunk`s numbered from 0, then
    /// `ProviderEnd`; or with an `Error` at any point, which ends the stream.
    /// A lock, pairing replacement or pairing void ends it with an `Error`; a
    /// closed connection abandons the upstream request.
    ProviderStream(ProviderCall),
    /// Gateway role, version 8: open a WebSocket session for the rest of
    /// this connection. The broker answers with `SocketOpened` or
    /// `SocketRefused`, then relays each upstream text message as a
    /// `SocketFrame` numbered from 0, until `SocketClosed` or an `Error`
    /// ends the session. A lock, pairing replacement or pairing void ends it
    /// with an `Error`; a closed connection closes the socket. The broker
    /// sends WebSocket pings and ends a session that stays silent too long.
    SocketOpen(SocketCall),
    /// Send one text message on this connection's open socket; at most
    /// [`MAX_SOCKET_MESSAGE`] bytes. Never answered on its own id: a failure
    /// ends the session with an `Error` on the `SocketOpen` id.
    SocketSend {
        text: String,
    },
    /// Gateway role, version 13: one round of a git push. Answered with
    /// `Provider`, whose body is empty for an unsuccessful status.
    GitReceivePack(GitCall),
    /// Gateway role, version 14: one round of a git fetch. Answered like
    /// `GitReceivePack`.
    GitUploadPack(GitCall),
    /// Gateway role, version 16: the public key of an SSH key bound to a
    /// destination, and a signature of an authentication request. Answered
    /// with `SshAgent`; the private key never leaves the broker.
    SshAgent(SshAgentCall),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerStatus {
    pub exists: bool,
    pub unlocked: bool,
    /// Unknown while locked: enrollment lives inside the encrypted vault.
    pub totp_enrolled: Option<bool>,
    pub agent_access: Option<bool>,
    pub step_up_active: bool,
    /// Whether a gateway has claimed a current pairing for this unlock; unknown while
    /// locked or from an older broker. Locking voids every pairing.
    #[serde(default)]
    pub gateway_paired: Option<bool>,
}

/// Structured broker failures. Clients render these with `Display`; nothing
/// is flattened to a string before it reaches the client or tracing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum BrokerError {
    #[error("vault import failed")]
    ImportFailure(#[source] a206_core::ErrorReport),
    #[error("vault import review is unavailable; preview the source again")]
    ImportReviewUnavailable,
    #[error("source vault changed; preview it again before importing")]
    ImportSourceChanged,
    #[error("encrypted vault channel failed")]
    Channel(#[source] a206_core::ErrorReport),
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
    #[error("the gateway is not paired with the unlocked vault; run `agent206 vault pair`")]
    GatewayNotPaired,
    #[error("no two-factor enrollment is pending on this connection")]
    NoPendingEnrollment,
    #[error("credential use denied")]
    Denied(#[source] a206_vault::Denial),
    #[error("credential not found")]
    NotFound,
    #[error("a credential with that name already exists")]
    AlreadyExists,
    #[error("this profile already has a vault; an import never replaces or merges into it")]
    VaultExists,
    #[error("invalid request")]
    Invalid(#[source] InvalidRequest),
    #[error("vault storage failed")]
    Vault(#[source] VaultFailure),
    #[error("provider request failed")]
    Upstream(#[source] UpstreamFailure),
    #[error("the SSH key could not be used")]
    SshKey(#[source] a206_vault::SshKeyError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum InvalidRequest {
    #[error("credential page limit or cursor is invalid")]
    CredentialPage,
    #[error("API key is empty or exceeds the credential setup size limit")]
    ApiKeyValue,
    #[error("credential label is empty, too long or contains control characters")]
    CredentialLabel,
    #[error("unsupported broker protocol version")]
    UnsupportedVersion,
    #[error("the connection already identified itself")]
    AlreadyGreeted,
    #[error("credential name is empty, reserved, too long or has control characters")]
    Name,
    #[error("credential value does not match its kind")]
    KindMismatch,
    #[error(
        "destinations must be HTTPS (or loopback HTTP) URLs, ircs:// servers or ssh://host[:port] servers, without embedded credentials"
    )]
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
    #[error("this connection has no open socket")]
    NoSocket,
    #[error("this connection carries a socket session and takes no other requests")]
    SocketBusy,
    #[error("a socket message is larger than the vault broker relays")]
    SocketMessage,
    #[error("IRC servers are ircs:// (or loopback irc://) URLs without credentials or paths")]
    IrcServer,
    #[error("an IRC login must be a valid nickname")]
    IrcLogin,
    #[error("an IRC message must be one line of at most 510 bytes without NUL")]
    IrcLine,
    #[error("a webhook call names only the webhook's origin, without a path or query")]
    WebhookPath,
    #[error(
        "git repositories are HTTPS (or loopback HTTP) URLs without credentials, a query or a fragment"
    )]
    GitRepository,
    #[error("a git username must be 1 to 256 characters without a colon or control characters")]
    GitUsername,
    #[error("an SSH key use names an ssh://host:port server and at most 16 KiB to sign")]
    SshAgent,
    #[error(
        "the vault to import must be an absolute path to a readable regular file of at most 16 MiB"
    )]
    ImportSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum VaultFailure {
    #[error(
        "credential metadata exceeds the private page limit; inspect it with a trusted local tool"
    )]
    MetadataTooLarge,
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
    #[error("the stored credential is not a valid bot token")]
    InvalidBotToken,
    #[error("the stored credential cannot be sent as an IRC account password")]
    InvalidPassword,
    #[error("the stored credential is not a webhook URL on the origin it is bound to")]
    InvalidWebhookUrl,
    #[error("the service named a socket URL that is not allowed")]
    SocketUrl,
    #[error("the service broke its socket protocol")]
    SocketProtocol,
    #[error("the server did not answer as a git smart HTTP service")]
    NotGit,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    CredentialPage(a206_vault_channel::CredentialPage),
    Ok,
    VaultImportReview(a206_vault_channel::ImportReview),
    VaultImportApplied(a206_vault_channel::ImportApplied),
    ChannelKey(Vec<u8>),
    ChannelOpened(#[serde(with = "serde_bytes")] Vec<u8>),
    ChannelMessage(#[serde(with = "serde_bytes")] Vec<u8>),
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
    GatewayPairing {
        token: WireSecret,
    },
    Provider {
        status: u16,
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },
    /// An SSH key's public key and, when asked for, a signature, both in SSH
    /// wire format.
    SshAgent {
        #[serde(with = "serde_bytes")]
        public_key: Vec<u8>,
        #[serde(with = "serde_bytes")]
        signature: Option<Vec<u8>>,
    },
    /// The upstream status of a streamed call. Like `Provider`, an
    /// unsuccessful status is followed directly by `ProviderEnd`: upstream
    /// error bodies are never relayed.
    ProviderHead {
        status: u16,
    },
    /// At most [`MAX_STREAM_CHUNK`] bytes of a streamed body. `seq` counts
    /// chunks from 0 without gaps.
    ProviderChunk {
        seq: u64,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    /// A streamed body ended normally, after `chunks` chunks totalling `bytes`.
    ProviderEnd {
        chunks: u64,
        bytes: u64,
    },
    /// The socket is connected; frames follow.
    SocketOpened,
    /// The service refused to name a socket: its HTTP status, and for Slack
    /// the error code it reported (lowercase letters, digits and
    /// underscores only; anything else is dropped). The session is over.
    SocketRefused {
        status: u16,
        code: Option<String>,
    },
    /// One upstream text message. `seq` counts frames from 0 without gaps.
    SocketFrame {
        seq: u64,
        text: String,
    },
    /// The service closed the socket, with its close code if it sent one.
    /// Close reasons are not relayed. The session is over.
    SocketClosed {
        code: Option<u16>,
    },
    Error(BrokerError),
}
