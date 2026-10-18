//! Messenger configuration and status, shared by clients and the gateway.
//!
//! A messenger entry is user configuration: nothing here enables it, connects
//! it or lets a chat reach a thread. Entries start disabled, accept no chat
//! until the user allows it, and spend their bot or access token only through
//! the vault broker for the messenger's API origin. Imported entries stay
//! disabled.
use crate::{ErrorReport, Mode, ModelKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Longest messenger name, chat id or credential name accepted in settings.
/// Matrix room and user ids may be up to 255 bytes.
pub const MAX_MESSENGER_FIELD: usize = 255;
/// Most chats, and most senders, one messenger may allow.
pub const MAX_ALLOWED_CHATS: usize = 256;
/// Most refused chats a status remembers, so the user can allow one.
pub const MAX_REFUSED_CHATS: usize = 16;

/// Whether `name` can name a messenger: lowercase ASCII letters, digits and
/// single inner hyphens, at most 64 bytes.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// Whether `value` is acceptable as a chat id or credential name: not blank,
/// at most [`MAX_MESSENGER_FIELD`] bytes, no control characters.
pub fn valid_field(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_MESSENGER_FIELD
        && !value.chars().any(char::is_control)
}

/// A chat service Agent 206 can connect to. Only implemented services are
/// listed; RustyClaw's other kinds are reported by import as not yet supported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessengerKind {
    /// Telegram Bot API, polled with `getUpdates`.
    #[default]
    Telegram,
    /// Matrix client-server API on the user's homeserver, polled with `/sync`.
    Matrix,
    /// Discord bot: events over the Gateway WebSocket, answers with the REST API.
    Discord,
    /// Slack app: events over Socket Mode, answers with the Web API.
    Slack,
    /// IRC over TLS, signed in to the network's services with SASL.
    Irc,
    /// Signal through a signal-cli daemon the user runs, which holds the
    /// account's keys; Agent 206 talks to its local JSON-RPC socket.
    Signal,
    /// Beeper Desktop's local API, which reaches the chat networks the user
    /// connected to Beeper (WhatsApp, iMessage, Google Chat and others) and
    /// answers as the user's own accounts there.
    Beeper,
    /// An incoming webhook of a chat service (Slack, Discord, Google Chat,
    /// Teams) or any JSON endpoint. It only sends: models post to it with
    /// the `send_message` tool, and the user approves each message.
    Webhook,
}
impl MessengerKind {
    pub const ALL: [Self; 8] = [
        Self::Telegram,
        Self::Matrix,
        Self::Discord,
        Self::Slack,
        Self::Irc,
        Self::Signal,
        Self::Beeper,
        Self::Webhook,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Matrix => "Matrix",
            Self::Discord => "Discord",
            Self::Slack => "Slack",
            Self::Irc => "IRC",
            Self::Signal => "Signal",
            Self::Beeper => "Beeper",
            Self::Webhook => "Webhook",
        }
    }
    /// The service's official API endpoint, used unless an entry names
    /// another. Federated services have none: the user names their server.
    pub fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Self::Telegram => Some("https://api.telegram.org"),
            Self::Matrix => None,
            Self::Discord => Some("https://discord.com"),
            Self::Slack => Some("https://slack.com"),
            // A webhook's origin is the service the user created it on.
            Self::Irc | Self::Signal | Self::Webhook => None,
            // Beeper Desktop serves its API on this loopback port.
            Self::Beeper => Some("http://localhost:23373"),
        }
    }
    /// How the user obtains the token the vault stores for this kind.
    pub fn credential_help(self) -> &'static str {
        match self {
            Self::Telegram => {
                "Create a bot with @BotFather and store its token in the vault, bound to the API origin"
            }
            Self::Matrix => {
                "Sign the bot account in to your homeserver and store its access token in the vault, bound to the homeserver origin"
            }
            Self::Discord => {
                "Create a bot in the Discord developer portal, turn on its Message Content intent, and store its token in the vault, bound to the API origin"
            }
            Self::Slack => {
                "Create a Slack app with Socket Mode and the message events, then store its bot token (xoxb-) and app-level token (xapp-, connections:write) in the vault, both bound to the API origin"
            }
            Self::Irc => {
                "Register the bot's nickname as an account with the network's services, and store its password in the vault, bound to the server (ircs://host:port)"
            }
            Self::Signal => {
                "Register or link the account with signal-cli and run its daemon yourself (signal-cli daemon --socket PATH --receive-mode=manual); signal-cli keeps the account's keys, so no vault credential is used"
            }
            Self::Beeper => {
                "In Beeper Desktop, turn on the Desktop API, create a token under Settings, Integrations, Approved connections, and store it in the vault, bound to the API origin. Replies are sent as you, from your own accounts"
            }
            Self::Webhook => {
                "Create an incoming webhook in the chat service (a Slack app, a Discord channel's Integrations, a Google Chat space's Apps & integrations, or a Teams channel's Workflows) and store its whole URL in the vault, bound to the URL's origin, which is also the endpoint. A webhook only sends: you approve each message a model posts to it"
            }
        }
    }
    /// Whether the kind receives events with a second, app-level token
    /// ([`MessengerConfig::app_credential`]).
    pub fn needs_app_credential(self) -> bool {
        matches!(self, Self::Slack)
    }
    /// Whether the kind signs in with an account name of its own
    /// ([`MessengerConfig::login`]): IRC's services account, or the Signal
    /// account signal-cli serves.
    pub fn needs_login(self) -> bool {
        matches!(self, Self::Irc | Self::Signal)
    }
    /// Whether the kind spends a vault credential. Signal does not: the
    /// user's signal-cli holds the account's keys, and Agent 206 never sees
    /// or stores them.
    pub fn needs_credential(self) -> bool {
        !matches!(self, Self::Signal)
    }
    /// Whether the kind only sends ([`MessengerKind::Webhook`]): it never
    /// connects or receives, and its allow-lists, model and mode are unused.
    pub fn is_send_only(self) -> bool {
        matches!(self, Self::Webhook)
    }
    /// Whether the kind's endpoint is a signal-cli daemon ([`SignalDaemon`]).
    pub fn is_signal(self) -> bool {
        matches!(self, Self::Signal)
    }
    /// A typed chat or sender id as this kind's messenger compares it: IRC
    /// names fold the way the network compares them, and Signal ACIs are
    /// written in signal-cli's lowercase form. Other ids, and Signal group
    /// ids (case-sensitive base64), are kept as typed.
    pub fn fold(self, id: &str) -> String {
        match self {
            Self::Irc => irc_lowercase(id),
            Self::Signal => match uuid::Uuid::try_parse(id) {
                Ok(aci) if id.len() == 36 => aci.hyphenated().to_string(),
                _ => id.to_owned(),
            },
            _ => id.to_owned(),
        }
    }
    /// Whether the kind's endpoint is an IRC server ([`IrcServer`]) rather
    /// than an HTTPS origin.
    pub fn is_irc(self) -> bool {
        matches!(self, Self::Irc)
    }
}

/// The body a webhook messenger posts, by the service the webhook belongs to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookFormat {
    /// `{"text": …}`: the shape Slack, Google Chat and many other services
    /// (Mattermost, Rocket.Chat, Zulip's Slack-compatible hooks) accept.
    #[default]
    Text,
    /// Discord: `{"content": …}`.
    Discord,
    /// A Teams Workflows webhook: one Adaptive Card with the text.
    Teams,
}
impl WebhookFormat {
    pub const ALL: [Self; 3] = [Self::Text, Self::Discord, Self::Teams];
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Slack, Google Chat or JSON text",
            Self::Discord => "Discord",
            Self::Teams => "Microsoft Teams",
        }
    }
    /// The name `--format` and TOML use.
    pub fn key(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Discord => "discord",
            Self::Teams => "teams",
        }
    }
}

/// The text a user's test send posts, so they can see where a messenger's
/// messages arrive before a model posts there.
pub const TEST_MESSAGE: &str = "Test message from Agent 206.";

/// Port of IRC over TLS when an `ircs://` endpoint names none.
pub const IRCS_PORT: u16 = 6697;
/// Port of plain IRC, accepted only on loopback.
const IRC_PORT: u16 = 6667;
/// Longest IRC account name or nickname accepted.
pub const MAX_IRC_NICK: usize = 32;

/// Why an endpoint is not an IRC server Agent 206 connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum IrcServerError {
    #[error("write an IRC server as ircs://host or ircs://host:port, without a path")]
    Shape,
    #[error("IRC needs TLS (ircs://); plain irc:// is accepted only on loopback")]
    Insecure,
}

/// An IRC server as a messenger endpoint: `ircs://host[:port]`, or `irc://`
/// on loopback only, so the account password never crosses a network in
/// the clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrcServer {
    /// Lowercase host name or IP address (IPv6 in brackets).
    pub host: String,
    pub port: u16,
    pub tls: bool,
}
impl IrcServer {
    pub fn parse(endpoint: &str) -> Result<Self, IrcServerError> {
        let url = url::Url::parse(endpoint).map_err(|_| IrcServerError::Shape)?;
        let tls = match url.scheme() {
            "ircs" => true,
            "irc" => false,
            _ => return Err(IrcServerError::Shape),
        };
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err(IrcServerError::Shape);
        }
        let host = url
            .host_str()
            .ok_or(IrcServerError::Shape)?
            .to_ascii_lowercase();
        let valid = !host.is_empty()
            && host.len() <= 253
            && host.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')
            });
        if !valid {
            return Err(IrcServerError::Shape);
        }
        if !tls && !matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
            return Err(IrcServerError::Insecure);
        }
        let port = url.port().unwrap_or(if tls { IRCS_PORT } else { IRC_PORT });
        if port == 0 {
            return Err(IrcServerError::Shape);
        }
        Ok(Self { host, port, tls })
    }
    /// The exact destination a vault credential for this server is bound to.
    pub fn origin(&self) -> String {
        let scheme = if self.tls { "ircs" } else { "irc" };
        format!("{scheme}://{}:{}", self.host, self.port)
    }
}

/// Why an endpoint is not a signal-cli daemon Agent 206 connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum SignalDaemonError {
    #[error("write a signal-cli daemon as unix:///absolute/path/to/socket or tcp://127.0.0.1:port")]
    Shape,
    #[error(
        "signal-cli's JSON-RPC socket has no authentication; connect to it on loopback or through a Unix socket only"
    )]
    NotLoopback,
}

/// A signal-cli JSON-RPC daemon as a messenger endpoint: a Unix socket
/// (`signal-cli daemon --socket PATH`) or a loopback TCP port
/// (`--tcp 127.0.0.1:PORT`). The socket is unauthenticated, so anything that
/// can reach it can use the account; nothing farther than loopback is
/// accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalDaemon {
    /// An absolute socket path.
    Socket(String),
    Tcp(std::net::SocketAddr),
}
impl SignalDaemon {
    pub fn parse(endpoint: &str) -> Result<Self, SignalDaemonError> {
        let url = url::Url::parse(endpoint).map_err(|_| SignalDaemonError::Shape)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(SignalDaemonError::Shape);
        }
        match url.scheme() {
            "unix" => {
                let path = url.path();
                let valid = url.host_str().is_none_or(str::is_empty)
                    && path.starts_with('/')
                    && path.len() > 1
                    && path.len() <= 255
                    && !path.contains('%');
                if valid {
                    Ok(Self::Socket(path.to_owned()))
                } else {
                    Err(SignalDaemonError::Shape)
                }
            }
            "tcp" => {
                if !matches!(url.path(), "" | "/") {
                    return Err(SignalDaemonError::Shape);
                }
                let port = url
                    .port()
                    .filter(|&p| p != 0)
                    .ok_or(SignalDaemonError::Shape)?;
                let ip = match url.host().ok_or(SignalDaemonError::Shape)? {
                    url::Host::Ipv4(ip) => std::net::IpAddr::V4(ip),
                    url::Host::Ipv6(ip) => std::net::IpAddr::V6(ip),
                    // A `tcp:` URL's host is opaque to the URL parser, so an
                    // IPv4 address arrives as a name.
                    url::Host::Domain(name) if name.eq_ignore_ascii_case("localhost") => {
                        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                    }
                    url::Host::Domain(name) => name
                        .parse::<std::net::Ipv4Addr>()
                        .map(std::net::IpAddr::V4)
                        .map_err(|_| SignalDaemonError::NotLoopback)?,
                };
                if !ip.is_loopback() {
                    return Err(SignalDaemonError::NotLoopback);
                }
                Ok(Self::Tcp(std::net::SocketAddr::new(ip, port)))
            }
            _ => Err(SignalDaemonError::Shape),
        }
    }
}

/// Whether `account` names a Signal account signal-cli serves: a phone
/// number in E.164 form (`+` and 7 to 15 digits), or the account's ACI for
/// an account without one.
pub fn valid_signal_account(account: &str) -> bool {
    valid_signal_number(account) || valid_signal_aci(account)
}

fn valid_signal_number(number: &str) -> bool {
    number.strip_prefix('+').is_some_and(|digits| {
        (7..=15).contains(&digits.len())
            && !digits.starts_with('0')
            && digits.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Whether `id` is a Signal account's ACI (a service id), in the lowercase
/// hyphenated form signal-cli reports, which senders and direct chats use.
pub fn valid_signal_aci(id: &str) -> bool {
    id.len() == 36
        && uuid::Uuid::try_parse(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
}

/// Whether `id` is a Signal group id as signal-cli writes it: standard
/// base64 of a 32-byte group identifier.
pub fn valid_signal_group(id: &str) -> bool {
    id.len() == 44
        && id.ends_with('=')
        && id[..43]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// Whether `nick` can be an IRC nickname and account name: a letter or one
/// of `[]\^_`{|}` first, then those, digits and `-`, at most
/// [`MAX_IRC_NICK`] bytes. Anything else could change a command's shape.
pub fn valid_irc_nick(nick: &str) -> bool {
    let special = |b: u8| {
        matches!(
            b,
            b'[' | b']' | b'\\' | b'^' | b'_' | b'`' | b'{' | b'|' | b'}'
        )
    };
    let mut bytes = nick.bytes();
    nick.len() <= MAX_IRC_NICK
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || special(b))
        && bytes.all(|b| b.is_ascii_alphanumeric() || special(b) || b == b'-')
}

/// Whether `name` is an IRC channel Agent 206 joins: `#` or `&` first, at
/// most 200 bytes, without spaces, commas or control characters (BEL among
/// them), so it is always exactly one parameter of a command.
pub fn valid_irc_channel(name: &str) -> bool {
    (name.starts_with('#') || name.starts_with('&'))
        && name.len() > 1
        && name.len() <= 200
        && !name.chars().any(|c| c == ' ' || c == ',' || c.is_control())
}

/// `name` folded for comparison: ASCII letters only (the `ascii` case
/// mapping). Networks using RFC 1459 mapping also treat `[]\\~` as `{}|^`,
/// but folding those would merge names that `ascii` networks keep apart,
/// letting an account listed as `[bot]` admit a different `{bot}`. Folding
/// less only ever refuses a name written another way, never admits one.
pub fn irc_lowercase(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// A user-configured messenger account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessengerConfig {
    /// Lowercase ASCII letters, digits and single hyphens; unique per profile.
    pub name: String,
    pub kind: MessengerKind,
    /// Disabled messengers never connect. Off unless the user turns it on.
    #[serde(default)]
    pub enabled: bool,
    /// Vault credential holding the bot or access token. The broker spends it only for
    /// the origin it is bound to; the gateway never sees its value.
    #[serde(default)]
    pub credential: Option<String>,
    /// Vault credential holding the app-level token a kind such as Slack
    /// receives events with, bound to the same origin. Unused by other kinds.
    #[serde(default)]
    pub app_credential: Option<String>,
    /// Account name a kind such as IRC signs in with: the services account
    /// (SASL) and nickname. Unused by other kinds.
    #[serde(default)]
    pub login: Option<String>,
    /// API base URL; the kind's official endpoint when absent. Required for
    /// kinds without one, such as Matrix (the homeserver) and IRC (an
    /// `ircs://` server).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Chats whose messages reach a thread. Everything else is refused and
    /// listed in the status so the user can allow it.
    #[serde(default)]
    pub allowed_chats: BTreeSet<String>,
    /// Senders whose messages are accepted. Group chats need it: a group's
    /// messages are accepted only from senders listed here. When it is not
    /// empty, it also applies to private chats.
    #[serde(default)]
    pub allowed_users: BTreeSet<String>,
    /// Model a new chat thread uses. Without one, chats are answered with
    /// nothing and the status reports why.
    #[serde(default)]
    pub model: Option<ModelKey>,
    /// Mode, and so system prompt and grants, of new chat threads.
    #[serde(default)]
    pub mode: Mode,
    /// The body a webhook posts. Unused by other kinds.
    #[serde(default)]
    pub format: WebhookFormat,
}
impl MessengerConfig {
    pub fn new(name: String, kind: MessengerKind) -> Self {
        Self {
            name,
            kind,
            enabled: false,
            credential: None,
            app_credential: None,
            login: None,
            endpoint: None,
            allowed_chats: BTreeSet::new(),
            allowed_users: BTreeSet::new(),
            model: None,
            mode: Mode::Chat,
            format: WebhookFormat::Text,
        }
    }
    /// The configured endpoint, else the kind's official one; `None` when
    /// the kind has none and the user has not named one.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint
            .as_deref()
            .or_else(|| self.kind.default_endpoint())
    }
    /// Whether a message from `sender` in `chat` may reach a thread: the chat
    /// is allowed, and the sender is listed, or the list is empty and the
    /// chat is private. Never true for an unknown sender in a group.
    pub fn accepts(&self, chat: &str, sender: Option<&str>, group: bool) -> bool {
        self.allowed_chats.contains(chat)
            && match sender {
                Some(sender) => {
                    self.allowed_users.contains(sender) || (self.allowed_users.is_empty() && !group)
                }
                None => self.allowed_users.is_empty() && !group,
            }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessengerState {
    /// Disabled, disconnected by the user, or not started yet.
    #[default]
    Stopped,
    /// Enabled, but waiting for the gateway to pair with an unlocked vault.
    WaitingForVault,
    Connecting,
    Connected,
    /// A send-only messenger (a webhook) is enabled; it has nothing to
    /// connect, and a failed send is reported to the call that made it.
    Ready,
    /// The last attempt failed; the gateway retries with backoff unless the
    /// failure needs the user (for example a rejected token).
    Failed,
}
impl MessengerState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::WaitingForVault => "Waiting for vault",
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Ready => "Ready to send",
            Self::Failed => "Failed",
        }
    }
}

/// A chat, or a sender in a chat, whose message the messenger refused because
/// it is not allowed. Titles come from the chat service and are untrusted
/// display text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedChat {
    pub chat: String,
    /// The sender, when the chat is allowed but the sender is not.
    #[serde(default)]
    pub sender: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    pub count: u32,
    pub last_at: u64,
}

/// Runtime state of one configured messenger, reported by the gateway.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessengerStatus {
    pub name: String,
    pub state: MessengerState,
    /// The bot account the service reported, as display text.
    #[serde(default)]
    pub account: Option<String>,
    /// Why the last connection or delivery failed.
    #[serde(default)]
    pub failure: Option<ErrorReport>,
    /// Recently refused chats, newest last, at most [`MAX_REFUSED_CHATS`].
    #[serde(default)]
    pub refused: Vec<RefusedChat>,
}
impl MessengerStatus {
    /// Remember a refused chat or sender, keeping the list bounded.
    pub fn refuse(&mut self, chat: &str, sender: Option<&str>, title: Option<String>, at: u64) {
        let sender = sender.map(str::to_owned);
        if let Some(index) = self
            .refused
            .iter()
            .position(|r| r.chat == chat && r.sender == sender)
        {
            let mut entry = self.refused.remove(index);
            entry.count = entry.count.saturating_add(1);
            entry.last_at = at;
            entry.title = title.or(entry.title);
            self.refused.push(entry);
        } else {
            if self.refused.len() >= MAX_REFUSED_CHATS {
                self.refused.remove(0);
            }
            self.refused.push(RefusedChat {
                chat: chat.to_owned(),
                sender,
                title,
                count: 1,
                last_at: at,
            });
        }
    }
}

/// The chat a thread converses with. A thread has at most one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChatBinding {
    pub messenger: String,
    pub chat: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_disabled_and_allow_no_chat_unless_configured() {
        let name = uuid::Uuid::new_v4().simple().to_string();
        let config: MessengerConfig =
            toml::from_str(&format!("name = \"{name}\"\nkind = \"telegram\"\n")).unwrap();
        assert!(!config.enabled);
        assert!(config.allowed_chats.is_empty());
        assert!(config.credential.is_none());
        assert_eq!(
            config.endpoint(),
            MessengerKind::Telegram.default_endpoint()
        );
        assert_eq!(config, MessengerConfig::new(name, MessengerKind::Telegram));
        // A homeserver is the user's choice, never assumed.
        let matrix = MessengerConfig::new(uuid::Uuid::new_v4().to_string(), MessengerKind::Matrix);
        assert_eq!(matrix.endpoint(), None);
        let origin = format!("https://{}.test", uuid::Uuid::new_v4().simple());
        let matrix = MessengerConfig {
            endpoint: Some(origin.clone()),
            ..matrix
        };
        assert_eq!(matrix.endpoint(), Some(origin.as_str()));
    }

    #[test]
    fn refused_chats_stay_bounded_and_move_to_the_end() {
        let mut status = MessengerStatus::default();
        let chats: Vec<String> = (0..MAX_REFUSED_CHATS + 3)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        for (at, chat) in chats.iter().enumerate() {
            status.refuse(chat, None, None, at as u64);
        }
        assert_eq!(status.refused.len(), MAX_REFUSED_CHATS);
        assert_eq!(status.refused[0].chat, chats[3]);
        let title = uuid::Uuid::new_v4().to_string();
        status.refuse(&chats[3], None, Some(title.clone()), 99);
        let last = status.refused.last().unwrap();
        assert_eq!((last.chat.as_str(), last.count), (chats[3].as_str(), 2));
        assert_eq!(last.title.as_deref(), Some(title.as_str()));
        assert_eq!(status.refused.len(), MAX_REFUSED_CHATS);
        // A refused sender in that chat is a separate entry.
        let sender = uuid::Uuid::new_v4().to_string();
        status.refuse(&chats[3], Some(&sender), None, 100);
        assert_eq!(
            status.refused.last().unwrap().sender.as_ref(),
            Some(&sender)
        );
        assert_eq!(status.refused.len(), MAX_REFUSED_CHATS);
    }

    #[test]
    fn signal_daemons_are_local_sockets_or_loopback_ports() {
        let port = 1024 + (uuid::Uuid::new_v4().as_u128() % 60000) as u16;
        let dir = uuid::Uuid::new_v4().simple().to_string();
        assert_eq!(
            SignalDaemon::parse(&format!("unix:///run/{dir}/signal.sock")),
            Ok(SignalDaemon::Socket(format!("/run/{dir}/signal.sock")))
        );
        for (endpoint, ip) in [
            (format!("tcp://127.0.0.1:{port}"), "127.0.0.1"),
            (format!("tcp://localhost:{port}"), "127.0.0.1"),
            (format!("tcp://[::1]:{port}"), "::1"),
        ] {
            assert_eq!(
                SignalDaemon::parse(&endpoint),
                Ok(SignalDaemon::Tcp(std::net::SocketAddr::new(
                    ip.parse().unwrap(),
                    port
                ))),
                "{endpoint}"
            );
        }
        for remote in [
            format!("tcp://192.0.2.1:{port}"),
            format!("tcp://{dir}.example:{port}"),
            format!("tcp://0.0.0.0:{port}"),
        ] {
            assert_eq!(
                SignalDaemon::parse(&remote),
                Err(SignalDaemonError::NotLoopback),
                "{remote}"
            );
        }
        for bad in [
            "tcp://127.0.0.1".to_owned(),
            format!("tcp://127.0.0.1:{port}/rpc"),
            format!("http://127.0.0.1:{port}"),
            "unix://relative".to_owned(),
            format!("unix:///run/{dir}%2f"),
            format!("unix:///run/{dir}?x"),
        ] {
            assert_eq!(
                SignalDaemon::parse(&bad),
                Err(SignalDaemonError::Shape),
                "{bad}"
            );
        }
    }

    #[test]
    fn typed_ids_fold_as_each_service_compares_them() {
        let name = format!("#{}", uuid::Uuid::new_v4().simple()).to_uppercase();
        assert_eq!(MessengerKind::Irc.fold(&name), name.to_lowercase());
        let aci = uuid::Uuid::new_v4().hyphenated().to_string();
        assert_eq!(MessengerKind::Signal.fold(&aci.to_uppercase()), aci);
        // Group ids are case-sensitive base64.
        let group = format!("{}A=", "Ab".repeat(21));
        assert_eq!(MessengerKind::Signal.fold(&group), group);
        for kind in MessengerKind::ALL
            .into_iter()
            .filter(|k| !k.is_irc() && !k.is_signal())
        {
            assert_eq!(kind.fold(&name), name);
        }
    }

    #[test]
    fn signal_ids_are_numbers_acis_and_group_ids() {
        let aci = uuid::Uuid::new_v4().hyphenated().to_string();
        assert!(valid_signal_account(&aci) && valid_signal_aci(&aci));
        assert!(valid_signal_account("+15551234567"));
        for bad in ["15551234567", "+0555", "+1555 123", "+1234567890123456"] {
            assert!(!valid_signal_account(bad), "{bad}");
        }
        assert!(!valid_signal_aci(&aci.to_uppercase()));
        assert!(!valid_signal_aci(&aci.replace('-', "")));
        let group = format!("{}=", "A".repeat(43));
        assert!(valid_signal_group(&group));
        for bad in [group.replace('=', "A"), format!("{}=", "-".repeat(43)), aci] {
            assert!(!valid_signal_group(&bad), "{bad}");
        }
    }

    #[test]
    fn irc_servers_need_tls_off_loopback_and_bind_to_host_and_port() {
        let host = format!("{}.test", uuid::Uuid::new_v4().simple());
        let server = IrcServer::parse(&format!("ircs://{}", host.to_uppercase())).unwrap();
        assert_eq!(
            (server.host.as_str(), server.port, server.tls),
            (host.as_str(), IRCS_PORT, true)
        );
        assert_eq!(server.origin(), format!("ircs://{host}:{IRCS_PORT}"));
        let port = 7000 + (uuid::Uuid::new_v4().as_u128() % 1000) as u16;
        let server = IrcServer::parse(&format!("ircs://{host}:{port}/")).unwrap();
        assert_eq!(server.origin(), format!("ircs://{host}:{port}"));
        assert_eq!(
            IrcServer::parse(&format!("irc://{host}")),
            Err(IrcServerError::Insecure)
        );
        assert!(IrcServer::parse("irc://127.0.0.1:6667").is_ok_and(|s| !s.tls));
        for endpoint in [
            format!("https://{host}"),
            format!("ircs://nick@{host}"),
            format!("ircs://nick:pass@{host}"),
            format!("ircs://{host}/#channel"),
            format!("ircs://{host}/path"),
            format!("ircs://{host}?query"),
            format!("ircs://{host}:0"),
            "ircs://".to_owned(),
            format!("ircs://{host}%0d"),
        ] {
            assert_eq!(
                IrcServer::parse(&endpoint),
                Err(IrcServerError::Shape),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn irc_nicks_cannot_change_a_command_and_names_fold_like_irc() {
        let nick = format!("a{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        assert!(valid_irc_nick(&nick));
        assert!(valid_irc_nick("[bot]-2"));
        for bad in [
            String::new(),
            format!("{nick} x"),
            format!("{nick}\r\nQUIT"),
            format!(":{nick}"),
            format!("#{nick}"),
            format!("1{nick}"),
            format!("-{nick}"),
            "a".repeat(MAX_IRC_NICK + 1),
        ] {
            assert!(!valid_irc_nick(&bad), "{bad:?}");
        }
        assert_eq!(irc_lowercase("#Rust"), "#rust");
        // Distinct accounts on networks with ASCII case mapping stay apart.
        assert_ne!(irc_lowercase("[Bot]"), irc_lowercase("{bot}"));
        assert_eq!(irc_lowercase("[Bot]\\~"), "[bot]\\~");
        assert!(valid_irc_channel("#rust") && valid_irc_channel("&local"));
        for bad in ["#", "rust", "#a b", "#a,#b", "#a\u{7}", "#a\r\nQUIT"] {
            assert!(!valid_irc_channel(bad), "{bad:?}");
        }
    }

    #[test]
    fn groups_need_listed_senders_and_a_sender_list_applies_everywhere() {
        let random = || uuid::Uuid::new_v4().to_string();
        let (chat, user, other) = (random(), random(), random());
        let mut config = MessengerConfig::new(random(), MessengerKind::Telegram);
        assert!(!config.accepts(&chat, Some(&user), false));
        config.allowed_chats.insert(chat.clone());
        assert!(config.accepts(&chat, Some(&user), false));
        assert!(!config.accepts(&chat, Some(&user), true));
        assert!(!config.accepts(&chat, None, true));
        assert!(!config.accepts(&random(), Some(&user), false));
        config.allowed_users.insert(user.clone());
        assert!(config.accepts(&chat, Some(&user), true));
        assert!(!config.accepts(&chat, Some(&other), true));
        assert!(!config.accepts(&chat, Some(&other), false));
        assert!(!config.accepts(&chat, None, false));
    }
}
