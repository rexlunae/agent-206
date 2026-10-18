//! Images attached to messages. Their bytes are kept beside the conversation
//! snapshot, which stays small, in files named by the SHA-256 digest of the
//! bytes; a message records only the name, format, digest and size.
//!
//! Every read checks the digest and the format again, so a file changed on
//! disk is reported rather than sent to a model.
use crate::error::{Classified, check};
use a206_core::{ErrorKind, Ignore, ImageAttachment, ImageType, MAX_IMAGE_BYTES};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
};

/// Why an image could not be stored or read.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("an image must be 1 to {MAX_IMAGE_BYTES} bytes; this one is {size}")]
    Size { size: usize },
    #[error("only PNG, JPEG, GIF and WebP images are accepted")]
    Format,
    #[error("invalid image name")]
    Name,
    #[error("invalid image digest")]
    Digest,
    #[error("the image is no longer stored; attach it again")]
    Missing { digest: String },
    #[error("the stored image no longer matches the one attached")]
    Changed { digest: String },
    #[error("could not store the image")]
    Write(#[source] io::Error),
    #[error("could not read the image")]
    Read {
        digest: String,
        #[source]
        source: io::Error,
    },
}
impl Classified for ImageError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Size { .. } | Self::Format | Self::Name | Self::Digest => {
                ErrorKind::InvalidRequest
            }
            Self::Missing { .. } => ErrorKind::NotFound,
            Self::Changed { .. } | Self::Write(_) => ErrorKind::Storage,
            Self::Read { .. } => return None,
        })
    }
}
type Result<T, E = ImageError> = std::result::Result<T, E>;

/// The most staged images kept that no message refers to; staging another
/// removes the oldest. Room for a few drafts' worth of images.
pub const MAX_UNSENT: usize = 32;
/// Check an attachment name as text attachment names are checked.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.chars().any(char::is_control)
}
fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The directory of stored images.
#[derive(Debug, Clone)]
pub struct Images {
    dir: PathBuf,
}
impl Images {
    /// Images kept under the gateway's state directory `root`.
    pub fn new(root: &Path) -> Self {
        Self {
            dir: root.join("images"),
        }
    }
    fn path(&self, digest: &str) -> PathBuf {
        self.dir.join(digest)
    }
    /// Store `data` and describe it. Storing the same bytes again reuses the
    /// stored file.
    pub fn stage(&self, name: &str, data: &[u8]) -> Result<ImageAttachment> {
        check(valid_name(name), ImageError::Name)?;
        check(
            !data.is_empty() && data.len() <= MAX_IMAGE_BYTES,
            ImageError::Size { size: data.len() },
        )?;
        let media_type = ImageType::sniff(data).ok_or(ImageError::Format)?;
        let digest = hex(&Sha256::digest(data));
        let path = self.path(&digest);
        // A stored file is kept only once it holds these exact bytes.
        if std::fs::read(&path).ok().as_deref() != Some(data) {
            std::fs::create_dir_all(&self.dir).map_err(ImageError::Write)?;
            let partial = self
                .dir
                .join(format!(".{digest}.{}", uuid::Uuid::new_v4().simple()));
            let written =
                std::fs::write(&partial, data).and_then(|()| std::fs::rename(&partial, &path));
            if let Err(error) = written {
                // Nothing refers to a partial file; a leftover is swept on
                // the next start.
                std::fs::remove_file(&partial).ignore();
                return Err(ImageError::Write(error));
            }
        }
        Ok(ImageAttachment {
            name: name.to_owned(),
            media_type,
            digest,
            size: data.len() as u64,
        })
    }
    /// The bytes stored under `digest`, checked against it.
    pub fn read_digest(&self, digest: &str) -> Result<Vec<u8>> {
        check(valid_digest(digest), ImageError::Digest)?;
        let data = std::fs::read(self.path(digest)).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                ImageError::Missing {
                    digest: digest.to_owned(),
                }
            } else {
                ImageError::Read {
                    digest: digest.to_owned(),
                    source,
                }
            }
        })?;
        check(
            hex(&Sha256::digest(&data)) == digest,
            ImageError::Changed {
                digest: digest.to_owned(),
            },
        )?;
        Ok(data)
    }
    /// The bytes of `image`, checked against everything it records.
    pub fn read(&self, image: &ImageAttachment) -> Result<Vec<u8>> {
        check(valid_name(&image.name), ImageError::Name)?;
        let data = self.read_digest(&image.digest)?;
        check(
            data.len() as u64 == image.size && ImageType::sniff(&data) == Some(image.media_type),
            ImageError::Changed {
                digest: image.digest.clone(),
            },
        )?;
        Ok(data)
    }
    /// Whether bytes are stored under `digest`.
    pub fn exists(&self, digest: &str) -> bool {
        valid_digest(digest) && self.path(digest).is_file()
    }
    /// Remove the bytes stored under `digest`; already absent is fine.
    pub fn remove(&self, digest: &str) -> io::Result<()> {
        if !valid_digest(digest) {
            return Ok(());
        }
        match std::fs::remove_file(self.path(digest)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
    /// Remove every stored file whose digest is not in `keep`, including
    /// partial files a failed write left. Returns how many were removed.
    pub fn sweep(&self, keep: &BTreeSet<&str>) -> io::Result<usize> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_str().is_some_and(|name| keep.contains(name)) {
                continue;
            }
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG signature followed by bytes unique to this test run.
    fn png() -> Vec<u8> {
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        data
    }

    #[test]
    fn a_staged_image_reads_back_and_is_described_by_its_own_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let images = Images::new(dir.path());
        let data = png();
        let name = uuid::Uuid::new_v4().to_string();
        let image = images.stage(&name, &data).unwrap();
        assert_eq!(image.name, name);
        assert_eq!(image.media_type, ImageType::Png);
        assert_eq!(image.size, data.len() as u64);
        assert_eq!(images.read(&image).unwrap(), data);
        // The same bytes are stored once.
        assert_eq!(images.stage(&name, &data).unwrap(), image);
        assert_eq!(
            std::fs::read_dir(dir.path().join("images"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn only_bounded_images_in_an_accepted_format_are_stored() {
        let dir = tempfile::tempdir().unwrap();
        let images = Images::new(dir.path());
        let text = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            images.stage("a", text.as_bytes()),
            Err(ImageError::Format)
        ));
        assert!(matches!(
            images.stage("a", &[]),
            Err(ImageError::Size { size: 0 })
        ));
        let mut large = png();
        large.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(matches!(
            images.stage("a", &large),
            Err(ImageError::Size { .. })
        ));
        assert!(matches!(
            images.stage("a\nb", &png()),
            Err(ImageError::Name)
        ));
        // Every format's signature is recognised.
        for (signature, kind) in [
            (&b"\xff\xd8\xff\xe0"[..], ImageType::Jpeg),
            (b"GIF89a", ImageType::Gif),
            (b"RIFF\0\0\0\0WEBPVP8 ", ImageType::Webp),
        ] {
            let mut data = signature.to_vec();
            data.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
            assert_eq!(images.stage("a", &data).unwrap().media_type, kind);
        }
    }

    #[test]
    fn a_missing_or_altered_image_is_reported_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let images = Images::new(dir.path());
        let image = images.stage("a", &png()).unwrap();
        let path = dir.path().join("images").join(&image.digest);
        std::fs::write(&path, png()).unwrap();
        assert!(matches!(
            images.read(&image),
            Err(ImageError::Changed { .. })
        ));
        std::fs::remove_file(&path).unwrap();
        let error = images.read(&image).unwrap_err();
        assert!(matches!(error, ImageError::Missing { .. }));
        assert_eq!(crate::error::report(&error).kind, ErrorKind::NotFound);
        // A digest is never a path.
        assert!(matches!(
            images.read_digest("../settings.toml"),
            Err(ImageError::Digest)
        ));
    }

    #[test]
    fn sweeping_keeps_only_the_named_images() {
        let dir = tempfile::tempdir().unwrap();
        let images = Images::new(dir.path());
        let kept = images.stage("a", &png()).unwrap();
        let dropped = images.stage("b", &png()).unwrap();
        std::fs::write(dir.path().join("images").join(".partial"), b"x").unwrap();
        let keep = BTreeSet::from([kept.digest.as_str()]);
        assert_eq!(images.sweep(&keep).unwrap(), 2);
        assert!(images.read(&kept).is_ok());
        assert!(matches!(
            images.read(&dropped),
            Err(ImageError::Missing { .. })
        ));
    }
}
