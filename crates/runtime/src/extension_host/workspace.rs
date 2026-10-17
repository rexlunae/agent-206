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
/// Directories one search may enter, and how deep it may go.
pub const MAX_SEARCH_DIRECTORIES: usize = 10_000;
pub const MAX_SEARCH_DEPTH: usize = 64;
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
    let prefix = match path {
        "" | "." => String::new(),
        path => format!("{}/", path.trim_end_matches('/')),
    };
    let mut search = Search {
        query,
        limit: max_matches.clamp(1, MAX_MATCHES) as usize,
        files: 0,
        directories: 0,
        bytes: 0,
        result: SearchResult::default(),
    };
    // A bound was reached: the result is complete up to that point.
    if search
        .walk(&open_directory(root, path)?, &prefix, 0)?
        .is_break()
    {
        search.result.truncated = true;
    }
    Ok(HostReply::Matches(search.result))
}

/// One search's progress. The walk is depth-first in name order, so results
/// are in global path order, and it holds only one open directory per level.
struct Search<'a> {
    query: &'a str,
    limit: usize,
    files: usize,
    directories: usize,
    bytes: u64,
    result: SearchResult,
}
impl Search<'_> {
    fn walk(
        &mut self,
        directory: &Dir,
        prefix: &str,
        depth: usize,
    ) -> Result<std::ops::ControlFlow<()>, HostFailure> {
        use std::ops::ControlFlow::{Break, Continue};
        let mut entries = Vec::new();
        for entry in directory.entries().map_err(|e| io_failure(&e))? {
            let entry = entry.map_err(|e| io_failure(&e))?;
            let file_type = entry.file_type().map_err(|e| io_failure(&e))?;
            entries.push((entry.file_name().to_string_lossy().into_owned(), file_type));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, file_type) in entries {
            let relative = format!("{prefix}{name}");
            match kind(file_type) {
                EntryKind::Directory if !is_git(&name) => {
                    if depth == MAX_SEARCH_DEPTH || self.directories == MAX_SEARCH_DIRECTORIES {
                        return Ok(Break(()));
                    }
                    self.directories += 1;
                    let child = directory.open_dir(&name).map_err(|e| io_failure(&e))?;
                    if self
                        .walk(&child, &format!("{relative}/"), depth + 1)?
                        .is_break()
                    {
                        return Ok(Break(()));
                    }
                }
                EntryKind::File => {
                    if self.files == MAX_SEARCH_FILES || self.bytes >= MAX_SEARCH_BYTES {
                        return Ok(Break(()));
                    }
                    self.files += 1;
                    let Some(contents) = read_bounded(open_regular(directory, &name)?)? else {
                        continue;
                    };
                    self.bytes += contents.len() as u64;
                    let Ok(text) = String::from_utf8(contents) else {
                        continue;
                    };
                    for (index, line) in text.lines().enumerate() {
                        if !line.contains(self.query) {
                            continue;
                        }
                        if self.result.matches.len() == self.limit {
                            return Ok(Break(()));
                        }
                        self.result.matches.push(SearchMatch {
                            path: relative.clone(),
                            line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                            text: super::bounded(line, MAX_MATCH_TEXT).to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(Continue(()))
    }
}

/// `.git` in any letter case: case-insensitive file systems treat `.GIT`
/// as the same directory.
fn is_git(name: impl AsRef<std::ffi::OsStr>) -> bool {
    name.as_ref().eq_ignore_ascii_case(".git")
}

/// Serializes workspace writes in this gateway, so an edit's read, check and
/// replace cannot interleave with another extension's write. Writes run on
/// blocking threads and never hold it across an `.await`.
static WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    // The lock guards no data, so a panic while held leaves nothing torn.
    WRITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Refuse a path whose existing components include a symlink: cap-std keeps
/// links inside the root, but a link could still lead a write into `.git`
/// or anywhere else the plain-name check would refuse.
fn no_links(directory: &Dir, relative: &Path) -> Result<(), HostFailure> {
    let mut prefix = PathBuf::new();
    for component in relative.components() {
        prefix.push(component);
        match directory.symlink_metadata(&prefix) {
            Ok(metadata) if metadata.is_symlink() => {
                return Err(failure(
                    HostErrorCode::Invalid,
                    "the path goes through a symbolic link",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_failure(&error)),
        }
    }
    Ok(())
}

/// A relative path the guest may write: plain components only, and `.git`
/// is never writable through workspace calls.
fn writable_path(path: &str) -> Result<&Path, HostFailure> {
    let relative = Path::new(path);
    let plain = relative
        .components()
        .all(|c| matches!(c, Component::Normal(name) if !is_git(name)));
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
    let parent = relative.parent().unwrap_or(Path::new(""));
    let workspace = open_workspace(root)?;
    no_links(&workspace, parent)?;
    let directory = if parent.as_os_str().is_empty() {
        workspace
    } else {
        workspace.open_dir(parent).map_err(|e| io_failure(&e))?
    };
    Ok((directory, name))
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
    let _writing = write_lock();
    let (directory, name) = writable(root, path)?;
    replace(&directory, &name, contents, overwrite, None)?;
    Ok(HostReply::Done)
}

/// Write `name` in `directory` through a temporary file in the same
/// directory, so no reader sees a partial file. A new file is published with
/// a hard link, which refuses a file created since the check; an existing
/// regular file is replaced by rename and keeps its permissions. With
/// `expected`, the replace is refused when the file no longer holds it.
fn replace(
    directory: &Dir,
    name: &str,
    contents: &[u8],
    overwrite: bool,
    expected: Option<&[u8]>,
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
    let temporary = format!(".{name}.{}.a206-tmp", uuid::Uuid::new_v4().simple());
    let file = directory
        .open_with(&temporary, OpenOptions::new().write(true).create_new(true))
        .map_err(|e| io_failure(&e))?;
    let filled = match &existing {
        Some(permissions) => file
            .set_permissions(permissions.clone())
            .map_err(|e| io_failure(&e))
            .and_then(|()| fill(file, contents)),
        None => fill(file, contents),
    };
    if let Err(error) = filled {
        discard(directory, &temporary);
        return Err(error);
    }
    if existing.is_none() {
        let linked = directory
            .hard_link(&temporary, directory, name)
            .map_err(|e| io_failure(&e));
        discard(directory, &temporary);
        return linked;
    }
    let published = unchanged(directory, name, expected).and_then(|()| {
        directory
            .rename(&temporary, directory, name)
            .map_err(|e| io_failure(&e))
    });
    published.inspect_err(|_| discard(directory, &temporary))
}
/// Whether `name` still holds what an edit read. Workspace writes from
/// extensions are serialized; this catches a change made outside them.
fn unchanged(directory: &Dir, name: &str, expected: Option<&[u8]>) -> Result<(), HostFailure> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let current = read_bounded(open_regular(directory, name)?)?;
    if current.as_deref() == Some(expected) {
        Ok(())
    } else {
        Err(failure(
            HostErrorCode::Conflict,
            "the file changed while it was being edited",
        ))
    }
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
    let _writing = write_lock();
    let (directory, name) = writable(root, path)?;
    let bytes = read_bounded(open_regular(&directory, &name)?)?.ok_or_else(too_large)?;
    let text = std::str::from_utf8(&bytes)
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
    replace(&directory, &name, edited.as_bytes(), true, Some(&bytes))?;
    Ok(HostReply::Done)
}

pub(super) fn create_directory(root: PathBuf, path: &str) -> Result<HostReply, HostFailure> {
    let relative = writable_path(path)?;
    let _writing = write_lock();
    let workspace = open_workspace(root)?;
    no_links(&workspace, relative)?;
    match workspace.create_dir_all(relative) {
        Ok(()) => Ok(HostReply::Done),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(failure(HostErrorCode::Exists, "a file has that name"))
        }
        Err(error) => Err(io_failure(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replace_is_refused_when_the_file_changed_since_it_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let directory = open_workspace(dir.path().to_path_buf()).unwrap();
        let (read, changed, edited) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        std::fs::write(dir.path().join("f.txt"), &changed).unwrap();
        let refused = replace(
            &directory,
            "f.txt",
            edited.as_bytes(),
            true,
            Some(read.as_bytes()),
        );
        assert_eq!(refused.unwrap_err().code, HostErrorCode::Conflict);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            changed
        );
        replace(
            &directory,
            "f.txt",
            edited.as_bytes(),
            true,
            Some(changed.as_bytes()),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            edited
        );
        // Neither attempt leaves its temporary file behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
