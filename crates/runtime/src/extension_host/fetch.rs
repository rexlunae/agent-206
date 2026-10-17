//! The `fetch` host call: a public HTTPS `GET` through the SSRF guard in
//! [`crate::network`], limited to the hosts the extension's manifest
//! declares. Replies never carry the URL back, and failures never name it:
//! links can carry signed tokens.
use super::failure;
use crate::network::{DownloadError, Net, public_request};
use a206_extensions::abi::{FetchResponse, HostErrorCode, HostFailure, HostReply};
use std::time::Duration;

/// The most body bytes a fetch returns; the rest is reported as truncated.
pub const MAX_FETCH: usize = 512 * 1024;
/// The whole fetch, redirects included, within the invocation's own budget.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Longest `Content-Type` value passed on.
const MAX_CONTENT_TYPE: usize = 256;

pub(super) async fn fetch(
    net: &dyn Net,
    hosts: &[String],
    url: &str,
) -> Result<HostReply, HostFailure> {
    let allowed = |host: &str| a206_extensions::allows_host(hosts, host);
    let response = tokio::time::timeout(FETCH_TIMEOUT, async {
        let mut response = public_request(url, allowed, net).await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= MAX_CONTENT_TYPE)
            .map(str::to_owned);
        let mut body = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = response.chunk().await.map_err(crate::network::transport)? {
            let room = MAX_FETCH - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok::<_, DownloadError>(FetchResponse {
            status,
            content_type,
            body: body.into(),
            truncated,
        })
    })
    .await;
    match response {
        Ok(Ok(response)) => Ok(HostReply::Fetched(response)),
        Ok(Err(error)) => Err(fetch_failure(&error)),
        Err(_) => Err(failure(HostErrorCode::Network, "the request timed out")),
    }
}

fn fetch_failure(error: &DownloadError) -> HostFailure {
    use DownloadError as E;
    tracing::warn!(
        error = error as &dyn std::error::Error,
        "extension fetch failed"
    );
    let (code, message) = match error {
        E::InvalidUrl(_) | E::RedirectTarget(_) | E::InvalidRedirect | E::MissingHost => {
            (HostErrorCode::Invalid, "the URL or a redirect is not valid")
        }
        E::InsecureUrl | E::NonstandardPort | E::Fragment => (
            HostErrorCode::Invalid,
            "only credential-free HTTPS URLs on the standard port, without a fragment",
        ),
        E::Destination => (
            HostErrorCode::Denied,
            "the extension does not declare this host",
        ),
        E::PrivateAddress => (
            HostErrorCode::Denied,
            "the host resolves to a private, reserved or local address",
        ),
        E::Resolve(_) | E::ResolveTimeout(_) => {
            (HostErrorCode::Network, "the host could not be resolved")
        }
        E::TooManyRedirects => (HostErrorCode::Network, "too many redirects"),
        // `public_request` returns every final status as a response.
        E::Status { .. } | E::Transport(_) => {
            (HostErrorCode::Network, "the request could not be completed")
        }
    };
    failure(code, message)
}
