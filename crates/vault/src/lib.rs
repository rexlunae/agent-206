//! SecureStore-compatible storage. Secret values have no serialization implementation.
//! Atomic replacement follows RustyClaw's vault.rs, with private staging files.
#![forbid(unsafe_code)]
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
    pub fn names(&self) -> Result<Vec<String>, Error> {
        Ok(self
            .inner
            .as_ref()
            .ok_or(Error::Locked)?
            .keys()
            .map(str::to_owned)
            .collect())
    }
    pub fn put(&mut self, name: &str, secret: Secret) -> Result<(), Error> {
        if name.is_empty() || name.starts_with("service:") || name.starts_with("__") {
            return Err(Error::InvalidName);
        }
        let inner = self.inner.as_mut().ok_or(Error::Locked)?;
        secret.expose(|value| inner.set(name, value));
        if let Err(error) = save_atomic(inner, &self.path) {
            self.lock();
            return Err(error);
        }
        Ok(())
    }
    /// Broker-internal only: the result must be consumed by a destination-bound operation.
    pub fn with_secret<T>(
        &self,
        name: &str,
        use_secret: impl FnOnce(&str) -> T,
    ) -> Result<T, Error> {
        let value: String = self
            .inner
            .as_ref()
            .ok_or(Error::Locked)?
            .get(name)
            .map_err(|_| Error::NotFound)?;
        Ok(Secret::new(value).expose(use_secret))
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
mod tests {
    use super::*;
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
}
