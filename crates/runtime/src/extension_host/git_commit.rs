//! The one git write host call: commit named working-tree files on top of
//! `HEAD`, like `git commit --only`. Nothing the repository or the user
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
    /// The user's excludes file: `core.excludesFile` or git's default.
    excludes: Option<(PathBuf, Vec<u8>)>,
    home: Option<PathBuf>,
}
impl UserGit {
    /// System, installation and global configuration, as `git` reads it.
    pub(super) fn load() -> Result<Self, HostFailure> {
        let config =
            gix::config::File::from_globals().map_err(|e| git_failure("user_config", &e))?;
        let home = gix::path::env::home_dir();
        let configured = config
            .path("core.excludesFile")
            .map(|path| {
                path.interpolate(gix::config::path::interpolate::Context {
                    home_dir: home.as_deref(),
                    ..Default::default()
                })
                .map_err(|e| git_failure("user_config", &e))
            })
            .transpose()?;
        let default = || {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(".config")))
                .map(|base| base.join("git").join("ignore"))
        };
        let excludes = match configured.or_else(default) {
            Some(path) => read_user_file(&path)?.map(|bytes| (path, bytes)),
            None => None,
        };
        Ok(Self {
            config,
            excludes,
            home,
        })
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

/// A setting from the repository, else from the user.
struct Settings<'a> {
    repository: &'a gix::config::File,
    user: &'a UserGit,
}
impl Settings<'_> {
    fn string(&self, key: &str) -> Option<BString> {
        self.repository
            .string(key)
            .or_else(|| self.user.config.string(key))
    }
    /// An unreadable value counts as `true`, so it never weakens a check.
    fn flag(&self, key: &str) -> Option<bool> {
        let read = |config: &gix::config::File| config.boolean(key).unwrap_or(Some(true));
        read(self.repository).or_else(|| read(&self.user.config))
    }
}

enum Change {
    Write { id: ObjectId, mode: Mode, size: u32 },
    Remove,
}

pub(super) fn commit(
    root: PathBuf,
    message: &str,
    paths: &[String],
    user: &UserGit,
    interrupt: &AtomicBool,
) -> Result<HostReply, HostFailure> {
    if message.trim().is_empty() || message.len() > MAX_COMMIT_MESSAGE || message.contains('\0') {
        return Err(failure(
            HostErrorCode::Invalid,
            "the commit message must be 1 to 16 KiB of text",
        ));
    }
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
    let snapshot = repo.config_snapshot();
    let settings = Settings {
        repository: snapshot.plumbing(),
        user,
    };
    if settings.flag("commit.gpgSign").unwrap_or(false) {
        return Err(unsupported(
            "this repository signs commits (commit.gpgSign), which extensions cannot do",
        ));
    }
    if has_hooks(&repo, &settings, user)? {
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
    let file_mode = settings.flag("core.fileMode").unwrap_or(cfg!(unix));
    let autocrlf = settings
        .string("core.autocrlf")
        .is_some_and(|value| !value.eq_ignore_ascii_case(b"false"));

    // Hold the index lock from here on, as `git commit` does, so no other
    // git process changes the index or commits in between.
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
    let mut index = if index_path.is_file() {
        repo.open_index().map_err(|e| git_failure("index", &e))?
    } else {
        gix::index::File::from_state(gix::index::State::new(repo.object_hash()), index_path)
    };
    if index.link().is_some() || index.is_sparse() {
        return Err(unsupported(
            "split and sparse indexes are not supported for commits",
        ));
    }
    if index
        .entries()
        .iter()
        .any(|e| e.stage() != Stage::Unconflicted)
    {
        return Err(unsupported("resolve the merge conflicts first"));
    }

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
    let mut tree = repo.edit_tree(base).map_err(|e| git_failure("tree", &e))?;
    let overrides = user_excludes(user, &repo)?;
    let mut rules = repo
        .attributes(
            &index,
            gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
            overrides,
        )
        .map_err(|e| git_failure("attributes", &e))?;
    let mut conversions = rules.selected_attribute_matches(CONVERSIONS);

    let mut changes = Vec::new();
    let mut written = 0u64;
    for path in &named {
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
        if !known && parent_is_file(&tree, &index, path) {
            return Err(failure(
                HostErrorCode::Conflict,
                "a parent of the path is a file in git",
            ));
        }
        let Some(file) = workspace::tracked(root.clone(), &path.to_str_lossy())? else {
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
        let (bytes, kind) = match file {
            Tracked::Link(target) => (target, EntryKind::Link),
            Tracked::File { bytes, executable } => {
                // Without `core.fileMode` the recorded bit is kept.
                let executable = if file_mode {
                    executable
                } else {
                    previous == Some(EntryKind::BlobExecutable)
                        || indexed.is_some_and(|e| e.mode == Mode::FILE_EXECUTABLE)
                };
                let kind = if executable {
                    EntryKind::BlobExecutable
                } else {
                    EntryKind::Blob
                };
                (bytes, kind)
            }
        };
        let platform = rules
            .at_entry(path, Some(index_mode(kind)))
            .map_err(|e| git_failure("attributes", &e))?;
        if !known && platform.is_excluded() {
            return Err(failure(
                HostErrorCode::Invalid,
                "a named path is ignored by git and not tracked",
            ));
        }
        platform.matching_attributes(&mut conversions);
        let mut converts_line_endings = autocrlf;
        for matched in conversions.iter_selected() {
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
        written += bytes.len() as u64;
        if written > MAX_COMMIT_BYTES {
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
        tree.upsert(path, kind, id)
            .map_err(|e| git_failure("tree", &e))?;
        changes.push((
            path.to_owned(),
            Change::Write {
                id,
                mode: index_mode(kind),
                size,
            },
        ));
    }
    let new_tree = tree.write().map_err(|e| git_failure("tree", &e))?.detach();
    if new_tree == base {
        return Err(failure(
            HostErrorCode::Invalid,
            "nothing to commit: the named paths match HEAD",
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
    update_index(&mut index, changes);
    if !write_index(&index, lock, id) {
        return Err(failure(
            HostErrorCode::Io,
            &format!(
                "committed {id}, but the git index could not be updated; \
                 run `git reset -- <paths>` on the committed paths"
            ),
        ));
    }
    Ok(HostReply::Committed(GitCommit {
        id: id.to_string(),
        summary: message.lines().next().unwrap_or_default().trim().into(),
        author: signature.name.to_str_lossy().into_owned(),
        time: signature.time.seconds,
    }))
}

/// `a/b` for any spelling `check_relative` accepts, such as `a//b/`.
fn normalized(path: &str) -> BString {
    let parts: Vec<_> = std::path::Path::new(path)
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/").into()
}
fn index_mode(kind: EntryKind) -> Mode {
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

fn has_hooks(
    repo: &gix::Repository,
    settings: &Settings<'_>,
    user: &UserGit,
) -> Result<bool, HostFailure> {
    let directory = match settings.string("core.hooksPath") {
        Some(configured) => {
            // A hooks path that cannot be resolved is assumed to hold hooks.
            let Ok(configured) = gix::path::from_bstring(configured) else {
                return Ok(true);
            };
            match configured.strip_prefix("~") {
                Ok(rest) => match &user.home {
                    Some(home) => home.join(rest),
                    None => return Ok(true),
                },
                Err(_) if configured.is_relative() => match repo.workdir() {
                    Some(workdir) => workdir.join(configured),
                    None => return Ok(true),
                },
                Err(_) => configured,
            }
        }
        None => repo.git_dir().join("hooks"),
    };
    Ok(COMMIT_HOOKS
        .iter()
        .any(|hook| std::fs::symlink_metadata(directory.join(hook)).is_ok()))
}

fn user_excludes(
    user: &UserGit,
    repo: &gix::Repository,
) -> Result<Option<gix::ignore::Search>, HostFailure> {
    let Some((path, bytes)) = &user.excludes else {
        return Ok(None);
    };
    let mut search = gix::ignore::Search::default();
    search
        .add_patterns_buffer(bytes, path.clone(), repo.workdir(), Default::default())
        .map_err(|e| git_failure("user_config", &e))?;
    Ok(Some(search))
}

fn update_index(index: &mut gix::index::File, changes: Vec<(BString, Change)>) {
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
/// Write the updated index through the lock taken before committing,
/// recording any failure here, where it is handled.
fn write_index(index: &gix::index::File, lock: gix::lock::File, commit: ObjectId) -> bool {
    let mut out = std::io::BufWriter::new(lock);
    if let Err(error) = index.write_to(&mut out, gix::index::write::Options::default()) {
        tracing::warn!(error = &error as &dyn std::error::Error, %commit, "committed but could not write the git index");
        return false;
    }
    let lock = match out.into_inner() {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(error = error.error() as &dyn std::error::Error, %commit, "committed but could not write the git index");
            return false;
        }
    };
    if let Err(error) = lock.commit() {
        tracing::warn!(error = &error.error as &dyn std::error::Error, %commit, "committed but could not replace the git index");
        return false;
    }
    true
}
