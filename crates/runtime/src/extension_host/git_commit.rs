//! Git commits: of named working-tree files on top of `HEAD`, like
//! `git commit --only`, or of what is staged, like `git commit`. Nothing the repository or the user
//! configures runs. Where git would run something or protect the commit in a
//! way gitoxide cannot (signing, hooks, clean filters, line-ending
//! conversion), the commit is refused rather than made without it.
use super::{
    failure,
    git::{check, git_failure, open, unsupported},
    workspace::{self, Tracked},
};
use a206_extensions::abi::{GitCommit, HostErrorCode, HostFailure, HostReply};
use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
    index::entry::{Flags, Mode, Stage, Stat},
    objs::tree::EntryKind,
};
use std::{collections::BTreeSet, path::PathBuf, sync::atomic::AtomicBool};

/// Most paths one commit names.
pub const MAX_COMMIT_PATHS: usize = 100;
/// Longest commit message.
pub const MAX_COMMIT_MESSAGE: usize = 16 * 1024;
/// Most file bytes one commit writes.
pub const MAX_COMMIT_BYTES: u64 = 8 * 1024 * 1024;
/// Longest user excludes file read.
const MAX_USER_EXCLUDES: u64 = 1024 * 1024;
/// Hooks `git commit` runs. Their presence refuses the commit.
const COMMIT_HOOKS: [&str; 4] = [
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
];
/// Attributes that make git change content on the way into a commit.
const CONVERSIONS: [&str; 6] = [
    "filter",
    "ident",
    "working-tree-encoding",
    "text",
    "eol",
    "crlf",
];

/// The user's own git settings a commit honours: identity, signing, hooks
/// and their excludes file. They are read as data; nothing they name runs.
/// Repository settings take precedence, as in git.
#[derive(Default)]
pub(super) struct UserGit {
    config: gix::config::File,
    /// Git's default excludes file, used when no `core.excludesFile` is set.
    default_excludes: Option<PathBuf>,
    home: Option<PathBuf>,
}
impl UserGit {
    /// System, installation and global configuration, as `git` reads it.
    pub(super) fn load() -> Result<Self, HostFailure> {
        let config =
            gix::config::File::from_globals().map_err(|e| git_failure("user_config", &e))?;
        let home = gix::path::env::home_dir();
        let default_excludes = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".config")))
            .map(|base| base.join("git").join("ignore"));
        Ok(Self {
            config,
            default_excludes,
            home,
        })
    }
    /// Given user configuration, for tests that must not read the machine's.
    #[cfg(test)]
    pub(super) fn given(config: gix::config::File, default_excludes: Option<PathBuf>) -> Self {
        Self {
            config,
            default_excludes,
            home: None,
        }
    }
}
fn read_user_file(path: &std::path::Path) -> Result<Option<Vec<u8>>, HostFailure> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        opened => opened.map_err(|e| git_failure("user_config", &e))?,
    };
    let mut bytes = Vec::new();
    file.take(MAX_USER_EXCLUDES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| git_failure("user_config", &e))?;
    if bytes.len() as u64 > MAX_USER_EXCLUDES {
        return Err(unsupported("your git excludes file is larger than 1 MiB"));
    }
    Ok(Some(bytes))
}

/// A setting from the repository, else from the user, with every include
/// git would follow resolved, conditional ones too.
pub(super) struct Settings {
    repository: gix::config::File,
    user: gix::config::File,
    home: Option<PathBuf>,
}
impl Settings {
    pub(super) fn resolve(repo: &gix::Repository, user: &UserGit) -> Result<Self, HostFailure> {
        let failed = |e: &(dyn std::error::Error + 'static)| git_failure("config", e);
        let git_dir = repo.git_dir();
        let read = |name: &str, source| {
            let path = git_dir.join(name);
            if path.is_file() {
                gix::config::File::from_path_no_includes(path, source).map_err(|e| failed(&e))
            } else {
                Ok(gix::config::File::default())
            }
        };
        let mut repository = read("config", gix::config::Source::Local)?;
        // Extension-scoped settings only apply when the repository opts in.
        if repository
            .boolean("extensions.worktreeConfig")
            .unwrap_or(Some(true))
            .unwrap_or(false)
        {
            let worktree = read("config.worktree", gix::config::Source::Worktree)?;
            repository.append(worktree).map_err(|e| failed(&e))?;
        }
        let head = repo.head_name().map_err(|e| failed(&e))?;
        let mut options = gix::config::file::includes::Options::follow(
            gix::config::path::interpolate::Context {
                home_dir: user.home.as_deref(),
                ..Default::default()
            },
            gix::config::file::includes::conditional::Context {
                git_dir: Some(git_dir),
                branch_name: head.as_ref().map(|name| name.as_ref()),
            },
        )
        .strict();
        // Git skips an include whose file is missing; it holds no settings.
        options.err_on_missing_config_path = false;
        let options = gix::config::file::init::Options {
            includes: options,
            ..Default::default()
        };
        // An include that cannot be resolved, such as `onbranch` on a
        // detached HEAD, refuses the commit instead of being skipped.
        let mut user_config = user.config.clone();
        for config in [&mut repository, &mut user_config] {
            config.resolve_includes(options).map_err(|e| failed(&e))?;
        }
        Ok(Self {
            repository,
            user: user_config,
            home: user.home.clone(),
        })
    }
    fn string(&self, key: &str) -> Option<BString> {
        self.repository
            .string(key)
            .or_else(|| self.user.string(key))
    }
    /// An unreadable value counts as `true`, so it never weakens a check.
    fn flag(&self, key: &str) -> Option<bool> {
        let read = |config: &gix::config::File| config.boolean(key).unwrap_or(Some(true));
        read(&self.repository).or_else(|| read(&self.user))
    }
    /// A path setting, with `~` expanded; a relative path is taken from
    /// `base`. `Err` when it is set but cannot be resolved.
    fn path(&self, key: &str, base: &std::path::Path) -> Result<Option<PathBuf>, ()> {
        let Some(value) = self.string(key) else {
            return Ok(None);
        };
        let value = gix::path::from_bstring(value).map_err(drop)?;
        Ok(Some(match value.strip_prefix("~") {
            Ok(rest) => self.home.as_ref().ok_or(())?.join(rest),
            Err(_) => base.join(value),
        }))
    }
}

pub(super) enum Change {
    Write { id: ObjectId, mode: Mode, size: u32 },
    Remove,
}

/// What a commit records.
pub(super) enum Selection<'a> {
    /// The working-tree content of exactly these paths, on top of `HEAD`.
    Paths(&'a [String]),
    /// Everything the index holds, like a plain `git commit`.
    Staged,
}

pub(super) fn commit(
    root: PathBuf,
    message: &str,
    selection: Selection<'_>,
    user: &UserGit,
    interrupt: &AtomicBool,
) -> Result<HostReply, HostFailure> {
    if message.trim().is_empty() || message.len() > MAX_COMMIT_MESSAGE || message.contains('\0') {
        return Err(failure(
            HostErrorCode::Invalid,
            "the commit message must be 1 to 16 KiB of text",
        ));
    }
    let named = match selection {
        Selection::Paths(paths) => {
            if paths.is_empty() || paths.len() > MAX_COMMIT_PATHS {
                return Err(failure(
                    HostErrorCode::Invalid,
                    "name 1 to 100 paths to commit",
                ));
            }
            let mut named = BTreeSet::new();
            for path in paths {
                workspace::check_relative(path)?;
                named.insert(normalized(path));
            }
            Some(named)
        }
        Selection::Staged => None,
    };
    // Workspace writes cannot change a file between its read and its commit.
    let _writing = workspace::write_lock();
    let repo = open(root.clone(), interrupt)?;
    if repo.workdir().is_none() {
        return Err(unsupported("a bare repository has no working tree"));
    }
    if repo.state().is_some() {
        return Err(unsupported(
            "finish or abort the merge, rebase or other operation in progress first",
        ));
    }
    let settings = Settings::resolve(&repo, user)?;
    if settings.flag("commit.gpgSign").unwrap_or(false) {
        return Err(unsupported(
            "this repository signs commits (commit.gpgSign), which extensions cannot do",
        ));
    }
    if has_hooks(&repo, &settings) {
        return Err(unsupported(
            "the repository has commit hooks, which extensions never run",
        ));
    }
    let identity = |key| {
        settings
            .string(key)
            .filter(|value| !value.trim().is_empty())
    };
    let (Some(name), Some(email)) = (identity("user.name"), identity("user.email")) else {
        return Err(unsupported(
            "set user.name and user.email in git configuration first",
        ));
    };
    // Hold the index lock from here on, as `git commit` does, so no other
    // git process changes the index or commits in between.
    let (lock, mut index) = lock_index(&repo)?;

    let head = repo.head().map_err(|e| git_failure("head", &e))?;
    let parent = head.id().map(gix::Id::detach);
    let base = match parent {
        Some(id) => repo
            .find_commit(id)
            .and_then(|commit| commit.tree_id())
            .map_err(|e| git_failure("head", &e))?
            .detach(),
        None => ObjectId::empty_tree(repo.object_hash()),
    };
    let (new_tree, changes) = match &named {
        Some(named) => {
            let files = WorkingFiles::new(&repo, &index, &settings, user)?;
            named_tree(&repo, &root, &index, files, base, named, interrupt)?
        }
        None => (staged_tree(&repo, &index, interrupt)?, Vec::new()),
    };
    if new_tree == base {
        return Err(failure(
            HostErrorCode::Invalid,
            if named.is_some() {
                "nothing to commit: the named paths match HEAD"
            } else {
                "nothing to commit: nothing is staged"
            },
        ));
    }

    // The last point at which the call can stop without committing.
    check(interrupt)?;
    let signature = gix::actor::Signature {
        name,
        email,
        time: gix::date::Time::now_local_or_utc(),
    };
    let (mut committer_time, mut author_time) = Default::default();
    let made = repo.commit_as(
        signature.to_ref(&mut committer_time),
        signature.to_ref(&mut author_time),
        "HEAD",
        message,
        new_tree,
        parent,
    );
    let id = match made {
        Ok(id) => id.detach(),
        Err(error) => {
            let moved = repo.head_id().ok().map(gix::Id::detach) != parent;
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                moved,
                "extension commit failed"
            );
            return Err(if moved {
                failure(
                    HostErrorCode::Conflict,
                    "HEAD moved while committing; nothing was committed",
                )
            } else {
                failure(HostErrorCode::Io, "the commit could not be made")
            });
        }
    };

    // Committed: from here on a failure must say so.
    tracing::info!(commit = %id, "extension made a git commit");
    // A commit of what is staged leaves the index as it is; dropping the
    // lock releases it unchanged.
    if named.is_some() {
        update_index(&mut index, changes);
        if !write_index(&index, lock) {
            tracing::warn!(commit = %id, "committed, but the git index is not updated");
            return Err(failure(
                HostErrorCode::Io,
                &format!(
                    "committed {id}, but the git index could not be updated; \
                     run `git reset -- <paths>` on the committed paths"
                ),
            ));
        }
    }
    Ok(HostReply::Committed(GitCommit {
        id: id.to_string(),
        summary: message.lines().next().unwrap_or_default().trim().into(),
        author: signature.name.to_str_lossy().into_owned(),
        time: signature.time.seconds,
    }))
}

/// `HEAD`'s tree with the named paths set to their working-tree content,
/// and the index changes that match it.
fn named_tree(
    repo: &gix::Repository,
    root: &std::path::Path,
    index: &gix::index::File,
    mut files: WorkingFiles<'_>,
    base: ObjectId,
    named: &BTreeSet<BString>,
    interrupt: &AtomicBool,
) -> Result<(ObjectId, Vec<(BString, Change)>), HostFailure> {
    let mut tree = repo.edit_tree(base).map_err(|e| git_failure("tree", &e))?;
    let mut changes = Vec::new();
    for path in named {
        check(interrupt)?;
        let path = path.as_bstr();
        let indexed = index.entry_by_path_and_stage(path, Stage::Unconflicted);
        if indexed.is_some_and(|e| e.flags.contains(Flags::SKIP_WORKTREE)) {
            return Err(unsupported(
                "a path outside the sparse checkout cannot be committed",
            ));
        }
        let previous = tree.get(path).map(|entry| entry.mode().kind());
        if matches!(previous, Some(EntryKind::Tree | EntryKind::Commit)) {
            return Err(unsupported(
                "directories and submodules cannot be committed; name their files",
            ));
        }
        let known = indexed.is_some() || previous.is_some();
        if !known && parent_is_file(&tree, index, path) {
            return Err(failure(
                HostErrorCode::Conflict,
                "a parent of the path is a file in git",
            ));
        }
        let recorded_executable = previous == Some(EntryKind::BlobExecutable)
            || indexed.is_some_and(|e| e.mode == Mode::FILE_EXECUTABLE);
        let Some(change) = files.read(root, path, named, known, recorded_executable)? else {
            if !known {
                return Err(failure(
                    HostErrorCode::NotFound,
                    "a named path is neither in the working tree nor tracked",
                ));
            }
            tree.remove(path).map_err(|e| git_failure("tree", &e))?;
            changes.push((path.to_owned(), Change::Remove));
            continue;
        };
        if let Change::Write { id, mode, .. } = &change {
            tree.upsert(path, entry_kind(*mode), *id)
                .map_err(|e| git_failure("tree", &e))?;
        }
        changes.push((path.to_owned(), change));
    }
    Ok((
        tree.write().map_err(|e| git_failure("tree", &e))?.detach(),
        changes,
    ))
}

/// The tree the index describes, as `git write-tree` writes it.
fn staged_tree(
    repo: &gix::Repository,
    index: &gix::index::File,
    interrupt: &AtomicBool,
) -> Result<ObjectId, HostFailure> {
    let mut tree = repo
        .edit_tree(ObjectId::empty_tree(repo.object_hash()))
        .map_err(|e| git_failure("tree", &e))?;
    for (n, entry) in index.entries().iter().enumerate() {
        if n % 1024 == 0 {
            check(interrupt)?;
        }
        if entry.flags.contains(Flags::INTENT_TO_ADD) {
            return Err(unsupported(
                "a path is only marked to be added (git add -N); stage its content first",
            ));
        }
        let Some(mode) = entry.mode.to_tree_entry_mode() else {
            return Err(unsupported("the index holds an entry git cannot commit"));
        };
        tree.upsert(entry.path(index), mode.kind(), entry.id)
            .map_err(|e| git_failure("tree", &e))?;
    }
    Ok(tree.write().map_err(|e| git_failure("tree", &e))?.detach())
}

/// `a/b` for any spelling `check_relative` accepts, such as `a//b/`.
pub(super) fn normalized(path: &str) -> BString {
    let parts: Vec<_> = std::path::Path::new(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/").into()
}
fn entry_kind(mode: Mode) -> EntryKind {
    match mode {
        Mode::FILE_EXECUTABLE => EntryKind::BlobExecutable,
        Mode::SYMLINK => EntryKind::Link,
        _ => EntryKind::Blob,
    }
}
/// Whether `named` holds a path below `path`.
pub(super) fn names_below(named: &BTreeSet<BString>, path: &BStr) -> bool {
    let mut prefix = path.to_owned();
    prefix.push(b'/');
    named
        .range(prefix.clone()..)
        .next()
        .is_some_and(|below| below.starts_with(prefix.as_slice()))
}
pub(super) fn index_mode(kind: EntryKind) -> Mode {
    match kind {
        EntryKind::BlobExecutable => Mode::FILE_EXECUTABLE,
        EntryKind::Link => Mode::SYMLINK,
        _ => Mode::FILE,
    }
}
/// Whether a new path would sit below something git records as a file.
fn parent_is_file(
    tree: &gix::object::tree::Editor<'_>,
    index: &gix::index::State,
    path: &BStr,
) -> bool {
    path.char_indices()
        .filter(|(_, _, c)| *c == '/')
        .map(|(start, _, _)| path[..start].as_bstr())
        .any(|parent| {
            index.entry_by_path(parent).is_some()
                || tree
                    .get(parent)
                    .is_some_and(|entry| entry.mode().kind() != EntryKind::Tree)
        })
}

fn has_hooks(repo: &gix::Repository, settings: &Settings) -> bool {
    let base = repo.workdir().unwrap_or(repo.git_dir());
    let directory = match settings.path("core.hooksPath", base) {
        Ok(Some(directory)) => directory,
        Ok(None) => repo.git_dir().join("hooks"),
        // A hooks path that cannot be resolved is assumed to hold hooks.
        Err(()) => return true,
    };
    COMMIT_HOOKS
        .iter()
        .any(|hook| std::fs::symlink_metadata(directory.join(hook)).is_ok())
}

/// The excludes file git would use: `core.excludesFile` from the
/// repository or the user, else git's default location.
fn user_excludes(
    settings: &Settings,
    user: &UserGit,
    repo: &gix::Repository,
) -> Result<Option<gix::ignore::Search>, HostFailure> {
    let base = repo.workdir().unwrap_or(repo.git_dir());
    let path = match settings.path("core.excludesFile", base) {
        Ok(Some(path)) => path,
        Ok(None) => match &user.default_excludes {
            Some(path) => path.clone(),
            None => return Ok(None),
        },
        Err(()) => {
            return Err(unsupported(
                "git's core.excludesFile setting could not be resolved",
            ));
        }
    };
    let Some(bytes) = read_user_file(&path)? else {
        return Ok(None);
    };
    let mut search = gix::ignore::Search::default();
    search
        .add_patterns_buffer(&bytes, path, repo.workdir(), Default::default())
        .map_err(|e| git_failure("user_config", &e))?;
    Ok(Some(search))
}

pub(super) fn update_index(index: &mut gix::index::File, changes: Vec<(BString, Change)>) {
    // Lookups need sorted entries, so new ones are added only at the end.
    let mut added = Vec::new();
    for (path, change) in changes {
        match change {
            Change::Remove => index.remove_entries(|_, entry_path, entry| {
                entry_path == path && entry.stage() == Stage::Unconflicted
            }),
            Change::Write { id, mode, size } => {
                // A size with no times makes git compare the content again.
                let stat = Stat {
                    size,
                    ..Stat::default()
                };
                match index.entry_mut_by_path_and_stage(path.as_bstr(), Stage::Unconflicted) {
                    Some(entry) => {
                        entry.id = id;
                        entry.mode = mode;
                        entry.stat = stat;
                        entry.flags.remove(Flags::INTENT_TO_ADD);
                    }
                    None => added.push((path, stat, id, mode)),
                }
            }
        }
    }
    let sort = !added.is_empty();
    for (path, stat, id, mode) in added {
        index.dangerously_push_entry(stat, id, Flags::empty(), mode, path.as_bstr());
    }
    if sort {
        index.sort_entries();
    }
    // The cached trees no longer match the entries.
    index.remove_tree();
}
/// Write the updated index through the lock taken before changing it,
/// recording any failure here, where it is handled.
pub(super) fn write_index(index: &gix::index::File, lock: gix::lock::File) -> bool {
    let mut out = std::io::BufWriter::new(lock);
    if let Err(error) = index.write_to(&mut out, gix::index::write::Options::default()) {
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            "could not write the git index"
        );
        return false;
    }
    let lock = match out.into_inner() {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(
                error = error.error() as &dyn std::error::Error,
                "could not write the git index"
            );
            return false;
        }
    };
    if let Err(error) = lock.commit() {
        tracing::warn!(
            error = &error.error as &dyn std::error::Error,
            "could not replace the git index"
        );
        return false;
    }
    true
}

/// The index, locked against every other git process until the lock is
/// committed or dropped. Split, sparse and conflicted indexes are refused.
pub(super) fn lock_index(
    repo: &gix::Repository,
) -> Result<(gix::lock::File, gix::index::File), HostFailure> {
    let index_path = repo.index_path();
    let lock = gix::lock::File::acquire_to_update_resource(
        &index_path,
        gix::lock::acquire::Fail::Immediately,
        None,
        0,
    )
    .map_err(|e| {
        tracing::warn!(error = &e as &dyn std::error::Error, "git index is locked");
        failure(
            HostErrorCode::Conflict,
            "another git operation is using the repository; try again",
        )
    })?;
    let index = if index_path.is_file() {
        repo.open_index().map_err(|e| git_failure("index", &e))?
    } else {
        gix::index::File::from_state(gix::index::State::new(repo.object_hash()), index_path)
    };
    if index.link().is_some() || index.is_sparse() {
        return Err(unsupported("split and sparse indexes are not supported"));
    }
    if index
        .entries()
        .iter()
        .any(|e| e.stage() != Stage::Unconflicted)
    {
        return Err(unsupported("resolve the merge conflicts first"));
    }
    Ok((lock, index))
}

/// Working-tree files read as git would store them. Where git would change
/// content on the way in (filters, ident, encodings, line-ending
/// conversion), the file is refused rather than stored unconverted.
pub(super) struct WorkingFiles<'repo> {
    rules: gix::AttributeStack<'repo>,
    conversions: gix::attrs::search::Outcome,
    file_mode: bool,
    autocrlf: bool,
    /// File bytes read so far, bounded by [`MAX_COMMIT_BYTES`].
    written: u64,
}
impl<'repo> WorkingFiles<'repo> {
    pub(super) fn new(
        repo: &'repo gix::Repository,
        index: &gix::index::State,
        settings: &Settings,
        user: &UserGit,
    ) -> Result<Self, HostFailure> {
        let overrides = user_excludes(settings, user, repo)?;
        let rules = repo
            .attributes(
                index,
                gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
                gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
                overrides,
            )
            .map_err(|e| git_failure("attributes", &e))?;
        let conversions = rules.selected_attribute_matches(CONVERSIONS);
        Ok(Self {
            rules,
            conversions,
            file_mode: settings.flag("core.fileMode").unwrap_or(cfg!(unix)),
            autocrlf: settings
                .string("core.autocrlf")
                .is_some_and(|value| !value.eq_ignore_ascii_case(b"false")),
            written: 0,
        })
    }

    /// Store `path`'s working-tree content as a blob; `None` when nothing is
    /// there. `known` paths are tracked, so ignore rules do not refuse them;
    /// `recorded_executable` is kept when `core.fileMode` is off. A
    /// directory is refused, unless it replaces a tracked file and `named`
    /// also holds paths below it: then the file is gone, as with `None`.
    pub(super) fn read(
        &mut self,
        root: &std::path::Path,
        path: &BStr,
        named: &BTreeSet<BString>,
        known: bool,
        recorded_executable: bool,
    ) -> Result<Option<Change>, HostFailure> {
        let repo = self.rules.repo;
        let Some(file) = workspace::tracked(root.into(), &path.to_str_lossy())? else {
            return Ok(None);
        };
        let (bytes, kind) = match file {
            Tracked::Directory if known && names_below(named, path) => return Ok(None),
            Tracked::Directory => {
                return Err(unsupported(
                    "a named path is a directory; name the files in it",
                ));
            }
            Tracked::Link(target) => (target, EntryKind::Link),
            Tracked::File { bytes, executable } => {
                let executable = if self.file_mode {
                    executable
                } else {
                    recorded_executable
                };
                let kind = if executable {
                    EntryKind::BlobExecutable
                } else {
                    EntryKind::Blob
                };
                (bytes, kind)
            }
        };
        let platform = self
            .rules
            .at_entry(path, Some(index_mode(kind)))
            .map_err(|e| git_failure("attributes", &e))?;
        if !known && platform.is_excluded() {
            return Err(failure(
                HostErrorCode::Invalid,
                "a named path is ignored by git and not tracked",
            ));
        }
        platform.matching_attributes(&mut self.conversions);
        let mut converts_line_endings = self.autocrlf;
        for matched in self.conversions.iter_selected() {
            use gix::attrs::StateRef;
            if matches!(
                matched.assignment.state,
                StateRef::Unspecified | StateRef::Unset
            ) {
                continue;
            }
            match matched.assignment.name.as_str() {
                "text" | "eol" | "crlf" => converts_line_endings = true,
                _ => {
                    return Err(unsupported(
                        "a named file has a git filter, ident or encoding attribute, \
                         which extensions do not apply",
                    ));
                }
            }
        }
        if converts_line_endings && bytes.contains(&b'\r') {
            return Err(unsupported(
                "a named file has carriage returns that git would convert; \
                 extensions do not convert line endings",
            ));
        }
        self.written += bytes.len() as u64;
        if self.written > MAX_COMMIT_BYTES {
            return Err(failure(
                HostErrorCode::TooLarge,
                "the named files exceed 8 MiB together",
            ));
        }
        let size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        let id = repo
            .write_blob(&bytes)
            .map_err(|e| git_failure("blob", &e))?
            .detach();
        Ok(Some(Change::Write {
            id,
            mode: index_mode(kind),
            size,
        }))
    }
}
