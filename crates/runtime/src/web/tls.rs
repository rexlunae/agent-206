//! The HTTPS certificate for a web server opened to other devices: the user's
//! own certificate files, or a self-signed one generated for the server's
//! names and kept in the profile, so its fingerprint stays the same across
//! restarts and can be compared with what the browser shows.
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer,
    pem::{self, PemObject},
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Where the server's certificate comes from.
#[derive(Clone, Debug)]
pub enum Certificate {
    /// Self-signed for the server's names, generated once and reused.
    Generated,
    /// PEM files: the certificate chain and its private key.
    Files { cert: PathBuf, key: PathBuf },
}

/// Why the HTTPS certificate could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("could not prepare the certificate directory {}", path.display())]
    Directory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not generate a self-signed certificate")]
    Generate(#[source] rcgen::Error),
    #[error("could not read {}", path.display())]
    Pem {
        path: PathBuf,
        #[source]
        source: pem::Error,
    },
    #[error("{} holds no certificate", path.display())]
    NoCertificate { path: PathBuf },
    #[error("the certificate and key cannot be used for HTTPS")]
    Config(#[source] rustls::Error),
}
impl crate::error::Classified for TlsError {
    fn kind(&self) -> Option<a206_core::ErrorKind> {
        use a206_core::ErrorKind;
        Some(match self {
            Self::Directory { .. } | Self::Write { .. } => ErrorKind::Storage,
            Self::Generate(_) => ErrorKind::Internal,
            Self::Pem { .. } | Self::NoCertificate { .. } | Self::Config(_) => {
                ErrorKind::InvalidRequest
            }
        })
    }
}

/// A ready server configuration and the SHA-256 fingerprint of its certificate.
pub(super) struct Loaded {
    pub config: Arc<rustls::ServerConfig>,
    pub fingerprint: String,
}

/// A generated certificate is replaced this long before it expires.
const RENEW_BEFORE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Apple platforms refuse hand-trusted certificates valid for over 825 days;
/// this also stays within the limit for public certificates.
const VALIDITY: Duration = Duration::from_secs(397 * 24 * 60 * 60);

pub(super) fn load(
    root: &Path,
    names: &[String],
    certificate: &Certificate,
    now: SystemTime,
) -> Result<Loaded, TlsError> {
    let (cert, key) = match certificate {
        Certificate::Files { cert, key } => (cert.clone(), key.clone()),
        Certificate::Generated => generated(root, names, now)?,
    };
    let chain = CertificateDer::pem_file_iter(&cert)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|source| TlsError::Pem {
            path: cert.clone(),
            source,
        })?;
    let first = chain
        .first()
        .ok_or_else(|| TlsError::NoCertificate { path: cert.clone() })?;
    let fingerprint = fingerprint(first);
    let key = PrivateKeyDer::from_pem_file(&key).map_err(|source| TlsError::Pem {
        path: key.clone(),
        source,
    })?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(TlsError::Config)?
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .map_err(TlsError::Config)?;
    Ok(Loaded {
        config: Arc::new(config),
        fingerprint,
    })
}

/// `AB:CD:…`, as browsers display a certificate's SHA-256 fingerprint.
fn fingerprint(cert: &CertificateDer<'_>) -> String {
    Sha256::digest(cert.as_ref())
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The profile's certificate for exactly these names, generated when missing,
/// incomplete or close to expiry. Each name set has its own directory, so
/// changing names never reuses a certificate that does not cover them.
fn generated(
    root: &Path,
    names: &[String],
    now: SystemTime,
) -> Result<(PathBuf, PathBuf), TlsError> {
    let mut sorted = names.to_vec();
    sorted.sort();
    sorted.dedup();
    let digest = Sha256::digest(sorted.join("\n").as_bytes());
    let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let dir = root.join("web").join("tls").join(id);
    let (cert, key, expires) = (
        dir.join("cert.pem"),
        dir.join("key.pem"),
        dir.join("expires"),
    );
    let now_secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let current = std::fs::read_to_string(&expires)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_some_and(|until| until > now_secs + RENEW_BEFORE.as_secs());
    if current && cert.is_file() && key.is_file() {
        return Ok((cert, key));
    }
    private_dir(&dir)?;
    let mut params = rcgen::CertificateParams::new(sorted).map_err(TlsError::Generate)?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Agent 206 web server");
    let start = time::OffsetDateTime::from(now);
    // A day of slack for clocks that run behind the server's.
    params.not_before = start - Duration::from_secs(24 * 60 * 60);
    params.not_after = start + VALIDITY;
    let pair = rcgen::KeyPair::generate().map_err(TlsError::Generate)?;
    let issued = params.self_signed(&pair).map_err(TlsError::Generate)?;
    // The key first: a certificate is never left without its key.
    private_file(&key, pair.serialize_pem().as_bytes())?;
    private_file(&cert, issued.pem().as_bytes())?;
    private_file(
        &expires,
        (now_secs + VALIDITY.as_secs()).to_string().as_bytes(),
    )?;
    Ok((cert, key))
}

fn private_dir(dir: &Path) -> Result<(), TlsError> {
    let failed = |source| TlsError::Directory {
        path: dir.into(),
        source,
    };
    std::fs::create_dir_all(dir).map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(failed)?;
    }
    Ok(())
}

/// Replace `path` with an owner-only file holding `bytes`.
fn private_file(path: &Path, bytes: &[u8]) -> Result<(), TlsError> {
    let failed = |source| TlsError::Write {
        path: path.into(),
        source,
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(failed)?;
    std::io::Write::write_all(&mut file, bytes).map_err(failed)?;
    // NamedTempFile is created owner-only on unix.
    file.persist(path).map_err(|error| failed(error.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed moment, so certificate renewal never depends on the test's clock.
    fn moment() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }
    fn names() -> Vec<String> {
        vec![
            format!("h{}.test", uuid::Uuid::new_v4().simple()),
            "192.0.2.7".into(),
        ]
    }

    #[test]
    fn a_generated_certificate_is_reused_for_the_same_names_and_renewed_near_expiry() {
        let root = tempfile::tempdir().unwrap();
        let names = names();
        let now = moment();
        let first = generated(root.path(), &names, now).unwrap();
        let pem = std::fs::read(&first.0).unwrap();
        let mut reversed = names.clone();
        reversed.reverse();
        let again = generated(root.path(), &reversed, now).unwrap();
        assert_eq!(first, again);
        assert_eq!(std::fs::read(&again.0).unwrap(), pem);
        // Other names get their own certificate.
        let other = generated(root.path(), &names[..1], now).unwrap();
        assert_ne!(other.0, first.0);
        // Within the renewal window a new certificate replaces it.
        let late = now + VALIDITY - RENEW_BEFORE;
        let renewed = generated(root.path(), &names, late).unwrap();
        assert_eq!(renewed, first);
        assert_ne!(std::fs::read(&renewed.0).unwrap(), pem);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&renewed.1).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "key is readable by others: {mode:o}");
        }
        let loaded = load(root.path(), &names, &Certificate::Generated, late).unwrap();
        assert_eq!(loaded.fingerprint.len(), 32 * 3 - 1);
    }

    #[test]
    fn unusable_certificate_files_are_reported_with_their_path() {
        let root = tempfile::tempdir().unwrap();
        let cert = root.path().join("cert.pem");
        let key = root.path().join("key.pem");
        std::fs::write(&cert, "not a certificate").unwrap();
        std::fs::write(&key, "not a key").unwrap();
        let files = Certificate::Files {
            cert: cert.clone(),
            key: key.clone(),
        };
        let Err(error) = load(root.path(), &names(), &files, moment()) else {
            panic!("loaded an empty certificate file");
        };
        assert!(matches!(&error, TlsError::NoCertificate { path } if *path == cert));
        assert_eq!(
            crate::error::report(&error).kind,
            a206_core::ErrorKind::InvalidRequest
        );
        // A real certificate with a missing key names the key.
        let (generated_cert, _) = generated(root.path(), &names(), moment()).unwrap();
        let files = Certificate::Files {
            cert: generated_cert,
            key: root.path().join("missing.pem"),
        };
        let Err(error) = load(root.path(), &names(), &files, moment()) else {
            panic!("loaded without a key");
        };
        assert!(matches!(&error, TlsError::Pem { path, .. } if path.ends_with("missing.pem")));
    }
}
