# Enroll a gateway from the web client

Choose the local gateway in first-time setup. The client then opens Security.
The Gateway page also links to Security and provides Start, Stop, Restart and
Reconnect through the existing lifecycle controller. These controls do not
unlock or enroll the vault, and polling does not undo a deliberate Stop.

## Verify before entering a password

In a trusted terminal on the gateway host, run:

```sh
agent206 --data-dir <profile-directory> vault channel-key
```

Use the **same profile directory** as `agent206 web`. Paste the printed public
key into Security's **Broker public key** field and select **Verify and open
vault setup**. This is a public identity key, not a vault key or password. Do not
obtain it from an untrusted web page. The browser proves that the broker possesses
the corresponding private key before showing any password inputs. A broker
restart generates a new identity: verify it again. Keys are not silently updated
or accepted from the relay's API. An older broker must be stopped explicitly with
`agent206 vault stop` before retrying with the current executable.

## Create, enroll and pair

1. Create a vault with a confirmed password, or unlock the existing password
   vault. Password and code fields are cleared when submitted or cancelled.
2. choose **Set up two-factor**, scan the QR code with an authenticator,
   or expand **Can't scan? Enter a setup key instead**, then enter its first code. The broker commits enrollment only after verification.
   Cancel discards the pending seed in the broker. Closing the panel also closes
   its connection; a lost browser is bounded by the relay's five-minute idle expiry.
3. Start the gateway, then choose **Pair gateway**. Unlock automatically pairs a
   gateway that is already running. Pairing failure is reported separately from
   successful unlock. Pairing uses an opaque token, never a credential value.
4. Lock and unlock to verify the workflow. An enrolled vault requires its
   authenticator regardless of editable TOML flags. A code works only once; use
   the next code for a new step-up. Locking voids the pairing and every step-up;
   after a gateway restart, pair it again.

The panel reports encryption, enrolled TOTP, agent access and pairing from the
broker's actual state. Global agent access remains off for new vaults. Setup does
not grant tools, activate providers, or authorize agents to spend credentials.

## Credential metadata and API keys

Once unlocked, **Credentials** shows typed metadata only: kind, policy and its
skill/trigger scopes, permitted destinations and disabled state. Refresh starts
at the first page; Next page follows the live name cursor. Pages contain at most
50 entries and 48 KiB encoded. An entry larger than that limit produces an error
instead of a shortened policy. Untyped legacy values are counted, never read.

**Add API key** requires fresh broker authentication (TOTP when enrolled).
Choose a unique name and label, enter exact HTTPS origins separated by spaces,
and submit the key. Loopback HTTP is allowed for local services. Paths, queries
and fragments are refused. Empty destinations prevent brokered use. Keys are
saved encrypted with approval required for each agent use; global agent access
does not change. Existing entries cannot be overwritten. If a save is not
confirmed, refresh metadata before retrying: the write may have committed.

These controls require broker v17 and refuse older brokers before sending the
new requests. Generic setup still works with v10, and migration with v11.
Global agent credential access has separate enable/disable actions when unlocked.
Both require recent authentication on this encrypted connection and broker v18.
Enabling access does not approve an agent operation or override any credential's
policy, destinations or disabled state. Locked or unknown state offers no action.

Reveal, legacy adoption, deletion
and TOTP removal remain trusted CLI operations. The CLI also offers token, login,
passkey, SSH-key, note and other entry kinds; this panel adds API keys only. Form
and payment entry are still unavailable in both clients.

Each typed metadata row offers **Disable** or **Enable**, naming its credential.
These actions require recent authentication and broker v19. They change only the
disabled flag; policy, destinations, label and private value are retained. Enabling
a credential does not enable global agent access or satisfy per-use approval.
After confirmation the panel reloads metadata. If confirmation fails, refresh
before retrying because a lost response may follow a completed write. Controls
are disabled while another authentication or credential-entry flow is active.

## Optional migration

Before creating a new empty vault, existing RustyClaw users can instead preview:

```sh
agent206 --data-dir <profile-directory> import rustyclaw <source-directory>
```

Review the preview and use `--apply` only for the categories you intend to import.
The command runs on the gateway host and uses the isolated broker for encrypted
vault adoption. It never replaces an existing vault. See [imports.md](imports.md)
for policy preservation, supported categories and partial-failure behavior. Security
and Get started also offer reviewed whole-vault migration, described below;
the Import settings page separately reviews selected connection settings.
Adapters for other tools remain planned. Neither flow invokes foreign commands
or imports grants implicitly.

## Channel and trust boundary

The common Rust `a206-vault-channel` crate uses Snow 0.10's
`Noise_NK_25519_ChaChaPoly_SHA256`, with a versioned Agent 206 prologue. The broker
holds its ephemeral static private key; the browser initiator pins the public key
obtained in the trusted terminal. The ciphertext relay holds neither transport keys
nor decrypted setup requests/replies. The browser still has to trust the application
code and its serving origin: encryption does not make compromised JavaScript safe.
Loopback authorization and remote HTTPS/TOTP sign-in retain their existing guards.

The authenticated `/api/vault` endpoint remains limited to public status and
locking. Encrypted setup uses `/api/vault/channel` and requires broker protocol
10 or newer. Versions 8 and 9 provide messenger transports, not encrypted setup;
the client refuses setup against those versions without sending a password or
setup opcode. Pairing status from older brokers is unknown because their status
counts an unredeemed pairing token as a paired gateway.

Only a typed setup subset travels inside the encrypted channel: status,
create/unlock, lock, step-up, begin/confirm/cancel TOTP, and minting a gateway
pairing. Reveal, export, grants, provider requests and shutdown are absent. A new
broker `Relay` role rejects plaintext privileged requests and allows just one
handshake per socket, so a relay cannot replace a channel and inherit its step-up.
MessagePack plaintext buffers and UI secret wrappers zeroize on drop; Debug
redacts secret fields. Ordinary snapshots and events contain no seeds/passwords.

Gateway pairing is a separate capability transfer: after minting it through the
encrypted channel, the browser sends the short-lived pairing token through the
existing authenticated command bridge to the local gateway. That bridge sees
the token and is trusted for this operation; this is not an end-to-end encrypted
browser-to-gateway transfer. The token is redacted, never persisted, and bound by
the broker to the first gateway process that proves it. Passwords, authenticator
seeds and credential values do not follow that command path. Do not use this
setup with a command bridge you do not trust to deliver pairing capabilities.

Stateful Noise nonces reject replay, reordering, tampering and cross-session
substitution. A lost response voids the browser channel and is never retried as a
mutation. The UI refreshes actual status on a new channel to show what succeeded.
The relay binds each opaque session to its authenticated browser bearer, permits
at most 32 live channels, one queued exchange per channel, a 30-second exchange
timeout and a five-minute idle timeout. Broker sockets preserve per-connection
step-up and enrollment state. Noise messages are bounded at 65,535 bytes. TLS and
SSH still provide transport reliability; no packet-loss layer is invented.

This implements initial browser setup on the loopback web interface for a gateway
local to the web server. HTTPS startup already requires an enrolled authenticator;
HTTPS clients can subsequently unlock and manage that vault, but cannot bootstrap
its initial creation or first TOTP enrollment. Vault setup for a gateway reached by the web
server over SSH remains unavailable and fails closed. Key-file workflows, modern
vault-envelope migration, recovery codes and full RustyClaw parity remain listed
in [security-setup.md](security-setup.md); this flow retains the existing encrypted
SecureStore storage rather than claiming new at-rest cryptography.

Acceptance includes Rust tests over the real broker and HTTP router for encrypted
creation/enrollment, cancellation, TOTP replay, role restrictions, channel replay,
key substitution and session ownership, plus renderer builds and a disposable
profile browser walkthrough.

QR codes are generated locally in Rust with `qrcode`, using inline SVG geometry
and a four-module quiet zone. The raw seed/URI is never an image URL, attribute,
remote QR request or persisted asset. Fixed black-on-white QR contrast is
intentional in every theme; surrounding controls remain themed fc-dioxus
components. The `Disclosure` manual-key alternative is expanded automatically
for TUI, where SVG is unavailable, or if QR encoding fails. An independent Rust
`rqrr` decoder tests the actual rendered geometry against the complete URI.
The pending QR geometry is sensitive and is discarded with the enrollment.

## Import an existing password vault

After trusted-key verification, open **Import a RustyClaw vault** on Security or
Get started while this profile has no vault. Select its absolute encrypted file
path on the web server's broker host, then **Preview vault import**. Check the
path, byte count and source SHA-256 before entering its original password and
current authenticator code. This is a whole-vault operation; existing vaults are
never replaced or merged, and key-file vaults are not supported yet.

The broker holds the review on this private connection and rechecks source bytes
before every attempt, including TOTP retries. Its policies, disabled flags,
agent-access setting and authenticator enrollment are retained; task/tool grants
and provider accounts are unchanged. Source bytes stay intact, while successful
TOTP verification advances the target copy's replay state. Inputs clear on
submission; a challenge asks you to reenter the password and code.

The result shows actual vault/pairing state and an owner-only TOML receipt path.
A receipt or pairing failure is shown beside the confirmed copy. If the request
was interrupted, check vault status before retrying: a sent copy may have
committed. Cancel clears an unapplied review; it is not rollback. Broker v11 is
required for migration, with no fallback to an older unreviewed import. Existing
unlock/enrollment controls still work with v10. The relay reports the negotiated
version of the channel's socket; missing or older version metadata refuses
migration before sending a reviewed request, with instructions to stop the old
broker and retry. Remote SSH migration,
key-file migration and rollback remain pending.

The equivalent terminal flow on the broker host is `agent206 vault import FILE`
for preview and `agent206 vault import FILE --apply` for a fresh review and
authenticated import. Neither starts a gateway.

### Edit permitted destinations

Choose Edit destinations on a credential row. Review the credential name and
replace its complete list with exact origins, separated by spaces. Paths,
queries and fragments are refused. An empty list prevents brokered use. Save
destinations asks for fresh authentication when required and retains the exact
requested name/list for that challenge; Cancel authentication makes no change.
Rejected saves keep the nonsecret draft available for correction. Refresh and
page navigation wait until the edit is saved or explicitly cancelled. Locking or
disconnecting clears the draft along with the rest of the panel state.
After a confirmed save, the panel reloads metadata. If confirmation fails, refresh
credentials before retrying. Policy, enabled state, private value and global agent
access are unchanged. This encrypted operation requires broker v20; other panel
features keep their existing minimum versions. TUI vault controls stay unavailable.

### Edit a credential policy

Choose Edit policy on the credential row. Select approval per use, fresh
authentication, no per-use condition, listed skills only or listed triggers only.
For scoped policies, add or remove individual identifiers. They are case-sensitive
and are never trimmed or split; an empty list permits no uses. The panel accepts
up to 64 identifiers of at most 256 bytes each, without empty identifiers or control
characters. Stored lists above that count are retained with CLI guidance rather
than mounting unbounded inputs or truncating the rule. Changing the selector and
switching back preserves each scope draft.

Save policy uses broker-owned authentication and shows the exact credential and
proposed rule when authentication is required. A rejected save or authentication
cancel keeps the nonsecret draft. Refresh and paging wait until Save confirms or
you cancel the edit. An uncertain result asks you to cancel and refresh before
retrying. Locking or disconnecting discards the draft.

Only policy changes; enabled state, permitted destinations, global agent access
and the private value are retained. Saving a policy neither approves a use nor
authenticates an agent. Credential-use approval remains unavailable. Broker v21
adds the encrypted request, with no fallback to an older peer. The desktop policy
editor also requires v21's metadata-size capability before sending the existing
native update opcode; other native/CLI operations keep their prior compatibility. Native updates
also refuse a resulting metadata entry too large for a bounded page before saving
any change, so scope expansion cannot strand the entry outside this panel.
