//! The broker process: owns the decrypted vault, enforces authentication,
//! throttling and step-up, and performs destination-bound requests so the
//! gateway never receives a credential value.
use crate::proto::*;
use a206_protocol::{Decoder, Response, encode};
use a206_vault::{
    AuthThrottle, CredentialMeta, Error as VaultError, ImportCheck, PendingTotp, Secret,
    StepUpWindow, TotpOutcome, Vault, unix_now,
};
use futures_util::StreamExt;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

mod credentials;
mod import;
mod irc;
mod socket;

const MAX_NAME: usize = a206_vault_channel::MAX_CREDENTIAL_NAME_BYTES;
const MAX_DESTINATIONS: usize = 16;

struct Shared {
    root: std::path::PathBuf,
    identity: a206_vault_channel::Identity,
    vault: Mutex<Vault>,
    throttle: AuthThrottle<()>,
    stop: CancellationToken,
    /// For HTTPS, through the system proxy if one is configured: a proxy
    /// sees only the tunnelled, encrypted request.
    http: reqwest::Client,
    /// For plain HTTP, which is only ever loopback: never through a proxy,
    /// which would see the credential in the clear.
    direct: reqwest::Client,
    /// Cancels provider requests started under an earlier vault generation.
    in_flight: Mutex<(u64, CancellationToken)>,
    pairing: Mutex<Option<Pairing>>,
    /// Numbers pairings so a replaced one stops working for sessions that proved it.
    serials: std::sync::atomic::AtomicU64,
    /// Counts pairing replacements and voids; changed only under the pairing lock.
    revocations: std::sync::atomic::AtomicU64,
}

/// The current gateway pairing. The first process to prove it becomes its
/// holder; a copied token is useless to any other process afterwards. The
/// holder's first connection anchors it: when that connection closes (the
/// gateway exited), the pairing is void, so a reused pid finds nothing.
struct Pairing {
    serial: u64,
    token: Zeroizing<String>,
    /// The vault generation it was minted under; any lock or unlock voids it.
    generation: u64,
    holder: Option<i32>,
}

impl Shared {
    fn vault(&self) -> MutexGuard<'_, Vault> {
        self.vault.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pairing(&self) -> MutexGuard<'_, Option<Pairing>> {
        self.pairing.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Advances with every lock, unlock, pairing replacement and pairing void.
    /// Read the revocation count under the pairing lock it was checked with.
    fn epoch(&self, generation: u64) -> u64 {
        generation + self.revocations.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn revoke(&self) {
        self.revocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// The token for requests sent under `epoch`. Moving to a newer epoch
    /// cancels every older request.
    fn in_flight(&self, epoch: u64) -> CancellationToken {
        let mut current = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        // Epochs only grow; a stale reader must not cancel newer sends, and
        // gets a token that is already cancelled.
        if epoch < current.0 {
            let stale = CancellationToken::new();
            stale.cancel();
            return stale;
        }
        if epoch > current.0 {
            current.1.cancel();
            *current = (epoch, CancellationToken::new());
        }
        current.1.clone()
    }
}

#[derive(Default)]
struct Session {
    version: u16,
    import_review: Option<a206_vault_channel::ImportReview>,
    channel: Option<a206_vault_channel::Channel>,
    channel_opened: bool,
    role: Option<Role>,
    step_up: StepUpWindow,
    /// A pending enrollment and the vault generation it was started under.
    pending: Option<(PendingTotp, u64)>,
    /// The serial of the pairing this gateway connection proved.
    paired: Option<u64>,
    /// This connection is the holder's anchor for that pairing.
    anchor: Option<u64>,
    /// The connected process, from the kernel's socket credentials.
    peer: Option<i32>,
}

struct Outbound {
    method: HttpMethod,
    url: url::Url,
    /// The credential header, or `None` when the credential is in `url`.
    header: Option<(&'static str, Zeroizing<String>)>,
    anthropic_version: Option<String>,
    /// Ask for a JSON response (`Accept: application/json`).
    accept_json: bool,
    body: Option<Vec<u8>>,
    /// Sent as the body's `Content-Type`.
    content_type: &'static str,
    /// For a git call, the `Content-Type` a successful response must have;
    /// it is also sent as `Accept`.
    expect: Option<&'static str>,
    cancel: CancellationToken,
    /// Relay the body as it arrives (`Request::ProviderStream`).
    stream: bool,
}

/// A socket session to open: the credential is attached for `url`'s origin.
struct SocketOut {
    url: url::Url,
    scheme: SocketScheme,
    token: Zeroizing<String>,
    cancel: CancellationToken,
}

enum Outcome {
    Reply(Reply),
    Send(Box<Outbound>),
    Socket(Box<SocketOut>),
}

type Handled = Result<Outcome, BrokerError>;

fn vault_error(error: VaultError) -> BrokerError {
    use InvalidRequest as I;
    if matches!(
        error,
        VaultError::Io(_) | VaultError::Crypto | VaultError::Corrupted
    ) {
        tracing::error!(
            error = &error as &dyn std::error::Error,
            "vault operation failed"
        );
    }
    match error {
        VaultError::Locked => BrokerError::Locked,
        VaultError::NotFound => BrokerError::NotFound,
        VaultError::Denied(denial) => BrokerError::Denied(denial),
        VaultError::InvalidName => BrokerError::Invalid(I::Name),
        VaultError::KindMismatch => BrokerError::Invalid(I::KindMismatch),
        VaultError::TotpEnrolled => BrokerError::Invalid(I::TotpEnrolled),
        VaultError::TotpNotEnrolled => BrokerError::Invalid(I::TotpNotEnrolled),
        VaultError::EnrollmentExpired => BrokerError::Invalid(I::EnrollmentExpired),
        VaultError::TotpRejected => BrokerError::AuthenticationFailed,
        VaultError::Corrupted => BrokerError::Vault(VaultFailure::Corrupted),
        VaultError::Crypto => BrokerError::Vault(VaultFailure::Crypto),
        VaultError::Io(_) | VaultError::ImportLeftInPlace(_) => {
            BrokerError::Vault(VaultFailure::Storage)
        }
        VaultError::Exists => BrokerError::VaultExists,
        VaultError::ImportSource(_) => BrokerError::Invalid(I::ImportSource),
        VaultError::SshKey(error) => BrokerError::SshKey(error),
    }
}

fn upstream(error: reqwest::Error) -> BrokerError {
    let error = error.without_url();
    tracing::warn!(
        error = &error as &dyn std::error::Error,
        "provider request failed"
    );
    BrokerError::Upstream(if error.is_timeout() {
        UpstreamFailure::Timeout
    } else if error.is_connect() {
        UpstreamFailure::Connect
    } else {
        UpstreamFailure::Transfer
    })
}

/// Bind a pairing to the first process that proves it: `Some(true)` for that
/// first claim, `Some(false)` for its holder again, `None` for anyone else. A
/// caller the kernel cannot identify is refused rather than trusted.
fn claim(holder: &mut Option<i32>, peer: Option<i32>) -> Option<bool> {
    match (*holder, peer) {
        (_, None) => None,
        (None, Some(pid)) => {
            *holder = Some(pid);
            Some(true)
        }
        (Some(holder), Some(pid)) => (holder == pid).then_some(false),
    }
}

/// Compare without an early exit on the first differing byte.
fn same_secret(expected: &str, given: &str) -> bool {
    expected.len() == given.len()
        && expected
            .bytes()
            .zip(given.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

fn ok() -> Handled {
    Ok(Outcome::Reply(Reply::Ok))
}

/// Normalize user-entered destinations to exact origins.
fn origins(destinations: Vec<String>) -> Result<Vec<String>, BrokerError> {
    if destinations.len() > MAX_DESTINATIONS {
        return Err(BrokerError::Invalid(InvalidRequest::TooManyDestinations));
    }
    destinations
        .iter()
        .map(|d| {
            if d.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("ssh:")) {
                return a206_core::ssh_host::SshHost::parse(d)
                    .map(|host| host.origin())
                    .map_err(|_| BrokerError::Invalid(InvalidRequest::Destination));
            }
            if d.starts_with("ircs:") || d.starts_with("irc:") {
                return a206_core::messengers::IrcServer::parse(d)
                    .map(|server| server.origin())
                    .map_err(|_| BrokerError::Invalid(InvalidRequest::Destination));
            }
            let url = url::Url::parse(d)
                .map_err(|_| BrokerError::Invalid(InvalidRequest::Destination))?;
            allowed_scheme(&url).map_err(|_| BrokerError::Invalid(InvalidRequest::Destination))?;
            Ok(url.origin().ascii_serialization())
        })
        .collect()
}

fn allowed_scheme(url: &url::Url) -> Result<(), BrokerError> {
    let loopback = url
        .host_str()
        .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]" | "::1"));
    if url.username().is_empty()
        && url.password().is_none()
        && (url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        Ok(())
    } else {
        Err(BrokerError::Invalid(InvalidRequest::ProviderUrl))
    }
}

fn check_name(name: &str) -> Result<(), BrokerError> {
    if name.len() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(BrokerError::Invalid(InvalidRequest::Name));
    }
    Ok(())
}

fn require_step_up(session: &Session, generation: u64, now: Instant) -> Result<(), BrokerError> {
    if session.step_up.is_active(now, generation) {
        Ok(())
    } else {
        Err(BrokerError::StepUpRequired)
    }
}

/// Record an authentication failure and translate a triggered lockout.
fn failed(shared: &Shared, now: Instant, error: BrokerError) -> BrokerError {
    match shared.throttle.failure(&(), now) {
        Some(lock) => BrokerError::LockedOut {
            retry_after_secs: lock.retry_after.as_secs().max(1),
        },
        None => error,
    }
}

fn totp_error(outcome: TotpOutcome) -> BrokerError {
    match outcome {
        TotpOutcome::Replayed => BrokerError::TotpReplayed,
        TotpOutcome::ClockDrift { steps } => BrokerError::ClockDrift {
            seconds: steps.unsigned_abs() * 30,
        },
        _ => BrokerError::AuthenticationFailed,
    }
}

fn handle(shared: &Shared, session: &mut Session, request: Request) -> Handled {
    let now = Instant::now();
    let Some(role) = session.role else {
        return match request {
            Request::Hello { role, version }
                if (MIN_BROKER_VERSION..=BROKER_VERSION).contains(&version) =>
            {
                session.role = Some(role);
                session.version = version;
                ok()
            }
            Request::Hello { .. } => Err(BrokerError::Invalid(InvalidRequest::UnsupportedVersion)),
            // Version-independent so an upgraded client can stop an older
            // broker. Any same-user client could already greet and stop it.
            Request::Shutdown => {
                shared.vault().lock();
                shared.stop.cancel();
                ok()
            }
            _ => Err(BrokerError::Forbidden),
        };
    };
    if matches!(request, Request::ChannelKey) && matches!(role, Role::User | Role::Relay) {
        return Ok(Outcome::Reply(Reply::ChannelKey(
            shared.identity.public.clone(),
        )));
    }
    if role == Role::Relay {
        return match request {
            Request::OpenChannel { first } if !session.channel_opened => {
                // One handshake per socket. A relay must never replace a
                // browser's channel and inherit its step-up or pending seed.
                session.channel_opened = true;
                let (channel, response) = shared
                    .identity
                    .accept(&first)
                    .map_err(crate::channel::channel_error)?;
                session.channel = Some(channel);
                Ok(Outcome::Reply(Reply::ChannelOpened(response)))
            }
            Request::ChannelMessage { bytes } => {
                // Take first: every crypto/protocol failure permanently voids this channel.
                let mut channel = session.channel.take().ok_or(BrokerError::Forbidden)?;
                let operation = channel
                    .open::<a206_vault_channel::Request>(&bytes)
                    .map_err(crate::channel::channel_error)?;
                session.role = Some(Role::User);
                let outcome = crate::channel::validate(&operation)
                    .and_then(|()| handle(shared, session, crate::channel::request(operation)));
                session.role = Some(Role::Relay);
                let reply = crate::channel::reply(outcome.map(|outcome| match outcome {
                    Outcome::Reply(reply) => reply,
                    // The typed setup surface cannot send or open provider sockets.
                    Outcome::Send(_) | Outcome::Socket(_) => Reply::Error(BrokerError::Forbidden),
                }));
                let bytes = channel
                    .seal(&reply)
                    .map_err(crate::channel::channel_error)?;
                session.channel = Some(channel);
                Ok(Outcome::Reply(Reply::ChannelMessage(bytes)))
            }
            _ => Err(BrokerError::Forbidden),
        };
    }
    match (role, &request) {
        (_, Request::Hello { .. }) => {
            return Err(BrokerError::Invalid(InvalidRequest::AlreadyGreeted));
        }
        (_, Request::Status)
        | (
            Role::Gateway,
            Request::GatewayAuth { .. }
            | Request::Provider(_)
            | Request::ProviderStream(_)
            | Request::SocketOpen(_)
            | Request::SocketSend { .. }
            | Request::GitReceivePack(_)
            | Request::GitUploadPack(_)
            | Request::SshAgent(_),
        ) => {}
        (Role::Gateway, _)
        | (
            Role::User,
            Request::GatewayAuth { .. }
            | Request::Provider(_)
            | Request::ProviderStream(_)
            | Request::SocketOpen(_)
            | Request::SocketSend { .. }
            | Request::GitReceivePack(_)
            | Request::GitUploadPack(_)
            | Request::SshAgent(_),
        ) => {
            return Err(BrokerError::Forbidden);
        }
        _ => {}
    }
    // Step-ups are bound to the vault generation, so any lock voids them.
    let require_step_up =
        |session: &Session, generation: u64| require_step_up(session, generation, now);
    match request {
        Request::CredentialPage {
            version,
            after,
            limit,
        } => credentials::page(shared, session, version, after, limit),
        Request::AddApiKey(input) => credentials::add(shared, session, input, now),
        Request::ReviewVaultImport { version, source } => {
            import::version(session, version)?;
            import::review(shared, session, source)
        }
        Request::ApplyVaultImport {
            version,
            review,
            password,
            totp,
        } => {
            import::version(session, version)?;
            let held = session
                .import_review
                .as_ref()
                .filter(|held| held.id == review)
                .cloned()
                .ok_or(BrokerError::ImportReviewUnavailable)?;
            let source = held.source.clone();
            import::apply(
                shared,
                session,
                &source,
                &password,
                totp.as_ref().map(WireSecret::expose),
                now,
                Some(held),
            )
        }
        Request::CancelVaultImport { version } => {
            import::version(session, version)?;
            session.import_review = None;
            ok()
        }
        Request::Hello { .. }
        | Request::ChannelKey
        | Request::OpenChannel { .. }
        | Request::ChannelMessage { .. } => Err(BrokerError::Forbidden),
        Request::CancelTotp => {
            session.pending = None;
            ok()
        }
        Request::Status => {
            let vault = shared.vault();
            let unlocked = !vault.is_locked();
            let gateway_paired = unlocked.then(|| {
                shared.pairing().as_ref().is_some_and(|pairing| {
                    pairing.generation == vault.generation() && pairing.holder.is_some()
                })
            });
            Ok(Outcome::Reply(Reply::Status(BrokerStatus {
                exists: vault.exists(),
                unlocked,
                totp_enrolled: vault.totp_enrolled().ok(),
                agent_access: vault.agent_access().ok(),
                step_up_active: unlocked && session.step_up.is_active(now, vault.generation()),
                gateway_paired,
            })))
        }
        Request::Unlock {
            password,
            totp,
            create,
        } => {
            shared
                .throttle
                .check(&(), now)
                .map_err(|l| BrokerError::LockedOut {
                    retry_after_secs: l.retry_after.as_secs().max(1),
                })?;
            let mut vault = shared.vault();
            if !vault.exists() && !create {
                return Err(BrokerError::NoVault);
            }
            let secret = Secret::new(password.expose().to_owned());
            let was_locked = vault.is_locked();
            if was_locked {
                if vault.unlock(&secret).is_err() {
                    vault.lock();
                    return Err(failed(shared, now, BrokerError::AuthenticationFailed));
                }
            } else if !vault.verify_password(&secret) {
                return Err(failed(shared, now, BrokerError::AuthenticationFailed));
            }
            if vault.totp_enrolled().map_err(vault_error)? {
                let relock = |vault: &mut Vault| {
                    if was_locked {
                        vault.lock();
                    }
                };
                let Some(code) = totp else {
                    relock(&mut vault);
                    return Err(BrokerError::TotpRequired);
                };
                let outcome = vault.verify_totp(&code, unix_now()).map_err(vault_error)?;
                if outcome != TotpOutcome::Valid {
                    relock(&mut vault);
                    return Err(failed(shared, now, totp_error(outcome)));
                }
            }
            let generation = vault.generation();
            drop(vault);
            shared.throttle.success(&());
            session.step_up.grant(now, generation);
            session.import_review = None;
            ok()
        }
        Request::ImportVault {
            source,
            password,
            totp,
        } => import::apply(
            shared,
            session,
            &source,
            &password,
            totp.as_deref(),
            now,
            None,
        ),
        Request::Lock => {
            session.import_review = None;
            shared.vault().lock();
            session.step_up.revoke();
            session.pending = None;
            ok()
        }
        Request::Shutdown => {
            shared.vault().lock();
            shared.stop.cancel();
            ok()
        }
        Request::StepUp { password, totp } => {
            shared
                .throttle
                .check(&(), now)
                .map_err(|l| BrokerError::LockedOut {
                    retry_after_secs: l.retry_after.as_secs().max(1),
                })?;
            let mut vault = shared.vault();
            if vault.is_locked() {
                return Err(BrokerError::Locked);
            }
            if vault.totp_enrolled().map_err(vault_error)? {
                let code = totp.ok_or(BrokerError::TotpRequired)?;
                let outcome = vault.verify_totp(&code, unix_now()).map_err(vault_error)?;
                if outcome != TotpOutcome::Valid {
                    return Err(failed(shared, now, totp_error(outcome)));
                }
            } else {
                let password =
                    password.ok_or(BrokerError::Invalid(InvalidRequest::PasswordRequired))?;
                if !vault.verify_password(&Secret::new(password.expose().to_owned())) {
                    return Err(failed(shared, now, BrokerError::AuthenticationFailed));
                }
            }
            let generation = vault.generation();
            drop(vault);
            shared.throttle.success(&());
            session.step_up.grant(now, generation);
            ok()
        }
        Request::BeginTotp { account } => {
            check_name(&account)?;
            let vault = shared.vault();
            let generation = vault.generation();
            require_step_up(session, generation)?;
            let pending = vault
                .begin_totp_enrollment(&account, unix_now())
                .map_err(vault_error)?;
            drop(vault);
            let reply = Reply::TotpEnrollment {
                otpauth_url: WireSecret::new(
                    pending
                        .otpauth_url()
                        .map_err(vault_error)?
                        .expose(str::to_owned),
                ),
                manual_key: WireSecret::new(pending.manual_key().expose(str::to_owned)),
                expires_at: pending.expires_at(),
            };
            session.pending = Some((pending, generation));
            Ok(Outcome::Reply(reply))
        }
        Request::ConfirmTotp { code } => {
            shared
                .throttle
                .check(&(), now)
                .map_err(|l| BrokerError::LockedOut {
                    retry_after_secs: l.retry_after.as_secs().max(1),
                })?;
            let mut vault = shared.vault();
            // A lock since enrollment began discards it.
            let Some((pending, _)) = session
                .pending
                .as_ref()
                .filter(|(_, generation)| *generation == vault.generation())
            else {
                session.pending = None;
                return Err(BrokerError::NoPendingEnrollment);
            };
            let confirmed = vault.confirm_totp_enrollment(pending, &code, unix_now());
            drop(vault);
            match confirmed {
                Ok(()) => {
                    session.pending = None;
                    shared.throttle.success(&());
                    ok()
                }
                Err(VaultError::TotpRejected) => {
                    Err(failed(shared, now, BrokerError::AuthenticationFailed))
                }
                Err(error) => {
                    session.pending = None;
                    Err(vault_error(error))
                }
            }
        }
        Request::RemoveTotp { password, code } => {
            shared
                .throttle
                .check(&(), now)
                .map_err(|l| BrokerError::LockedOut {
                    retry_after_secs: l.retry_after.as_secs().max(1),
                })?;
            let mut vault = shared.vault();
            if vault.is_locked() {
                return Err(BrokerError::Locked);
            }
            if !vault.verify_password(&Secret::new(password.expose().to_owned())) {
                return Err(failed(shared, now, BrokerError::AuthenticationFailed));
            }
            match vault.remove_totp(&code, unix_now()) {
                Ok(()) => {
                    drop(vault);
                    shared.throttle.success(&());
                    ok()
                }
                Err(VaultError::TotpRejected) => {
                    Err(failed(shared, now, BrokerError::AuthenticationFailed))
                }
                Err(error) => Err(vault_error(error)),
            }
        }
        Request::Credentials => {
            let vault = shared.vault();
            Ok(Outcome::Reply(Reply::Credentials {
                typed: vault.credentials().map_err(vault_error)?,
                legacy: vault.names().map_err(vault_error)?,
            }))
        }
        Request::AdoptLegacy {
            legacy,
            name,
            mut meta,
        } => {
            check_name(&name)?;
            meta.destinations = origins(meta.destinations)?;
            let mut vault = shared.vault();
            require_step_up(session, vault.generation())?;
            vault
                .adopt_raw(&legacy, &name, &meta)
                .map_err(vault_error)?;
            ok()
        }
        Request::PutCredential {
            name,
            meta,
            value,
            replace,
        } => credentials::put(shared, session, name, meta, value, replace, now),
        Request::UpdateCredential {
            name,
            policy,
            disabled,
            destinations,
        } => {
            let destinations = destinations.map(origins).transpose()?;
            let mut vault = shared.vault();
            require_step_up(session, vault.generation())?;
            let mut updated = vault.credential(&name).map_err(vault_error)?;
            if let Some(policy) = policy {
                updated.policy = policy;
            }
            if let Some(disabled) = disabled {
                updated.disabled = disabled;
            }
            if let Some(destinations) = destinations {
                updated.destinations = destinations;
            }
            credentials::validate_updated_metadata(&name, updated.clone())?;
            vault
                .update_credential(&name, |meta: &mut CredentialMeta| {
                    *meta = updated;
                })
                .map_err(vault_error)?;
            ok()
        }
        Request::DeleteCredential { name } => {
            let mut vault = shared.vault();
            require_step_up(session, vault.generation())?;
            vault.delete_credential(&name).map_err(vault_error)?;
            ok()
        }
        Request::SetAgentAccess { enabled } => {
            let mut vault = shared.vault();
            require_step_up(session, vault.generation())?;
            vault.set_agent_access(enabled).map_err(vault_error)?;
            ok()
        }
        Request::Reveal { name } => {
            let vault = shared.vault();
            require_step_up(session, vault.generation())?;
            let value = vault
                .reveal_credential(&name, &session.step_up, now, |v| WireValue::from(v))
                .map_err(vault_error)?;
            Ok(Outcome::Reply(Reply::Value(value)))
        }
        Request::PairGateway => {
            let vault = shared.vault();
            let generation = vault.generation();
            require_step_up(session, generation)?;
            if vault.is_locked() {
                return Err(BrokerError::Locked);
            }
            // Two v4 UUIDs: 244 bits from the OS CSPRNG.
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let reply = Reply::GatewayPairing {
                token: WireSecret::new(token.clone()),
            };
            let mut pairing = shared.pairing();
            if pairing.is_some() {
                shared.revoke();
            }
            *pairing = Some(Pairing {
                serial: shared
                    .serials
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                token: Zeroizing::new(token),
                generation,
                holder: None,
            });
            Ok(Outcome::Reply(reply))
        }
        Request::GatewayAuth { token } => {
            let vault = shared.vault();
            let generation = vault.generation();
            let mut pairing = shared.pairing();
            let claimed = match pairing.as_mut() {
                Some(pairing)
                    if !vault.is_locked()
                        && pairing.generation == generation
                        && same_secret(&pairing.token, token.expose()) =>
                {
                    claim(&mut pairing.holder, session.peer).map(|first| (pairing.serial, first))
                }
                _ => None,
            };
            let Some((serial, first)) = claimed else {
                return Err(BrokerError::GatewayNotPaired);
            };
            session.paired = Some(serial);
            if first {
                session.anchor = Some(serial);
            }
            ok()
        }
        Request::Provider(call) => provider(shared, session, call, false),
        Request::ProviderStream(call) => provider(shared, session, call, true),
        Request::SocketOpen(call) => socket(shared, session, call),
        // A connection carrying a socket never reaches here with one.
        Request::SocketSend { .. } => Err(BrokerError::Invalid(InvalidRequest::NoSocket)),
        Request::GitReceivePack(call) => git(shared, session, call, GitService::ReceivePack),
        Request::GitUploadPack(call) => git(shared, session, call, GitService::UploadPack),
        Request::SshAgent(call) => ssh_agent(shared, session, call),
    }
}

/// The unlocked vault, still held, and the epoch, once this connection is
/// shown to hold the current pairing. Unpaired callers learn nothing else,
/// not even whether a URL is valid.
fn paired<'a>(
    shared: &'a Shared,
    session: &Session,
) -> Result<(MutexGuard<'a, Vault>, u64), BrokerError> {
    let vault = shared.vault();
    if vault.is_locked() {
        return Err(BrokerError::Locked);
    }
    let (current, epoch) = {
        let pairing = shared.pairing();
        let current = pairing.as_ref().and_then(|pairing| {
            (pairing.generation == vault.generation()).then_some(pairing.serial)
        });
        (current, shared.epoch(vault.generation()))
    };
    if current.is_none() || session.paired != current {
        return Err(BrokerError::GatewayNotPaired);
    }
    Ok((vault, epoch))
}

/// An HTTPS (or loopback HTTP) URL without credentials or a fragment.
fn request_url(url: &str) -> Result<url::Url, BrokerError> {
    let url =
        url::Url::parse(url).map_err(|_| BrokerError::Invalid(InvalidRequest::ProviderUrl))?;
    allowed_scheme(&url)?;
    if url.fragment().is_some() {
        return Err(BrokerError::Invalid(InvalidRequest::ProviderUrl));
    }
    Ok(url)
}

/// Validate a socket call and take its credential for the exact origin.
fn socket(shared: &Shared, session: &Session, call: SocketCall) -> Handled {
    let (vault, epoch) = paired(shared, session)?;
    let (url, origin) = match &call.scheme {
        SocketScheme::IrcSasl { login } => {
            if !a206_core::messengers::valid_irc_nick(login) {
                return Err(BrokerError::Invalid(InvalidRequest::IrcLogin));
            }
            let server = a206_core::messengers::IrcServer::parse(&call.url)
                .map_err(|_| BrokerError::Invalid(InvalidRequest::IrcServer))?;
            let origin = server.origin();
            let url = url::Url::parse(&origin)
                .map_err(|_| BrokerError::Invalid(InvalidRequest::IrcServer))?;
            (url, origin)
        }
        SocketScheme::DiscordGateway { .. } | SocketScheme::SlackSocketMode => {
            let url = request_url(&call.url)?;
            let origin = url.origin().ascii_serialization();
            (url, origin)
        }
    };
    // Read with the pairing check, so a later lock, replacement or void
    // always ends this session.
    let cancel = shared.in_flight(epoch);
    let token = vault
        .use_for_destination(&call.credential, &origin, |s| {
            Zeroizing::new(s.expose(str::to_owned))
        })
        .map_err(vault_error)?;
    Ok(Outcome::Socket(Box::new(SocketOut {
        url,
        scheme: call.scheme,
        token,
        cancel,
    })))
}

/// Validate a provider call and attach its credential for the exact origin.
fn provider(shared: &Shared, session: &Session, call: ProviderCall, stream: bool) -> Handled {
    let (vault, epoch) = paired(shared, session)?;
    let url = request_url(&call.url)?;
    if call
        .body
        .as_ref()
        .is_some_and(|b| b.len() > MAX_PROVIDER_BODY)
    {
        return Err(BrokerError::Invalid(InvalidRequest::ProviderBody));
    }
    if call.auth == AuthScheme::WebhookUrl && (url.path() != "/" || url.query().is_some()) {
        return Err(BrokerError::Invalid(InvalidRequest::WebhookPath));
    }
    let origin = url.origin().ascii_serialization();
    // The epoch was read with the pairing check, so a later lock,
    // replacement or void always cancels this send.
    let cancel = shared.in_flight(epoch);
    let value = vault
        .use_for_destination(&call.credential, &origin, |s| {
            Zeroizing::new(match call.auth {
                AuthScheme::Bearer => format!("Bearer {}", s.expose(str::to_owned)),
                AuthScheme::DiscordBot => format!("Bot {}", s.expose(str::to_owned)),
                _ => s.expose(str::to_owned),
            })
        })
        .map_err(vault_error)?;
    drop(vault);
    let (url, header) = match call.auth {
        AuthScheme::Bearer | AuthScheme::DiscordBot => (url, Some(("authorization", value))),
        AuthScheme::ApiKeyHeader => (url, Some(("x-api-key", value))),
        AuthScheme::GoogleApiKey => (url, Some(("x-goog-api-key", value))),
        AuthScheme::SubscriptionToken => (url, Some(("x-subscription-token", value))),
        AuthScheme::TelegramBot => (telegram_url(url, &value)?, None),
        AuthScheme::WebhookUrl => (webhook_url(&url, &value)?, None),
    };
    Ok(Outcome::Send(Box::new(Outbound {
        method: call.method,
        url,
        header,
        anthropic_version: call.anthropic_version,
        accept_json: call.auth == AuthScheme::SubscriptionToken,
        body: call.body,
        content_type: "application/json",
        expect: None,
        cancel,
        stream,
    })))
}

/// Validate one round of a git push and attach the token, as HTTP Basic
/// authentication, for the repository's exact origin.
/// The smart HTTP service a [`GitCall`] is for.
#[derive(Clone, Copy)]
enum GitService {
    ReceivePack,
    UploadPack,
}
impl GitService {
    fn name(self) -> &'static str {
        match self {
            Self::ReceivePack => "git-receive-pack",
            Self::UploadPack => "git-upload-pack",
        }
    }
    /// The content types of its advertisement, its request and its result.
    fn types(self) -> [&'static str; 3] {
        match self {
            Self::ReceivePack => [
                "application/x-git-receive-pack-advertisement",
                "application/x-git-receive-pack-request",
                "application/x-git-receive-pack-result",
            ],
            Self::UploadPack => [
                "application/x-git-upload-pack-advertisement",
                "application/x-git-upload-pack-request",
                "application/x-git-upload-pack-result",
            ],
        }
    }
}

fn git(shared: &Shared, session: &Session, call: GitCall, service: GitService) -> Handled {
    use base64::Engine as _;
    let (vault, epoch) = paired(shared, session)?;
    let invalid = || BrokerError::Invalid(InvalidRequest::GitRepository);
    let mut url = request_url(&call.repository).map_err(|_| invalid())?;
    if url.query().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(invalid());
    }
    let username = &call.username;
    if username.is_empty()
        || username.chars().count() > 256
        || username.chars().any(|c| c == ':' || c.is_control())
    {
        return Err(BrokerError::Invalid(InvalidRequest::GitUsername));
    }
    if call
        .request
        .as_ref()
        .is_some_and(|b| b.len() > MAX_PROVIDER_BODY)
    {
        return Err(BrokerError::Invalid(InvalidRequest::ProviderBody));
    }
    let origin = url.origin().ascii_serialization();
    let cancel = shared.in_flight(epoch);
    let value = vault
        .use_for_destination(&call.credential, &origin, |s| {
            let pair = Zeroizing::new(s.expose(|token| format!("{username}:{token}")));
            Zeroizing::new(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair.as_bytes())
            ))
        })
        .map_err(vault_error)?;
    drop(vault);
    let base = url.path().trim_end_matches('/').to_owned();
    let [advertisement, request, result] = service.types();
    let (method, expect, content_type) = match call.request {
        None => {
            url.set_path(&format!("{base}/info/refs"));
            url.set_query(Some(&format!("service={}", service.name())));
            (HttpMethod::Get, advertisement, "application/json")
        }
        Some(_) => {
            url.set_path(&format!("{base}/{}", service.name()));
            (HttpMethod::Post, result, request)
        }
    };
    Ok(Outcome::Send(Box::new(Outbound {
        method,
        url,
        header: Some(("authorization", value)),
        anthropic_version: None,
        accept_json: false,
        body: call.request,
        content_type,
        expect: Some(expect),
        cancel,
        stream: false,
    })))
}

/// Use an SSH key for the client the gateway runs: the vault checks the
/// destination binding and signs only an authentication request for that
/// key.
fn ssh_agent(shared: &Shared, session: &Session, call: SshAgentCall) -> Handled {
    let invalid = || BrokerError::Invalid(InvalidRequest::SshAgent);
    let destination = a206_core::ssh_host::SshHost::parse(&call.destination)
        .map_err(|_| invalid())?
        .origin();
    if destination != call.destination
        || call
            .sign
            .as_ref()
            .is_some_and(|d| d.len() > MAX_SSH_SIGN_DATA)
    {
        return Err(invalid());
    }
    let (vault, _) = paired(shared, session)?;
    let used = vault
        .ssh_for_destination(&call.credential, &destination, call.sign.as_deref())
        .map_err(vault_error)?;
    Ok(Outcome::Reply(Reply::SshAgent {
        public_key: used.public_key,
        signature: used.signature,
    }))
}

/// Telegram takes the bot token as the first path segment. A token that could
/// change the path's shape is refused rather than escaped. The URL is not
/// zeroized, but it lives only until the request is sent, and transport
/// errors drop it (see `upstream`).
fn telegram_url(mut url: url::Url, token: &str) -> Result<url::Url, BrokerError> {
    let valid = !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'));
    if !valid {
        return Err(BrokerError::Upstream(UpstreamFailure::InvalidBotToken));
    }
    let path = format!("/bot{token}{}", url.path());
    url.set_path(&path);
    Ok(url)
}

/// A webhook credential is the whole URL to send to. It must be one the
/// broker would accept as a request URL, on the origin the call named (which
/// the credential is bound to), so a stored URL cannot redirect the request
/// elsewhere. Like a Telegram URL, it lives only until the request is sent.
fn webhook_url(called: &url::Url, stored: &str) -> Result<url::Url, BrokerError> {
    let invalid = || BrokerError::Upstream(UpstreamFailure::InvalidWebhookUrl);
    let url = request_url(stored).map_err(|_| invalid())?;
    if url.origin() != called.origin() {
        return Err(invalid());
    }
    Ok(url)
}

/// The user agent of git calls.
const GIT_AGENT: &str = concat!("git/agent206-", env!("CARGO_PKG_VERSION"));

fn http_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        // Forge APIs refuse requests that do not name their client.
        .user_agent(concat!("agent206/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(180))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
}

impl Shared {
    /// The client for `url`: plain HTTP never goes through a proxy.
    fn client(&self, url: &url::Url) -> &reqwest::Client {
        if url.scheme() == "https" {
            &self.http
        } else {
            &self.direct
        }
    }
}

/// Why a send was abandoned: the vault locked, or the pairing it was sent
/// under was replaced or voided.
fn revoked(shared: &Shared) -> BrokerError {
    if shared.vault().is_locked() {
        BrokerError::Locked
    } else {
        BrokerError::GatewayNotPaired
    }
}

/// Send a prepared request unless its epoch ended first; a lock, pairing
/// replacement or pairing void while it is in flight abandons it.
async fn send(shared: &Shared, out: Outbound) -> Result<Reply, BrokerError> {
    let cancel = out.cancel.clone();
    if !cancel.is_cancelled() {
        tokio::select! {
            reply = transmit(shared.client(&out.url), out) => return reply,
            _ = cancel.cancelled() => {}
        }
    }
    Err(revoked(shared))
}

/// Attach the credential and send; resolves once the upstream status arrives.
async fn request(http: &reqwest::Client, out: Outbound) -> Result<reqwest::Response, BrokerError> {
    let method = match out.method {
        HttpMethod::Get => reqwest::Method::GET,
        HttpMethod::Post => reqwest::Method::POST,
        HttpMethod::Put => reqwest::Method::PUT,
    };
    let mut request = http.request(method, out.url);
    if let Some((name, value)) = &out.header {
        let mut header = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| BrokerError::Upstream(UpstreamFailure::InvalidHeader))?;
        header.set_sensitive(true);
        request = request.header(*name, header);
    }
    if let Some(version) = out.anthropic_version {
        request = request.header("anthropic-version", version);
    }
    if out.accept_json {
        request = request.header(reqwest::header::ACCEPT, "application/json");
    }
    if let Some(expect) = out.expect {
        // Some forges serve git only to clients that name themselves git.
        request = request
            .header(reqwest::header::ACCEPT, expect)
            .header(reqwest::header::USER_AGENT, GIT_AGENT);
    }
    if let Some(body) = out.body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, out.content_type)
            .body(body);
    }
    request.send().await.map_err(upstream)
}

async fn transmit(http: &reqwest::Client, out: Outbound) -> Result<Reply, BrokerError> {
    let expect = out.expect;
    let response = request(http, out).await?;
    let status = response.status().as_u16();
    // Never relay upstream error bodies: they can echo credentials or prompts.
    if !response.status().is_success() {
        return Ok(Reply::Provider {
            status,
            body: vec![],
        });
    }
    // A sign-in page or a dumb HTTP server is not git's answer.
    if let Some(expect) = expect {
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if content_type != Some(expect) {
            return Err(BrokerError::Upstream(UpstreamFailure::NotGit));
        }
    }
    let mut stream = response.bytes_stream();
    let mut body = vec![];
    while let Some(part) = stream.next().await {
        let part = part.map_err(upstream)?;
        if body.len() + part.len() > MAX_PROVIDER_BODY {
            return Err(BrokerError::Upstream(UpstreamFailure::TooLarge));
        }
        body.extend_from_slice(&part);
    }
    Ok(Reply::Provider { status, body })
}

/// Relay a streamed provider call as `ProviderHead`, `ProviderChunk`s and a
/// final `ProviderEnd` or `Error`. Returns `false` when the connection must
/// close: the peer went away (which abandons the upstream request), the
/// broker is stopping, or a revocation interrupted a frame mid-write.
async fn relay(
    shared: &Shared,
    out: Outbound,
    id: uuid::Uuid,
    writer: &mut OwnedWriteHalf,
    inbox: &mut Inbox,
) -> std::io::Result<bool> {
    let cancel = out.cancel.clone();
    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(revoked(shared)),
        _ = shared.stop.cancelled() => return Ok(false),
        closed = inbox.closed() => return closed.map(|()| false),
        response = request(shared.client(&out.url), out) => response,
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => return write(writer, id, Reply::Error(error)).await.map(|()| true),
    };
    let status = response.status();
    write(
        writer,
        id,
        Reply::ProviderHead {
            status: status.as_u16(),
        },
    )
    .await?;
    // Never relay upstream error bodies: they can echo credentials or prompts.
    if !status.is_success() {
        let end = Reply::ProviderEnd {
            chunks: 0,
            bytes: 0,
        };
        return write(writer, id, end).await.map(|()| true);
    }
    let mut body = response.bytes_stream();
    let (mut chunks, mut total) = (0u64, 0usize);
    let error = loop {
        let part = tokio::select! {
            biased;
            _ = cancel.cancelled() => break revoked(shared),
            _ = shared.stop.cancelled() => return Ok(false),
            closed = inbox.closed() => return closed.map(|()| false),
            part = body.next() => part,
        };
        let part = match part {
            None => {
                let end = Reply::ProviderEnd {
                    chunks,
                    bytes: total as u64,
                };
                return write(writer, id, end).await.map(|()| true);
            }
            Some(Err(error)) => break upstream(error),
            Some(Ok(part)) => part,
        };
        total += part.len();
        if total > MAX_PROVIDER_BODY {
            break BrokerError::Upstream(UpstreamFailure::TooLarge);
        }
        for piece in part.chunks(MAX_STREAM_CHUNK) {
            let frame = Reply::ProviderChunk {
                seq: chunks,
                bytes: piece.to_vec(),
            };
            // A peer that stops reading must not keep a revoked send alive.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(false),
                written = write(writer, id, frame) => written?,
            }
            chunks += 1;
        }
    };
    tracing::info!(
        error = &error as &dyn std::error::Error,
        "vault broker ended a provider stream early"
    );
    write(writer, id, Reply::Error(error)).await.map(|()| true)
}

/// Encode one reply frame; a reply too large for a frame becomes an error.
async fn write(writer: &mut OwnedWriteHalf, id: uuid::Uuid, body: Reply) -> std::io::Result<()> {
    let frame = match encode(&Response { id, body }) {
        Err(a206_protocol::Error::TooLarge) => encode(&Response {
            id,
            body: Reply::Error(BrokerError::Upstream(UpstreamFailure::TooLarge)),
        }),
        frame => frame,
    }
    .map_err(std::io::Error::other)?;
    writer.write_all(&frame).await
}

/// Requests read from one connection, decoded in order.
struct Inbox {
    reader: OwnedReadHalf,
    decoder: Decoder,
    queue: VecDeque<a206_protocol::Request<Request>>,
    bytes: Vec<u8>,
}

impl Inbox {
    /// The next request, or `None` once the peer closed. Cancel-safe.
    async fn next(&mut self) -> std::io::Result<Option<a206_protocol::Request<Request>>> {
        loop {
            if let Some(request) = self.queue.pop_front() {
                return Ok(Some(request));
            }
            if !self.read().await? {
                return Ok(None);
            }
        }
    }

    /// Read once and queue what decodes; `false` once the peer closed.
    /// Cancel-safe: a dropped read consumed nothing.
    async fn read(&mut self) -> std::io::Result<bool> {
        let count = self.reader.read(&mut self.bytes).await?;
        if count == 0 {
            return Ok(false);
        }
        let requests = self
            .decoder
            .feed(&self.bytes[..count])
            .map_err(std::io::Error::other)?;
        self.queue.extend(requests);
        Ok(true)
    }

    /// Resolves when the peer closes, queueing whatever it sends meanwhile.
    /// Clients never half-close, so end of input means the peer is gone.
    async fn closed(&mut self) -> std::io::Result<()> {
        while self.read().await? {}
        Ok(())
    }
}

async fn connection(shared: Arc<Shared>, stream: tokio::net::UnixStream) -> std::io::Result<()> {
    let mut anchor = None;
    let result = session(&shared, stream, &mut anchor).await;
    if let Some(serial) = anchor {
        let vault = shared.vault();
        let mut pairing = shared.pairing();
        if pairing.as_ref().is_some_and(|p| p.serial == serial) {
            // The holding gateway went away; its pid may be reused.
            *pairing = None;
            shared.revoke();
            shared.in_flight(shared.epoch(vault.generation()));
        }
    }
    result
}

async fn session(
    shared: &Arc<Shared>,
    stream: tokio::net::UnixStream,
    anchor: &mut Option<u64>,
) -> std::io::Result<()> {
    // The socket is mode 0600 in a 0700 directory; check the kernel's view too.
    let credentials = stream.peer_cred()?;
    if credentials.uid() != nix::unistd::geteuid().as_raw() {
        tracing::warn!(
            uid = credentials.uid(),
            "refused a vault broker connection from another user"
        );
        return Ok(());
    }
    let (reader, mut writer) = stream.into_split();
    let mut inbox = Inbox {
        reader,
        decoder: Decoder::default(),
        queue: VecDeque::new(),
        bytes: vec![0; 8192],
    };
    let mut session = Session {
        peer: credentials.pid(),
        ..Session::default()
    };
    loop {
        let next = tokio::select! {
            next = inbox.next() => next?,
            _ = shared.stop.cancelled() => return Ok(()),
        };
        let Some(request) = next else {
            return Ok(());
        };
        let shared_for_task = shared.clone();
        // Password derivation is deliberately slow; keep it off the reactor.
        let (returned, outcome) = tokio::task::spawn_blocking(move || {
            let outcome = handle(&shared_for_task, &mut session, request.body);
            (session, outcome)
        })
        .await
        .map_err(std::io::Error::other)?;
        session = returned;
        *anchor = anchor.or(session.anchor);
        // Any lock or unlock during this request cancels older sends.
        {
            let vault = shared.vault();
            let _pairing = shared.pairing();
            shared.in_flight(shared.epoch(vault.generation()));
        }
        let body = match outcome {
            Ok(Outcome::Reply(reply)) => reply,
            Ok(Outcome::Send(out)) if out.stream => {
                if relay(shared, *out, request.id, &mut writer, &mut inbox).await? {
                    continue;
                }
                return Ok(());
            }
            Ok(Outcome::Send(out)) => send(shared, *out).await.unwrap_or_else(Reply::Error),
            Ok(Outcome::Socket(out)) => {
                // The session is the rest of this connection.
                socket::tunnel(shared, *out, request.id, &mut writer, &mut inbox).await?;
                return Ok(());
            }
            Err(error) => {
                tracing::info!(
                    error = &error as &dyn std::error::Error,
                    "vault broker refused a request"
                );
                Reply::Error(error)
            }
        };
        write(&mut writer, request.id, body).await?;
    }
}

/// Serve the broker for one profile until a user Shutdown. Holds an exclusive
/// lock so a profile has at most one broker.
pub async fn serve(root: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = root.join("vault");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("broker.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .map_err(|_| std::io::Error::other("a vault broker already serves this profile"))?;
    let path = crate::socket_path(root);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let shared = Arc::new(Shared {
        root: root.to_owned(),
        identity: a206_vault_channel::Identity::generate().map_err(std::io::Error::other)?,
        vault: Mutex::new(Vault::locked(crate::vault_path(root))),
        throttle: AuthThrottle::default(),
        stop: CancellationToken::new(),
        http: http_client().build().map_err(std::io::Error::other)?,
        direct: http_client()
            .no_proxy()
            .build()
            .map_err(std::io::Error::other)?,
        in_flight: Mutex::new((0, CancellationToken::new())),
        pairing: Mutex::new(None),
        serials: std::sync::atomic::AtomicU64::new(0),
        revocations: std::sync::atomic::AtomicU64::new(0),
    });
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                connections.spawn(connection(shared.clone(), stream));
            }
            _ = shared.stop.cancelled() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        // A stale socket is replaced on the next start; shutdown still completes.
        let error: &(dyn std::error::Error + 'static) = &error;
        tracing::warn!(error, "could not remove the broker socket");
    }
    let drained = tokio::time::timeout(Duration::from_secs(2), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::debug!("aborting broker connections that outlived shutdown");
    }
    connections.abort_all();
    drop(lock);
    Ok(())
}
