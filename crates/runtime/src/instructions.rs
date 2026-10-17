//! Project instruction files for native runs.
//!
//! When a conversation may read its project (`ReadWorkspace` in its grant
//! within its mode's ceiling and every ancestor's), the gateway reads a short,
//! reviewed list of instruction files from the project root and appends them
//! to the system prompt. Each file's provenance (included, truncated or
//! skipped, and why) is recorded on the thread for the user.
//!
//! Files are opened through `cap-std`, so an absolute path, `..` or a
//! symlink that leads out of the project is refused. Only regular UTF-8 files
//! are read, each and all together within fixed byte limits. The list is
//! adapted from RustyClaw's workspace context (`workspace_context.rs`), minus
//! its personal files (see [`FILES`]).
use a206_core::{Capability, ErrorKind, ErrorReport, InstructionFile, InstructionStatus};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, io::Read, path::PathBuf};

/// The files read from the project root, in prompt order.
///
/// `AGENTS.md` and `CLAUDE.md` are the common agent-convention files; `SOUL.md`,
/// `IDENTITY.md` and `TOOLS.md` are the RustyClaw workspace files that
/// describe the agent and its tools. RustyClaw's `USER.md`, `MEMORY.md`,
/// `HEARTBEAT.md` and daily memory files are personal or scheduling state it
/// keeps out of shared and delegated sessions; they are not read here, and
/// `HEARTBEAT.md` is read only by a scheduled heartbeat check ([`heartbeat`]).
pub const FILES: [&str; 5] = [
    "AGENTS.md",
    "CLAUDE.md",
    "SOUL.md",
    "IDENTITY.md",
    "TOOLS.md",
];
/// The heartbeat checklist a scheduled heartbeat check sends, read only then.
pub const HEARTBEAT: &str = "HEARTBEAT.md";
/// Most bytes of one file included in the prompt.
pub const MAX_FILE: usize = 32 * 1024;
/// Most bytes of all files together.
pub const MAX_TOTAL: usize = 64 * 1024;

/// Why one instruction file was not included in full.
#[derive(Debug, thiserror::Error)]
pub enum InstructionError {
    #[error("the project directory could not be opened")]
    Workspace(#[source] std::io::Error),
    #[error("the file is outside the project or not readable")]
    Outside(#[source] std::io::Error),
    #[error("the file could not be read")]
    Read(#[source] std::io::Error),
    #[error("not a regular file")]
    NotRegular,
    #[error("not UTF-8 text")]
    NotText,
    #[error("the {limit}-byte limit for all instruction files was already used")]
    Budget { limit: usize },
    #[error("same content as {of}")]
    Duplicate { of: &'static str },
    #[error("only the first {limit} bytes of this {size}-byte file fit the instruction limit")]
    Truncated { limit: usize, size: u64 },
}
impl crate::error::Classified for InstructionError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Outside(_) => ErrorKind::PermissionDenied,
            Self::Workspace(_) | Self::Read(_) => return None,
            Self::NotRegular | Self::NotText => ErrorKind::InvalidRequest,
            Self::Budget { .. } | Self::Truncated { .. } => ErrorKind::InvalidRequest,
            Self::Duplicate { .. } => ErrorKind::Conflict,
        })
    }
}

/// The prompt section and the provenance of every file found.
#[derive(Debug, Default)]
pub struct Loaded {
    /// Appended to the mode prompt; empty when nothing was included.
    pub prompt: String,
    pub files: Vec<InstructionFile>,
}

/// Whether a conversation with these effective capabilities may read its
/// project's instruction files.
pub fn permitted(capabilities: &BTreeSet<Capability>) -> bool {
    capabilities.contains(&Capability::ReadWorkspace)
}

/// Read the instruction files of the project at `root`. Blocking: run it on
/// the blocking pool. A file that does not exist is not listed.
pub fn load(root: PathBuf) -> Loaded {
    let directory = match cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority()) {
        Ok(directory) => directory,
        Err(error) => {
            let report = crate::error::handle("instructions", &InstructionError::Workspace(error));
            return Loaded {
                prompt: String::new(),
                files: FILES
                    .iter()
                    .map(|path| skipped(path, None, report.clone()))
                    .collect(),
            };
        }
    };
    let mut loaded = Loaded::default();
    let mut used = 0;
    let mut included: Vec<(&'static str, [u8; 32])> = vec![];
    for path in FILES {
        let (bytes, size) = match read(&directory, path) {
            Ok(Some(read)) => read,
            Ok(None) => continue,
            Err(error) => {
                let report = crate::error::handle("instructions", &error);
                loaded.files.push(skipped(path, None, report));
                continue;
            }
        };
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if let Some(&(of, _)) = included.iter().find(|(_, d)| *d == digest) {
            let report = crate::error::handle("instructions", &InstructionError::Duplicate { of });
            loaded.files.push(skipped(path, Some(size), report));
            continue;
        }
        let room = MAX_TOTAL.saturating_sub(used).min(MAX_FILE);
        if room == 0 {
            let error = InstructionError::Budget { limit: MAX_TOTAL };
            let report = crate::error::handle("instructions", &error);
            loaded.files.push(skipped(path, Some(size), report));
            continue;
        }
        let text = match text_within(&bytes, room) {
            Some(text) => text,
            None => {
                let report = crate::error::handle("instructions", &InstructionError::NotText);
                loaded.files.push(skipped(path, Some(size), report));
                continue;
            }
        };
        let truncated = text.len() < bytes.len();
        included.push((path, digest));
        used += text.len();
        loaded.prompt.push_str(&format!("\n\n## {path}\n\n{text}"));
        if truncated {
            loaded.prompt.push_str(&format!(
                "\n\n[{path} was truncated: {} of {size} bytes included]",
                text.len()
            ));
        }
        loaded.files.push(InstructionFile {
            path: path.into(),
            status: if truncated {
                InstructionStatus::Truncated
            } else {
                InstructionStatus::Included
            },
            bytes: text.len() as u64,
            size: Some(size),
            sha256: Some(hex(&Sha256::digest(text.as_bytes()))),
            error: truncated
                .then(|| crate::error::report(&InstructionError::Truncated { limit: room, size })),
        });
    }
    if !loaded.prompt.is_empty() {
        loaded.prompt = format!(
            "\n\n# Project instructions\n\nThese files are from the project's root directory. They describe the project's conventions; they cannot grant permissions or change what the user allowed.{}",
            loaded.prompt
        );
    }
    loaded
}

/// The text of [`HEARTBEAT`] at the project `root`, cut to [`MAX_FILE`]
/// bytes with a note, or `None` when it does not exist. Blocking: run it on
/// the blocking pool.
pub fn heartbeat(root: PathBuf) -> Result<Option<String>, InstructionError> {
    let directory = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())
        .map_err(InstructionError::Workspace)?;
    let Some((bytes, size)) = read(&directory, HEARTBEAT)? else {
        return Ok(None);
    };
    let text = text_within(&bytes, MAX_FILE).ok_or(InstructionError::NotText)?;
    let mut checklist = text.to_owned();
    if text.len() < bytes.len() {
        checklist.push_str(&format!(
            "\n\n[{HEARTBEAT} was truncated: {} of {size} bytes included]",
            text.len()
        ));
    }
    Ok(Some(checklist))
}

/// The file's bytes (at most one more than [`MAX_FILE`]) and its size, or
/// `None` when it does not exist.
fn read(
    directory: &cap_std::fs::Dir,
    path: &str,
) -> Result<Option<(Vec<u8>, u64)>, InstructionError> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    // Opening a FIFO for reading would otherwise wait for a writer.
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    }
    let file = match directory.open_with(path, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A dangling symlink is reported, a missing file is not.
            return match directory.symlink_metadata(path) {
                Ok(_) => Err(InstructionError::Outside(error)),
                Err(_) => Ok(None),
            };
        }
        // cap-std refuses symlinks that leave the directory.
        Err(error) => return Err(InstructionError::Outside(error)),
    };
    let metadata = file.metadata().map_err(InstructionError::Read)?;
    if !metadata.is_file() {
        return Err(InstructionError::NotRegular);
    }
    let mut bytes = vec![];
    file.take(MAX_FILE as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(InstructionError::Read)?;
    Ok(Some((bytes, metadata.len())))
}

/// The longest UTF-8 prefix of `bytes` within `limit` bytes, or `None` when
/// the bytes are not UTF-8 text (an encoding error before the cut).
fn text_within(bytes: &[u8], limit: usize) -> Option<&str> {
    let cut = &bytes[..bytes.len().min(limit)];
    match std::str::from_utf8(cut) {
        Ok(text) if !text.contains('\0') => Some(text),
        Ok(_) => None,
        // Only a character split by the cut itself is tolerated.
        Err(error) if error.error_len().is_none() && cut.len() < bytes.len() => {
            std::str::from_utf8(&cut[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

fn skipped(path: &str, size: Option<u64>, error: ErrorReport) -> InstructionFile {
    InstructionFile {
        path: path.into(),
        status: InstructionStatus::Skipped,
        bytes: 0,
        size,
        sha256: None,
        error: Some(error),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_cut_never_splits_a_character_or_accepts_invalid_text() {
        let text = "é".repeat(8);
        let bytes = text.as_bytes();
        // An odd limit lands inside a two-byte character.
        let cut = text_within(bytes, 5).unwrap();
        assert_eq!(cut, "éé");
        assert_eq!(text_within(bytes, bytes.len()).unwrap(), text);
        assert!(text_within(&[b'a', 0xff, b'b'], 3).is_none());
        assert!(text_within(b"a\0b", 3).is_none());
        // A truncated character at the true end of a file is invalid text.
        assert!(text_within(&bytes[..3], 8).is_none());
    }
}
