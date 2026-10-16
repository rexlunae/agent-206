//! Adopt an existing password-protected SecureStore vault (RustyClaw's
//! `credentials/secrets.json`) as this profile's vault. The source is copied
//! byte for byte into a private staging file and only that copy is opened, so
//! the original installation is never written; every policy, kind, disabled
//! flag and two-factor enrollment travels inside the encrypted file unchanged.
use crate::{Error, Secret, TotpOutcome, Vault};
use securestore::{KeySource, SecretsManager};
use std::path::{Path, PathBuf};

/// Larger than any credential vault; a bigger "vault" is refused unread.
pub const MAX_IMPORT_BYTES: u64 = 16 * 1024 * 1024;

/// Why an import did not complete without being an error: the source vault
/// has two-factor enrolled and needs a current code from its authenticator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportCheck {
    Imported,
    TotpRequired,
    Totp(TotpOutcome),
}

/// Removes the staging copy unless it became the vault.
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        // Unactionable: the copy is encrypted and owner-only, and each import
        // stages under a fresh name, so a leftover is clutter, not exposure.
        std::fs::remove_file(&self.0).ok();
    }
}

impl Vault {
    /// Copy `source` in as this vault, verifying its password and, when it
    /// has two-factor enrolled, a current code. Refuses when this vault
    /// already exists: merging two vaults is not an import. On success the
    /// vault is unlocked.
    pub fn import(
        &mut self,
        source: &Path,
        password: &Secret,
        totp: Option<&str>,
        now: u64,
    ) -> Result<ImportCheck, Error> {
        if self.path.exists() {
            return Err(Error::Exists);
        }
        let bytes = read_source(source)?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("vault needs a parent directory"))?;
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        let staged = tempfile::Builder::new()
            .prefix(".import-")
            .tempfile_in(parent)?;
        std::io::Write::write_all(&mut staged.as_file(), &bytes)?;
        staged.as_file().sync_all()?;
        // Hand cleanup to `Staging`, which also covers the replacement that a
        // two-factor check writes over the staged path.
        let staging = Staging(staged.into_temp_path().keep().map_err(|e| e.error)?);
        let inner = password
            .expose(|pw| SecretsManager::load(&staging.0, KeySource::Password(pw)))
            .map_err(|_| Error::Crypto)?;
        let mut copy = Vault {
            path: staging.0.clone(),
            inner: Some(inner),
            generation: 0,
        };
        if copy.totp_enrolled()? {
            let Some(code) = totp else {
                return Ok(ImportCheck::TotpRequired);
            };
            // Consumes the step in the copy only; the source keeps its state.
            let outcome = copy.verify_totp(code, now)?;
            if outcome != TotpOutcome::Valid {
                return Ok(ImportCheck::Totp(outcome));
            }
        }
        // A hard link never replaces an existing file, unlike a rename.
        std::fs::hard_link(&staging.0, &self.path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Exists
            } else {
                Error::Io(error)
            }
        })?;
        drop(staging);
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        self.lock();
        self.inner = copy.inner.take();
        Ok(ImportCheck::Imported)
    }
}

/// Only a regular file within the size bound; a symlink is refused rather
/// than followed somewhere unexpected.
fn read_source(source: &Path) -> Result<Vec<u8>, Error> {
    use std::io::Read;
    let metadata = std::fs::symlink_metadata(source).map_err(Error::ImportSource)?;
    if !metadata.is_file() {
        return Err(Error::ImportSource(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the vault to import must be a regular file",
        )));
    }
    if metadata.len() > MAX_IMPORT_BYTES {
        return Err(Error::ImportSource(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the vault to import is larger than any credential vault",
        )));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(source)
        .and_then(|file| file.take(MAX_IMPORT_BYTES + 1).read_to_end(&mut bytes))
        .map_err(Error::ImportSource)?;
    if bytes.len() as u64 > MAX_IMPORT_BYTES {
        return Err(Error::ImportSource(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the vault to import grew past the size limit while it was read",
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::totp::tests::code_at;
    use crate::{AccessPolicy, CredentialKind, CredentialMeta, CredentialValue, unix_now};

    fn random() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// A source vault written independently of the importer, in its own directory.
    fn source(password: &Secret) -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        let mut vault = Vault::locked(path.clone());
        vault.unlock(password).unwrap();
        let name = random();
        vault
            .put_credential(
                &name,
                &CredentialMeta {
                    label: random(),
                    kind: CredentialKind::ApiKey,
                    policy: AccessPolicy::SkillOnly(vec![random()]),
                    description: None,
                    disabled: true,
                    destinations: vec![],
                },
                CredentialValue::Single(Secret::new(random())),
            )
            .unwrap();
        (dir, path, name)
    }

    fn target() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::locked(dir.path().join("vault").join("secrets.json"));
        (dir, vault)
    }

    #[test]
    fn import_keeps_policies_and_leaves_the_source_untouched() {
        let password = Secret::new(random());
        let (_src, path, name) = source(&password);
        let before = std::fs::read(&path).unwrap();
        let (_dir, mut vault) = target();
        assert_eq!(
            vault.import(&path, &password, None, unix_now()).unwrap(),
            ImportCheck::Imported
        );
        assert!(!vault.is_locked());
        let (_, meta) = vault
            .credentials()
            .unwrap()
            .into_iter()
            .find(|(n, _)| *n == name)
            .unwrap();
        assert!(meta.disabled);
        assert!(matches!(meta.policy, AccessPolicy::SkillOnly(_)));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // No staging copy is left beside the vault.
        let parent = vault.path.parent().unwrap();
        assert_eq!(std::fs::read_dir(parent).unwrap().count(), 1);
        vault.lock();
        vault.unlock(&password).unwrap();
    }

    #[test]
    fn wrong_password_existing_vault_and_symlinks_are_refused() {
        let password = Secret::new(random());
        let (src, path, _) = source(&password);
        let (_dir, mut vault) = target();
        assert!(matches!(
            vault.import(&path, &Secret::new(random()), None, unix_now()),
            Err(Error::Crypto)
        ));
        assert!(!vault.exists());
        assert_eq!(
            std::fs::read_dir(vault.path.parent().unwrap())
                .unwrap()
                .count(),
            0
        );
        #[cfg(unix)]
        {
            let link = src.path().join("link.json");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(matches!(
                vault.import(&link, &password, None, unix_now()),
                Err(Error::ImportSource(_))
            ));
        }
        assert!(matches!(
            vault.import(&src.path().join(random()), &password, None, unix_now()),
            Err(Error::ImportSource(_))
        ));
        vault.import(&path, &password, None, unix_now()).unwrap();
        vault.lock();
        assert!(matches!(
            vault.import(&path, &password, None, unix_now()),
            Err(Error::Exists)
        ));
    }

    #[test]
    fn an_enrolled_source_needs_a_valid_code_and_keeps_its_replay_state() {
        let password = Secret::new(random());
        let (_src, path, _) = source(&password);
        let mut original = Vault::locked(path.clone());
        original.unlock(&password).unwrap();
        let now = unix_now();
        let pending = original.begin_totp_enrollment(&random(), now).unwrap();
        // Enroll with the code of an earlier step so a current one stays fresh.
        let earlier = now - 30;
        original
            .confirm_totp_enrollment(&pending, &code_at(&pending, earlier), earlier)
            .unwrap();
        original.lock();
        let before = std::fs::read(&path).unwrap();
        let (_dir, mut vault) = target();
        assert_eq!(
            vault.import(&path, &password, None, now).unwrap(),
            ImportCheck::TotpRequired
        );
        assert!(!vault.exists());
        assert_eq!(
            vault
                .import(&path, &password, Some(&code_at(&pending, earlier)), now)
                .unwrap(),
            ImportCheck::Totp(TotpOutcome::Replayed)
        );
        assert!(!vault.exists());
        assert_eq!(
            vault
                .import(&path, &password, Some(&code_at(&pending, now)), now)
                .unwrap(),
            ImportCheck::Imported
        );
        assert!(vault.totp_enrolled().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
