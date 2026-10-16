//! Authentication throttling and step-up windows. Limits match RustyClaw's
//! gateway `auth.rs` and `secrets_handler.rs`: 3 failures within 60 seconds
//! lock for 30 seconds; a reveal step-up lasts 120 seconds.
use std::{
    collections::HashMap,
    hash::Hash,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("too many failed attempts; retry in {} seconds", retry_after.as_secs().max(1))]
pub struct Lockout {
    pub retry_after: Duration,
}

struct Attempts {
    failures: u32,
    first_failure: Instant,
    locked_until: Option<Instant>,
}

/// Failure tracking shared by every connection to one broker. Keyed by a
/// stable principal (profile, peer identity), so reconnecting does not reset it.
pub struct AuthThrottle<K> {
    attempts: Mutex<HashMap<K, Attempts>>,
}

impl<K> Default for AuthThrottle<K> {
    fn default() -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Hash + Eq + Clone> AuthThrottle<K> {
    pub const MAX_FAILURES: u32 = 3;
    pub const WINDOW: Duration = Duration::from_secs(60);
    pub const LOCKOUT: Duration = Duration::from_secs(30);

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<K, Attempts>> {
        // A panic while holding the lock cannot leave a weaker state than the
        // recorded counters, so keep enforcing them.
        self.attempts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Call before verifying a credential; a lockout must reject the attempt
    /// without evaluating it.
    pub fn check(&self, key: &K, now: Instant) -> Result<(), Lockout> {
        let mut table = self.table();
        let Some(entry) = table.get(key) else {
            return Ok(());
        };
        if let Some(until) = entry.locked_until {
            if now < until {
                return Err(Lockout {
                    retry_after: until - now,
                });
            }
            table.remove(key);
        } else if now.saturating_duration_since(entry.first_failure) > Self::WINDOW {
            table.remove(key);
        }
        Ok(())
    }

    /// Record a rejected attempt; returns the lockout it triggered, if any.
    pub fn failure(&self, key: &K, now: Instant) -> Option<Lockout> {
        let mut table = self.table();
        let entry = table.entry(key.clone()).or_insert(Attempts {
            failures: 0,
            first_failure: now,
            locked_until: None,
        });
        if let Some(until) = entry.locked_until {
            if now < until {
                return Some(Lockout {
                    retry_after: until - now,
                });
            }
            *entry = Attempts {
                failures: 0,
                first_failure: now,
                locked_until: None,
            };
        } else if now.saturating_duration_since(entry.first_failure) > Self::WINDOW {
            entry.failures = 0;
            entry.first_failure = now;
        }
        entry.failures += 1;
        (entry.failures >= Self::MAX_FAILURES).then(|| {
            entry.locked_until = Some(now + Self::LOCKOUT);
            Lockout {
                retry_after: Self::LOCKOUT,
            }
        })
    }

    pub fn success(&self, key: &K) {
        self.table().remove(key);
    }
}

/// A short-lived reveal authorization for one connection. Lock, reconnect and
/// expiry each end it; it never authorizes agent use.
#[derive(Debug, Default)]
pub struct StepUpWindow {
    granted_at: Option<Instant>,
}

impl StepUpWindow {
    pub const DURATION: Duration = Duration::from_secs(120);

    /// Call only after a fresh password/TOTP verification succeeded.
    pub fn grant(&mut self, now: Instant) {
        self.granted_at = Some(now);
    }
    pub fn revoke(&mut self) {
        self.granted_at = None;
    }
    pub fn is_active(&self, now: Instant) -> bool {
        self.granted_at
            .is_some_and(|at| now >= at && now.duration_since(at) < Self::DURATION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type Throttle = AuthThrottle<String>;

    #[test]
    fn locks_after_repeated_failures_and_recovers() {
        let throttle = Throttle::default();
        let key = uuid::Uuid::new_v4().to_string();
        let t0 = Instant::now();
        assert!(throttle.failure(&key, t0).is_none());
        assert!(
            throttle
                .failure(&key, t0 + Duration::from_secs(1))
                .is_none()
        );
        let lock = throttle.failure(&key, t0 + Duration::from_secs(2)).unwrap();
        assert_eq!(lock.retry_after, Throttle::LOCKOUT);
        let during = t0 + Duration::from_secs(2) + Throttle::LOCKOUT / 2;
        assert_eq!(
            throttle.check(&key, during).unwrap_err().retry_after,
            Throttle::LOCKOUT / 2
        );
        // Failures during a lockout neither shorten nor extend it.
        throttle.failure(&key, during);
        assert!(throttle.check(&key, during).is_err());
        let after = t0 + Duration::from_secs(2) + Throttle::LOCKOUT;
        assert!(throttle.check(&key, after).is_ok());
        assert!(throttle.failure(&key, after).is_none());
    }

    #[test]
    fn failures_outside_the_window_do_not_accumulate() {
        let throttle = Throttle::default();
        let key = uuid::Uuid::new_v4().to_string();
        let t0 = Instant::now();
        throttle.failure(&key, t0);
        throttle.failure(&key, t0 + Duration::from_secs(1));
        let later = t0 + Throttle::WINDOW + Duration::from_secs(2);
        assert!(throttle.failure(&key, later).is_none());
        assert!(throttle.check(&key, later).is_ok());
    }

    #[test]
    fn principals_are_independent_and_success_clears() {
        let throttle = Throttle::default();
        let (a, b) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let t0 = Instant::now();
        for _ in 0..Throttle::MAX_FAILURES {
            throttle.failure(&a, t0);
        }
        assert!(throttle.check(&a, t0).is_err());
        assert!(throttle.check(&b, t0).is_ok());
        throttle.failure(&b, t0);
        throttle.failure(&b, t0);
        throttle.success(&b);
        assert!(throttle.failure(&b, t0).is_none());
    }

    #[test]
    fn step_up_expires() {
        let mut window = StepUpWindow::default();
        let t0 = Instant::now();
        assert!(!window.is_active(t0));
        window.grant(t0);
        assert!(window.is_active(t0 + StepUpWindow::DURATION - Duration::from_millis(1)));
        assert!(!window.is_active(t0 + StepUpWindow::DURATION));
    }
}
