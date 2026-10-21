# Gateway enrollment and RustyClaw security parity

This is the required implementation contract, not a claim that enrollment is
complete. Gateway lifecycle controls are implemented separately. The gateway must
not advertise security readiness or enable vault use merely because it is running.

Implemented now: a separate vault broker process (`a206-broker`, started by
`agent206 vault`) owns the decrypted vault. Trusted user surfaces reach it directly
over a mode-0600 socket in the mode-0700 `vault/` profile directory, so the
gateway never relays passwords, codes or values. The gateway connects with the
`Gateway` role, which can only ask the broker to send a provider request to an
origin the user bound to that credential; it never receives the value. The CLI and shared desktop/web Security page are the trusted surfaces. CLI and desktop clients use the shared
`a206_runtime::vault` operations; web clients use the broker-authenticated encrypted
channel described in [web-vault-setup.md](web-vault-setup.md). The TUI refuses vault
controls before mounting secret-entry state because its renderer cannot keep
password input private; use the desktop or `agent206 vault` in a trusted terminal.
Agent approvals are the next gate.

Gateway pairing: the `Gateway` role must present a pairing before any provider
request. Only a stepped-up user connection can mint one (`agent206 vault unlock`
and the Security page's unlock do so when the gateway is running; `agent206 vault
pair` and the page's Pair gateway action do so explicitly).
The trusted client hands it straight to the local gateway over the gateway
socket; the gateway verifies it with the broker, keeps it only in memory and
never journals, logs or returns it. The broker binds a pairing to the first
process that proves it, identified by the kernel's socket credentials, so a
copied token is refused to every other process; a pairing whose holder the
kernel cannot identify is refused rather than trusted. The holder's first
connection anchors the pairing: the gateway keeps it open, and when it closes
(the gateway exited) the pairing is void, so a pid later reused by another
process finds nothing to claim. Minting a new pairing also stops every session
that proved the old one. If another process wins
that race the gateway's verification fails and `vault pair` reports it. A new
pairing replaces the old one, and any lock voids it, so after a relock or a
gateway restart the user pairs again. Both the broker and gateway sockets also
refuse connections whose kernel-reported user differs from their own, behind
the 0600 socket modes.

Limits of today's boundary: any process running as the same OS user can open the
socket and claim either role (only the first claimant can use a pairing). Step-up (password, or a TOTP code when enrolled)
guards adding, revealing, overwriting and deleting credentials and policy,
destination and agent-access changes, but a same-user attacker that already
holds the password is not stopped. Pairing stops such a process from choosing
its own credential and URL at the broker, but the gateway socket itself is not
yet authenticated: a same-user process can still ask the gateway for ordinary
model work (catalogs and completions for configured providers), which the
gateway then performs with the paired broker. Locking the vault, replacing
the pairing or closing its anchor cancels provider requests already in flight. Agent
execution must therefore keep the profile directory outside its OS sandbox, as
the macOS backend isolation already does. Remote vault access over SSH is not
wired yet; the broker is local to the gateway host.

## Source baseline

Inspect code, not only prose. RustyClaw reference:
`481a428cb7a3b82d1a5e556ffef40e3dd9f52ce3`.

- [Security layers](https://github.com/rexlunae/RustyClaw/blob/481a428cb7a3b82d1a5e556ffef40e3dd9f52ce3/docs/SECURITY.md)
- [Vault implementation](https://github.com/rexlunae/RustyClaw/blob/481a428cb7a3b82d1a5e556ffef40e3dd9f52ce3/crates/rustyclaw-core/src/secrets/vault.rs)
- [Policies and credential metadata](https://github.com/rexlunae/RustyClaw/blob/481a428cb7a3b82d1a5e556ffef40e3dd9f52ce3/crates/rustyclaw-core/src/secrets/types.rs)
- [TOTP implementation](https://github.com/rexlunae/RustyClaw/blob/481a428cb7a3b82d1a5e556ffef40e3dd9f52ce3/crates/rustyclaw-core/src/secrets/vault_ext.rs)
- [Authentication rate limits](https://github.com/rexlunae/RustyClaw/blob/481a428cb7a3b82d1a5e556ffef40e3dd9f52ce3/crates/rustyclaw-gateway/src/auth.rs)
- [Step-up secret viewing](https://github.com/rexlunae/RustyClaw/blob/481a428cb7a3b82d1a5e556ffef40e3dd9f52ce3/crates/rustyclaw-gateway/src/secrets_handler.rs)

The upstream security document describes AES-256/Argon2id, but its vault code uses
SecureStore. The selected SecureStore 0.100.3 source implements AES-128-CBC with
HMAC-SHA1 and password derivation through PBKDF2-HMAC-SHA1 (256,000 rounds).
Do not claim the documented algorithms as implementation evidence. Retain this
format as a legacy import reader; design/review a versioned modern authenticated
encryption envelope and memory-hard password KDF for new vaults, with explicit
migration and rollback. Never rewrite or replace an unreadable source vault.

## Required setup flow

1. Select a local gateway/profile or a verified SSH host. Pair unknown host keys
   through an explicit trusted user flow; never turn off host verification.
2. Establish the isolated vault broker and authenticated encrypted UI/broker
   channel before collecting any password, credential, recovery code or TOTP seed.
   The gateway relays ciphertext/opaque handles. It must not decrypt these values.
   Origin-scoped browser bearer authentication alone does not provide this channel.
3. Create a new encrypted vault or select a supported existing one. Support both
   password-based and protected key-file workflows, password confirmation,
   atomic private writes, corruption detection and explicit key rotation.
4. Offer authenticator enrollment. Generate the seed inside the broker, display it
   only in the trusted user surface as a QR/manual key, and require a valid code
   before committing. Cancel/expiry discards the pending seed. Never put it in
   ordinary snapshots, events, URLs, logs, plugin memory or model context.
5. Enforce enrolled vault state on every new connection/unlock, independently of
   editable TOML flags. Use a bounded accepted TOTP window, replay protection and
   shared rate limits across reconnects. Separate enrollment from ordinary auth.
6. Review default agent access, credential policies, destination restrictions and
   tool/sandbox settings. Least privilege is the default; no secret is granted
   merely because a provider account or import was configured.
7. Optionally import supported sources through the reviewed flow in imports.md.
   Reauthenticate legacy vaults and preserve every restrictive policy.
8. Verify a fresh authenticated connection, lock/unlock and cancellation before
   recording setup completion. Interrupted enrollment must resume or safely roll
   back. Startup must fail closed on inconsistent security state.

Desktop is the primary setup UI; web and terminal use the same state machine.
Remote setup operates through ordinary verified SSH without copying plaintext
vaults or putting passwords in argv/environment. Do not silently weaken a
protection on a renderer or platform that cannot enforce it.

## Parity matrix and acceptance gates

| Requirement | RustyClaw behavior to retain | Agent 206 status / acceptance |
| --- | --- | --- |
| Vault encryption | Password or owner-only key file, atomic encrypted writes | Legacy password-based library tested; broker v11 holds reviewed file identities and verifies password/enrolled TOTP on a private copy before no-replace publication. Shared GUI and CLI use native/encrypted schema-v1 review/apply/cancel; receipt failures are explicit partial results. Remote SSH controls, key-file import and modern envelope pending |
| Process/channel separation | Secrets outside agent context | Separate broker process with role-limited bounded MessagePack IPC, tested; gateway role is value-free. Same-user socket access and remote SSH channel are open items (see above) |
| TOTP | Vault-held seed is authoritative; 6 digits, 30-second steps, ±1 step | Library: seed generated in memory and committed only after a valid code, enrollment expiry, ±1 step, persisted replay protection, drift diagnosis, code-gated removal; seed stored under RustyClaw's key so imported vaults stay enrolled; tested. Standalone CLI challenges enrolled vaults. Broker enrollment from the CLI and the shared Security page show the manual key and otpauth link as text with a locally generated QR code and an expandable manual-key alternative and reports enrollment only after the broker confirms a code. Broker connect challenge and recovery codes pending |
| Unlock and step-up surfaces | Password unlock, TOTP challenge when enrolled, step-up before sensitive operations | CLI and desktop/web Security page (gateway local to the client or web server) send the same broker `Unlock`/`StepUp` requests; the TOTP challenge follows the broker's `TotpRequired`, never a client flag; secrets are cleared from fields on submit and kept out of status text and logs. TUI vault controls are unavailable; remote-gateway GUI controls remain pending (CLI on that host) |
| Authentication throttling | Shared 3-failure / 30-second lockout over a 60-second failure window | Library throttle keyed by principal, not connection, tested; failures during lockout neither shorten nor extend it. Broker-wide enforcement covers connections and encrypted sessions, tested |
| Per-credential policy | Always, WithApproval, WithAuth, SkillOnly, TriggerOnly | All variants, RustyClaw JSON shape, unknown variants rejected; evaluated deny-by-default and tested. Approval/step-up facts must come from the broker's trusted surface, pending |
| Disabled credentials | Disabled entry cannot be used | Enforced for agent use and tested; missing flag reads as RustyClaw's default; owner reveal still allowed |
| Global agent switch | User access distinct from agent access | Held in the encrypted vault, off by default. Deliberate tightening: it is a precondition and never satisfies WithApproval (RustyClaw let it). CLI and shared desktop/web Security changes require broker step-up; the web panel requires broker v18 and independently verified identity. TUI vault controls remain unavailable |
| Credential kinds | API/token, login, SSH key, passkey, form, payment and secure note | All kinds stored in RustyClaw's `cred:`/`val:` layout with kind/value checks and stale-value removal, tested; unknown kinds fail to parse and stay unavailable |
| SSH key use | Keys stored as credentials | `vault add --kind ssh-key` generates an Ed25519 key or stores a decrypted Ed25519 or ECDSA (P-256, P-384) key file; RSA keys are refused, since the `rsa` crate's private-key operations leak timing (RUSTSEC-2023-0071). Broker v16 `SshAgent` (gateway role) returns the public key and signs only a public-key authentication request for that key (RFC 4252, or OpenSSH's host-bound method), for an exact `ssh://host:port` the key is bound to; the private key never leaves the broker. Tested in the vault and over the broker socket. No SSH client uses it yet |
| Protected namespaces/paths | Internal/provider/service records cannot be read through generic bypasses | Private state and backend isolation tested; all tool/extension/import paths require explicit vault exclusion |
| Scoped use | Skill/trigger and destination restrictions, explicit approvals | Existing narrowed tool/model grants; credential-use approval UI pending (configured providers already use exact-origin broker binding) |
| Reveal authentication | Per-connection short step-up window (120 seconds upstream) | 120-second window gates owner reveal, separate from agent use, expiry/revoke tested; lock and reconnect revoke broker authentication, tested |
| Prompt and leak defenses | Prompt-injection validation and outbound secret leak detection | Full boundary scanning/attack corpus pending |
| OS sandbox | Strongest supported OS boundary, vault path excluded | macOS (Seatbelt) and Linux (bubblewrap + Landlock) isolation of external backends, MCP servers and native `execute_command` from one shared profile, tested; the WASM worker confined by the same profile on both, tested; Windows and full RustyClaw tool parity pending |
| Audit and redaction | Log decisions without secret material | Redacted secret type exists; broker audit and adversarial no-secret leakage tests pending |

Retain security semantics, not unsafe helpers or vulnerabilities. Unsupported
policy variants must deny access. Agents never approve themselves, change these
policies, enroll factors, export keys or unlock the vault.

Before enabling authenticated providers/browser injection, test wrong credentials,
malformed/corrupted vaults, failed writes, cancelled enrollment, invalid/replayed
codes, clock drift, lockout, reconnect/expiry, cross-session approvals, destination
confusion, path/symlink escape, policy migration and secret absence from all agent
channels. A completed wizard alone is not evidence of security parity.

## Browser enrollment channel (2026-10-16)

The Security page now shares its create/unlock/lock, step-up, TOTP enrollment and
pairing panel across desktop and web. TUI vault controls remain unavailable.
For a gateway local to the web server,
web requests use a Noise NK channel terminating in the browser and separate
broker. The user enters the broker's public key from `vault channel-key` in a
trusted terminal; the UI verifies possession before showing password fields.
The HTTP bridge carries ciphertext, with bounded, browser-owner-bound sessions.
Cancellation now explicitly discards broker-held pending enrollment. See
[web-vault-setup.md](web-vault-setup.md) for exact procedures and limitations.
SSH vault controls,
key-file workflows and modern at-rest encryption remain pending. Browser-delivered
code and the serving origin remain trusted, as does the native client executable.

## Shared credential controls

The Security page offers explicit enable/disable actions for global agent credential
access when the unlocked broker reports its current state. Both actions use the
existing broker-owned step-up check; a stale authentication window asks again.
The setting is saved in the encrypted vault. It never replaces per-credential
approval, destination checks or disabled flags, and does not change user access.
Broker v18 adds this operation to the independently authenticated encrypted panel
channel. Older web peers are refused before the request is sealed. Real encrypted
broker tests cover both directions, cross-connection step-up denial, lock revocation
and persisted state after reopening. Renderer tests cover known/unknown action
visibility. Release-web testing in a fresh empty synthetic vault verified enable,
reconnect/step-up, incorrect-password refusal, authenticated disable, lock hiding
the controls and the saved off state after reopening; the gateway stayed stopped.
Native desktop interaction remains unverified and the TUI guard is unchanged.

Security uses the private broker session to read typed metadata and add API keys
(broker v17). Metadata/policy definitions live in `a206-vault-types`, a pure data
crate shared by the vault and all renderers; the web build does not link vault
storage. Metadata pages are bounded by entry count and encoded bytes, with an
exclusive sorted-name cursor. Policies/scopes are never truncated to fit a page;
an oversized single entry fails explicitly. Legacy entries are counted only.

Key creation shares the CLI's locked-vault, step-up, name, destination and atomic
no-overwrite checks. The new operation accepts only a single bounded key and
forces `ApiKey`/`WithApproval`; no policy or agent-access field is caller-controlled.
Destinations must be exact origins, without paths, queries or fragments. No value
read/reveal operation is added to the UI channel. Older/unknown peers are refused
before a new secret-bearing request is sealed; no plaintext fallback exists.
Native and encrypted broker integration tests cover authentication/role denials,
overwrite refusal, origin validation, byte/count bounds and metadata without values.
Shared credential rows also offer Enable/Disable through the same broker step-up.
The requested name and target flag are retained across authentication; the panel
reloads broker metadata after confirmation instead of optimistically changing a
row. Broker v19 adds only this bounded encrypted request, mapped to the existing
native `UpdateCredential` operation with policy and destinations absent. Older web
peers are refused before sending it. Tests cover both directions, connection-owned
authentication, lock revocation, persistence, the unchanged private value and other
metadata, an untouched second entry and missing-name refusal. Release-web testing
in a disposable synthetic vault verified disable, reconnect/step-up, cancel without
change, authenticated re-enable and disabled-state persistence across lock/reopen,
with policy and origin retained and global access off. Native desktop interaction
remains unverified; the TUI guard is unchanged.

Other entry kinds, SSH-key/form/payment entry, agent-use approvals and remote vault
controls remain pending; the CLI supports SSH-key and other entry kinds and policy
edits. Form and payment entry remain unavailable in both clients.

Credential rows also offer Edit destinations. The editor replaces the complete
list of permitted origins, including an empty list that prevents brokered use.
The exact credential name and list are retained through step-up authentication;
policy, enabled state and private value stay unchanged. Broker v20 adds this
encrypted-panel operation, mapped to the existing native `UpdateCredential`.
Panel validation rejects paths, queries and fragments instead of silently
reducing a URL to an origin; the older CLI normalization remains compatible.
Old web peers are refused before sealing. Owning broker tests cover invalid-input
atomicity, empty/multiple origins, authentication, lock revocation, persistence,
isolation from another entry and preservation of private values and restrictions.
Release-web checks in a disposable synthetic vault verified rejection without a
write, multiple-origin replacement, reconnect/step-up with the exact target/list,
cancel without a change, an authenticated empty list and lock/reopen persistence.
Approval policy, enabled state and global access remained unchanged; the gateway
stayed stopped. No real credentials, accounts or model calls were used.
Native desktop interaction remains unverified and TUI vault controls stay blocked.

Shared credential rows also edit all five policy variants through broker step-up
(both policy editors require broker v21). Skill/trigger identifiers are separate fields,
preserved exactly with no trimming or splitting; empty scope lists deny every use.
Each scope draft survives selector changes, rejection and authentication cancel.
Only a confirmed broker write clears the editor and reloads metadata. The broker
limits panel policies to 64 nonempty identifiers of at most 256 bytes without
control characters. It also refuses policy expansions whose metadata would exceed a bounded
single-entry page, under the same vault lock before committing the change. An
already oversized entry may shrink even if it still exceeds that panel limit.
Existing native flag and destination edits can still restrict oversized imported
entries; this policy-size check does not block those recovery operations.
The desktop policy editor checks the negotiated v21 metadata-size capability
before sending; this is not an unknown-opcode guard and leaves ordinary CLI/native
operations compatible. Native `UpdateCredential` remains the opcode for policy changes; disabled state,
destinations and private values are retained. No agent approvals or authentication
facts are created by saving a policy. Credential-use approval and native desktop
interaction acceptance remain pending; TUI vault controls remain unavailable.
