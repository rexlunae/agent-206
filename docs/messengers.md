# Messengers

A messenger connects a chat service account to Agent 206 so that people can
talk to a thread from that service. Telegram, Matrix, Discord, Slack, IRC,
Signal and Beeper (which reaches WhatsApp, iMessage, Google Chat and the
other networks Beeper Desktop connects to) are implemented;
RustyClaw's other channels are listed in
[parity.md](parity.md#messengers--channels).

## Configuration

Messengers are `[[messengers]]` entries in the profile settings (TOML through
`agent206 settings`, the `agent206 messenger` commands, or the Messengers page
of the desktop, web and terminal apps):

```toml
[[messengers]]
name = "team"                 # lowercase letters, digits, single hyphens
kind = "telegram"             # or "matrix", "discord", "slack", "irc", "signal" or "beeper"
enabled = false               # off until the user turns it on
credential = "team-token"     # vault credential name, never the token
                              # (Signal has none)
# app_credential = "team-app"  # Slack only: the app-level token's credential
# login = "teambot"           # IRC: the services account (and nickname);
                              # Signal: the account's +number or ACI
# endpoint = "https://..."    # an origin; defaults to the official API,
                              # required for Matrix (the homeserver) and
                              # IRC (the server, ircs://host[:port]) and
                              # Signal (the signal-cli daemon)
allowed_chats = ["-1001234"]  # chats whose messages reach a thread
allowed_users = ["5551234"]   # senders accepted; groups need it
model = { provider = "<provider id>", model = "<id from the provider's API>" }
mode = "chat"                 # mode of new chat threads
```

The gateway validates every save: unique names, an endpoint for kinds without
an official one, an HTTPS (or loopback HTTP) origin without a path,
credentials, query or fragment, bounded chat and sender lists and printable
ids of at most 255 bytes. Entries start disabled, allow no chat and have no model, so
adding one, including through import, connects nothing and answers nobody.

## Security

- **Tokens stay in the vault.** The gateway asks the vault broker to make
  each API call (Telegram: `AuthScheme::TelegramBot`, broker protocol v6;
  Matrix: a bearer token, with `PUT` from protocol v7; Discord:
  `AuthScheme::DiscordBot` and a brokered WebSocket session, protocol v8,
  see [below](#discord)). The broker checks
  that the credential is bound to the endpoint's exact origin and attaches
  the token itself (Telegram's goes in the request path). Plain HTTP (loopback only)
  never goes through a configured proxy, which would see the token. The token never reaches the
  gateway, events, the model or logs, and the broker refuses a token whose
  characters could change the path. Error bodies from the service are not
  relayed.
- **Deny by default.** A message is accepted only from a chat in
  `allowed_chats`, and only from a sender in `allowed_users`. The one
  exception: while `allowed_users` is empty, private chats (whose only other
  member is the sender) are accepted. Allowing a group therefore admits
  nobody until its senders are listed too, so a new member cannot run turns
  with the thread's grants. Everything else is dropped. The status keeps the
  last 16 refused chats and senders (ids, the service-provided title as
  untrusted display text, a count and time), so the user can allow one
  deliberately.
- **Ordinary turns.** An allowed message becomes a `Send` request on the
  chat's thread, journaled and admitted like one typed in the app. It runs
  under that thread's grants, mode and model allowlist. A chat thread is
  created on first contact with the entry's mode and its model as the only
  allowed model, validated against the current catalog. Without a model the
  chat is told so and nothing runs.
- **Lifecycle.** Messengers run only while the gateway runs and is paired with
  an unlocked vault. They wait while the vault is locked or unpaired, and
  resume when it is paired again. `messenger disconnect` stops one until
  `messenger connect` or a gateway restart; `disable` stops it for good.
- **Failures.** A rejected token (401/404), a stored value that is not a bot
  token, a credential that is missing or refused for this destination, or a
  second poller on the same bot (409) stops the messenger as Failed until the
  user acts; `messenger connect` (or any settings change) retries it. Other failures
  retry with exponential backoff (1 s to 60 s). Each failure is kept in the
  status as a structured `ErrorReport`. A failed turn is reported to the chat
  as "Agent 206 could not answer: …" and in the status.

## Delivery

Telegram is long-polled (`getUpdates`, 25 s). Messages are handled one at a
time per messenger, in order. The poll offset advances as each message's
handling starts and survives reconnects, so while the gateway runs a message
is handled at most once (a reply that fails to send is not retried; the
failure is in the status). The offset is held in memory, so a batch the
gateway handled just before stopping can be delivered again after a
restart. Update ids belong to one bot, so the offset is kept per bot: the
gateway checks which bot the credential names (`getMe`) before each poll, and
a credential replaced with another bot's token starts from a fresh offset
instead of confirming that bot's undelivered updates. Replies longer than Telegram's limit are split at newlines without
cutting a character.

A turn runs as its own task. A messenger stopped mid-turn (disabled,
reconfigured or shut down) lets the turn finish or be cancelled by gateway
shutdown like any other run, and does not send its reply.

## Matrix

A Matrix messenger is a bot account on the user's homeserver. The endpoint is
the homeserver's client API origin (as `.well-known/matrix/client` names it;
discovery is not automatic), and the credential is the account's access
token, bound to that origin:

```sh
agent206 vault add bot-token --kind token --destination https://matrix.example.org
agent206 messenger add home --kind matrix --endpoint https://matrix.example.org --credential bot-token
agent206 messenger allow home '!roomid:example.org'
agent206 messenger allow-sender home '@me:example.org'
agent206 messenger enable home
```

- **Every room is a group.** Matrix does not say reliably whether a room is a
  direct chat, so a message is accepted only from a sender in
  `allowed_users`, in a room in `allowed_chats`. Room and user ids are the
  canonical `!room:server` and `@user:server` forms.
- **Joining.** The bot joins a room only when invited to one in
  `allowed_chats`; other invitations are left pending and listed as refused.
  Allowing one restarts the messenger, whose first sync sees the pending
  invitation and joins it.
- **No end-to-end encryption.** Encrypted messages cannot be read. Before
  each turn and before each reply the gateway checks the room's encryption
  state through a successful full-room state read (a 404 is not proof of
  missing encryption), and never sends anything, not even a failure notice, to an encrypted
  room, since a plain-text message there would downgrade it. The refusal is
  reported in the status only.
- **What is read and sent.** Only `m.text` messages from other accounts are
  read; notices (which bots send), edits, reactions and attachments are
  ignored. Replies are `m.notice` messages, so other bots do not answer them,
  split at 8000 UTF-16 units.
- **Delivery.** `/sync` is long-polled (25 s) with a filter that drops
  presence, receipts and room state. Each time the messenger starts (a
  gateway start, enabling it or changing its settings), its first sync
  starts from now and answers nothing, so history is never answered. The
  sync token is held in memory and survives reconnects, but messages sent
  while the messenger was stopped are not answered. A 401 (expired or
  revoked token) stops the messenger until the user acts. The account is
  checked (`whoami`) before each sync, and another account behind the
  credential starts from a fresh sync token.
  Limited timelines are backfilled using `/rooms/{roomId}/messages` before
  accepting the next sync token, with event-ID deduplication across pages
  and the timeline. Each batch is capped at 64 allowed rooms, 1,000 total events
  (including invitations and backfill), and 32 pagination requests across
  all rooms. Overflow or cyclic pagination fails the batch and preserves
  the previous cursor; it never silently skips messages.
  Unrelated rooms contribute discovery only, outside the allowed-room work
  budget. Up to 4,096 unapproved room IDs are retained and surfaced for
  the recent-refusal feed in chunks of 64 across polls. **The review list
  retains only the latest 16 refused chats or senders**, not a full room
  directory; older entries are replaced. Other room IDs can be added
  manually under Allowed chats or through the CLI. The old cursor is held until
  every discovery reaches that feed; authorized turns and joins are delivered
  only in the final chunk, so reconnecting during discovery cannot repeat
  them. Deduplication keys survive intermediate chunks and reconnects until
  the actual sync token advances. Larger discovery sets produce a budget error without confirming
  the sync. Full
  room state is scanned with constant additional memory under the broker's
  byte limit, separately from sync-event budgets, so large membership lists
  do not prevent checking encryption.

The adapter follows the [Matrix Client-Server API](https://spec.matrix.org/latest/client-server-api/)
for `/join/{roomIdOrAlias}`, full room state, sync and forward message pagination.

## Discord

A Discord messenger is a bot application from the
[developer portal](https://discord.com/developers/applications). Its owner
turns on the bot's **Message Content** intent (a privileged intent; without
it Discord withholds message text), invites the bot to a server, and stores
the bot token bound to the API origin:

```sh
agent206 vault add discord-bot --kind token --destination https://discord.com
agent206 messenger add guild --kind discord --credential discord-bot
agent206 messenger allow guild <channel id>
agent206 messenger allow-sender guild <user id>
agent206 messenger enable guild
```

- **Ids.** Chats are channel ids (a direct message has its own channel id,
  shown as refused on first contact) and senders are user ids, as Discord's
  "Copy ID" (developer mode) gives them. Server channels are groups, so
  their senders must be listed; a direct message is private. A message
  without a server id counts as private only once Discord reports its
  channel as a direct message (`GET /channels/{id}`, remembered per
  channel); a lookup that fails counts it as a group.
- **Brokered socket.** Messages arrive over the
  [Gateway](https://discord.com/developers/docs/events/gateway) WebSocket.
  The gateway asks the broker to open it (`Request::SocketOpen`,
  `SocketScheme::DiscordGateway`, broker protocol v8): the broker asks
  `/api/v10/gateway/bot` with the token where to connect, connects there
  itself, relays Discord's Hello, and sends the Identify message that carries
  the token. The gateway then exchanges heartbeats and receives events
  through the broker, and never sees the token. The broker accepts only a
  `wss` socket URL (a `ws` one only on loopback, for a loopback API), relays
  text messages of at most 2 MiB, pings the service every 30 s and ends a
  session that stays silent for 90 s. Close reasons and error bodies are not
  relayed; close codes are. The socket connects directly, not through a
  configured HTTPS proxy.
- **What is read and sent.** Only default messages and replies with text are
  read; the bot's own messages, webhook posts and system messages are
  ignored. Replies go through `POST /api/v10/channels/{id}/messages`, split
  at 2000 characters, with every mention disabled
  (`allowed_mentions.parse = []`) so a model's reply cannot ping a role or
  `@everyone`.
- **Delivery.** The socket runs in its own task, which keeps heartbeating
  while a turn runs and queues up to 1024 messages; more than that restarts
  the session. Sessions are not resumed: a disconnect, a Reconnect or
  Invalid Session request, or a heartbeat still unacknowledged when the next
  one is due start a new
  session (with the usual backoff), and messages sent meanwhile are not
  answered. A rejected token (HTTP 401 or close code 4004) or refused intents
  (4013, 4014) stop the messenger until the user acts. The account is
  checked (`/users/@me`) before each batch; another bot behind the
  credential gets its own session. A failed check or reply keeps the
  running socket and its queued messages for the retry. A token reset in
  the developer portal ends Discord's session (close code 4004), so the
  next session identifies with the token the vault holds then.

## Slack

A Slack messenger is a Slack app with
[Socket Mode](https://api.slack.com/apis/socket-mode) turned on. It uses two
tokens, each its own vault credential bound to `https://slack.com`: the bot
token (`xoxb-`, scopes `chat:write` plus the `*:history` scopes for the
conversations it reads) answers through the Web API, and the app-level token
(`xapp-`, scope `connections:write`) receives events. Subscribe the app to
the `message.channels`, `message.groups`, `message.im` and `message.mpim` bot
events it should hear, and invite the bot to those channels.

```sh
agent206 vault add slack-bot --kind token --destination https://slack.com
agent206 vault add slack-app --kind token --destination https://slack.com
agent206 messenger add workspace --kind slack --credential slack-bot --app-credential slack-app
agent206 messenger allow workspace <channel id>
agent206 messenger allow-sender workspace <member id>
agent206 messenger enable workspace
```

- **Ids.** Chats are conversation ids (`C…`, `G…`, or `D…` for a direct
  message) and senders are member ids (`U…`). Only direct messages are
  private; channels and group conversations need listed senders.
- **Brokered socket.** The broker asks `apps.connections.open` with the
  app-level token where to connect and connects there itself
  (`SocketScheme::SlackSocketMode`, broker protocol v8), so the ticketed
  socket URL stays in the broker, as both tokens do. The broker's limits for
  socket sessions apply (see [Discord](#discord)).
- **Acknowledgements.** Every envelope is acknowledged as it arrives, before
  any turn runs, so Slack does not deliver it again. A message delivered
  again anyway (the same channel and timestamp), including on the next
  socket after a reconnect, is answered once: the last 1024 message keys are
  kept for as long as the bot user and vault pairing stay the same.
- **What is read and sent.** Only people's plain messages are read: the
  app's own, other bots', edits, joins and every other subtype are ignored,
  as are slash commands and interactions. Slack's `&amp;`, `&lt;` and
  `&gt;` escapes are undone for the model; mentions stay in Slack's
  `<@U…>` form. Replies go through `chat.postMessage`, split at 4000
  characters, with `&`, `<` and `>` escaped so a reply cannot write a
  mention (such as `<!channel>`) or a link, and with unfurling off. Replies
  go to the conversation, not to a message's thread.
- **Delivery.** A `disconnect` request starts a new socket. Messages sent
  while the app was disconnected are not answered. A rejected token
  (`invalid_auth` and similar, from `auth.test` or `apps.connections.open`)
  or a missing app credential stops the messenger until the user acts. The
  bot user is checked (`auth.test`) before each batch.

## IRC

An IRC messenger signs in to one network as a registered services account,
over TLS, with SASL PLAIN. Register the bot's nickname with the network's
services (NickServ) first; the account name is the entry's `login` and is
also the nickname it asks for. The account password is a vault credential
bound to the server.

```sh
agent206 vault add libera-bot --kind token --destination ircs://irc.libera.chat
agent206 messenger add libera --kind irc --endpoint ircs://irc.libera.chat --login teambot --credential libera-bot
agent206 messenger allow libera '#team'
agent206 messenger allow-sender libera alice
agent206 messenger enable libera
```

- **Server.** The endpoint is `ircs://host[:port]` (port 6697 by default).
  Plain `irc://` is accepted only for a loopback address, where nothing
  crosses the network; a public server without TLS is refused rather than
  sent the password in plain text. The broker connects directly, checks the
  certificate against the bundled web roots and binds the credential to
  `ircs://host:port`.
- **Brokered sign-in.** The broker opens the connection, negotiates
  capabilities and sends the SASL payload itself
  (`SocketScheme::IrcSasl`, broker protocol v9), so the password never
  reaches the gateway. It refuses a server that does not offer SASL PLAIN or
  `account-tag`, a rejected password, or a nickname already in use, each with
  its own code; these stop the messenger until the user acts. After the
  welcome (001) the broker relays whole lines; the gateway can send only a
  single line of at most 510 bytes, and the broker answers keepalives and
  ends a silent connection after 90 seconds.
- **Identity.** Nicknames can be taken by anyone, so senders are services
  accounts, read from each message's `account` tag. Allowed senders are
  account names; a message without an account (its sender is not signed in)
  is never answered. Chats are channel names (`#team`, which need listed
  senders like any group) or, for direct messages, the sender's account
  name. Names are compared with ASCII letters folded to lowercase, and the
  CLI, the Messengers page and import fold them as they are saved. Networks
  that also treat `[]\~` as `{}|^` (RFC 1459 case mapping) are not folded
  that far, since networks with ASCII case mapping keep `[bot]` and `{bot}`
  as different accounts; a name written the other way is refused rather
  than taken for someone else.
- **Direct replies.** Each line of a reply to a direct message goes to the
  nickname that sent it only after a `WHOIS` shows that nickname is still
  signed in to the same account; otherwise nothing more is sent and the
  status says the recipient is gone. The messenger keeps running.
- **What is read and sent.** Only `PRIVMSG` text is read; CTCP requests,
  notices, the bot's own lines and lines tagged as from a bot are ignored.
  The bot joins its allowed channels on connecting and when invited to one.
  Replies are `PRIVMSG` lines with control characters removed (so no
  formatting or CTCP), short enough that the relayed line fits in 512 bytes
  whatever the channel's length, and at most 24 lines per reply (longer
  replies end with a note that they were cut). They are sent 700 ms apart,
  or 1.4 s for direct replies, whose lines each follow a `WHOIS`, so the
  server does not disconnect the bot for flooding.
- **Delivery.** Messages sent while the bot was disconnected are not
  answered; a dropped connection signs in again. A nickname in use (often
  the bot's own earlier connection, until the server notices it is gone) is
  retried with the usual backoff, with the failure shown meanwhile.

## Signal

A Signal messenger talks through a [signal-cli](https://github.com/AsamK/signal-cli)
daemon that the user installs, registers or links and runs. signal-cli holds
the account's keys and does all of Signal's encryption, so the account has
no vault credential: Agent 206 never sees or stores those keys, and the
gateway never starts, stops or updates the daemon. Run it in multi-account
mode with manual receiving, so that messages wait with Signal until the
gateway subscribes:

```sh
signal-cli daemon --socket "$XDG_RUNTIME_DIR/signal-cli/socket" --receive-mode=manual
agent206 messenger add phone --kind signal --endpoint "unix://$XDG_RUNTIME_DIR/signal-cli/socket" --login +15551234567
agent206 messenger allow phone <ACI of the person>
agent206 messenger allow-sender phone <ACI of the person>
agent206 messenger enable phone
```

- **Daemon.** The endpoint is `unix:///absolute/path` (the daemon's
  `--socket`) or `tcp://127.0.0.1:port` / `tcp://[::1]:port` (its `--tcp`);
  any other host is refused, since the JSON-RPC interface has no
  authentication and anyone who can reach it can use the account. Prefer
  the Unix socket, whose file permissions decide who else can. A TCP
  connection whose peer turns out not to be loopback is closed.
- **Account.** `login` is the account's number in `+E.164` form or its ACI.
  On connecting the gateway asks the daemon to list its accounts and stops,
  waiting for the user, when the login is not one of them; otherwise it
  subscribes to that account's messages.
- **Identity.** Senders are ACIs (the account UUID signal-cli reports once it
  has decrypted and authenticated a message), not phone numbers, which can
  be hidden or change hands. A direct chat is the sender's ACI; a group chat
  is its base64 group id. UUIDs are saved lowercase. `agent206 messenger
  list` and the Messengers page show refused chats and senders by these ids,
  so the first message from someone can be allowed from there.
- **What is read and sent.** Only text messages are answered; reactions,
  edits, deletions, receipts, typing, stories and messages synced from the
  account's other devices are ignored, and each message is handled once,
  also when signal-cli delivers it again after a reconnect. A message with
  group information is never taken for a direct chat. Replies go to the
  same ACI or group in parts of at most 2000 characters, at most eight
  (longer replies end with a note that they were cut), each sent after
  signal-cli accepted the previous one. A request the daemon does not
  answer within a minute fails; when the daemon refuses one, its reason is
  shown on one line with anything shaped like a credential removed.
- **Delivery.** With `--receive-mode=manual` messages that arrive while the
  gateway is stopped wait with Signal and are answered once it reconnects;
  a daemon receiving on its own may have taken them already. As with every
  messenger, the gateway runs it only while paired with an unlocked vault.

## Beeper

A Beeper messenger talks to [Beeper Desktop](https://www.beeper.com)'s
local API, so one entry reaches every chat network the user connected to
Beeper: WhatsApp, iMessage, Google Chat, Google Messages, Instagram,
LinkedIn and the others it bridges. Unlike a bot, it answers **as the
user**, from their own accounts on those networks, the same way as a
message typed in Beeper; the people in an allowed chat see the agent's
replies as coming from the user.

In Beeper Desktop turn on the Desktop API, then create a token under
Settings, Integrations, Approved connections, and store it in the vault
bound to the API origin:

```sh
agent206 vault add beeper-token --kind token --destination http://localhost:23373
agent206 messenger add beeper --kind beeper --credential beeper-token
agent206 messenger allow beeper '<chat id from the refused list>'
agent206 messenger enable beeper
```

- **Token.** The Desktop API token reads and sends in every chat of every
  connected account, so it stays in the vault: the broker attaches it as a
  bearer token for the bound origin only. The default endpoint is Beeper
  Desktop on the gateway's computer (`http://localhost:23373`); a Beeper
  elsewhere needs an `https://` origin.
- **What is read.** The chat list says which chats have new activity. Only
  allowed chats have their messages fetched; another chat with activity
  from someone else is listed as refused, by its id and its network and
  title, so it can be allowed from there. A direct chat (Beeper's "single")
  is answered while `allowed_users` is empty; every other chat answers only
  listed senders, whose ids are Beeper's fully qualified user ids. Only text
  from other people is answered: the user's own messages (which include the
  agent's replies), notices, media, deleted and hidden messages are not.
- **Delivery.** Beeper has no long poll, so the chat list is polled every
  two seconds. A session first marks where each allowed chat's messages
  end, so history is never answered. Each message is handled at most once
  while the gateway runs: reading resumes from the last batch the gateway
  finished, and a reply that fails to send is reported, not retried.
  Cursors are held in memory, so messages that arrive while the gateway is
  stopped are not answered. When more chats changed since the last look than
  one page of the chat list holds (100), every allowed chat is checked
  directly. An allowed chat that did not exist, or was empty, when the
  session started has everything it holds answered once it appears. The
  gateway checks which accounts the token reaches before every look, so a
  replaced token starts afresh.
- **Replies** go to the same chat in parts of at most 4000 characters, at
  most eight (longer replies end with a note that they were cut). Beeper
  converts Markdown to its rich text. End-to-end encryption to the networks
  is Beeper's: a reply travels the same way as one the user types there.
- **Account.** The status names the networks of the connected accounts
  ("Beeper: WhatsApp, Signal"). With no account connected the messenger
  retries until one is. Adding or removing an account starts afresh, without
  answering what arrived in between.

## Interface for the run engine

`a206_runtime::messengers::Host` is the boundary between a messenger and the
rest of the gateway:

```rust
fn thread_for(&self, config: &MessengerConfig, chat: &ChatBinding, title: &str)
    -> Result<Id, MessengerError>;
fn converse(&self, thread: Id, text: String)
    -> impl Future<Output = Result<Option<String>, MessengerError>> + Send;
```

`converse` currently sends `Command::Send` through `Gateway::request` and
returns the last assistant text after the user's message. A turn whose tool
call waits for approval stays pending: `Send` returns only the finished thread
(or a failed one with its `ErrorReport`), so the messenger waits in `converse`
and an approval made in the app completes the reply. The run engine has no
messenger-specific code; `docs/run-engine.md` documents this contract and
`send_stays_pending_while_an_approval_waits_and_returns_the_finished_thread`
tests it. Approval prompts are never sent to the chat, so a chat
participant cannot approve their own request.

## Not yet

- WhatsApp, Google Chat and iMessage on their own (they are reached through
  Beeper), Teams and webhooks.
- Beeper attachments, reactions and replies to a specific message, the
  experimental WebSocket event stream, and remembering cursors across
  restarts.
- Signal attachments, mentions and quoted replies.
- IRC certificate fingerprints or custom roots, SASL EXTERNAL, networks
  without `account-tag`, and channel keys.
- Slack thread replies, the Events API over HTTP (Socket Mode only) and
  conversation names in the refused list.
- Discord session resume, slash commands, threads' own channel ids beyond
  allowing them, and channel names in the refused list (only direct messages
  carry a title).
- Attachments, edits, reactions and group mention handling; only text
  messages are read.
- A persisted poll offset or sync token for exactly-once delivery across
  restarts.
- Matrix end-to-end encryption, homeserver discovery through
  `.well-known/matrix/client`, and password sign-in (the user supplies an
  access token).
