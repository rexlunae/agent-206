//! The broker process: owns the decrypted vault, enforces authentication,
//! throttling and step-up, and performs destination-bound requests so the
//! gateway never receives a credential value.
use crate::proto::*;
use a206_protocol::{Decoder, Response, encode};
use a206_vault::{
    AuthThrottle, CredentialMeta, Error as VaultError, PendingTotp, Secret, StepUpWindow,
    TotpOutcome, Vault, unix_now,
};
use futures_util::StreamExt;
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

const MAX_NAME: usize = 256;
const MAX_DESTINATIONS: usize = 16;

struct Shared {
    vault: Mutex<Vault>,
    throttle: AuthThrottle<()>,
    stop: CancellationToken,
    http: reqwest::Client,
    /// Cancels provider requests started under an earlier vault generation.
    in_flight: Mutex<(u64, CancellationToken)>,
}

impl Shared {
    fn vault(&self) -> MutexGuard<'_, Vault> {
        self.vault.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The token for requests sent under `generation`. Moving to a new
    /// generation (any lock or unlock) cancels every older request.
    fn in_flight(&self, generation: u64) -> CancellationToken {
        let mut current = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        if current.0 != generation {
            current.1.cancel();
            *current = (generation, CancellationToken::new());
        }
        current.1.clone()
    }
}

#[derive(Default)]
struct Session {
    role: Option<Role>,
    step_up: StepUpWindow,
    pending: Option<PendingTotp>,
}

struct Outbound {
    method: HttpMethod,
    url: url::Url,
    header: &'static str,
    value: Zeroizing<String>,
    anthropic_version: Option<String>,
    body: Option<Vec<u8>>,
    cancel: CancellationToken,
}

enum Outcome {
    Reply(Reply),
    Send(Box<Outbound>),
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
        VaultError::Io(_) => BrokerError::Vault(VaultFailure::Storage),
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
            Request::Hello { role, version } if version == BROKER_VERSION => {
                session.role = Some(role);
                ok()
            }
            Request::Hello { .. } => Err(BrokerError::Invalid(InvalidRequest::UnsupportedVersion)),
            _ => Err(BrokerError::Forbidden),
        };
    };
    match (role, &request) {
        (_, Request::Hello { .. }) => {
            return Err(BrokerError::Invalid(InvalidRequest::AlreadyGreeted));
        }
        (_, Request::Status) | (Role::Gateway, Request::Provider(_)) => {}
        (Role::Gateway, _) | (Role::User, Request::Provider(_)) => {
            return Err(BrokerError::Forbidden);
        }
        _ => {}
    }
    // Step-ups are bound to the vault generation, so any lock voids them.
    let require_step_up = |session: &Session, generation: u64| {
        if session.step_up.is_active(now, generation) {
            Ok(())
        } else {
            Err(BrokerError::StepUpRequired)
        }
    };
    match request {
        Request::Hello { .. } => Err(BrokerError::Forbidden),
        Request::Status => {
            let vault = shared.vault();
            let unlocked = !vault.is_locked();
            Ok(Outcome::Reply(Reply::Status(BrokerStatus {
                exists: vault.exists(),
                unlocked,
                totp_enrolled: vault.totp_enrolled().ok(),
                agent_access: vault.agent_access().ok(),
                step_up_active: unlocked && session.step_up.is_active(now, vault.generation()),
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
            ok()
        }
        Request::Lock => {
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
            require_step_up(session, shared.vault().generation())?;
            check_name(&account)?;
            let pending = shared
                .vault()
                .begin_totp_enrollment(&account, unix_now())
                .map_err(vault_error)?;
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
            session.pending = Some(pending);
            Ok(Outcome::Reply(reply))
        }
        Request::ConfirmTotp { code } => {
            shared
                .throttle
                .check(&(), now)
                .map_err(|l| BrokerError::LockedOut {
                    retry_after_secs: l.retry_after.as_secs().max(1),
                })?;
            let pending = session
                .pending
                .as_ref()
                .ok_or(BrokerError::NoPendingEnrollment)?;
            match shared
                .vault()
                .confirm_totp_enrollment(pending, &code, unix_now())
            {
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
        Request::AdoptLegacy { name, mut meta } => {
            require_step_up(session, shared.vault().generation())?;
            meta.destinations = origins(meta.destinations)?;
            shared
                .vault()
                .adopt_raw(&name, &meta)
                .map_err(vault_error)?;
            ok()
        }
        Request::PutCredential {
            name,
            mut meta,
            value,
        } => {
            check_name(&name)?;
            meta.destinations = origins(meta.destinations)?;
            let mut vault = shared.vault();
            require_step_up(session, vault.generation())?;
            vault
                .put_credential(&name, &meta, (&value).into())
                .map_err(vault_error)?;
            ok()
        }
        Request::UpdateCredential {
            name,
            policy,
            disabled,
            destinations,
        } => {
            require_step_up(session, shared.vault().generation())?;
            let destinations = destinations.map(origins).transpose()?;
            shared
                .vault()
                .update_credential(&name, |meta: &mut CredentialMeta| {
                    if let Some(policy) = policy {
                        meta.policy = policy;
                    }
                    if let Some(disabled) = disabled {
                        meta.disabled = disabled;
                    }
                    if let Some(destinations) = destinations {
                        meta.destinations = destinations;
                    }
                })
                .map_err(vault_error)?;
            ok()
        }
        Request::DeleteCredential { name } => {
            require_step_up(session, shared.vault().generation())?;
            shared
                .vault()
                .delete_credential(&name)
                .map_err(vault_error)?;
            ok()
        }
        Request::SetAgentAccess { enabled } => {
            require_step_up(session, shared.vault().generation())?;
            shared
                .vault()
                .set_agent_access(enabled)
                .map_err(vault_error)?;
            ok()
        }
        Request::Reveal { name } => {
            require_step_up(session, shared.vault().generation())?;
            let value = shared
                .vault()
                .reveal_credential(&name, &session.step_up, now, |v| WireValue::from(v))
                .map_err(vault_error)?;
            Ok(Outcome::Reply(Reply::Value(value)))
        }
        Request::Provider(call) => {
            let url = url::Url::parse(&call.url)
                .map_err(|_| BrokerError::Invalid(InvalidRequest::ProviderUrl))?;
            allowed_scheme(&url)?;
            if url.fragment().is_some() {
                return Err(BrokerError::Invalid(InvalidRequest::ProviderUrl));
            }
            if call
                .body
                .as_ref()
                .is_some_and(|b| b.len() > MAX_PROVIDER_BODY)
            {
                return Err(BrokerError::Invalid(InvalidRequest::ProviderBody));
            }
            let origin = url.origin().ascii_serialization();
            let vault = shared.vault();
            // Taken under the vault mutex, so a later lock always cancels it.
            let cancel = shared.in_flight(vault.generation());
            let value = vault
                .use_for_destination(&call.credential, &origin, |s| {
                    Zeroizing::new(match call.auth {
                        AuthScheme::Bearer => format!("Bearer {}", s.expose(str::to_owned)),
                        _ => s.expose(str::to_owned),
                    })
                })
                .map_err(vault_error)?;
            drop(vault);
            let header = match call.auth {
                AuthScheme::Bearer => "authorization",
                AuthScheme::ApiKeyHeader => "x-api-key",
                AuthScheme::GoogleApiKey => "x-goog-api-key",
            };
            Ok(Outcome::Send(Box::new(Outbound {
                method: call.method,
                url,
                header,
                value,
                anthropic_version: call.anthropic_version,
                body: call.body,
                cancel,
            })))
        }
    }
}

/// Send a prepared request unless the vault is locked first; a lock while it
/// is in flight abandons it.
async fn send(http: &reqwest::Client, out: Outbound) -> Result<Reply, BrokerError> {
    let cancel = out.cancel.clone();
    if cancel.is_cancelled() {
        return Err(BrokerError::Locked);
    }
    tokio::select! {
        reply = transmit(http, out) => reply,
        _ = cancel.cancelled() => Err(BrokerError::Locked),
    }
}

async fn transmit(http: &reqwest::Client, out: Outbound) -> Result<Reply, BrokerError> {
    let mut header = reqwest::header::HeaderValue::from_str(&out.value)
        .map_err(|_| BrokerError::Upstream(UpstreamFailure::InvalidHeader))?;
    header.set_sensitive(true);
    let method = match out.method {
        HttpMethod::Get => reqwest::Method::GET,
        HttpMethod::Post => reqwest::Method::POST,
    };
    let mut request = http.request(method, out.url).header(out.header, header);
    if let Some(version) = out.anthropic_version {
        request = request.header("anthropic-version", version);
    }
    if let Some(body) = out.body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let response = request.send().await.map_err(upstream)?;
    let status = response.status().as_u16();
    // Never relay upstream error bodies: they can echo credentials or prompts.
    if !response.status().is_success() {
        return Ok(Reply::Provider {
            status,
            body: vec![],
        });
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

async fn connection(shared: Arc<Shared>, stream: tokio::net::UnixStream) -> std::io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let mut decoder = Decoder::default();
    let mut session = Session::default();
    let mut bytes = vec![0; 8192];
    loop {
        let count = tokio::select! {
            read = reader.read(&mut bytes) => read?,
            _ = shared.stop.cancelled() => return Ok(()),
        };
        if count == 0 {
            return Ok(());
        }
        let requests: Vec<a206_protocol::Request<Request>> = decoder
            .feed(&bytes[..count])
            .map_err(std::io::Error::other)?;
        for request in requests {
            let shared_for_task = shared.clone();
            // Password derivation is deliberately slow; keep it off the reactor.
            let (returned, outcome) = tokio::task::spawn_blocking(move || {
                let outcome = handle(&shared_for_task, &mut session, request.body);
                (session, outcome)
            })
            .await
            .map_err(std::io::Error::other)?;
            session = returned;
            // Any lock or unlock during this request cancels older sends.
            let generation = shared.vault().generation();
            shared.in_flight(generation);
            let body = match outcome {
                Ok(Outcome::Reply(reply)) => reply,
                Ok(Outcome::Send(out)) => {
                    send(&shared.http, *out).await.unwrap_or_else(Reply::Error)
                }
                Err(error) => {
                    tracing::info!(
                        error = &error as &dyn std::error::Error,
                        "vault broker refused a request"
                    );
                    Reply::Error(error)
                }
            };
            let frame = match encode(&Response {
                id: request.id,
                body,
            }) {
                Err(a206_protocol::Error::TooLarge) => encode(&Response {
                    id: request.id,
                    body: Reply::Error(BrokerError::Upstream(UpstreamFailure::TooLarge)),
                }),
                frame => frame,
            }
            .map_err(std::io::Error::other)?;
            writer.write_all(&frame).await?;
        }
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
        vault: Mutex::new(Vault::locked(dir.join("secrets.json"))),
        throttle: AuthThrottle::default(),
        stop: CancellationToken::new(),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(std::io::Error::other)?,
        in_flight: Mutex::new((0, CancellationToken::new())),
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
    let _ = std::fs::remove_file(&path);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.abort_all();
    drop(lock);
    Ok(())
}
