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
}
impl MessengerKind {
    pub const ALL: [Self; 5] = [
        Self::Telegram,
        Self::Matrix,
        Self::Discord,
        Self::Slack,
        Self::Irc,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Matrix => "Matrix",
            Self::Discord => "Discord",
            Self::Slack => "Slack",
            Self::Irc => "IRC",
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
            Self::Irc => None,
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
        }
    }
    /// Whether the kind receives events with a second, app-level token
    /// ([`MessengerConfig::app_credential`]).
    pub fn needs_app_credential(self) -> bool {
        matches!(self, Self::Slack)
    }
    /// Whether the kind signs in with an account name of its own
    /// ([`MessengerConfig::login`]).
    pub fn needs_login(self) -> bool {
        matches!(self, Self::Irc)
    }
    /// Whether the kind's endpoint is an IRC server ([`IrcServer`]) rather
    /// than an HTTPS origin.
    pub fn is_irc(self) -> bool {
        matches!(self, Self::Irc)
    }
}

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
