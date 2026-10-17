# Messengers

A messenger connects a chat service account to Agent 206 so that people can
talk to a thread from that service. Telegram is implemented; RustyClaw's other
channels are listed in [parity.md](parity.md#messengers--channels).

## Configuration

Messengers are `[[messengers]]` entries in the profile settings (TOML through
`agent206 settings`, the `agent206 messenger` commands, or the Messengers page
of the desktop, web and terminal apps):

```toml
[[messengers]]
name = "team"                 # lowercase letters, digits, single hyphens
kind = "telegram"
enabled = false               # off until the user turns it on
credential = "team-token"     # vault credential name, never the token
# endpoint = "https://..."    # an origin; defaults to the official API
allowed_chats = ["-1001234"]  # chats whose messages reach a thread
allowed_users = ["5551234"]   # senders accepted; groups need it
model = { provider = "<provider id>", model = "<id from the provider's API>" }
mode = "chat"                 # mode of new chat threads
```

The gateway validates every save: unique names, an HTTPS (or loopback HTTP)
origin without a path, credentials, query or fragment, bounded chat and
sender lists and printable ids. Entries start disabled, allow no chat and have no model, so
adding one, including through import, connects nothing and answers nobody.

## Security

- **Tokens stay in the vault.** The gateway asks the vault broker to make
  each Bot API call (`AuthScheme::TelegramBot`, broker protocol v6). The broker
  checks that the credential is bound to the endpoint's exact origin and
  places the token in the request path itself. Plain HTTP (loopback only)
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
returns the last assistant text after the user's message. When the run engine
gains tool calls and approvals, a turn that waits for approval should stay
pending: the messenger waits for `converse` to return, so an approval made in
the app completes the reply. The run engine needs no messenger-specific code
as long as `Send` returns the finished thread (or a failed one with its
`ErrorReport`). Approval prompts are never sent to the chat, so a chat
participant cannot approve their own request.

## Not yet

- Discord, Slack, Matrix, IRC, Signal, WhatsApp, Google Chat, Teams, iMessage
  and webhooks. Discord's gateway and Slack's Socket Mode need a brokered
  WebSocket connection, which the broker does not offer yet.
- Attachments, edits, reactions and group mention handling; only text
  messages are read.
- A persisted poll offset for exactly-once delivery across restarts.
