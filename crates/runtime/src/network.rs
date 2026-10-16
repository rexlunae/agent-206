//! Public download transport. Validate every redirect and pin the checked DNS answers.
//! RustyClaw's SSRF policy is preserved without its DNS re-check race.
use anyhow::{Context, Result, ensure};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
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
fn validate_url(url: &Url) -> Result<()> {
    ensure!(
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none(),
        "downloads require credential-free HTTPS URLs"
    );
    ensure!(
        url.port_or_known_default() == Some(443),
        "public downloads require the standard HTTPS port"
    );
    ensure!(
        url.fragment().is_none(),
        "download URL must not contain a fragment"
    );
    Ok(())
}
pub async fn public_get(source: &str) -> Result<reqwest::Response> {
    let mut url = Url::parse(source)?;
    for hop in 0..=5 {
        validate_url(&url)?;
        let host = url.host_str().context("download hostname missing")?;
        let answers: Vec<SocketAddr> = match url.host() {
            Some(url::Host::Ipv4(address)) => vec![SocketAddr::new(address.into(), 443)],
            Some(url::Host::Ipv6(address)) => vec![SocketAddr::new(address.into(), 443)],
            Some(url::Host::Domain(_)) => tokio::time::timeout(
                Duration::from_secs(10),
                tokio::net::lookup_host((host, 443)),
            )
            .await??
            .collect(),
            None => anyhow::bail!("download hostname missing"),
        };
        ensure!(
            !answers.is_empty() && answers.iter().all(|a| public_address(a.ip())),
            "download destination resolves to a private, reserved or local address"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .https_only(true)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, &answers)
            .build()?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        if response.status().is_redirection() {
            ensure!(hop < 5, "download redirected too many times");
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("download redirect has no destination")?
                .to_str()?;
            url = url.join(location)?;
        } else {
            ensure!(
                response.status().is_success(),
                "download returned HTTP {}",
                response.status()
            );
            return Ok(response);
        }
    }
    anyhow::bail!("download redirected too many times")
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
