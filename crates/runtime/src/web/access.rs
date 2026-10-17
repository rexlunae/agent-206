//! Who may use the web bridge. On loopback a one-time link hands the browser
//! the bridge's bearer. Opened to other devices, browsers sign in with a code
//! from the vault's enrolled authenticator, checked by the vault broker, and
//! each sign-in gets its own expiring bearer.
use a206_core::ErrorKind;
use axum::http::{HeaderMap, header};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// How long a sign-in lasts. A restart of the bridge ends every session.
pub(super) const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
/// Sessions kept at once; signing in beyond this ends the oldest.
const MAX_SESSIONS: usize = 32;
/// Longer input cannot be an authenticator code; it is refused unread.
const MAX_CODE: usize = 16;

pub(super) enum Access {
    Loopback(Loopback),
    Network(Network),
}
pub(super) struct Loopback {
    pub token: String,
    pub bootstrap: String,
    pub claimed: AtomicBool,
    pub origin: String,
    pub host: String,
}
pub(super) struct Network {
    /// Accepted `Host` values (`name:port`); the origin must be `https://` one of them.
    pub hosts: Vec<String>,
    pub root: PathBuf,
    pub sessions: Mutex<Sessions>,
}

/// Why a browser could not sign in.
#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    #[error("this web server signs browsers in with the link printed by `agent206 web`")]
    NotOffered,
    #[error("enter the 6-digit code from your authenticator app")]
    Malformed,
    #[error("the vault broker is not running on the web server's computer; unlock the vault there")]
    BrokerUnavailable(#[source] a206_broker::ClientError),
    #[error("could not check the vault on the web server's computer")]
    Vault(#[source] crate::vault::VaultError),
    #[error("the vault on the web server's computer is locked; unlock it there, then sign in")]
    Locked,
    #[error("the web server's profile has no vault")]
    NoVault,
    #[error("the vault has no authenticator enrolled; enroll one on the web server's computer")]
    TotpNotEnrolled,
    #[error("the authenticator code was not accepted")]
    Rejected(#[source] a206_broker::ClientError),
    #[error("unexpected vault broker reply")]
    UnexpectedReply,
    #[error("signing in from other devices is not available on this platform")]
    Unsupported,
}
impl crate::error::Classified for SignInError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::NotOffered | Self::Malformed => ErrorKind::InvalidRequest,
            Self::BrokerUnavailable(_) | Self::Locked | Self::NoVault | Self::Unsupported => {
                ErrorKind::Unavailable
            }
            Self::TotpNotEnrolled => ErrorKind::PermissionDenied,
            Self::UnexpectedReply => ErrorKind::Protocol,
            // The broker's refusal (rejected, replayed, locked out) and the
            // vault's failure know their kind.
            Self::Rejected(_) | Self::Vault(_) => return None,
        })
    }
}

/// Live sign-ins, by bearer.
#[derive(Default)]
pub(super) struct Sessions {
    expiry: HashMap<String, Instant>,
}
impl Sessions {
    pub fn start(&mut self, now: Instant) -> String {
        self.expiry.retain(|_, expires| *expires > now);
        while self.expiry.len() >= MAX_SESSIONS {
            let Some(oldest) = self
                .expiry
                .iter()
                .min_by_key(|(_, expires)| **expires)
                .map(|(token, _)| token.clone())
            else {
                break;
            };
            self.expiry.remove(&oldest);
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        self.expiry.insert(token.clone(), now + SESSION_LIFETIME);
        token
    }
    pub fn valid(&self, token: &str, now: Instant) -> bool {
        self.expiry.get(token).is_some_and(|expires| *expires > now)
    }
    pub fn end(&mut self, token: &str) {
        self.expiry.remove(token);
    }
}

fn text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}
/// The bearer a request presents, if any.
pub(super) fn bearer(headers: &HeaderMap) -> Option<&str> {
    text(headers, header::AUTHORIZATION).and_then(|value| value.strip_prefix("Bearer "))
}

impl Access {
    /// Whether the request comes from a page this bridge served, in MessagePack.
    /// Sign-in needs this much; every other API also needs a valid bearer.
    pub fn same_origin(&self, headers: &HeaderMap) -> bool {
        let host = text(headers, header::HOST);
        let origin = text(headers, header::ORIGIN);
        let placed = match self {
            Self::Loopback(loopback) => {
                host == Some(loopback.host.as_str()) && origin == Some(loopback.origin.as_str())
            }
            Self::Network(network) => {
                network.serves(headers)
                    && origin.and_then(|origin| origin.strip_prefix("https://")) == host
            }
        };
        placed && text(headers, header::CONTENT_TYPE) == Some("application/msgpack")
    }
    pub fn authorized(&self, headers: &HeaderMap, now: Instant) -> bool {
        self.same_origin(headers)
            && bearer(headers).is_some_and(|token| match self {
                Self::Loopback(loopback) => token == loopback.token,
                Self::Network(network) => network
                    .sessions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .valid(token, now),
            })
    }
    pub fn loopback(&self) -> Option<&Loopback> {
        match self {
            Self::Loopback(loopback) => Some(loopback),
            Self::Network(_) => None,
        }
    }
}
impl Loopback {
    /// Claim the one-time link; only the first claim succeeds.
    pub fn claim(&self) -> bool {
        self.claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}
impl Network {
    /// Whether the request names one of this server's hosts.
    pub fn serves(&self, headers: &HeaderMap) -> bool {
        text(headers, header::HOST).is_some_and(|host| self.hosts.iter().any(|h| h == host))
    }
    /// Check `code` against the vault's authenticator and start a session.
    /// A code is accepted only while the vault is unlocked with an enrolled
    /// authenticator; the broker's throttle and replay protection apply.
    pub async fn sign_in(&self, code: &str) -> Result<String, SignInError> {
        if code.is_empty() || code.len() > MAX_CODE {
            return Err(SignInError::Malformed);
        }
        verify(&self.root, code).await?;
        Ok(self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .start(Instant::now()))
    }
    pub fn sign_out(&self, token: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .end(token);
    }
}

/// Whether this profile's vault can sign browsers in. A locked vault cannot
/// say whether an authenticator is enrolled, so it passes for now and each
/// sign-in checks again.
#[cfg(unix)]
pub(super) async fn ready(root: &Path) -> Result<(), SignInError> {
    let mut client = crate::vault::connect(root)
        .await
        .map_err(SignInError::Vault)?;
    let status = crate::vault::status(&mut client)
        .await
        .map_err(SignInError::Vault)?;
    check(&status).or_else(|error| match error {
        SignInError::Locked => Ok(()),
        error => Err(error),
    })
}
#[cfg(not(unix))]
pub(super) async fn ready(_root: &Path) -> Result<(), SignInError> {
    Err(SignInError::Unsupported)
}

#[cfg(unix)]
fn check(status: &a206_broker::BrokerStatus) -> Result<(), SignInError> {
    if !status.exists {
        Err(SignInError::NoVault)
    } else if !status.unlocked {
        Err(SignInError::Locked)
    } else if status.totp_enrolled != Some(true) {
        Err(SignInError::TotpNotEnrolled)
    } else {
        Ok(())
    }
}

/// A fresh broker connection per attempt, so the step-up it earns is dropped
/// with it. The broker is never started from here: an unauthenticated request
/// must not launch processes, and a broker that is not running has no
/// unlocked vault anyway.
#[cfg(unix)]
async fn verify(root: &Path, code: &str) -> Result<(), SignInError> {
    use a206_broker::{Client, Reply, Request, Role};
    let mut client = Client::connect(root, Role::User)
        .await
        .map_err(SignInError::BrokerUnavailable)?;
    match client.call(Request::Status).await {
        Ok(Reply::Status(status)) => check(&status)?,
        Ok(_) => return Err(SignInError::UnexpectedReply),
        Err(error) => return Err(SignInError::BrokerUnavailable(error)),
    }
    // No password is ever sent, so a vault whose authenticator was removed
    // since the check above refuses instead of falling back to one.
    match client
        .call(Request::StepUp {
            password: None,
            totp: Some(code.to_owned()),
        })
        .await
    {
        Ok(Reply::Ok) => Ok(()),
        Ok(_) => Err(SignInError::UnexpectedReply),
        Err(error) => Err(SignInError::Rejected(error)),
    }
}
#[cfg(not(unix))]
async fn verify(_root: &Path, _code: &str) -> Result<(), SignInError> {
    Err(SignInError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_expire_end_and_stay_bounded() {
        let mut sessions = Sessions::default();
        let start = Instant::now();
        let first = sessions.start(start);
        assert!(sessions.valid(&first, start));
        assert!(!sessions.valid(&first, start + SESSION_LIFETIME));
        assert!(!sessions.valid(&uuid::Uuid::new_v4().simple().to_string(), start));
        let second = sessions.start(start);
        sessions.end(&second);
        assert!(!sessions.valid(&second, start));
        let later = start + Duration::from_secs(1);
        let tokens: Vec<_> = (0..MAX_SESSIONS).map(|_| sessions.start(later)).collect();
        // The oldest sign-in made room for the newest.
        assert!(!sessions.valid(&first, later));
        assert!(tokens.iter().all(|token| sessions.valid(token, later)));
        assert_eq!(sessions.expiry.len(), MAX_SESSIONS);
    }

    fn network(hosts: &[&str]) -> Access {
        Access::Network(Network {
            hosts: hosts.iter().map(|host| host.to_string()).collect(),
            root: std::env::temp_dir().join(uuid::Uuid::new_v4().to_string()),
            sessions: Mutex::default(),
        })
    }
    fn headers(host: &str, origin: &str, bearer: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        headers.insert(header::ORIGIN, origin.parse().unwrap());
        headers.insert(header::CONTENT_TYPE, "application/msgpack".parse().unwrap());
        if let Some(bearer) = bearer {
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {bearer}").parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn network_access_needs_a_listed_https_origin_and_a_live_session() {
        let host = format!("h{}.test:8443", uuid::Uuid::new_v4().simple());
        let access = network(&[&host]);
        let Access::Network(net) = &access else {
            unreachable!()
        };
        let now = Instant::now();
        let token = net.sessions.lock().unwrap().start(now);
        let origin = format!("https://{host}");
        assert!(access.authorized(&headers(&host, &origin, Some(&token)), now));
        // Plain HTTP, another origin, a rebound host name, a stale or missing bearer.
        assert!(!access.authorized(
            &headers(&host, &format!("http://{host}"), Some(&token)),
            now
        ));
        assert!(!access.authorized(
            &headers(&host, "https://other.test:8443", Some(&token)),
            now
        ));
        let rebound = "rebound.test:8443";
        assert!(!access.authorized(
            &headers(rebound, &format!("https://{rebound}"), Some(&token)),
            now
        ));
        assert!(!access.authorized(
            &headers(&host, &origin, Some(&token)),
            now + SESSION_LIFETIME
        ));
        assert!(!access.authorized(&headers(&host, &origin, None), now));
        // Sign-in needs the origin but not a session.
        assert!(access.same_origin(&headers(&host, &origin, None)));
        net.sign_out(&token);
        assert!(!access.authorized(&headers(&host, &origin, Some(&token)), now));
    }

    #[tokio::test]
    async fn malformed_codes_never_reach_the_broker() {
        let Access::Network(net) = network(&["gateway.test:8443"]) else {
            unreachable!()
        };
        for code in [String::new(), "1".repeat(MAX_CODE + 1)] {
            assert!(matches!(
                net.sign_in(&code).await,
                Err(SignInError::Malformed)
            ));
        }
        assert!(net.sessions.lock().unwrap().expiry.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn without_a_running_broker_sign_in_is_refused_and_nothing_starts() {
        let root = tempfile::tempdir().unwrap();
        let net = Network {
            hosts: vec!["gateway.test:8443".into()],
            root: root.path().into(),
            sessions: Mutex::default(),
        };
        let error = net.sign_in("123456").await.unwrap_err();
        assert!(
            matches!(error, SignInError::BrokerUnavailable(_)),
            "{error:?}"
        );
        assert_eq!(crate::error::report(&error).kind, ErrorKind::Unavailable);
        assert!(!a206_broker::socket_path(root.path()).exists());
        assert!(net.sessions.lock().unwrap().expiry.is_empty());
    }
}
