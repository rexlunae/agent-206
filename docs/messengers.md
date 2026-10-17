# Messengers

A messenger connects a chat service account to Agent 206 so that people can
talk to a thread from that service. Telegram, Matrix and Discord are implemented;
RustyClaw's other channels are listed in
[parity.md](parity.md#messengers--channels).

## Configuration

Messengers are `[[messengers]]` entries in the profile settings (TOML through
`agent206 settings`, the `agent206 messenger` commands, or the Messengers page
of the desktop, web and terminal apps):

```toml
[[messengers]]
name = "team"                 # lowercase letters, digits, single hyphens
kind = "telegram"             # or "matrix" or "discord"
enabled = false               # off until the user turns it on
credential = "team-token"     # vault credential name, never the token
# endpoint = "https://..."    # an origin; defaults to the official API,
                              # required for Matrix (the homeserver)
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
  their senders must be listed; a direct message is private.
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
  credential gets its own session.

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

- Slack, IRC, Signal, WhatsApp, Google Chat, Teams, iMessage and webhooks.
  The broker's socket sessions already support Slack's Socket Mode
  (`SocketScheme::SlackSocketMode`); the Slack adapter is next.
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
