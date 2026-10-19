//! IRC adapter: the vault broker connects to the server over TLS and signs
//! in to the bot's services account with SASL ([`SocketScheme::IrcSasl`]),
//! so this side never sees the password. Lines arrive once sign-in
//! succeeded, and replies go out on the same session.
//!
//! IRC nicknames change hands, so people are identified by their services
//! account, which the server attaches to each message (`account-tag`):
//! sender lists name accounts, a direct message's chat is the sender's
//! account, and a message from someone not signed in to services is never
//! taken as from anyone listed. Before each line of a direct reply is sent,
//! the nickname is checked (`WHOIS`) to still belong to that account.
//!
//! The session runs in its own task, which answers the server's pings while
//! turns run and paces replies so the server does not disconnect the bot
//! for flooding. Messages sent while the bot was disconnected are not
//! delivered: IRC has no history to catch up from.
use super::listener::{Listener, Queue};
use super::{Account, Api, Batch, Event, Inbound, MessengerError, Result};
use a206_broker::{MAX_IRC_LINE, Socket, SocketEvent, SocketScheme};
use a206_core::messengers::{MAX_IRC_NICK, irc_lowercase, valid_irc_channel, valid_irc_nick};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const SERVICE: &str = "IRC";
/// Room left in each line for the source (`:nick!user@host `) the server
/// adds when it relays a message, so the relayed line still fits in 512 bytes.
const RELAY_ROOM: usize = 120;
/// Most lines one reply is sent as; the rest is cut with a notice.
pub(crate) const MAX_REPLY_LINES: usize = 24;
/// Time between lines, so a long reply is not taken for a flood. A direct
/// reply's lines each follow their own `WHOIS`, so they are spaced twice this.
const LINE_SPACING: Duration = Duration::from_millis(700);
/// How long the server may take to answer a `WHOIS`.
const WHOIS_TIMEOUT: Duration = Duration::from_secs(30);
/// Replies waiting for the session.
const OUTGOING: usize = 16;
/// Most nicknames remembered for direct-message accounts.
const MAX_NICKS: usize = 4096;
const TRUNCATED: &str = "(reply cut short; read the rest in Agent 206)";

pub(crate) struct Irc {
    api: Api,
    login: Option<String>,
    /// Channels joined once signed in: the allowed chats that are channels.
    channels: Vec<String>,
    session: Option<Session>,
}

struct Session {
    listener: Listener,
    outgoing: mpsc::Sender<Outgoing>,
    account: Account,
}

/// Work for the session task, answered once done.
enum Outgoing {
    Reply {
        chat: String,
        text: String,
        done: oneshot::Sender<Result<()>>,
    },
    Join {
        channel: String,
        done: oneshot::Sender<Result<()>>,
    },
}

impl Irc {
    pub(crate) fn new(api: Api, login: Option<String>, allowed: BTreeSet<String>) -> Self {
        Self {
            api,
            login,
            channels: allowed
                .into_iter()
                .filter(|chat| valid_irc_channel(chat))
                .collect(),
            session: None,
        }
    }

    /// Sign in, unless the current session is still running, and name the
    /// account. The broker signs in before relaying anything, so a session
    /// that opened is signed in to `login`.
    pub(crate) async fn connect(&mut self) -> Result<Account> {
        if let Some(session) = &self.session {
            return Ok(session.account.clone());
        }
        let login = self
            .login
            .clone()
            .filter(|login| valid_irc_nick(login))
            .ok_or(MessengerError::NoLogin)?;
        let mut socket = self
            .api
            .socket(
                SocketScheme::IrcSasl {
                    login: login.clone(),
                },
                self.api.base.clone(),
                false,
            )
            .await?;
        // The broker relays from the welcome on, which names the nickname
        // the server registered.
        let welcome = match socket.next().await.map_err(MessengerError::Broker)? {
            SocketEvent::Text(text) => text,
            SocketEvent::Closed(code) => {
                return Err(MessengerError::SocketClosed {
                    service: SERVICE,
                    code,
                });
            }
        };
        let welcome = Line::parse(&welcome);
        let nick = match (welcome.command, welcome.params.first()) {
            ("001", Some(nick)) if valid_irc_nick(nick) => (*nick).to_owned(),
            _ => return Err(MessengerError::UnexpectedReply),
        };
        let account = Account {
            id: irc_lowercase(&login),
            name: Some(nick.clone()),
        };
        let (outgoing, requests) = mpsc::channel(OUTGOING);
        let channels = self.channels.clone();
        let listener = Listener::start(SERVICE, account.id.clone(), move |queue| {
            serve(socket, nick, channels, requests, queue)
        });
        self.session = Some(Session {
            listener,
            outgoing,
            account: account.clone(),
        });
        Ok(account)
    }

    /// The messages the session queued, waiting for at least one. A session
    /// failure ends it; the next connect signs in again.
    pub(crate) async fn receive(&mut self, _cursor: Option<&str>) -> Result<Batch> {
        let Some(session) = self.session.as_mut() else {
            return Err(MessengerError::SocketEnded { service: SERVICE });
        };
        let batch = session.listener.receive().await;
        if batch.is_err() {
            self.session = None;
        }
        batch
    }

    /// Join an allowed channel the bot was invited to.
    pub(crate) async fn join(&self, chat: &str) -> Result<()> {
        if !valid_irc_channel(chat) {
            return Ok(());
        }
        self.request(|done| Outgoing::Join {
            channel: chat.to_owned(),
            done,
        })
        .await
    }

    /// Send `text` to a channel, or to the account a direct-message chat
    /// names, as plain lines with control characters removed, so a reply
    /// cannot send a command or a client-to-client request.
    pub(crate) async fn send(&self, chat: &str, text: &str) -> Result<()> {
        self.request(|done| Outgoing::Reply {
            chat: chat.to_owned(),
            text: text.to_owned(),
            done,
        })
        .await
    }

    async fn request(
        &self,
        work: impl FnOnce(oneshot::Sender<Result<()>>) -> Outgoing,
    ) -> Result<()> {
        let session = self.session.as_ref().ok_or_else(ended)?;
        request(&session.outgoing, work).await
    }

    /// A way for `send_message` to post through the running session.
    pub(crate) fn poster(&self) -> Option<Poster> {
        let session = self.session.as_ref()?;
        Some(Poster(session.outgoing.downgrade()))
    }
}

fn ended() -> MessengerError {
    MessengerError::SocketEnded { service: SERVICE }
}

async fn request(
    outgoing: &mpsc::Sender<Outgoing>,
    work: impl FnOnce(oneshot::Sender<Result<()>>) -> Outgoing,
) -> Result<()> {
    let (done, answer) = oneshot::channel();
    outgoing.send(work(done)).await.map_err(|_| ended())?;
    answer.await.map_err(|_| ended())?
}

/// Posts through a session without keeping it open: once the session ends,
/// a post fails as [`MessengerError::SocketEnded`].
#[derive(Clone)]
pub(crate) struct Poster(mpsc::WeakSender<Outgoing>);
impl Poster {
    /// Send one line made by [`post_lines`], as a reply is sent: paced, and
    /// to a direct chat only while the account still holds the nickname it
    /// last wrote from during this session.
    pub(crate) async fn send(&self, chat: &str, line: &str) -> Result<()> {
        let outgoing = self.0.upgrade().ok_or_else(ended)?;
        request(&outgoing, |done| Outgoing::Reply {
            chat: chat.to_owned(),
            text: line.to_owned(),
            done,
        })
        .await
    }
}

/// A server line's tags, source nickname, command and parameters.
struct Line<'a> {
    tags: Vec<(&'a str, String)>,
    nick: Option<&'a str>,
    command: &'a str,
    params: Vec<&'a str>,
}
impl<'a> Line<'a> {
    fn parse(line: &'a str) -> Self {
        let mut rest = line;
        let mut tags = vec![];
        if let Some(tagged) = rest.strip_prefix('@') {
            let (list, tail) = tagged.split_once(' ').unwrap_or((tagged, ""));
            tags = list
                .split(';')
                .filter(|tag| !tag.is_empty())
                .map(|tag| {
                    let (key, value) = tag.split_once('=').unwrap_or((tag, ""));
                    (key, unescape(value))
                })
                .collect();
            rest = tail.trim_start_matches(' ');
        }
        let mut nick = None;
        if let Some(source) = rest.strip_prefix(':') {
            let (source, tail) = source.split_once(' ').unwrap_or((source, ""));
            nick = source.split('!').next();
            rest = tail.trim_start_matches(' ');
        }
        let (command, mut rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let mut params = vec![];
        loop {
            rest = rest.trim_start_matches(' ');
            if rest.is_empty() {
                break;
            }
            if let Some(trailing) = rest.strip_prefix(':') {
                params.push(trailing);
                break;
            }
            let (param, tail) = rest.split_once(' ').unwrap_or((rest, ""));
            params.push(param);
            rest = tail;
        }
        Self {
            tags,
            nick,
            command,
            params,
        }
    }
    fn tag(&self, key: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    }
}

/// A message tag value with its escapes undone (IRCv3 message tags).
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Answer pings, queue messages and send replies until something fails.
async fn serve(
    socket: Socket,
    nick: String,
    channels: Vec<String>,
    requests: mpsc::Receiver<Outgoing>,
    queue: Queue,
) -> MessengerError {
    match session(socket, nick, channels, requests, queue).await {
        Ok(never) => match never {},
        Err(error) => error,
    }
}

/// A direct reply being sent a line at a time, each after a `WHOIS` shows
/// `nick` still belongs to `account`, so a nickname that changes hands
/// mid-reply receives none of the rest.
struct Check {
    nick: String,
    account: String,
    /// The account the server reported for `nick`, once it has.
    found: Option<String>,
    lines: VecDeque<String>,
    done: oneshot::Sender<Result<()>>,
    /// When to ask about the next line; `None` while a question is open.
    ask_at: Option<tokio::time::Instant>,
    /// When the open question times out.
    until: Option<tokio::time::Instant>,
}

/// Lines being sent, spaced out, and who waits for them.
#[derive(Default)]
struct Sending {
    lines: VecDeque<String>,
    /// Answered when the line before it in `lines` has been sent.
    waiting: VecDeque<(usize, oneshot::Sender<Result<()>>)>,
    sent: usize,
    next: Option<tokio::time::Instant>,
}
impl Sending {
    fn push(&mut self, lines: Vec<String>, done: oneshot::Sender<Result<()>>) {
        self.lines.extend(lines);
        self.waiting.push_back((self.sent + self.lines.len(), done));
        // A reply with no text to send is done at once.
        self.settle();
    }
    /// Answer everyone whose lines have all been sent.
    fn settle(&mut self) {
        while self
            .waiting
            .front()
            .is_some_and(|(last, _)| *last <= self.sent)
        {
            if let Some((_, done)) = self.waiting.pop_front() {
                answer(done, Ok(()));
            }
        }
    }
}

async fn session(
    mut socket: Socket,
    mut nick: String,
    channels: Vec<String>,
    mut requests: mpsc::Receiver<Outgoing>,
    queue: Queue,
) -> Result<std::convert::Infallible> {
    let broker = MessengerError::Broker;
    for channel in &channels {
        socket
            .send(format!("JOIN {channel}"))
            .await
            .map_err(broker)?;
    }
    // The nickname each account last wrote a direct message from.
    let mut nicks: HashMap<String, String> = HashMap::new();
    let mut check: Option<Check> = None;
    let mut sending = Sending::default();
    loop {
        // Channel and direct lines share one pace: a direct line waits for
        // the channel line before it, and channel lines wait while a direct
        // line's `WHOIS` is open, since that line goes out on its answer.
        let until = check.as_ref().and_then(|check| check.until);
        let ask_at = check
            .as_ref()
            .and_then(|check| check.ask_at)
            .map(|at| sending.next.map_or(at, |next| at.max(next)));
        let next_line = sending
            .next
            .filter(|_| !sending.lines.is_empty() && until.is_none());
        tokio::select! {
            event = socket.next() => {
                let text = match event.map_err(broker)? {
                    SocketEvent::Text(text) => text,
                    SocketEvent::Closed(code) => {
                        return Err(MessengerError::SocketClosed { service: SERVICE, code });
                    }
                };
                let line = Line::parse(&text);
                match (line.command, line.params.as_slice()) {
                    ("PING", [.., token]) => {
                        socket.send(format!("PONG :{token}")).await.map_err(broker)?;
                    }
                    ("ERROR", _) => {
                        return Err(MessengerError::SocketClosed { service: SERVICE, code: None });
                    }
                    ("NICK", [new]) => {
                        let old = line.nick.unwrap_or_default();
                        if irc_lowercase(old) == irc_lowercase(&nick) {
                            (*new).clone_into(&mut nick);
                        }
                        for known in nicks.values_mut() {
                            if irc_lowercase(known) == irc_lowercase(old) {
                                (*new).clone_into(known);
                            }
                        }
                    }
                    ("QUIT", _) => {
                        let old = irc_lowercase(line.nick.unwrap_or_default());
                        nicks.retain(|_, known| irc_lowercase(known) != old);
                    }
                    ("PRIVMSG", [target, text]) => {
                        let event = message_event(&line, target, text, &nick);
                        if let Event::Message(inbound) = &event
                            && !inbound.group
                            && let Some(from) = line.nick
                        {
                            if nicks.len() >= MAX_NICKS {
                                nicks.clear();
                            }
                            nicks.insert(inbound.chat.clone(), from.to_owned());
                        }
                        let key = line
                            .tag("msgid")
                            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);
                        queue.push(key, event)?;
                    }
                    ("INVITE", [_, channel]) => {
                        queue.push(uuid::Uuid::new_v4().to_string(), Event::Invite {
                            chat: irc_lowercase(channel),
                            title: Some((*channel).to_owned()),
                        })?;
                    }
                    // RPL_WHOISACCOUNT: <me> <nick> <account> :is logged in as
                    ("330", [_, whois, account, ..]) => {
                        if let Some(check) = check.as_mut()
                            && check.until.is_some()
                            && irc_lowercase(whois) == irc_lowercase(&check.nick)
                        {
                            check.found = Some(irc_lowercase(account));
                        }
                    }
                    // End of WHOIS, or no such nickname.
                    ("318" | "401", [_, whois, ..]) => {
                        if check.as_ref().is_some_and(|check| {
                            check.until.is_some()
                                && irc_lowercase(whois) == irc_lowercase(&check.nick)
                        }) && let Some(mut current) = check.take()
                        {
                            if current.found.take().as_deref() != Some(current.account.as_str()) {
                                nicks.remove(&current.account);
                                answer(current.done, Err(MessengerError::RecipientGone {
                                    chat: current.account,
                                }));
                            } else if let Some(line) = current.lines.pop_front() {
                                socket.send(line).await.map_err(broker)?;
                                sending.next = Some(tokio::time::Instant::now() + LINE_SPACING);
                                if current.lines.is_empty() {
                                    answer(current.done, Ok(()));
                                } else {
                                    current.until = None;
                                    current.ask_at =
                                        Some(tokio::time::Instant::now() + LINE_SPACING * 2);
                                    check = Some(current);
                                }
                            } else {
                                answer(current.done, Ok(()));
                            }
                        }
                    }
                    _ => {}
                }
            }
            request = requests.recv(), if check.is_none() => {
                // The adapter went away with its listener.
                let Some(request) = request else {
                    return Err(MessengerError::SocketEnded { service: SERVICE });
                };
                match request {
                    Outgoing::Join { channel, done } => {
                        socket.send(format!("JOIN {channel}")).await.map_err(broker)?;
                        answer(done, Ok(()));
                    }
                    Outgoing::Reply { chat, text, done } => {
                        let channel = valid_irc_channel(&chat);
                        let target = if channel {
                            Some(chat.clone())
                        } else {
                            nicks.get(&chat).cloned()
                        };
                        let Some(target) = target else {
                            answer(done, Err(MessengerError::RecipientGone { chat }));
                            continue;
                        };
                        let lines = reply_lines(&target, &text);
                        if channel {
                            sending.next.get_or_insert_with(tokio::time::Instant::now);
                            sending.push(lines, done);
                        } else {
                            check = Some(Check {
                                nick: target,
                                account: chat,
                                found: None,
                                lines: lines.into(),
                                done,
                                ask_at: Some(tokio::time::Instant::now()),
                                until: None,
                            });
                        }
                    }
                }
            }
            () = sleep_until(next_line) => {
                if let Some(line) = sending.lines.pop_front() {
                    socket.send(line).await.map_err(broker)?;
                    sending.sent += 1;
                    sending.settle();
                }
                sending.next = Some(tokio::time::Instant::now() + LINE_SPACING);
            }
            () = sleep_until(ask_at) => {
                if let Some(check) = check.as_mut() {
                    socket.send(format!("WHOIS {}", check.nick)).await.map_err(broker)?;
                    check.ask_at = None;
                    check.until = Some(tokio::time::Instant::now() + WHOIS_TIMEOUT);
                }
            }
            () = sleep_until(until) => {
                if let Some(check) = check.take() {
                    answer(check.done, Err(MessengerError::RecipientGone { chat: check.account }));
                }
            }
        }
    }
}

/// Answer whoever asked for `done`. A turn cancelled meanwhile no longer
/// listens, and then nobody needs the answer.
fn answer(done: oneshot::Sender<Result<()>>, result: Result<()>) {
    if done.send(result).is_err() {
        tracing::debug!("an IRC reply finished after its turn stopped waiting");
    }
}

/// Sleep until `at`, or forever without one.
async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// A person's plain message, unless it is the bot's own, another bot's, or
/// a client-to-client request (`ACTION` among them).
fn message_event(line: &Line<'_>, target: &str, text: &str, own: &str) -> Event {
    let Some(from) = line.nick else {
        return Event::Ignored;
    };
    if irc_lowercase(from) == irc_lowercase(own)
        || line.tag("bot").is_some()
        || text.starts_with('\u{1}')
        || text.trim().is_empty()
    {
        return Event::Ignored;
    }
    // `*` means the sender is not signed in to services.
    let account = line
        .tag("account")
        .filter(|account| *account != "*" && valid_irc_nick(account))
        .map(irc_lowercase);
    if valid_irc_channel(target) {
        return Event::Message(Inbound {
            chat: irc_lowercase(target),
            sender: account,
            group: true,
            title: Some(target.to_owned()),
            text: text.to_owned(),
            encrypted: false,
            thread: None,
        });
    }
    // A direct message is the account's chat; without one, it is nobody's.
    let Some(account) = account else {
        tracing::info!("ignored a direct message from someone not signed in to services");
        return Event::Ignored;
    };
    Event::Message(Inbound {
        chat: account.clone(),
        sender: Some(account),
        group: false,
        title: Some(from.to_owned()),
        text: text.to_owned(),
        encrypted: false,
        thread: None,
    })
}

/// `PRIVMSG` lines for `text`: one per line of text, split to fit, without
/// control characters, cut at [`MAX_REPLY_LINES`]. Each line fits the
/// broker's [`MAX_IRC_LINE`] with [`RELAY_ROOM`] to spare, whatever the
/// target's length.
fn reply_lines(target: &str, text: &str) -> Vec<String> {
    let header = format!("PRIVMSG {target} :");
    let mut parts = text_lines(text, line_budget(target.len()));
    if parts.len() > MAX_REPLY_LINES {
        parts.truncate(MAX_REPLY_LINES - 1);
        parts.push(TRUNCATED.to_owned());
    }
    parts
        .into_iter()
        .map(|part| format!("{header}{part}"))
        .collect()
}

/// The lines `send_message` posts `text` to `chat` in, one per
/// [`Poster::send`], or why it cannot be posted whole: a message that needs
/// more than [`MAX_REPLY_LINES`] lines is refused rather than cut. A direct
/// chat's lines are sized for the longest nickname, so the nickname the
/// account uses does not split them further.
pub(crate) fn post_lines(chat: &str, text: &str) -> Result<Vec<String>> {
    let target = if valid_irc_channel(chat) {
        chat.len()
    } else {
        MAX_IRC_NICK
    };
    let lines = text_lines(text, line_budget(target));
    if lines.is_empty() {
        return Err(MessengerError::EmptyMessage);
    }
    if lines.len() > MAX_REPLY_LINES {
        return Err(MessengerError::TooManyLines {
            most: MAX_REPLY_LINES,
        });
    }
    Ok(lines)
}

/// Bytes of text one `PRIVMSG` to a target of `target` bytes carries.
fn line_budget(target: usize) -> usize {
    let header = "PRIVMSG  :".len() + target;
    MAX_IRC_LINE
        .saturating_sub(RELAY_ROOM + header)
        .max(TRUNCATED.len())
}

/// `text` as non-empty lines of at most `budget` bytes, without control
/// characters.
fn text_lines(text: &str, budget: usize) -> Vec<String> {
    let mut parts = vec![];
    for line in text.lines() {
        let clean: String = line
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        let mut rest = clean.trim_end();
        while !rest.is_empty() {
            let (part, tail) = split_at_bytes(rest, budget);
            parts.push(part.to_owned());
            rest = tail.trim_start();
        }
    }
    parts
}

/// `text` split after at most `limit` bytes, at the last space when there
/// is one, never inside a character.
fn split_at_bytes(text: &str, limit: usize) -> (&str, &str) {
    if text.len() <= limit {
        return (text, "");
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(space) = text[..end].rfind(' ').filter(|&space| space > 0) {
        end = space;
    }
    text.split_at(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random() -> String {
        format!("a{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
    }

    #[test]
    fn senders_are_the_accounts_the_server_names() {
        let (own, from, account, channel) =
            (random(), random(), random(), format!("#{}", random()));
        let text = random();
        let line = format!(
            "@account={account};msgid=1 :{from}!u@h PRIVMSG {} :{text}",
            channel.to_uppercase()
        );
        let parsed = Line::parse(&line);
        let Event::Message(inbound) =
            message_event(&parsed, parsed.params[0], parsed.params[1], &own)
        else {
            panic!("expected a message");
        };
        assert_eq!(inbound.chat, channel);
        assert_eq!(inbound.sender, Some(account.clone()));
        assert!(inbound.group);
        assert_eq!(inbound.text, text);

        // Direct messages are the account's chat; the nickname is a title only.
        let line = format!(
            "@account={} :{from}!u@h PRIVMSG {own} :{text}",
            account.to_uppercase()
        );
        let parsed = Line::parse(&line);
        let Event::Message(inbound) =
            message_event(&parsed, parsed.params[0], parsed.params[1], &own)
        else {
            panic!("expected a message");
        };
        assert_eq!(
            (inbound.chat.as_str(), inbound.group),
            (account.as_str(), false)
        );
        assert_eq!(inbound.title.as_deref(), Some(from.as_str()));

        // Not signed in: a channel message has no sender, a direct one is nobody's.
        for tags in ["", "@account=* "] {
            let line = format!("{tags}:{from}!u@h PRIVMSG {channel} :{text}");
            let parsed = Line::parse(&line);
            let Event::Message(inbound) =
                message_event(&parsed, parsed.params[0], parsed.params[1], &own)
            else {
                panic!("expected a message");
            };
            assert_eq!(inbound.sender, None);
            let line = format!("{tags}:{from}!u@h PRIVMSG {own} :{text}");
            let parsed = Line::parse(&line);
            assert_eq!(
                message_event(&parsed, parsed.params[0], parsed.params[1], &own),
                Event::Ignored
            );
        }

        // The bot's own messages, other bots and CTCP requests are ignored.
        for line in [
            format!("@account={account} :{own}!u@h PRIVMSG {channel} :{text}"),
            format!("@account={account};bot :{from}!u@h PRIVMSG {channel} :{text}"),
            format!("@account={account} :{from}!u@h PRIVMSG {channel} :\u{1}ACTION {text}\u{1}"),
        ] {
            let parsed = Line::parse(&line);
            assert_eq!(
                message_event(&parsed, parsed.params[0], parsed.params[1], &own),
                Event::Ignored,
                "{line}"
            );
        }
    }

    #[test]
    fn tag_values_are_unescaped() {
        let line = Line::parse(r"@a=x\:y\sz\\;b= :n PRIVMSG #c :t");
        assert_eq!(line.tag("a"), Some(r"x;y z\"));
        assert_eq!(line.tag("b"), Some(""));
        assert_eq!(line.tag("c"), None);
    }

    #[test]
    fn replies_are_plain_bounded_lines() {
        let target = format!("#{}", random());
        let lines = reply_lines(&target, "one\r\ntwo\u{1}ACTION\u{1}\n\n\tthree\r\nQUIT :x");
        assert_eq!(
            lines,
            [
                format!("PRIVMSG {target} :one"),
                format!("PRIVMSG {target} :twoACTION"),
                format!("PRIVMSG {target} : three"),
                format!("PRIVMSG {target} :QUIT :x"),
            ]
        );
        // The longest channel a messenger may join still gets lines the
        // broker accepts, with room for the source the server adds.
        let longest = format!("#{}", "c".repeat(199));
        assert!(valid_irc_channel(&longest));
        for target in [target.as_str(), &longest] {
            let long = "é".repeat(MAX_IRC_LINE);
            let lines = reply_lines(target, &long);
            assert!(lines.len() > 1);
            assert!(lines.iter().all(|l| l.len() <= MAX_IRC_LINE - RELAY_ROOM));
            assert_eq!(
                lines
                    .iter()
                    .map(|l| l.split_once(" :").unwrap().1)
                    .collect::<String>(),
                long
            );
        }
        let many = (0..MAX_REPLY_LINES * 2)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let lines = reply_lines(&target, &many);
        assert_eq!(lines.len(), MAX_REPLY_LINES);
        assert!(lines.last().unwrap().ends_with(TRUNCATED));
    }
}
