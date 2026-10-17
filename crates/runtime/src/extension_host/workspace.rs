//! Workspace host calls, confined to the project root with cap-std: absolute
//! paths, `..` escapes and symlinks that leave the root are refused. Replies
//! never include host paths; the guest sees only its own request.
use super::{MAX_ENTRIES, MAX_FILE, failure};
use a206_extensions::abi::{
    DirEntry, EntryKind, HostErrorCode, HostFailure, HostReply, SearchMatch, SearchResult,
};
use cap_std::fs::{Dir, File, OpenOptions};
use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

/// Most matches one search returns, whatever the guest asks for.
pub const MAX_MATCHES: u32 = 500;
/// Files one search may open, and the bytes it may read across them.
pub const MAX_SEARCH_FILES: usize = 5_000;
pub const MAX_SEARCH_BYTES: u64 = 32 * 1024 * 1024;
/// Longest query, and the longest line text a match carries.
pub const MAX_QUERY: usize = 1024;
pub const MAX_MATCH_TEXT: usize = 256;

fn io_failure(error: &std::io::Error) -> HostFailure {
    use std::io::ErrorKind as Io;
    match error.kind() {
        Io::NotFound => failure(HostErrorCode::NotFound, "no such file or directory"),
        // cap-std refuses absolute paths, `..` escapes and symlinks that
        // leave the workspace with PermissionDenied.
        Io::PermissionDenied => failure(
            HostErrorCode::Invalid,
            "the path is outside the workspace or not accessible",
        ),
        Io::InvalidInput => failure(HostErrorCode::Invalid, "invalid path"),
        Io::AlreadyExists => failure(HostErrorCode::Exists, "the path already exists"),
        _ => failure(HostErrorCode::Io, "the workspace could not be accessed"),
    }
}
fn open_workspace(root: PathBuf) -> Result<Dir, HostFailure> {
    Dir::open_ambient_dir(root, cap_std::ambient_authority()).map_err(|e| io_failure(&e))
}
fn not_regular() -> HostFailure {
    failure(HostErrorCode::Invalid, "not a regular file")
}

/// Open for reading without blocking: opening a FIFO for reading otherwise
/// waits for a writer forever. A regular file reads the same either way.
fn open_regular(dir: &Dir, path: impl AsRef<Path>) -> Result<File, HostFailure> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    }
    let file = dir.open_with(path, &options).map_err(|e| io_failure(&e))?;
    if !file.metadata().map_err(|e| io_failure(&e))?.is_file() {
        return Err(not_regular());
    }
    Ok(file)
}
/// At most [`MAX_FILE`] bytes, or `None` when the file is larger.
fn read_bounded(file: File) -> Result<Option<Vec<u8>>, HostFailure> {
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_failure(&e))?;
    Ok((bytes.len() as u64 <= MAX_FILE).then_some(bytes))
}
fn too_large() -> HostFailure {
    failure(HostErrorCode::TooLarge, "the file exceeds 512 KiB")
}

pub(super) fn read_file(root: PathBuf, path: &str) -> Result<HostReply, HostFailure> {
    let file = open_regular(&open_workspace(root)?, path)?;
    let bytes = read_bounded(file)?.ok_or_else(too_large)?;
    Ok(HostReply::File(bytes.into()))
}

fn open_directory(root: PathBuf, path: &str) -> Result<Dir, HostFailure> {
    let directory = open_workspace(root)?;
    if path.is_empty() || path == "." {
        Ok(directory)
    } else {
        directory.open_dir(path).map_err(|e| io_failure(&e))
    }
}
fn kind(file_type: cap_std::fs::FileType) -> EntryKind {
    match file_type {
        kind if kind.is_dir() => EntryKind::Directory,
        kind if kind.is_symlink() => EntryKind::Symlink,
        kind if kind.is_file() => EntryKind::File,
        _ => EntryKind::Other,
    }
}

pub(super) fn list_directory(root: PathBuf, path: &str) -> Result<HostReply, HostFailure> {
    let directory = open_directory(root, path)?;
    let mut entries = Vec::new();
    for entry in directory.entries().map_err(|e| io_failure(&e))? {
        let entry = entry.map_err(|e| io_failure(&e))?;
        if entries.len() == MAX_ENTRIES {
            return Err(failure(
                HostErrorCode::TooLarge,
                "the directory has more than 1024 entries",
            ));
        }
        let kind = kind(entry.file_type().map_err(|e| io_failure(&e))?);
        // A size that cannot be read is reported as unknown, never as zero.
        let size = (kind == EntryKind::File)
            .then(|| entry.metadata().ok().map(|m| m.len()))
            .flatten();
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size,
        });
    }
    Ok(HostReply::Directory(entries))
}

/// Bounded, deterministic (name-ordered) search that never follows symlinks
/// and skips `.git`, files over [`MAX_FILE`] and files that are not UTF-8.
pub(super) fn search(
    root: PathBuf,
    path: &str,
    query: &str,
    max_matches: u32,
) -> Result<HostReply, HostFailure> {
    if query.is_empty() || query.len() > MAX_QUERY {
        return Err(failure(
            HostErrorCode::Invalid,
            "the query must be 1 to 1024 bytes",
        ));
    }
    let limit = max_matches.clamp(1, MAX_MATCHES) as usize;
    let prefix = match path {
        "" | "." => String::new(),
        path => format!("{}/", path.trim_end_matches('/')),
    };
    let mut pending = vec![(open_directory(root, path)?, prefix)];
    let mut result = SearchResult::default();
    let (mut files, mut bytes) = (0, 0);
    while let Some((directory, prefix)) = pending.pop() {
        let mut entries = Vec::new();
        for entry in directory.entries().map_err(|e| io_failure(&e))? {
            let entry = entry.map_err(|e| io_failure(&e))?;
            let file_type = entry.file_type().map_err(|e| io_failure(&e))?;
            entries.push((entry.file_name().to_string_lossy().into_owned(), file_type));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut directories = Vec::new();
        for (name, file_type) in entries {
            let relative = format!("{prefix}{name}");
            match kind(file_type) {
                EntryKind::Directory if name != ".git" => {
                    directories.push((
                        directory.open_dir(&name).map_err(|e| io_failure(&e))?,
                        format!("{relative}/"),
                    ));
                }
                EntryKind::File => {
                    if files == MAX_SEARCH_FILES || bytes >= MAX_SEARCH_BYTES {
                        result.truncated = true;
                        return Ok(HostReply::Matches(result));
                    }
                    files += 1;
                    let Some(contents) = read_bounded(open_regular(&directory, &name)?)? else {
                        continue;
                    };
                    bytes += contents.len() as u64;
                    let Ok(text) = String::from_utf8(contents) else {
                        continue;
                    };
                    for (index, line) in text.lines().enumerate() {
                        if !line.contains(query) {
                            continue;
                        }
                        if result.matches.len() == limit {
                            result.truncated = true;
                            return Ok(HostReply::Matches(result));
                        }
                        result.matches.push(SearchMatch {
                            path: relative.clone(),
                            line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                            text: super::bounded(line, MAX_MATCH_TEXT).to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
        // Visit subdirectories in name order: the stack pops the last first.
        pending.extend(directories.into_iter().rev());
    }
    Ok(HostReply::Matches(result))
}

/// A relative path the guest may write: plain components only, and `.git`
/// is never writable through workspace calls.
fn writable_path(path: &str) -> Result<&Path, HostFailure> {
    let relative = Path::new(path);
    let plain = relative
        .components()
        .all(|c| matches!(c, Component::Normal(name) if name != ".git"));
    if path.is_empty() || !plain {
        return Err(failure(HostErrorCode::Invalid, "invalid path"));
    }
    Ok(relative)
}
/// The parent directory and file name of a path the guest may write.
fn writable(root: PathBuf, path: &str) -> Result<(Dir, String), HostFailure> {
    let relative = writable_path(path)?;
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| failure(HostErrorCode::Invalid, "invalid path"))?
        .to_string();
    let parent = relative.parent().and_then(Path::to_str).unwrap_or_default();
    Ok((open_directory(root, parent)?, name))
}

pub(super) fn write_file(
    root: PathBuf,
    path: &str,
    contents: &[u8],
    overwrite: bool,
) -> Result<HostReply, HostFailure> {
    if contents.len() as u64 > MAX_FILE {
        return Err(too_large());
    }
    let (directory, name) = writable(root, path)?;
    replace(&directory, &name, contents, overwrite)?;
    Ok(HostReply::Done)
}

/// Write `name` in `directory`. A new file is created exclusively; an
/// existing regular file is replaced atomically through a temporary file in
/// the same directory and keeps its permissions.
fn replace(
    directory: &Dir,
    name: &str,
    contents: &[u8],
    overwrite: bool,
) -> Result<(), HostFailure> {
    let existing = match directory.symlink_metadata(name) {
        Ok(metadata) if !metadata.is_file() => return Err(not_regular()),
        Ok(_) if !overwrite => {
            return Err(failure(HostErrorCode::Exists, "the file already exists"));
        }
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_failure(&error)),
    };
    let Some(permissions) = existing else {
        // `create_new` refuses a file created since the check above.
        let file = directory
            .open_with(name, OpenOptions::new().write(true).create_new(true))
            .map_err(|e| io_failure(&e))?;
        return fill(file, contents).inspect_err(|_| discard(directory, name));
    };
    let temporary = format!(".{name}.{}.a206-tmp", uuid::Uuid::new_v4().simple());
    let file = directory
        .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
        .map_err(|e| io_failure(&e))?;
    let written = file
        .set_permissions(permissions)
        .map_err(|e| io_failure(&e))
        .and_then(|()| fill(file, contents))
        .and_then(|()| {
            directory
                .rename(&temporary, directory, name)
                .map_err(|e| io_failure(&e))
        });
    written.inspect_err(|_| discard(directory, &temporary))
}
fn fill(mut file: File, contents: &[u8]) -> Result<(), HostFailure> {
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|e| io_failure(&e))
}
/// Remove a file this call created and could not finish. The write already
/// failed and reports that; a leftover file is recorded, not returned.
fn discard(directory: &Dir, name: &str) {
    if let Err(error) = directory.remove_file(name) {
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            "could not remove an unfinished extension write"
        );
    }
}

pub(super) fn edit_file(
    root: PathBuf,
    path: &str,
    old: &str,
    new: &str,
) -> Result<HostReply, HostFailure> {
    if old.is_empty() {
        return Err(failure(
            HostErrorCode::Invalid,
            "the text to replace is empty",
        ));
    }
    let (directory, name) = writable(root, path)?;
    let bytes = read_bounded(open_regular(&directory, &name)?)?.ok_or_else(too_large)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| failure(HostErrorCode::Invalid, "the file is not UTF-8 text"))?;
    match text.matches(old).count() {
        1 => {}
        0 => {
            return Err(failure(
                HostErrorCode::Conflict,
                "the text to replace was not found",
            ));
        }
        _ => {
            return Err(failure(
                HostErrorCode::Conflict,
                "the text to replace occurs more than once",
            ));
        }
    }
    let edited = text.replacen(old, new, 1);
    if edited.len() as u64 > MAX_FILE {
        return Err(too_large());
    }
    replace(&directory, &name, edited.as_bytes(), true)?;
    Ok(HostReply::Done)
}

pub(super) fn create_directory(root: PathBuf, path: &str) -> Result<HostReply, HostFailure> {
    let relative = writable_path(path)?;
    match open_workspace(root)?.create_dir_all(relative) {
        Ok(()) => Ok(HostReply::Done),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(failure(HostErrorCode::Exists, "a file has that name"))
        }
        Err(error) => Err(io_failure(&error)),
    }
}
