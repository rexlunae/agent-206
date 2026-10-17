//! Workspace host calls, confined to the project root with cap-std: absolute
//! paths, `..` escapes and symlinks that leave the root are refused. Replies
//! never include host paths; the guest sees only its own request.
use super::{MAX_ENTRIES, MAX_FILE, failure};
use a206_extensions::abi::{
    DirEntry, EntryKind, FindResult, FoundEntry, HostErrorCode, HostFailure, HostReply,
    SearchMatch, SearchResult,
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
/// Compiled size a search's regular expression may reach.
pub const MAX_REGEX_SIZE: usize = 1024 * 1024;
/// Most entries one find returns, and directory entries any walk may read.
pub const MAX_FOUND: u32 = 1_000;
pub const MAX_WALK_ENTRIES: usize = 100_000;
/// Encoded size a search or find reply may reach, below the worker's 1 MiB
/// reply bound; [`ENTRY_OVERHEAD`] is the most framing one result adds.
pub const MAX_RESULT_BYTES: usize = 768 * 1024;
const ENTRY_OVERHEAD: usize = 64;

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
/// A regular workspace file of at most `limit` bytes, or `None` when it is
/// larger. Confined to the workspace like every other read here.
pub(crate) fn read_limited(
    root: PathBuf,
    path: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, HostFailure> {
    let file = open_regular(&open_workspace(root)?, path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_failure(&e))?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
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
    search_pattern(root, path, query, Pattern::default(), max_matches)
}

/// How [`search_pattern`] matches lines and chooses files.
#[derive(Debug, Default)]
pub(super) struct Pattern<'a> {
    pub regex: bool,
    pub ignore_case: bool,
    pub include: Option<&'a str>,
}

/// [`search`] with a regular expression or letter-case-blind text, and an
/// optional glob choosing the files.
pub(super) fn search_pattern(
    root: PathBuf,
    path: &str,
    pattern: &str,
    options: Pattern<'_>,
    max_matches: u32,
) -> Result<HostReply, HostFailure> {
    if pattern.is_empty() || pattern.len() > MAX_QUERY {
        return Err(failure(
            HostErrorCode::Invalid,
            "the query must be 1 to 1024 bytes",
        ));
    }
    let lines = if !options.regex && !options.ignore_case {
        Lines::Literal(pattern)
    } else {
        let source = if options.regex {
            pattern.to_owned()
        } else {
            regex::escape(pattern)
        };
        let compiled = regex::RegexBuilder::new(&source)
            .case_insensitive(options.ignore_case)
            .size_limit(MAX_REGEX_SIZE)
            .dfa_size_limit(MAX_REGEX_SIZE)
            .build()
            .map_err(|error| match error {
                regex::Error::CompiledTooBig(_) => failure(
                    HostErrorCode::TooLarge,
                    "the regular expression is too large",
                ),
                _ => failure(HostErrorCode::Invalid, "the regular expression is invalid"),
            })?;
        Lines::Regex(compiled)
    };
    let include = options.include.map(glob).transpose()?;
    let prefix = prefix(path);
    let limit = max_matches.clamp(1, MAX_MATCHES) as usize;
    let (mut files, mut bytes, mut reply) = (0, 0u64, 0);
    let mut result = SearchResult::default();
    let mut walk = Walk::default();
    let flow = walk.walk(
        &open_directory(root, path)?,
        "",
        0,
        &mut |directory, entry| {
            use std::ops::ControlFlow::{Break, Continue};
            if entry.kind != EntryKind::File
                || include
                    .as_ref()
                    .is_some_and(|glob| !glob.is_match(entry.below))
            {
                return Ok(Continue(()));
            }
            if files == MAX_SEARCH_FILES || bytes >= MAX_SEARCH_BYTES {
                return Ok(Break(()));
            }
            files += 1;
            let Some(contents) = read_bounded(open_regular(directory, entry.name)?)? else {
                return Ok(Continue(()));
            };
            bytes += contents.len() as u64;
            let Ok(text) = String::from_utf8(contents) else {
                return Ok(Continue(()));
            };
            for (index, line) in text.lines().enumerate() {
                if !lines.is_match(line) {
                    continue;
                }
                let found = SearchMatch {
                    path: format!("{prefix}{}", entry.below),
                    line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                    text: super::bounded(line, MAX_MATCH_TEXT).to_string(),
                };
                let size = found.path.len() + found.text.len() + ENTRY_OVERHEAD;
                if result.matches.len() == limit || reply + size > MAX_RESULT_BYTES {
                    return Ok(Break(()));
                }
                reply += size;
                result.matches.push(found);
            }
            Ok(Continue(()))
        },
    )?;
    // A bound was reached: the result is complete up to that point.
    result.truncated = flow.is_break();
    Ok(HostReply::Matches(result))
}

/// Entries under `path` whose path matches `pattern`, in path order.
pub(super) fn find(
    root: PathBuf,
    path: &str,
    pattern: &str,
    max_results: u32,
) -> Result<HostReply, HostFailure> {
    let matcher = glob(pattern)?;
    let prefix = prefix(path);
    let limit = max_results.clamp(1, MAX_FOUND) as usize;
    let mut reply = 0;
    let mut result = FindResult::default();
    let mut walk = Walk::default();
    let flow = walk.walk(&open_directory(root, path)?, "", 0, &mut |_, entry| {
        use std::ops::ControlFlow::{Break, Continue};
        if !matcher.is_match(entry.below) {
            return Ok(Continue(()));
        }
        let path = format!("{prefix}{}", entry.below);
        let size = path.len() + ENTRY_OVERHEAD;
        if result.entries.len() == limit || reply + size > MAX_RESULT_BYTES {
            return Ok(Break(()));
        }
        reply += size;
        result.entries.push(FoundEntry {
            path,
            kind: entry.kind,
        });
        Ok(Continue(()))
    })?;
    result.truncated = flow.is_break();
    Ok(HostReply::Found(result))
}

/// The workspace-relative prefix of paths found below `path`.
fn prefix(path: &str) -> String {
    match path {
        "" | "." => String::new(),
        path => format!("{}/", path.trim_end_matches('/')),
    }
}

/// A glob as [`HostRequest::Find`](a206_extensions::abi::HostRequest::Find)
/// reads it: without `/` it matches an entry's name at any depth.
fn glob(pattern: &str) -> Result<globset::GlobMatcher, HostFailure> {
    if pattern.is_empty() || pattern.len() > MAX_QUERY {
        return Err(failure(
            HostErrorCode::Invalid,
            "the glob must be 1 to 1024 bytes",
        ));
    }
    let anchored = if pattern.contains('/') {
        pattern.trim_start_matches('/').to_owned()
    } else {
        format!("**/{pattern}")
    };
    globset::GlobBuilder::new(&anchored)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|_| failure(HostErrorCode::Invalid, "the glob is invalid"))
}

/// How a search compares lines.
enum Lines<'a> {
    Literal(&'a str),
    Regex(regex::Regex),
}
impl Lines<'_> {
    fn is_match(&self, line: &str) -> bool {
        match self {
            Self::Literal(text) => line.contains(text),
            Self::Regex(regex) => regex.is_match(line),
        }
    }
}

/// One entry a [`Walk`] reaches.
struct Entry<'a> {
    name: &'a str,
    /// `/`-separated, relative to where the walk started.
    below: &'a str,
    kind: EntryKind,
}
type Visit<'a> = dyn FnMut(&Dir, &Entry<'_>) -> Result<std::ops::ControlFlow<()>, HostFailure> + 'a;

/// A bounded walk shared by searches. Entries arrive in global path order
/// (byte order of the whole `/`-separated path, so `a.rs` comes before
/// `a/child.rs`), it holds only one open directory per level, and it never
/// follows symlinks or enters `.git`. A directory is visited before its
/// contents.
#[derive(Default)]
struct Walk {
    directories: usize,
    entries: usize,
}
/// One step of a directory's walk.
enum Step {
    Visit(String, EntryKind),
    /// Walk the named directory's contents.
    Descend(String),
}
impl Step {
    /// Where the step falls in path order: a directory's contents sort as
    /// `name/`, after `name.rs` and before `name0`.
    fn key(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Visit(name, _) => name.into(),
            Self::Descend(name) => format!("{name}/").into(),
        }
    }
}
impl Walk {
    fn walk(
        &mut self,
        directory: &Dir,
        prefix: &str,
        depth: usize,
        visit: &mut Visit<'_>,
    ) -> Result<std::ops::ControlFlow<()>, HostFailure> {
        use std::ops::ControlFlow::{Break, Continue};
        // The whole directory is read before anything in it is visited: the
        // file system lists entries in no particular order, so stopping part
        // way through would leave an arbitrary prefix, not a sorted one.
        let mut steps = Vec::new();
        let mut read = 0;
        for entry in directory.entries().map_err(|e| io_failure(&e))? {
            if read == MAX_WALK_ENTRIES {
                return Err(failure(
                    HostErrorCode::TooLarge,
                    "a directory has more than 100,000 entries",
                ));
            }
            read += 1;
            let entry = entry.map_err(|e| io_failure(&e))?;
            let kind = kind(entry.file_type().map_err(|e| io_failure(&e))?);
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind == EntryKind::Directory {
                if is_git(&name) {
                    continue;
                }
                steps.push(Step::Descend(name.clone()));
            }
            steps.push(Step::Visit(name, kind));
        }
        // A directory that would pass the walk's total ends the walk before
        // any of its entries, so what was found stays a sorted prefix.
        if self.entries + read > MAX_WALK_ENTRIES {
            return Ok(Break(()));
        }
        self.entries += read;
        steps.sort_by(|a, b| a.key().cmp(&b.key()));
        for step in steps {
            match step {
                Step::Visit(name, kind) => {
                    let below = format!("{prefix}{name}");
                    let entry = Entry {
                        name: &name,
                        below: &below,
                        kind,
                    };
                    if visit(directory, &entry)?.is_break() {
                        return Ok(Break(()));
                    }
                }
                Step::Descend(name) => {
                    if depth == MAX_SEARCH_DEPTH || self.directories == MAX_SEARCH_DIRECTORIES {
                        return Ok(Break(()));
                    }
                    self.directories += 1;
                    let child = directory.open_dir(&name).map_err(|e| io_failure(&e))?;
                    if self
                        .walk(&child, &format!("{prefix}{name}/"), depth + 1, visit)?
                        .is_break()
                    {
                        return Ok(Break(()));
                    }
                }
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
pub(super) fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    // The lock guards no data, so a panic while held leaves nothing torn.
    WRITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Open `relative` one component at a time without following symlinks, so
/// a link (even one swapped in during the walk) can never lead a write into
/// `.git` or anywhere else the plain-name check refuses. With `create`,
/// missing directories are made on the way.
fn descend(mut directory: Dir, relative: &Path, create: bool) -> Result<Dir, HostFailure> {
    use cap_fs_ext::DirExt;
    for name in relative.components().map(Component::as_os_str) {
        let opened = match directory.open_dir_nofollow(name) {
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                match directory.create_dir(name) {
                    Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                        return Err(io_failure(&error));
                    }
                    _ => directory.open_dir_nofollow(name),
                }
            }
            opened => opened,
        };
        directory = opened.map_err(|error| match directory.symlink_metadata(name) {
            Ok(metadata) if metadata.is_symlink() => failure(
                HostErrorCode::Invalid,
                "the path goes through a symbolic link",
            ),
            // A real directory that cannot be opened keeps its own error.
            Ok(metadata) if metadata.is_dir() => io_failure(&error),
            Ok(_) if create => failure(HostErrorCode::Exists, "a file has that name"),
            Ok(_) => failure(
                HostErrorCode::Invalid,
                "a path component is not a directory",
            ),
            Err(_) => io_failure(&error),
        })?;
    }
    Ok(directory)
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
/// A plain relative path outside `.git`, for calls that name a tracked file.
pub(super) fn check_relative(path: &str) -> Result<(), HostFailure> {
    writable_path(path).map(drop)
}
/// A workspace file's bytes, or `None` when it does not exist.
/// A file as git sees it in the working tree: a symbolic link is its target
/// text, read without following it; anything else is read like `read_file`.
pub(super) fn read_worktree(root: PathBuf, path: &str) -> Result<Option<Vec<u8>>, HostFailure> {
    let dir = open_workspace(root)?;
    match dir.symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = dir.read_link_contents(path).map_err(|e| io_failure(&e))?;
            Ok(Some(target.into_os_string().into_encoded_bytes()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_failure(&error)),
        Ok(_) => match open_regular(&dir, path) {
            Ok(file) => read_bounded(file)?.map(Some).ok_or_else(too_large),
            Err(failure) if failure.code == HostErrorCode::NotFound => Ok(None),
            Err(failure) => Err(failure),
        },
    }
}
/// A working-tree file as a commit records it.
pub(super) enum Tracked {
    File { bytes: Vec<u8>, executable: bool },
    Link(Vec<u8>),
}
/// The file at `path` as git would commit it, reached without following a
/// symbolic link anywhere on the way; `None` when nothing is there.
pub(super) fn tracked(root: PathBuf, path: &str) -> Result<Option<Tracked>, HostFailure> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    let (directory, name) = match writable(root, path) {
        Err(failure) if failure.code == HostErrorCode::NotFound => return Ok(None),
        found => found?,
    };
    let metadata = match directory.symlink_metadata(&name) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        found => found.map_err(|e| io_failure(&e))?,
    };
    if metadata.is_symlink() {
        let target = directory
            .read_link_contents(&name)
            .map_err(|e| io_failure(&e))?;
        return Ok(Some(Tracked::Link(
            target.into_os_string().into_encoded_bytes(),
        )));
    }
    if !metadata.is_file() {
        return Err(not_regular());
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
    }
    let file = directory
        .open_with(&name, &options)
        .map_err(|e| io_failure(&e))?;
    let metadata = file.metadata().map_err(|e| io_failure(&e))?;
    if !metadata.is_file() {
        return Err(not_regular());
    }
    #[cfg(unix)]
    let executable = {
        use cap_std::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    let bytes = read_bounded(file)?.ok_or_else(too_large)?;
    Ok(Some(Tracked::File { bytes, executable }))
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
    Ok((descend(open_workspace(root)?, parent, false)?, name))
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
    if expected.is_some() && existing.is_none() {
        return Err(failure(
            HostErrorCode::Conflict,
            "the file was removed while it was being edited",
        ));
    }
    // Fixed length, so a valid name near the file system's limit still works.
    let temporary = format!(".a206-{}.tmp", uuid::Uuid::new_v4().simple());
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
    // `writable_path` accepts only plain names, so a last component exists.
    let (Some(name), parent) = (relative.file_name(), relative.parent()) else {
        return Err(failure(HostErrorCode::Invalid, "invalid path"));
    };
    let parent = descend(open_workspace(root)?, parent.unwrap_or(Path::new("")), true)?;
    // The directory itself is only created, never opened, so one that
    // exists but cannot be read still counts as done.
    match parent.create_dir(name) {
        Ok(()) => Ok(HostReply::Done),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match parent.symlink_metadata(name) {
                Ok(metadata) if metadata.is_symlink() => Err(failure(
                    HostErrorCode::Invalid,
                    "the path goes through a symbolic link",
                )),
                Ok(metadata) if metadata.is_dir() => Ok(HostReply::Done),
                Ok(_) => Err(failure(HostErrorCode::Exists, "a file has that name")),
                Err(error) => Err(io_failure(&error)),
            }
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

    #[test]
    fn an_edit_never_recreates_a_file_removed_since_it_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let directory = open_workspace(dir.path().to_path_buf()).unwrap();
        let (read, edited) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let refused = replace(
            &directory,
            "f.txt",
            edited.as_bytes(),
            true,
            Some(read.as_bytes()),
        );
        assert_eq!(refused.unwrap_err().code, HostErrorCode::Conflict);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_name_at_the_usual_length_limit_can_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let directory = open_workspace(dir.path().to_path_buf()).unwrap();
        // 255 bytes is the component limit of common Unix file systems.
        let name = "n".repeat(255);
        let contents = uuid::Uuid::new_v4().to_string();
        replace(&directory, &name, contents.as_bytes(), false, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&name)).unwrap(),
            contents
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_unreadable_directory_counts_as_created() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let name = uuid::Uuid::new_v4().simple().to_string();
        let path = dir.path().join(&name);
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o111)).unwrap();
        let created = create_directory(dir.path().to_path_buf(), &name);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(created.unwrap(), HostReply::Done);
    }
}
