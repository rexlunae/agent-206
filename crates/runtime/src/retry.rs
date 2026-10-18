//! Retrying model requests that failed before the provider sent anything.
//!
//! A request is retried only while nothing of its response has arrived: a
//! connection or timeout failure, or a refusing HTTP status (408, 429, 5xx,
//! and 529, which Anthropic uses for overload). Once any response bytes are
//! received the failure is final, so no text shown to the user is streamed
//! twice and no tool call is requested twice. A retry repeats the same
//! request to the same provider and model: it never switches to another
//! model or account. Each wait honours the server's `Retry-After` (clamped)
//! when the provider sends one directly; brokered requests do not carry the
//! header, so they use the backoff alone.
//!
//! Follows RustyClaw's `retry/policy.rs` (attempts, exponential backoff with
//! a cap, ±20% jitter, a ceiling on `Retry-After`) and its classification of
//! retryable statuses and transport failures.
use crate::provider::ProviderError;
use a206_broker::{BrokerError, ClientError, UpstreamFailure};
use std::time::Duration;

/// How a model request is retried. These are policy choices, not provider
/// facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts in all, the first included. At least one is always made.
    pub attempts: u32,
    /// The wait before the first retry; each later one doubles it.
    pub base: Duration,
    /// The longest backoff wait.
    pub max: Duration,
    /// The longest wait a server's `Retry-After` may ask for.
    pub max_retry_after: Duration,
}
impl RetryPolicy {
    /// Model requests from native runs and compaction.
    pub const DEFAULT: Self = Self {
        attempts: 4,
        base: Duration::from_millis(250),
        max: Duration::from_secs(8),
        max_retry_after: Duration::from_secs(30),
    };
    /// One attempt: every failure is final.
    pub const NONE: Self = Self {
        attempts: 1,
        base: Duration::ZERO,
        max: Duration::ZERO,
        max_retry_after: Duration::ZERO,
    };

    /// The wait before attempt `attempt + 1` after `error` ended attempt
    /// `attempt`, or `None` when the failure is final. `jitter` is in
    /// `[0, 1)` and spreads the backoff by ±20%.
    pub fn delay(&self, error: &ProviderError, attempt: u32, jitter: f64) -> Option<Duration> {
        if attempt >= self.attempts {
            return None;
        }
        let reason = Reason::of(error)?;
        if let Some(wait) = reason.retry_after {
            return Some(wait.min(self.max_retry_after));
        }
        let doubled = self
            .base
            .checked_mul(1 << attempt.saturating_sub(1).min(16))
            .unwrap_or(self.max)
            .min(self.max);
        let spread = doubled.mul_f64(0.8 + 0.4 * jitter.clamp(0.0, 1.0));
        Some(spread.min(self.max))
    }
}

/// Why a failed request may be repeated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reason {
    /// The refusing status, when the provider answered.
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
}
impl Reason {
    /// Whether `error` is a failure worth repeating the request for.
    pub fn of(error: &ProviderError) -> Option<Self> {
        match error {
            ProviderError::Status {
                status,
                retry_after,
            } if matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529) => Some(Self {
                status: Some(*status),
                retry_after: *retry_after,
            }),
            ProviderError::Transport(error)
                if error.is_connect() || error.is_timeout() || error.is_request() =>
            {
                Some(Self::unreachable())
            }
            // The broker reports the same failures for credentialed requests.
            ProviderError::Broker(ClientError::Broker(BrokerError::Upstream(
                UpstreamFailure::Connect | UpstreamFailure::Timeout,
            ))) => Some(Self::unreachable()),
            _ => None,
        }
    }
    fn unreachable() -> Self {
        Self {
            status: None,
            retry_after: None,
        }
    }
}

/// A server's `Retry-After` in seconds; an HTTP date is not used, since
/// that would depend on the local clock agreeing with the server's.
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(status: u16, retry_after: Option<Duration>) -> ProviderError {
        ProviderError::Status {
            status,
            retry_after,
        }
    }
    fn policy() -> RetryPolicy {
        let unit = 1 + u64::from(uuid::Uuid::new_v4().as_u128() as u8);
        RetryPolicy {
            attempts: 4,
            base: Duration::from_millis(unit),
            max: Duration::from_millis(unit * 3),
            max_retry_after: Duration::from_millis(unit * 5),
        }
    }

    #[test]
    fn backoff_doubles_up_to_its_cap_and_stops_after_the_last_attempt() {
        let policy = policy();
        let busy = status(503, None);
        let at = |attempt| policy.delay(&busy, attempt, 0.5);
        assert_eq!(at(1), Some(policy.base));
        assert_eq!(at(2), Some(policy.base * 2));
        assert_eq!(at(3), Some(policy.max));
        assert_eq!(at(4), None);
        // Jitter never stretches a wait past the cap.
        assert_eq!(policy.delay(&busy, 3, 1.0), Some(policy.max));
        assert_eq!(RetryPolicy::NONE.delay(&busy, 1, 0.5), None);
    }

    #[test]
    fn jitter_spreads_the_backoff_by_a_fifth() {
        let policy = policy();
        let busy = status(429, None);
        let low = policy.delay(&busy, 1, 0.0).unwrap();
        let high = policy.delay(&busy, 1, 1.0).unwrap();
        assert_eq!(low, policy.base.mul_f64(0.8));
        assert_eq!(high, policy.base.mul_f64(1.2));
    }

    #[test]
    fn a_servers_retry_after_is_honoured_up_to_its_ceiling() {
        let policy = policy();
        let short = policy.max_retry_after / 2;
        assert_eq!(policy.delay(&status(429, Some(short)), 1, 0.5), Some(short));
        let long = policy.max_retry_after * 2;
        assert_eq!(
            policy.delay(&status(503, Some(long)), 1, 0.5),
            Some(policy.max_retry_after)
        );
    }

    #[test]
    fn only_transient_failures_are_retried() {
        let policy = policy();
        for code in [408, 429, 500, 502, 503, 504, 529] {
            assert!(
                policy.delay(&status(code, None), 1, 0.5).is_some(),
                "{code}"
            );
        }
        for code in [400, 401, 403, 404, 413, 422, 501] {
            assert!(
                policy.delay(&status(code, None), 1, 0.5).is_none(),
                "{code}"
            );
        }
        let upstream =
            |failure| ProviderError::Broker(ClientError::Broker(BrokerError::Upstream(failure)));
        for failure in [UpstreamFailure::Connect, UpstreamFailure::Timeout] {
            assert!(
                policy.delay(&upstream(failure), 1, 0.5).is_some(),
                "{failure:?}"
            );
        }
        for error in [
            upstream(UpstreamFailure::Transfer),
            upstream(UpstreamFailure::TooLarge),
            upstream(UpstreamFailure::InvalidHeader),
            ProviderError::Broker(ClientError::Closed),
            ProviderError::Incomplete,
            ProviderError::ResponseTooLarge,
            ProviderError::Upstream { code: None },
            ProviderError::NotPaired,
        ] {
            assert!(policy.delay(&error, 1, 0.5).is_none(), "{error:?}");
        }
    }

    #[test]
    fn retry_after_reads_seconds_only() {
        let seconds = u64::from(uuid::Uuid::new_v4().as_u128() as u16);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            seconds.to_string().parse().unwrap(),
        );
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(seconds)));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&headers), None);
        assert_eq!(retry_after(&Default::default()), None);
    }
}
