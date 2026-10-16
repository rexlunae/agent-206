//! Vault-held TOTP (RFC 6238: SHA-1, 6 digits, 30-second steps, ±1 step),
//! stored under RustyClaw's key so imported vaults keep requiring 2FA.
//! Enrollment commits only after a valid code; accepted steps cannot replay.
use crate::{Error, Secret, Vault};
use std::time::{SystemTime, UNIX_EPOCH};
use totp_rs::{Algorithm, Secret as Seed, TOTP};
use zeroize::Zeroizing;

const SEED_KEY: &str = "__rustyclaw_totp_secret";
const LAST_STEP_KEY: &str = "__a206_totp_last_step";
const STEP: u64 = 30;
/// Diagnostic-only scan (±10 minutes) to report clock drift; never accepted.
const DRIFT_SCAN_STEPS: i64 = 20;
const ENROLLMENT_TTL: u64 = 600;
const ISSUER: &str = "Agent 206";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TotpOutcome {
    Valid,
    Invalid,
    /// The code was already accepted for this or a later step.
    Replayed,
    /// The code matches a step outside the accepted window: the clocks disagree.
    ClockDrift {
        steps: i64,
    },
}

/// An enrollment that exists only in broker memory until confirmed.
pub struct PendingTotp {
    seed: Zeroizing<Vec<u8>>,
    account: String,
    expires_at: u64,
}

impl std::fmt::Debug for PendingTotp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingTotp")
            .field("account", &self.account)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl PendingTotp {
    fn totp(&self) -> Result<TOTP, Error> {
        totp(self.seed.to_vec(), &self.account)
    }
    /// The `otpauth://` URI for a QR code, for the trusted setup surface only.
    pub fn otpauth_url(&self) -> Result<Secret, Error> {
        Ok(Secret::new(self.totp()?.get_url()))
    }
    /// Base32 seed for manual entry, for the trusted setup surface only.
    pub fn manual_key(&self) -> Secret {
        Secret::new(Seed::Raw(self.seed.to_vec()).to_encoded().to_string())
    }
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

fn totp(seed: Vec<u8>, account: &str) -> Result<TOTP, Error> {
    TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        STEP,
        seed,
        Some(ISSUER.to_owned()),
        account.to_owned(),
    )
    .map_err(|_| Error::Corrupted)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn normalize(code: &str) -> Option<String> {
    let digits: String = code.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Steps within ±1 whose code matches, newest first.
fn matching_steps(totp: &TOTP, code: &str, now: u64) -> Vec<u64> {
    let mut steps: Vec<u64> = [1i64, 0, -1]
        .into_iter()
        .filter_map(|off| now.checked_add_signed(off * STEP as i64))
        .filter(|t| same(&totp.generate(*t), code))
        .map(|t| t / STEP)
        .collect();
    steps.dedup();
    steps
}

fn drift(totp: &TOTP, code: &str, now: u64) -> TotpOutcome {
    for offset in 2..=DRIFT_SCAN_STEPS {
        for steps in [offset, -offset] {
            if let Some(t) = now.checked_add_signed(steps * STEP as i64)
                && same(&totp.generate(t), code)
            {
                return TotpOutcome::ClockDrift { steps };
            }
        }
    }
    TotpOutcome::Invalid
}

impl Vault {
    /// Derived from encrypted vault state, never from an editable config flag.
    pub fn totp_enrolled(&self) -> Result<bool, Error> {
        self.has_raw(SEED_KEY)
    }

    /// Generate a seed in broker memory. Nothing is stored until confirmation.
    pub fn begin_totp_enrollment(&self, account: &str, now: u64) -> Result<PendingTotp, Error> {
        if self.totp_enrolled()? {
            return Err(Error::TotpEnrolled);
        }
        let Seed::Raw(seed) = Seed::generate_secret() else {
            return Err(Error::Crypto);
        };
        let pending = PendingTotp {
            seed: Zeroizing::new(seed),
            account: account.to_owned(),
            expires_at: now + ENROLLMENT_TTL,
        };
        pending.totp()?;
        Ok(pending)
    }

    /// Commit an enrollment only when the user proves their authenticator
    /// produces a current code. The confirming step cannot be replayed later.
    pub fn confirm_totp_enrollment(
        &mut self,
        pending: &PendingTotp,
        code: &str,
        now: u64,
    ) -> Result<(), Error> {
        if self.totp_enrolled()? {
            return Err(Error::TotpEnrolled);
        }
        if now >= pending.expires_at {
            return Err(Error::EnrollmentExpired);
        }
        let code = normalize(code).ok_or(Error::TotpRejected)?;
        let step = *matching_steps(&pending.totp()?, &code, now)
            .first()
            .ok_or(Error::TotpRejected)?;
        let encoded = Zeroizing::new(Seed::Raw(pending.seed.to_vec()).to_encoded().to_string());
        self.commit(|inner| {
            inner.set(SEED_KEY, encoded.as_str());
            inner.set(LAST_STEP_KEY, step.to_string().as_str());
        })
    }

    /// Verify a code and consume its step on success.
    pub fn verify_totp(&mut self, code: &str, now: u64) -> Result<TotpOutcome, Error> {
        let seed = self.raw(SEED_KEY)?.ok_or(Error::TotpNotEnrolled)?;
        let seed = seed
            .expose(|s| Seed::Encoded(s.to_owned()).to_bytes())
            .map_err(|_| Error::Corrupted)?;
        let totp = totp(seed, "")?;
        let Some(code) = normalize(code) else {
            return Ok(TotpOutcome::Invalid);
        };
        let last = match self.raw(LAST_STEP_KEY)? {
            Some(v) => v
                .expose(|v| v.parse::<u64>())
                .map_err(|_| Error::Corrupted)?,
            None => 0,
        };
        let steps = matching_steps(&totp, &code, now);
        let Some(step) = steps.iter().copied().find(|s| *s > last) else {
            return Ok(if steps.is_empty() {
                drift(&totp, &code, now)
            } else {
                TotpOutcome::Replayed
            });
        };
        self.commit(|inner| inner.set(LAST_STEP_KEY, step.to_string().as_str()))?;
        Ok(TotpOutcome::Valid)
    }

    /// Removing the factor requires a current, unreplayed code.
    pub fn remove_totp(&mut self, code: &str, now: u64) -> Result<(), Error> {
        if self.verify_totp(code, now)? != TotpOutcome::Valid {
            return Err(Error::TotpRejected);
        }
        self.commit(|inner| {
            crate::forget(inner, SEED_KEY);
            crate::forget(inner, LAST_STEP_KEY);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::unlocked;

    fn code_at(pending: &PendingTotp, t: u64) -> String {
        pending.totp().unwrap().generate(t)
    }
    fn now() -> u64 {
        // Align to a step start so ±1 arithmetic in the tests is unambiguous.
        unix_now() / STEP * STEP
    }

    #[test]
    fn enrollment_commits_only_after_a_valid_code() {
        let (_dir, mut vault, password) = unlocked();
        let t = now();
        let account = uuid::Uuid::new_v4().to_string();
        let pending = vault.begin_totp_enrollment(&account, t).unwrap();
        assert!(!vault.totp_enrolled().unwrap());
        assert!(format!("{pending:?}").contains(&account));
        let manual = pending.manual_key().expose(str::to_owned);
        assert!(!format!("{pending:?}").contains(&manual));
        assert!(
            pending
                .otpauth_url()
                .unwrap()
                .expose(|u| u.starts_with("otpauth://totp/"))
        );
        let wrong = format!(
            "{:06}",
            (code_at(&pending, t).parse::<u32>().unwrap() + 1) % 1_000_000
        );
        assert!(matches!(
            vault.confirm_totp_enrollment(&pending, &wrong, t),
            Err(Error::TotpRejected)
        ));
        assert!(!vault.totp_enrolled().unwrap());

        let pending = vault.begin_totp_enrollment(&account, t).unwrap();
        let code = code_at(&pending, t);
        let expired_at = pending.expires_at();
        let late = vault.begin_totp_enrollment(&account, t).unwrap();
        let late_code = code_at(&late, expired_at);
        assert!(matches!(
            vault.confirm_totp_enrollment(&late, &late_code, expired_at),
            Err(Error::EnrollmentExpired)
        ));
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        vault.confirm_totp_enrollment(&pending, &spaced, t).unwrap();
        // Enrollment survives relock; the confirming code cannot be replayed.
        vault.lock();
        vault.unlock(&password).unwrap();
        assert!(vault.totp_enrolled().unwrap());
        assert_eq!(vault.verify_totp(&code, t).unwrap(), TotpOutcome::Replayed);
        assert!(matches!(
            vault.begin_totp_enrollment(&account, t),
            Err(Error::TotpEnrolled)
        ));
    }

    #[test]
    fn verification_window_replay_drift_and_removal() {
        let (_dir, mut vault, _) = unlocked();
        assert!(matches!(
            vault.verify_totp("000000", now()),
            Err(Error::TotpNotEnrolled)
        ));
        let t = now();
        let pending = vault.begin_totp_enrollment("a", t).unwrap();
        let generator = pending.totp().unwrap();
        vault
            .confirm_totp_enrollment(&pending, &generator.generate(t), t)
            .unwrap();
        let next = t + STEP;
        // A code from the previous step is accepted once if not yet consumed.
        let ahead = generator.generate(next + STEP);
        assert_eq!(vault.verify_totp(&ahead, next).unwrap(), TotpOutcome::Valid);
        // Consuming a later step also retires earlier ones.
        assert_eq!(
            vault.verify_totp(&generator.generate(next), next).unwrap(),
            TotpOutcome::Replayed
        );
        let far = t + 100 * STEP;
        let drifted = generator.generate(far + 5 * STEP);
        if generator.generate(far) != drifted
            && generator.generate(far + STEP) != drifted
            && generator.generate(far - STEP) != drifted
        {
            assert_eq!(
                vault.verify_totp(&drifted, far).unwrap(),
                TotpOutcome::ClockDrift { steps: 5 }
            );
        }
        assert_eq!(
            vault.verify_totp("12345", far).unwrap(),
            TotpOutcome::Invalid
        );
        assert_eq!(
            vault.verify_totp("abcdef", far).unwrap(),
            TotpOutcome::Invalid
        );
        let later = far + 50 * STEP;
        assert!(matches!(
            vault.remove_totp(&generator.generate(t), later),
            Err(Error::TotpRejected)
        ));
        assert!(vault.totp_enrolled().unwrap());
        vault
            .remove_totp(&generator.generate(later), later)
            .unwrap();
        assert!(!vault.totp_enrolled().unwrap());
    }
}
