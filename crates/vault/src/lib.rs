//! SecureStore-compatible storage. Secret values have no serialization implementation.
//! Atomic replacement follows RustyClaw's vault.rs, with private staging files.
//! Typed credentials, access policies and TOTP keep RustyClaw's vault layout
//! (MIT, see NOTICE) so existing vaults remain readable without conversion.
#![forbid(unsafe_code)]
mod auth;
mod credential;
mod policy;
mod totp;

pub use auth::{AuthThrottle, Lockout, StepUpWindow};
pub use credential::CredentialValue;
pub use policy::{AccessContext, AccessPolicy, CredentialKind, CredentialMeta, Denial};
pub use totp::{PendingTotp, TotpOutcome, unix_now};

use securestore::{KeySource, SecretsManager};
use std::{
    fmt,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

pub struct Secret(Zeroizing<String>);
impl Secret {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
    pub fn expose<T>(&self, use_secret: impl FnOnce(&str) -> T) -> T {
        use_secret(&self.0)
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("vault is locked")]
    Locked,
    #[error("vault operation failed")]
    Crypto,
    #[error("vault I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential name is reserved or empty")]
    InvalidName,
    #[error("credential not found")]
    NotFound,
    #[error("stored vault data is corrupted")]
    Corrupted,
    #[error("credential access denied: {0}")]
    Denied(#[from] Denial),
    #[error("credential value does not match its kind")]
    KindMismatch,
    #[error("two-factor authentication is already enrolled")]
    TotpEnrolled,
    #[error("two-factor authentication is not enrolled")]
    TotpNotEnrolled,
    #[error("two-factor enrollment expired")]
    EnrollmentExpired,
    #[error("two-factor code was not accepted")]
    TotpRejected,
}

/// Raw keys an ordinary caller may never write, because they hold typed
/// credential metadata/values, vault-internal state or gateway service records.
const RESERVED_PREFIXES: &[&str] = &["__", "service:", "messenger/", "cred:", "val:"];

fn reserved(name: &str) -> bool {
    name.is_empty() || RESERVED_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// This type belongs exclusively to the vault broker process.
pub struct Vault {
    path: PathBuf,
    inner: Option<SecretsManager>,
}
impl Vault {
    pub fn locked(path: PathBuf) -> Self {
        Self { path, inner: None }
    }
    pub fn is_locked(&self) -> bool {
        self.inner.is_none()
    }
    /// Opening never creates a replacement for a corrupted or inaccessible existing vault.
    pub fn unlock(&mut self, password: &Secret) -> Result<(), Error> {
        self.lock();
        let inner = password
            .expose(|pw| {
                if self.path.exists() {
                    SecretsManager::load(&self.path, KeySource::Password(pw))
                } else {
                    SecretsManager::new(KeySource::Password(pw))
                }
            })
            .map_err(|_| Error::Crypto)?;
        if !self.path.exists() {
            save_atomic(&inner, &self.path)?;
        }
        self.inner = Some(inner);
        Ok(())
    }
    pub fn lock(&mut self) {
        self.inner = None;
    }
    /// Names of ordinary (untyped) entries. Typed credentials and internal
    /// records are listed through [`Vault::credentials`] or not at all.
    pub fn names(&self) -> Result<Vec<String>, Error> {
        Ok(self
            .manager()?
            .keys()
            .filter(|key| !reserved(key))
            .map(str::to_owned)
            .collect())
    }
    pub fn put(&mut self, name: &str, secret: Secret) -> Result<(), Error> {
        if reserved(name) {
            return Err(Error::InvalidName);
        }
        secret.expose(|value| self.commit(|inner| inner.set(name, value)))
    }
    /// Broker-internal only: the result must be consumed by a destination-bound operation.
    pub fn with_secret<T>(
        &self,
        name: &str,
        use_secret: impl FnOnce(&str) -> T,
    ) -> Result<T, Error> {
        if reserved(name) {
            return Err(Error::InvalidName);
        }
        let value = self.raw(name)?.ok_or(Error::NotFound)?;
        Ok(value.expose(use_secret))
    }

    fn manager(&self) -> Result<&SecretsManager, Error> {
        self.inner.as_ref().ok_or(Error::Locked)
    }
    fn raw(&self, key: &str) -> Result<Option<Secret>, Error> {
        let inner = self.manager()?;
        if !inner.keys().any(|k| k == key) {
            return Ok(None);
        }
        inner
            .get(key)
            .map(|v| Some(Secret::new(v)))
            .map_err(|_| Error::Corrupted)
    }
    fn has_raw(&self, key: &str) -> Result<bool, Error> {
        Ok(self.manager()?.keys().any(|k| k == key))
    }
    /// Apply a mutation and persist it atomically. A failed write locks the
    /// vault so the in-memory state can never diverge from disk.
    fn commit(&mut self, mutate: impl FnOnce(&mut SecretsManager)) -> Result<(), Error> {
        let inner = self.inner.as_mut().ok_or(Error::Locked)?;
        mutate(inner);
        if let Err(error) = save_atomic(inner, &self.path) {
            self.lock();
            return Err(error);
        }
        Ok(())
    }
}
fn save_atomic(inner: &SecretsManager, path: &Path) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("vault needs a parent directory"))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let staging = tempfile::NamedTempFile::new_in(parent)?;
    inner.save_as(staging.path()).map_err(|_| Error::Crypto)?;
    staging.as_file().sync_all()?;
    staging.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn unlocked() -> (tempfile::TempDir, Vault, Secret) {
        let dir = tempfile::tempdir().unwrap();
        let password = Secret::new(uuid::Uuid::new_v4().to_string());
        let mut vault = Vault::locked(dir.path().join("vault.json"));
        vault.unlock(&password).unwrap();
        (dir, vault, password)
    }
    #[test]
    fn encrypted_roundtrip_wrong_password_and_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let password = Secret::new(uuid::Uuid::new_v4().to_string());
        let value = uuid::Uuid::new_v4().to_string();
        let mut vault = Vault::locked(path.clone());
        assert!(matches!(vault.names(), Err(Error::Locked)));
        vault.unlock(&password).unwrap();
        vault.put("credential", Secret::new(value.clone())).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains(&value));
        assert_eq!(format!("{password:?}"), "[REDACTED]");
        vault.lock();
        assert!(
            vault
                .unlock(&Secret::new(uuid::Uuid::new_v4().to_string()))
                .is_err()
        );
        assert!(vault.is_locked());
        vault.unlock(&password).unwrap();
        assert_eq!(
            vault.with_secret("credential", str::to_owned).unwrap(),
            value
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn raw_paths_cannot_reach_reserved_namespaces() {
        let (_dir, mut vault, _) = unlocked();
        for name in [
            "",
            "__rustyclaw_totp_secret",
            "cred:forged",
            "val:forged",
            "messenger/bot",
            "service:x",
        ] {
            assert!(matches!(
                vault.put(name, Secret::new(String::new())),
                Err(Error::InvalidName)
            ));
            assert!(matches!(
                vault.with_secret(name, str::to_owned),
                Err(Error::InvalidName)
            ));
        }
        vault.put("plain", Secret::new("v".into())).unwrap();
        assert_eq!(vault.names().unwrap(), ["plain"]);
    }
}
