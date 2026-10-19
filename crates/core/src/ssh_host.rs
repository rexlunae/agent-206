//! SSH servers as vault destinations: an SSH key credential is bound to
//! exact `ssh://host:port` origins, and the vault signs only for those.
use serde::{Deserialize, Serialize};

/// The port SSH listens on unless a destination names another.
pub const SSH_PORT: u16 = 22;
/// Longest host name accepted (DNS limit).
const MAX_HOST: usize = 253;

/// Why a destination is not an SSH server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum SshHostError {
    #[error("write an SSH server as ssh://host or ssh://host:port, without a user or path")]
    Shape,
}

/// An SSH server: a lowercase host name or IP address (IPv6 in brackets)
/// and a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    pub host: String,
    pub port: u16,
}
impl SshHost {
    /// Parse a destination the user enters: `ssh://host[:port]`.
    pub fn parse(destination: &str) -> Result<Self, SshHostError> {
        let url = url::Url::parse(destination).map_err(|_| SshHostError::Shape)?;
        if url.scheme() != "ssh"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err(SshHostError::Shape);
        }
        let host = url.host_str().ok_or(SshHostError::Shape)?;
        Self::new(host, url.port().unwrap_or(SSH_PORT))
    }
    /// A host as a remote or the user names it, which must be a plain host
    /// name or IP address (IPv6 with or without brackets).
    pub fn new(host: &str, port: u16) -> Result<Self, SshHostError> {
        let host = host.to_ascii_lowercase();
        let bare = if host.starts_with('[') || host.ends_with(']') {
            host.strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .ok_or(SshHostError::Shape)?
        } else {
            host.as_str()
        };
        let host = if bare.contains(':') {
            bare.parse::<std::net::Ipv6Addr>()
                .map_err(|_| SshHostError::Shape)?;
            format!("[{bare}]")
        } else {
            let valid = !bare.is_empty()
                && bare.len() <= MAX_HOST
                && bare == host
                && !bare.starts_with(['-', '.'])
                && bare
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
            if !valid {
                return Err(SshHostError::Shape);
            }
            host
        };
        if port == 0 {
            return Err(SshHostError::Shape);
        }
        Ok(Self { host, port })
    }
    /// The exact destination an SSH key for this server is bound to.
    pub fn origin(&self) -> String {
        format!("ssh://{}:{}", self.host, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    #[test]
    fn destinations_name_one_server_exactly() {
        let host = format!("{}.test", name());
        let parse = |d: &str| SshHost::parse(d).map(|h| h.origin());
        assert_eq!(
            parse(&format!("ssh://{host}")),
            Ok(format!("ssh://{host}:22"))
        );
        assert_eq!(
            parse(&format!("ssh://{}:2222/", host.to_uppercase())),
            Ok(format!("ssh://{host}:2222"))
        );
        assert_eq!(parse("ssh://[::1]:22"), Ok("ssh://[::1]:22".into()));
        assert_eq!(
            SshHost::new("::1", 22).map(|h| h.origin()),
            Ok("ssh://[::1]:22".into())
        );
        for bad in [
            format!("ssh://git@{host}"),
            format!("ssh://{host}/repo.git"),
            format!("ssh://{host}:0"),
            format!("ssh://{host}?x=1"),
            format!("https://{host}"),
            format!("{host}:22"),
            "ssh://".into(),
        ] {
            assert_eq!(parse(&bad), Err(SshHostError::Shape), "{bad}");
        }
        for bad in ["-oProxyCommand=x", "", "a b", ".x", "[1::2", "a;b"] {
            assert_eq!(SshHost::new(bad, 22), Err(SshHostError::Shape), "{bad}");
        }
    }
}
