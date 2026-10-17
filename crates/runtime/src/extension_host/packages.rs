//! Installed extension packages: a validated manifest and module stored in
//! the gateway's data directory under their digest, and verified again
//! before every use, so a changed file never runs as the reviewed package.
use a206_extensions::{Manifest, ManifestError};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
};

pub use a206_core::{MAX_EXTENSION_MANIFEST as MAX_MANIFEST, MAX_EXTENSION_MODULE as MAX_MODULE};
/// Ids under this prefix belong to bundled extensions.
pub const RESERVED_PREFIX: &str = "agent206.";
/// The WebAssembly binary magic and version 1.
const WASM_HEADER: &[u8] = b"\0asm\x01\0\0\0";
const MANIFEST_FILE: &str = "plugin.toml";
const MODULE_FILE: &str = "module.wasm";
/// Prefix of temporary directories inside the package store.
const STAGING: &str = ".staging-";

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("extension ids starting with `{RESERVED_PREFIX}` are reserved for bundled extensions")]
    Reserved,
    #[error("the manifest is larger than {} KiB", MAX_MANIFEST / 1024)]
    ManifestTooLarge,
    #[error("the module is larger than {} MiB", MAX_MODULE / (1024 * 1024))]
    ModuleTooLarge,
    #[error("the module is not a WebAssembly 1.0 binary")]
    NotWasm,
    #[error("could not store the extension package")]
    Write(#[source] std::io::Error),
    #[error("could not read the installed extension package")]
    Read(#[source] std::io::Error),
    #[error(
        "could not delete extension package files; they are deleted when the gateway next starts"
    )]
    Remove(#[source] std::io::Error),
    #[error("the installed package for `{0}` changed since it was installed; install it again")]
    Changed(String),
}
impl crate::error::Classified for PackageError {
    fn kind(&self) -> Option<a206_core::ErrorKind> {
        use a206_core::ErrorKind as K;
        Some(match self {
            Self::Manifest(_)
            | Self::Reserved
            | Self::ManifestTooLarge
            | Self::ModuleTooLarge
            | Self::NotWasm => K::InvalidRequest,
            Self::Write(_) | Self::Read(_) | Self::Remove(_) => K::Storage,
            Self::Changed(_) => K::Conflict,
        })
    }
}

/// A validated package.
#[derive(Debug, Clone)]
pub struct Package {
    pub manifest: Manifest,
    pub text: String,
    pub module: Vec<u8>,
    /// Lowercase hex SHA-256 over the manifest text and the module.
    pub digest: String,
}

/// Validate a package a user is installing. Only the WASM header is checked
/// here; the module itself is compiled only in the confined worker.
pub fn prepare(text: String, module: Vec<u8>) -> Result<Package, PackageError> {
    if text.len() > MAX_MANIFEST {
        return Err(PackageError::ManifestTooLarge);
    }
    if module.len() > MAX_MODULE {
        return Err(PackageError::ModuleTooLarge);
    }
    if !module.starts_with(WASM_HEADER) {
        return Err(PackageError::NotWasm);
    }
    let manifest = Manifest::parse(&text)?;
    if manifest.id.starts_with(RESERVED_PREFIX) {
        return Err(PackageError::Reserved);
    }
    let digest = digest(&text, &module);
    Ok(Package {
        manifest,
        text,
        module,
        digest,
    })
}

fn digest(text: &str, module: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agent206-extension-package-v1\0");
    hasher.update((text.len() as u64).to_be_bytes());
    hasher.update(text.as_bytes());
    hasher.update(module);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Where installed packages live inside the gateway's data directory.
pub fn directory(data_dir: &Path) -> PathBuf {
    data_dir.join("extensions")
}

/// Store `package` under its digest, returning whether this created its
/// directory. Storing the same package again keeps the verified copy already
/// there. A damaged copy is replaced only once the new one is in place, and
/// restored if that fails. Callers serialize package operations.
pub fn store(dir: &Path, package: &Package) -> Result<bool, PackageError> {
    let target = dir.join(&package.digest);
    if load(dir, &package.manifest.id, &package.digest).is_ok() {
        return Ok(false);
    }
    std::fs::create_dir_all(dir).map_err(PackageError::Write)?;
    let staging = tempfile::Builder::new()
        .prefix(STAGING)
        .tempdir_in(dir)
        .map_err(PackageError::Write)?;
    for (name, bytes) in [
        (MANIFEST_FILE, package.text.as_bytes()),
        (MODULE_FILE, package.module.as_slice()),
    ] {
        let mut file =
            std::fs::File::create(staging.path().join(name)).map_err(PackageError::Write)?;
        std::io::Write::write_all(&mut file, bytes).map_err(PackageError::Write)?;
        file.sync_all().map_err(PackageError::Write)?;
    }
    // Move a damaged copy aside rather than deleting it first.
    let aside = tempfile::Builder::new()
        .prefix(STAGING)
        .tempdir_in(dir)
        .map_err(PackageError::Write)?;
    let damaged = aside.path().join("damaged");
    let existed = match std::fs::rename(&target, &damaged) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(PackageError::Write(error)),
    };
    if let Err(error) = std::fs::rename(staging.path(), &target) {
        if existed && let Err(restore) = std::fs::rename(&damaged, &target) {
            tracing::warn!(error = &restore as &dyn std::error::Error, digest = %package.digest, "could not restore the earlier extension package");
        }
        return Err(PackageError::Write(error));
    }
    // The staging directory was renamed away; `aside` removes the damaged copy.
    drop(staging);
    if let Err(error) = aside.close() {
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            "could not remove a damaged extension package"
        );
    }
    Ok(!existed)
}

/// Read the package installed for `id` and check it is still the one with
/// digest `expected`.
pub fn load(dir: &Path, id: &str, expected: &str) -> Result<Package, PackageError> {
    let changed = || PackageError::Changed(id.into());
    if !valid_digest(expected) {
        return Err(changed());
    }
    let package = dir.join(expected);
    let text = read(&package.join(MANIFEST_FILE), MAX_MANIFEST)?.ok_or_else(changed)?;
    let module = read(&package.join(MODULE_FILE), MAX_MODULE)?.ok_or_else(changed)?;
    let text = String::from_utf8(text).map_err(|_| changed())?;
    if digest(&text, &module) != expected {
        return Err(changed());
    }
    let manifest = Manifest::parse(&text)?;
    if manifest.id != id {
        return Err(changed());
    }
    Ok(Package {
        manifest,
        text,
        module,
        digest: expected.into(),
    })
}
/// The file's bytes, or `None` when it is missing or larger than `limit`.
fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, PackageError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(PackageError::Read(error)),
    };
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(PackageError::Read)?;
    Ok((bytes.len() <= limit).then_some(bytes))
}

/// Remove the package stored under `digest`, if any.
pub fn remove(dir: &Path, digest: &str) -> Result<(), PackageError> {
    if !valid_digest(digest) {
        return Err(PackageError::Changed(digest.into()));
    }
    match std::fs::remove_dir_all(dir.join(digest)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PackageError::Remove(error)),
    }
}

/// Remove stored packages no extension records, and staging left by an
/// interrupted install. Only names this module creates are touched.
pub fn prune(dir: &Path, keep: &BTreeSet<String>) -> Result<(), PackageError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(PackageError::Read(error)),
    };
    for entry in entries {
        let name = entry.map_err(PackageError::Read)?.file_name();
        let Some(name) = name.to_str() else { continue };
        let ours = name.starts_with(STAGING) || valid_digest(name);
        if ours && !keep.contains(name) {
            match std::fs::remove_dir_all(dir.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(PackageError::Remove(error)),
            }
        }
    }
    Ok(())
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package() -> Package {
        let builtin = a206_extensions::builtin("agent206.web").unwrap().unwrap();
        let id = format!("test.{}", uuid::Uuid::new_v4().simple());
        let text = include_str!("../../../../plugins/web/plugin.toml").replacen(
            "id = \"agent206.web\"",
            &format!("id = \"{id}\""),
            1,
        );
        prepare(text, builtin.wasm.to_vec()).unwrap()
    }

    #[test]
    fn stored_packages_load_only_under_their_digest_and_id() {
        let dir = tempfile::tempdir().unwrap();
        let package = package();
        let id = &package.manifest.id;
        store(dir.path(), &package).unwrap();
        let loaded = load(dir.path(), id, &package.digest).unwrap();
        assert_eq!(loaded.module, package.module);
        assert_eq!(loaded.text, package.text);
        let other = format!("test.{}", uuid::Uuid::new_v4().simple());
        assert!(matches!(
            load(dir.path(), &other, &package.digest),
            Err(PackageError::Changed(_))
        ));
        // A digest is only ever a directory name, never a path.
        let outside = dir.path().join(&package.digest);
        let escape = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        for digest in [escape.as_str(), "", &package.digest[1..]] {
            assert!(matches!(
                load(dir.path(), id, digest),
                Err(PackageError::Changed(_))
            ));
        }
        std::fs::remove_file(dir.path().join(&package.digest).join(MANIFEST_FILE)).unwrap();
        assert!(matches!(
            load(dir.path(), id, &package.digest),
            Err(PackageError::Changed(_))
        ));
        remove(dir.path(), &package.digest).unwrap();
        assert!(!dir.path().join(&package.digest).exists());
        remove(dir.path(), &package.digest).unwrap();
    }

    #[test]
    fn storing_repairs_a_damaged_copy_and_prune_keeps_recorded_packages() {
        let dir = tempfile::tempdir().unwrap();
        let (kept, dropped) = (package(), package());
        assert!(store(dir.path(), &kept).unwrap());
        assert!(!store(dir.path(), &kept).unwrap());
        std::fs::write(dir.path().join(&kept.digest).join(MODULE_FILE), b"x").unwrap();
        // Repairing reuses the recorded directory, so it is not new.
        assert!(!store(dir.path(), &kept).unwrap());
        load(dir.path(), &kept.manifest.id, &kept.digest).unwrap();
        assert!(store(dir.path(), &dropped).unwrap());
        let staging = dir
            .path()
            .join(format!("{STAGING}{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&staging).unwrap();
        let unrelated = dir.path().join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir(&unrelated).unwrap();
        prune(dir.path(), &[kept.digest.clone()].into()).unwrap();
        load(dir.path(), &kept.manifest.id, &kept.digest).unwrap();
        assert!(!dir.path().join(&dropped.digest).exists());
        assert!(!staging.exists());
        assert!(unrelated.exists());
        // A name that is not a digest is never removed.
        assert!(matches!(
            remove(dir.path(), "../x"),
            Err(PackageError::Changed(_))
        ));
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 2);
    }
}
