//! What the native `git_push` tool needs from the gateway: the git
//! credential the user configured for a forge, and the vault broker that
//! attaches it.
use super::{Gateway, GatewayError, Result};
use crate::error::check;
use a206_core::{BrokerPairing, GitCredential};

/// Most forges the user may set git credentials for.
pub const MAX_GIT_CREDENTIALS: usize = 32;

/// Check the git credentials of a settings save, and write each origin in
/// the form the vault binds credentials to.
pub(super) fn validate(entries: &mut [GitCredential]) -> Result<()> {
    check(
        entries.len() <= MAX_GIT_CREDENTIALS,
        GatewayError::Invalid("at most 32 git credentials may be set"),
    )?;
    let mut origins = std::collections::BTreeSet::new();
    for entry in entries.iter_mut() {
        let invalid = || {
            GatewayError::Invalid(
                "a git credential's origin must be https://host[:port], or http:// on a loopback host",
            )
        };
        let url = url::Url::parse(entry.origin.trim()).map_err(|_| invalid())?;
        let loopback = matches!(
            url.host(),
            Some(url::Host::Domain("localhost"))
                | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
                | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
        );
        check(
            (url.scheme() == "https" || url.scheme() == "http" && loopback)
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            invalid(),
        )?;
        entry.origin = url.origin().ascii_serialization();
        check(
            origins.insert(entry.origin.clone()),
            GatewayError::Invalid("each origin may have only one git credential"),
        )?;
        let username = &entry.username;
        check(
            !username.is_empty()
                && username.chars().count() <= 256
                && !username.chars().any(|c| c == ':' || c.is_control()),
            GatewayError::Invalid(
                "a git username must be 1 to 256 characters without a colon or control characters",
            ),
        )?;
        let credential = &entry.credential;
        check(
            !credential.trim().is_empty()
                && credential.len() <= 256
                && !credential.chars().any(char::is_control),
            GatewayError::Invalid("a git credential needs a vault credential name"),
        )?;
    }
    Ok(())
}

/// The gateway, as `git_push` sees it.
#[derive(Clone)]
pub struct Pushes {
    base: Gateway,
}
impl Gateway {
    pub(crate) fn pushes(&self) -> Pushes {
        let mut base = self.clone();
        base.tool_hosts = Default::default();
        base.delegation = Default::default();
        Pushes { base }
    }
}
impl Pushes {
    /// The credential the user chose for repositories on `origin`.
    pub(crate) fn credential(&self, origin: &str) -> Result<Option<GitCredential>> {
        Ok(self
            .base
            .lock()?
            .state
            .settings
            .git_credentials
            .iter()
            .find(|entry| entry.origin == origin)
            .cloned())
    }
    /// Whether the user has set a git credential for any forge.
    pub(crate) fn configured(&self) -> Result<bool> {
        Ok(!self.base.lock()?.state.settings.git_credentials.is_empty())
    }
    /// The profile whose broker holds the vault, and this unlock's pairing.
    pub(crate) fn broker(&self) -> Result<(std::path::PathBuf, Option<BrokerPairing>)> {
        let root = self.base.lock()?.root.clone();
        Ok((root, self.base.pairing()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(origin: &str) -> GitCredential {
        let name = || uuid::Uuid::new_v4().simple().to_string();
        GitCredential {
            origin: origin.into(),
            username: name(),
            credential: name(),
        }
    }

    #[test]
    fn git_credentials_are_saved_by_origin_or_refused() {
        let host = format!("{}.example", uuid::Uuid::new_v4().simple());
        let mut entries = vec![
            entry(&format!(" HTTPS://{host}:443 ")),
            entry("http://127.0.0.1:8080"),
        ];
        validate(&mut entries).unwrap();
        assert_eq!(entries[0].origin, format!("https://{host}"));
        assert_eq!(entries[1].origin, "http://127.0.0.1:8080");

        let invalid = |entries: &mut [GitCredential]| {
            assert!(
                matches!(validate(entries), Err(GatewayError::Invalid(_))),
                "{entries:?}"
            );
        };
        for origin in [
            format!("http://{host}"),
            format!("https://{host}/team/repo.git"),
            format!("https://user@{host}"),
            format!("https://{host}/?q"),
            format!("ssh://{host}"),
            host.clone(),
        ] {
            invalid(&mut [entry(&origin)]);
        }
        invalid(&mut [
            entry(&format!("https://{host}")),
            entry(&format!("https://{host}/")),
        ]);
        let mut bad = entry(&format!("https://{host}"));
        bad.username = format!("{}:{}", bad.username, bad.credential);
        invalid(&mut [bad.clone()]);
        bad.username = String::new();
        invalid(&mut [bad]);
        let mut bad = entry(&format!("https://{host}"));
        bad.credential = " ".into();
        invalid(&mut [bad]);
        let mut many: Vec<_> = (0..=MAX_GIT_CREDENTIALS)
            .map(|i| entry(&format!("https://h{i}.{host}")))
            .collect();
        invalid(&mut many);
        many.pop();
        validate(&mut many).unwrap();
    }
}
