//! SSH keys held in the vault sign SSH public-key authentication for the
//! destinations the user bound them to. The private key never leaves the
//! vault: callers get the public key and, for a request to sign, the
//! signature, and only for data that is a public-key authentication request
//! for that key, so the key cannot sign anything else (a git commit or an
//! arbitrary challenge).
use crate::{CredentialKind, CredentialValue, Denial, Error, Vault};
use signature::Signer;
use ssh_encoding::Encode;
use ssh_key::{Algorithm, EcdsaCurve, LineEnding, PrivateKey, PublicKey};
use zeroize::Zeroizing;

/// `SSH_MSG_USERAUTH_REQUEST` (RFC 4252).
const USERAUTH_REQUEST: u8 = 50;
/// OpenSSH's public-key method that also names the server's host key.
const HOSTBOUND: &[u8] = b"publickey-hostbound-v00@openssh.com";
/// The longest session identifier a key exchange hash makes (SHA-512).
const MAX_SESSION_ID: usize = 64;

/// Why an SSH key could not be used.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub enum SshKeyError {
    #[error("the stored SSH key is not an OpenSSH private key")]
    Unreadable,
    #[error("the SSH key is protected by a passphrase; store it decrypted")]
    Encrypted,
    #[error("the passphrase does not open the SSH key")]
    Passphrase,
    #[error("{0} SSH keys are not supported; use an Ed25519 or ECDSA (P-256 or P-384) key")]
    Unsupported(String),
    #[error("the stored public key does not belong to the stored private key")]
    Mismatch,
    #[error("the data to sign is not an SSH public-key authentication request: {0}")]
    NotUserAuth(UserAuthDefect),
    #[error("the data to sign is for another key")]
    OtherKey,
    #[error("the SSH key could not sign")]
    Signing,
}

/// What is wrong with data an SSH key was asked to sign.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error,
)]
pub enum UserAuthDefect {
    #[error("no session identifier")]
    SessionId,
    #[error("not a user authentication request")]
    NotRequest,
    #[error("no user name")]
    User,
    #[error("not for the ssh-connection service")]
    Service,
    #[error("not the publickey method")]
    Method,
    #[error("not a signed request")]
    Unsigned,
    #[error("no host key")]
    HostKey,
    #[error("trailing data")]
    Trailing,
}

/// What an SSH key use returns: never the private key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshUse {
    /// The public key in SSH wire format (RFC 4253 section 6.6).
    pub public_key: Vec<u8>,
    /// The signature in SSH wire format (algorithm and signature blob), when
    /// one was asked for.
    pub signature: Option<Vec<u8>>,
}

/// Read an OpenSSH private key for storing: it must be decrypted (with
/// `passphrase` when it is protected) and of a supported algorithm. Returns
/// the decrypted key in OpenSSH format and its public key as an
/// `authorized_keys` line.
pub fn prepare_ssh_key(
    pem: &str,
    passphrase: Option<&str>,
) -> Result<(Zeroizing<String>, String), SshKeyError> {
    let key = PrivateKey::from_openssh(pem).map_err(|_| SshKeyError::Unreadable)?;
    let key = match (key.is_encrypted(), passphrase) {
        (false, _) => key,
        (true, Some(passphrase)) => key
            .decrypt(passphrase)
            .map_err(|_| SshKeyError::Passphrase)?,
        (true, None) => return Err(SshKeyError::Encrypted),
    };
    supported(key.algorithm())?;
    encoded(&key)
}

/// A new Ed25519 key, in OpenSSH format, with its public key as an
/// `authorized_keys` line.
pub fn generate_ssh_key(comment: &str) -> Result<(Zeroizing<String>, String), SshKeyError> {
    let mut key = PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519)
        .map_err(|_| SshKeyError::Signing)?;
    key.set_comment(comment);
    encoded(&key)
}

fn encoded(key: &PrivateKey) -> Result<(Zeroizing<String>, String), SshKeyError> {
    let private = key
        .to_openssh(LineEnding::LF)
        .map_err(|_| SshKeyError::Unreadable)?;
    let public = key
        .public_key()
        .to_openssh()
        .map_err(|_| SshKeyError::Unreadable)?;
    Ok((private, public))
}

fn supported(algorithm: Algorithm) -> Result<(), SshKeyError> {
    match algorithm {
        Algorithm::Ed25519
        | Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256 | EcdsaCurve::NistP384,
        } => Ok(()),
        other => Err(SshKeyError::Unsupported(other.as_str().to_owned())),
    }
}

impl Vault {
    /// Use the SSH key `name` for `destination` (an `ssh://host:port`
    /// origin): its public key, and with `data`, the signature of `data`,
    /// which must be a public-key authentication request for this key. Like
    /// [`Vault::use_for_destination`], agent policy does not apply because
    /// no agent receives the key; the user's destination binding does.
    pub fn ssh_for_destination(
        &self,
        name: &str,
        destination: &str,
        data: Option<&[u8]>,
    ) -> Result<SshUse, Error> {
        let meta = self.credential(name)?;
        if meta.disabled {
            return Err(Denial::Disabled.into());
        }
        if meta.kind != CredentialKind::SshKey {
            return Err(Denial::UnsupportedKind.into());
        }
        if !meta.destinations.iter().any(|d| d == destination) {
            return Err(Denial::DestinationNotBound.into());
        }
        let CredentialValue::SshKeyPair {
            private_key,
            public_key,
        } = self.load_value(name, meta.kind)?
        else {
            return Err(Error::Corrupted);
        };
        let key = private_key
            .expose(|pem| PrivateKey::from_openssh(pem))
            .map_err(|_| SshKeyError::Unreadable)?;
        if key.is_encrypted() {
            return Err(SshKeyError::Encrypted.into());
        }
        supported(key.algorithm())?;
        let stored = public_key
            .expose(|p| (!p.trim().is_empty()).then(|| PublicKey::from_openssh(p.trim())));
        match stored {
            Some(Ok(stored)) if stored.key_data() == key.public_key().key_data() => {}
            None => {}
            Some(_) => return Err(SshKeyError::Mismatch.into()),
        }
        let blob = key
            .public_key()
            .to_bytes()
            .map_err(|_| SshKeyError::Unreadable)?;
        let signature = match data {
            None => None,
            Some(data) => {
                check_userauth(data, key.algorithm().as_str().as_bytes(), &blob)?;
                let signature = key.try_sign(data).map_err(|_| SshKeyError::Signing)?;
                let mut encoded = Vec::new();
                signature
                    .encode(&mut encoded)
                    .map_err(|_| SshKeyError::Signing)?;
                Some(encoded)
            }
        };
        Ok(SshUse {
            public_key: blob,
            signature,
        })
    }
}

/// Check that `data` is what an SSH client signs to authenticate with the
/// key `blob` of `algorithm` (RFC 4252 section 7, or OpenSSH's host-bound
/// variant, which adds the server's host key): the session identifier,
/// `SSH_MSG_USERAUTH_REQUEST`, the user, `ssh-connection`, the method, TRUE,
/// the algorithm and the key.
fn check_userauth(data: &[u8], algorithm: &[u8], blob: &[u8]) -> Result<(), SshKeyError> {
    use SshKeyError::NotUserAuth;
    let mut rest = data;
    let session = string(&mut rest).ok_or(NotUserAuth(UserAuthDefect::SessionId))?;
    if session.is_empty() || session.len() > MAX_SESSION_ID {
        return Err(NotUserAuth(UserAuthDefect::SessionId));
    }
    if take(&mut rest, 1) != Some(&[USERAUTH_REQUEST][..]) {
        return Err(NotUserAuth(UserAuthDefect::NotRequest));
    }
    // A client always names the user it logs in as.
    string(&mut rest)
        .filter(|user| !user.is_empty())
        .ok_or(NotUserAuth(UserAuthDefect::User))?;
    if string(&mut rest) != Some(b"ssh-connection") {
        return Err(NotUserAuth(UserAuthDefect::Service));
    }
    let method = string(&mut rest).ok_or(NotUserAuth(UserAuthDefect::Method))?;
    if method != b"publickey" && method != HOSTBOUND {
        return Err(NotUserAuth(UserAuthDefect::Method));
    }
    if take(&mut rest, 1) != Some(&[1][..]) {
        return Err(NotUserAuth(UserAuthDefect::Unsigned));
    }
    if string(&mut rest) != Some(algorithm) {
        return Err(SshKeyError::OtherKey);
    }
    if string(&mut rest) != Some(blob) {
        return Err(SshKeyError::OtherKey);
    }
    if method == HOSTBOUND && string(&mut rest).is_none_or(<[u8]>::is_empty) {
        return Err(NotUserAuth(UserAuthDefect::HostKey));
    }
    if !rest.is_empty() {
        return Err(NotUserAuth(UserAuthDefect::Trailing));
    }
    Ok(())
}

fn take<'a>(rest: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if rest.len() < n {
        return None;
    }
    let (head, tail) = rest.split_at(n);
    *rest = tail;
    Some(head)
}

/// An SSH `string`: a big-endian `uint32` length and that many bytes.
fn string<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = take(rest, 4)?;
    let len = u32::from_be_bytes(len.try_into().ok()?);
    take(rest, usize::try_from(len).ok()?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use signature::Verifier;
    use ssh_encoding::Decode;
    use ssh_key::Signature;

    fn put(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend(u32::try_from(bytes.len()).unwrap().to_be_bytes());
        out.extend(bytes);
    }

    /// A public-key authentication request for `key` with a random session.
    pub(crate) fn userauth(key: &PublicKey, method: &[u8], extra: Option<&[u8]>) -> Vec<u8> {
        let mut data = Vec::new();
        put(&mut data, uuid::Uuid::new_v4().as_bytes());
        data.push(USERAUTH_REQUEST);
        put(
            &mut data,
            uuid::Uuid::new_v4().simple().to_string().as_bytes(),
        );
        put(&mut data, b"ssh-connection");
        put(&mut data, method);
        data.push(1);
        put(&mut data, key.algorithm().as_str().as_bytes());
        put(&mut data, &key.to_bytes().unwrap());
        if let Some(extra) = extra {
            put(&mut data, extra);
        }
        data
    }

    #[test]
    fn only_a_userauth_request_for_the_key_passes() {
        let (private, _) = generate_ssh_key("t").unwrap();
        let key = PrivateKey::from_openssh(private.as_str()).unwrap();
        let other = PrivateKey::from_openssh(generate_ssh_key("o").unwrap().0.as_str()).unwrap();
        let public = key.public_key();
        let algorithm = public.algorithm();
        let algorithm = algorithm.as_str().as_bytes();
        let blob = public.to_bytes().unwrap();
        let check = |data: &[u8]| check_userauth(data, algorithm, &blob);
        check(&userauth(public, b"publickey", None)).unwrap();
        let host = other.public_key().to_bytes().unwrap();
        check(&userauth(public, HOSTBOUND, Some(&host))).unwrap();
        assert!(matches!(
            check(&userauth(public, HOSTBOUND, None)),
            Err(SshKeyError::NotUserAuth(_))
        ));
        assert!(matches!(
            check(&userauth(other.public_key(), b"publickey", None)),
            Err(SshKeyError::OtherKey)
        ));
        assert!(matches!(
            check(&userauth(public, b"password", None)),
            Err(SshKeyError::NotUserAuth(_))
        ));
        // A request must name its user.
        let mut nameless = Vec::new();
        put(&mut nameless, uuid::Uuid::new_v4().as_bytes());
        nameless.push(USERAUTH_REQUEST);
        put(&mut nameless, b"");
        put(&mut nameless, b"ssh-connection");
        put(&mut nameless, b"publickey");
        nameless.push(1);
        put(&mut nameless, algorithm);
        put(&mut nameless, &blob);
        assert!(matches!(
            check(&nameless),
            Err(SshKeyError::NotUserAuth(UserAuthDefect::User))
        ));
        let mut trailing = userauth(public, b"publickey", None);
        trailing.push(0);
        assert!(matches!(check(&trailing), Err(SshKeyError::NotUserAuth(_))));
        let mut unsigned = userauth(public, b"publickey", None);
        let at = unsigned.len() - blob.len() - 4 - algorithm.len() - 4 - 1;
        unsigned[at] = 0;
        assert!(matches!(check(&unsigned), Err(SshKeyError::NotUserAuth(_))));
        // Arbitrary data, such as a commit, is not signed.
        assert!(check(uuid::Uuid::new_v4().as_bytes()).is_err());
        assert!(check(&[]).is_err());
        // A signature made over the request verifies with the public key.
        let data = userauth(public, b"publickey", None);
        let signature = key.try_sign(&data).unwrap();
        let mut encoded = Vec::new();
        signature.encode(&mut encoded).unwrap();
        let decoded = Signature::decode(&mut encoded.as_slice()).unwrap();
        Verifier::verify(public, &data, &decoded).unwrap();
    }

    #[test]
    fn keys_are_stored_decrypted_and_only_supported_algorithms_are_accepted() {
        let (private, public) = generate_ssh_key("t").unwrap();
        let key = PrivateKey::from_openssh(private.as_str()).unwrap();
        assert_eq!(
            PublicKey::from_openssh(&public).unwrap().key_data(),
            key.public_key().key_data()
        );
        let passphrase = uuid::Uuid::new_v4().to_string();
        let locked = key
            .encrypt(&mut ssh_key::rand_core::OsRng, &passphrase)
            .unwrap()
            .to_openssh(LineEnding::LF)
            .unwrap();
        assert!(matches!(
            prepare_ssh_key(&locked, None),
            Err(SshKeyError::Encrypted)
        ));
        assert!(matches!(
            prepare_ssh_key(&locked, Some("wrong")),
            Err(SshKeyError::Passphrase)
        ));
        let (opened, opened_public) = prepare_ssh_key(&locked, Some(&passphrase)).unwrap();
        assert!(
            !PrivateKey::from_openssh(opened.as_str())
                .unwrap()
                .is_encrypted()
        );
        assert_eq!(opened_public, public);
        assert!(matches!(
            prepare_ssh_key(&public, None),
            Err(SshKeyError::Unreadable)
        ));
        let p384 = PrivateKey::random(
            &mut ssh_key::rand_core::OsRng,
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP384,
            },
        )
        .unwrap()
        .to_openssh(LineEnding::LF)
        .unwrap();
        prepare_ssh_key(&p384, None).unwrap();
        assert!(matches!(
            supported(Algorithm::Rsa { hash: None }),
            Err(SshKeyError::Unsupported(_))
        ));
    }
}
