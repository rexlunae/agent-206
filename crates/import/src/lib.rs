//! Reviewed migration from other tools (docs/imports.md). An adapter reads a
//! source installation into a [`Preview`] without changing anything; applying
//! a preview only ever adds to Agent 206 and records a [`Receipt`].
//!
//! Readers treat every source file as untrusted data: bounded size, regular
//! files only (no symlinks followed), and nothing is executed or activated.
//! Credentials never pass through here; the vault broker copies an encrypted
//! vault itself (`a206_broker::Request::ImportVault`).
#![forbid(unsafe_code)]
pub mod claude_code;
pub mod rustyclaw;

use std::path::{Path, PathBuf};

/// No configuration file we read is anywhere near this large.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{} is not a JSON settings file this importer understands", path.display())]
    JsonConfig {
        path: PathBuf,
        #[source]
        source: JsonConfigFailure,
    },
    #[error("{} does not exist", path.display())]
    NotFound { path: PathBuf },
    #[error("{} is not a directory", path.display())]
    NotDirectory { path: PathBuf },
    #[error("{} is a symbolic link or not a regular file; imports read only regular files", path.display())]
    NotRegular { path: PathBuf },
    #[error("{} is larger than {limit} bytes", path.display())]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("could not read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not a configuration this importer understands", path.display())]
    Config {
        path: PathBuf,
        #[source]
        source: ConfigFailure,
    },
    #[error("could not write the import receipt {}", path.display())]
    Receipt {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not encode the import receipt")]
    EncodeReceipt(#[source] toml::ser::Error),
}
/// JSON diagnostics retain location only; parser messages can quote credentials.
#[derive(Debug, thiserror::Error)]
#[error("invalid foreign JSON at line {line}, column {column}; inspect the source locally")]
pub struct JsonConfigFailure {
    pub line: usize,
    pub column: usize,
}
impl From<serde_json::Error> for JsonConfigFailure {
    fn from(error: serde_json::Error) -> Self {
        Self {
            line: error.line(),
            column: error.column(),
        }
    }
}
/// Safe parser diagnostics for foreign configuration. TOML errors can include
/// plaintext credentials in both their source excerpt and semantic message.
/// Keep the location, deliberately discard those secret-bearing strings.
#[derive(Debug, thiserror::Error)]
#[error("invalid foreign configuration at byte range {span:?}; inspect the source locally")]
pub struct ConfigFailure {
    pub span: Option<std::ops::Range<usize>>,
}
impl From<toml::de::Error> for ConfigFailure {
    fn from(error: toml::de::Error) -> Self {
        Self { span: error.span() }
    }
}
type Result<T, E = ImportError> = std::result::Result<T, E>;

pub use a206_core::imports::*;

/// Write `receipt` under `<root>/imports/`, owner-only, and return its path.
pub fn write_receipt(root: &Path, receipt: &Receipt) -> Result<PathBuf> {
    use std::io::Write;
    let dir = root.join("imports");
    let path = dir.join(format!("{}.toml", receipt.id));
    let failed = |source| ImportError::Receipt {
        path: path.clone(),
        source,
    };
    std::fs::create_dir_all(&dir).map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(failed)?;
    }
    let text = toml::to_string_pretty(receipt).map_err(ImportError::EncodeReceipt)?;
    // NamedTempFile is owner-only before it is renamed into place.
    let mut file = tempfile::NamedTempFile::new_in(&dir).map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.as_file().sync_all().map_err(failed)?;
    file.persist_noclobber(&path)
        .map_err(|error| failed(error.error))?;
    Ok(path)
}

/// Read a regular file of at most `limit` bytes, or `None` if it is absent.
pub(crate) fn read_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    let read = |source| ImportError::Read {
        path: path.to_owned(),
        source,
    };
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(read(error)),
    };
    if !metadata.is_file() {
        return Err(ImportError::NotRegular {
            path: path.to_owned(),
        });
    }
    let too_large = || ImportError::TooLarge {
        path: path.to_owned(),
        limit,
    };
    if metadata.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(read)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(Some(bytes))
}

/// Size of a regular file, `None` if absent; symlinks are refused.
pub(crate) fn regular_len(path: &Path) -> Result<Option<u64>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        Ok(_) => Err(ImportError::NotRegular {
            path: path.to_owned(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ImportError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
