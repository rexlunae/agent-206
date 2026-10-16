# Gateway enrollment and RustyClaw security parity

This is the required implementation contract, not a claim that enrollment is
complete. Gateway lifecycle controls are implemented separately. The gateway must
not advertise security readiness or enable vault use merely because it is running.

Implemented now: a separate vault broker process (`a206-broker`, started by
`agent206 vault`) owns the decrypted vault. Trusted user surfaces reach it directly
over a mode-0600 socket in the mode-0700 `vault/` profile directory, so the
gateway never relays passwords, codes or values. The gateway connects with the
`Gateway` role, which can only ask the broker to send a provider request to an
origin the user bound to that credential; it never receives the value. The CLI
is the first trusted surface. GUI enrollment and agent approvals are the next gate.

Gateway pairing: the `Gateway` role must present a pairing before any provider
request. Only a stepped-up user connection can mint one (`agent206 vault unlock`
does so when the gateway is running, `agent206 vault pair` does so explicitly).
The trusted client hands it straight to the local gateway over the gateway
socket; the gateway verifies it with the broker, keeps it only in memory and
never journals, logs or returns it. A new pairing replaces the old one, and any
lock voids it, so after a relock or a gateway restart the user pairs again.

Limits of today's boundary: any process running as the same OS user can open the
socket and claim either role. Step-up (password, or a TOTP code when enrolled)
guards adding, revealing, overwriting and deleting credentials and policy,
destination and agent-access changes, but a same-user attacker that already
holds the password is not stopped. Pairing stops such a process from choosing
its own credential and URL at the broker, but the gateway socket itself is not
yet authenticated: a same-user process can still ask the gateway for ordinary
model work (catalogs and completions for configured providers), which the
gateway then performs with the paired broker. Locking the vault cancels
provider requests already in flight. Agent
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
| Vault encryption | Password or owner-only key file, atomic encrypted writes | Legacy password-based library tested; key-file import, modern envelope and broker pending |
| Process/channel separation | Secrets outside agent context | Separate broker process with role-limited bounded MessagePack IPC, tested; gateway role is value-free. Same-user socket access and remote SSH channel are open items (see above) |
| TOTP | Vault-held seed is authoritative; 6 digits, 30-second steps, ±1 step | Library: seed generated in memory and committed only after a valid code, enrollment expiry, ±1 step, persisted replay protection, drift diagnosis, code-gated removal; seed stored under RustyClaw's key so imported vaults stay enrolled; tested. Standalone CLI challenges enrolled vaults. Broker connect challenge, GUI enrollment and recovery codes pending |
| Authentication throttling | Shared 3-failure / 30-second lockout over a 60-second failure window | Library throttle keyed by principal, not connection, tested; failures during lockout neither shorten nor extend it. Broker-wide wiring pending (the standalone CLI is one attempt per process) |
| Per-credential policy | Always, WithApproval, WithAuth, SkillOnly, TriggerOnly | All variants, RustyClaw JSON shape, unknown variants rejected; evaluated deny-by-default and tested. Approval/step-up facts must come from the broker's trusted surface, pending |
| Disabled credentials | Disabled entry cannot be used | Enforced for agent use and tested; missing flag reads as RustyClaw's default; owner reveal still allowed |
| Global agent switch | User access distinct from agent access | Held in the encrypted vault, off by default. Deliberate tightening: it is a precondition and never satisfies WithApproval (RustyClaw let it). Broker control surface pending |
| Credential kinds | API/token, login, SSH key, passkey, form, payment and secure note | All kinds stored in RustyClaw's `cred:`/`val:` layout with kind/value checks and stale-value removal, tested; unknown kinds fail to parse and stay unavailable |
| Protected namespaces/paths | Internal/provider/service records cannot be read through generic bypasses | Private state and backend isolation tested; all tool/extension/import paths require explicit vault exclusion |
| Scoped use | Skill/trigger and destination restrictions, explicit approvals | Existing narrowed tool/model grants; credential-use binding pending |
| Reveal authentication | Per-connection short step-up window (120 seconds upstream) | 120-second window gates owner reveal, separate from agent use, expiry/revoke tested; lock/reconnect revocation wiring in the broker pending |
| Prompt and leak defenses | Prompt-injection validation and outbound secret leak detection | Full boundary scanning/attack corpus pending |
| OS sandbox | Strongest supported OS boundary, vault path excluded | macOS external backend isolation tested; other platforms and full RustyClaw tool parity pending |
| Audit and redaction | Log decisions without secret material | Redacted secret type exists; broker audit and adversarial no-secret leakage tests pending |

Retain security semantics, not unsafe helpers or vulnerabilities. Unsupported
policy variants must deny access. Agents never approve themselves, change these
policies, enroll factors, export keys or unlock the vault.

Before enabling authenticated providers/browser injection, test wrong credentials,
malformed/corrupted vaults, failed writes, cancelled enrollment, invalid/replayed
codes, clock drift, lockout, reconnect/expiry, cross-session approvals, destination
confusion, path/symlink escape, policy migration and secret absence from all agent
channels. A completed wizard alone is not evidence of security parity.
