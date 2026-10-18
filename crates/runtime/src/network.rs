//! Public download transport. Validate every redirect and pin the checked DNS answers.
//! RustyClaw's SSRF policy is preserved without its DNS re-check race.
use a206_core::ErrorKind;
use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    time::Duration,
};
use url::Url;

pub fn public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_v4(address),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return public_v4(mapped);
            }
            let segments = address.segments();
            // Global unicast only; exclude documentation and IPv4 transition tunnels.
            segments[0] & 0xe000 == 0x2000
                && !(segments[0] == 0x2001 && (segments[1] == 0 || segments[1] == 0x0db8))
                && segments[0] != 0x2002
        }
    }
}
fn public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && b & 0xc0 == 64)
        || (a == 169 && b == 254)
        || (a == 172 && b & 0xf0 == 16)
        || (a == 192 && ((b == 0 && (c == 0 || c == 2)) || b == 168 || (b == 88 && c == 99)))
        || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        || (a == 203 && b == 0 && c == 113))
}
/// Why a public download was refused or failed. URLs are never included:
/// download links can carry signed tokens.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("invalid download URL")]
    InvalidUrl(#[source] url::ParseError),
    #[error("downloads require credential-free HTTPS URLs")]
    InsecureUrl,
    #[error("public downloads require the standard HTTPS port")]
    NonstandardPort,
    #[error("download URL must not contain a fragment")]
    Fragment,
    #[error("download hostname missing")]
    MissingHost,
    #[error("could not resolve the download host")]
    Resolve(#[source] std::io::Error),
    #[error("resolving the download host timed out")]
    ResolveTimeout(#[source] tokio::time::error::Elapsed),
    #[error("download destination resolves to a private, reserved or local address")]
    PrivateAddress,
    #[error("the destination host is not one the request may reach")]
    Destination,
    #[error("download redirected too many times")]
    TooManyRedirects,
    #[error("download redirect has no valid destination")]
    InvalidRedirect,
    #[error("download redirect destination is not a valid URL")]
    RedirectTarget(#[source] url::ParseError),
    #[error("download returned HTTP {status}")]
    Status { status: reqwest::StatusCode },
    #[error("download transport failed")]
    Transport(#[source] reqwest::Error),
}
impl crate::error::Classified for DownloadError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::InvalidUrl(_)
            | Self::InsecureUrl
            | Self::NonstandardPort
            | Self::Fragment
            | Self::MissingHost => ErrorKind::InvalidRequest,
            Self::PrivateAddress | Self::Destination => ErrorKind::PermissionDenied,
            Self::Resolve(_) => ErrorKind::Unavailable,
            Self::ResolveTimeout(_) => ErrorKind::Timeout,
            Self::Transport(error) if error.is_timeout() => ErrorKind::Timeout,
            Self::Transport(error) if error.is_connect() => ErrorKind::Unavailable,
            Self::TooManyRedirects
            | Self::InvalidRedirect
            | Self::RedirectTarget(_)
            | Self::Status { .. }
            | Self::Transport(_) => ErrorKind::Provider,
        })
    }
}
fn validate_url(url: &Url) -> Result<(), DownloadError> {
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(DownloadError::InsecureUrl);
    }
    if url.port_or_known_default() != Some(443) {
        return Err(DownloadError::NonstandardPort);
    }
    if url.fragment().is_some() {
        return Err(DownloadError::Fragment);
    }
    Ok(())
}
/// The transport error without its URL, which can carry signed tokens.
pub(crate) fn transport(error: reqwest::Error) -> DownloadError {
    DownloadError::Transport(error.without_url())
}
/// A future a [`Net`] returns.
pub(crate) type Pending<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// DNS and HTTPS as public requests use them. The policy in
/// [`public_request`] applies to every implementation; tests substitute
/// one that never touches the network.
pub(crate) trait Net: Send + Sync {
    fn resolve<'a>(&'a self, host: &'a str) -> Pending<'a, std::io::Result<Vec<SocketAddr>>>;
    /// One `GET` to `url`, connecting only to the checked `answers`.
    fn get<'a>(
        &'a self,
        url: &'a Url,
        host: &'a str,
        answers: &'a [SocketAddr],
    ) -> Pending<'a, Result<reqwest::Response, reqwest::Error>>;
}
/// The real network.
pub(crate) struct Internet;
impl Net for Internet {
    fn resolve<'a>(&'a self, host: &'a str) -> Pending<'a, std::io::Result<Vec<SocketAddr>>> {
        Box::pin(async move { Ok(tokio::net::lookup_host((host, 443)).await?.collect()) })
    }
    fn get<'a>(
        &'a self,
        url: &'a Url,
        host: &'a str,
        answers: &'a [SocketAddr],
    ) -> Pending<'a, Result<reqwest::Response, reqwest::Error>> {
        Box::pin(async move {
            reqwest::Client::builder()
                .no_proxy()
                .https_only(true)
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .resolve_to_addrs(host, answers)
                .build()?
                .get(url.clone())
                .send()
                .await
        })
    }
}

/// A successful public download.
pub async fn public_get(source: &str) -> Result<reqwest::Response, DownloadError> {
    let (response, _) = public_request(source, |_| true, &Internet).await?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(DownloadError::Status {
            status: response.status(),
        })
    }
}

/// The final non-redirect response of a public `GET`, whatever its status.
/// Every hop must be a credential-free HTTPS URL on port 443 whose host
/// `allowed` accepts and whose every resolved address is public; the
/// connection is pinned to those checked addresses. Also says whether a
/// redirect was followed.
pub(crate) async fn public_request(
    source: &str,
    allowed: impl Fn(&str) -> bool,
    net: &dyn Net,
) -> Result<(reqwest::Response, bool), DownloadError> {
    let mut url = Url::parse(source).map_err(DownloadError::InvalidUrl)?;
    for hop in 0..=5 {
        validate_url(&url)?;
        let host = url.host_str().ok_or(DownloadError::MissingHost)?;
        if !allowed(host) {
            return Err(DownloadError::Destination);
        }
        let answers: Vec<SocketAddr> = match url.host() {
            Some(url::Host::Ipv4(address)) => vec![SocketAddr::new(address.into(), 443)],
            Some(url::Host::Ipv6(address)) => vec![SocketAddr::new(address.into(), 443)],
            Some(url::Host::Domain(_)) => {
                tokio::time::timeout(Duration::from_secs(10), net.resolve(host))
                    .await
                    .map_err(DownloadError::ResolveTimeout)?
                    .map_err(DownloadError::Resolve)?
            }
            None => return Err(DownloadError::MissingHost),
        };
        if answers.is_empty() || !answers.iter().all(|a| public_address(a.ip())) {
            return Err(DownloadError::PrivateAddress);
        }
        let response = net.get(&url, host, &answers).await.map_err(transport)?;
        // Only statuses that mean "go there" are followed; 300, 304 and the
        // like are final answers.
        use reqwest::StatusCode as S;
        let redirect = matches!(
            response.status(),
            S::MOVED_PERMANENTLY
                | S::FOUND
                | S::SEE_OTHER
                | S::TEMPORARY_REDIRECT
                | S::PERMANENT_REDIRECT
        );
        if !redirect {
            return Ok((response, hop > 0));
        }
        if hop >= 5 {
            return Err(DownloadError::TooManyRedirects);
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|location| location.to_str().ok())
            .ok_or(DownloadError::InvalidRedirect)?;
        url = url.join(location).map_err(DownloadError::RedirectTarget)?;
    }
    Err(DownloadError::TooManyRedirects)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_private_reserved_and_mapped_addresses() {
        for ip in [
            "0.0.0.1",
            "127.0.0.1",
            "10.1.2.3",
            "100.64.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.2.3",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2002:7f00:1::",
        ] {
            assert!(!public_address(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_address(ip.parse().unwrap()), "{ip}");
        }
    }
    #[test]
    fn redirects_cannot_downgrade_or_carry_credentials() {
        for url in [
            "http://example.org/file",
            "https://u:p@example.org/file",
            "https://example.org:444/file",
        ] {
            assert!(validate_url(&Url::parse(url).unwrap()).is_err());
        }
    }
}
