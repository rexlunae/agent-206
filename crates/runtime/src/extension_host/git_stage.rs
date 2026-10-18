//! Staging and unstaging named paths, like `git add` and `git reset --`
//! on them: only the index changes. The working tree, `HEAD` and every
//! other index entry are left as they are, and nothing the repository or
//! the user configures runs.
use super::{
    failure,
    git::{check, git_failure, open, unsupported},
    git_commit::{
        Change, MAX_COMMIT_PATHS, Settings, UserGit, WorkingFiles, index_mode, lock_index,
        normalized, update_index, write_index,
    },
    workspace,
};
use a206_extensions::abi::{HostErrorCode, HostFailure, HostReply};
use gix::{
    bstr::{BStr, BString, ByteSlice},
    index::entry::{Flags, Mode, Stage},
    objs::tree::EntryKind,
};
use std::{collections::BTreeSet, path::PathBuf, sync::atomic::AtomicBool};

pub(super) fn stage(
    root: PathBuf,
    paths: &[String],
    unstage: bool,
    // Read only to stage: unstaging uses nothing but `HEAD` and the index.
    user: impl FnOnce() -> Result<UserGit, HostFailure>,
    interrupt: &AtomicBool,
) -> Result<HostReply, HostFailure> {
    if paths.is_empty() || paths.len() > MAX_COMMIT_PATHS {
        return Err(failure(
            HostErrorCode::Invalid,
            "name 1 to 100 paths to stage or unstage",
        ));
    }
    let mut named = BTreeSet::new();
    for path in paths {
        workspace::check_relative(path)?;
        named.insert(normalized(path));
    }
    // Workspace writes cannot change a file between its read and staging.
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
    let (lock, mut index) = lock_index(&repo)?;
    let changes = if unstage {
        from_head(&repo, &index, &named, interrupt)?
    } else {
        let user = user()?;
        let settings = Settings::resolve(&repo, &user)?;
        from_working_tree(&repo, &root, &index, &settings, &user, &named, interrupt)?
    };
    refuse_mixed_shapes(&index, &changes)?;
    // The last point at which the call can stop without changing the index.
    check(interrupt)?;
    update_index(&mut index, changes);
    if !write_index(&index, lock) {
        return Err(failure(
            HostErrorCode::Io,
            "the git index could not be updated; nothing was staged or unstaged",
        ));
    }
    Ok(HostReply::Done)
}

/// Index entries for the named paths' working-tree content, as `git add`.
fn from_working_tree(
    repo: &gix::Repository,
    root: &std::path::Path,
    index: &gix::index::File,
    settings: &Settings,
    user: &UserGit,
    named: &BTreeSet<BString>,
    interrupt: &AtomicBool,
) -> Result<Vec<(BString, Change)>, HostFailure> {
    let mut files = WorkingFiles::new(repo, index, settings, user)?;
    let mut changes = Vec::new();
    for path in named {
        check(interrupt)?;
        let path = path.as_bstr();
        let indexed = index.entry_by_path_and_stage(path, Stage::Unconflicted);
        if let Some(entry) = indexed {
            if entry.flags.contains(Flags::SKIP_WORKTREE) {
                return Err(unsupported(
                    "a path outside the sparse checkout cannot be staged",
                ));
            }
            if entry.mode == Mode::COMMIT {
                return Err(directory());
            }
        }
        let recorded_executable = indexed.is_some_and(|e| e.mode == Mode::FILE_EXECUTABLE);
        match files.read(root, path, indexed.is_some(), recorded_executable)? {
            Some(change) => changes.push((path.to_owned(), change)),
            None if indexed.is_some() => changes.push((path.to_owned(), Change::Remove)),
            None if holds_entries(index, path) => return Err(directory()),
            None => {
                return Err(failure(
                    HostErrorCode::NotFound,
                    "a named path is neither in the working tree nor tracked",
                ));
            }
        }
    }
    Ok(changes)
}

/// Git's index holds a path as a file or as a directory, never both: no
/// written path may sit below a file that stays in the index, nor above
/// entries that stay in it.
fn refuse_mixed_shapes(
    index: &gix::index::File,
    changes: &[(BString, Change)],
) -> Result<(), HostFailure> {
    let (mut written, mut removed) = (BTreeSet::new(), BTreeSet::new());
    for (path, change) in changes {
        match change {
            Change::Write { .. } => written.insert(path.as_bstr()),
            Change::Remove => removed.insert(path.as_bstr()),
        };
    }
    let stays = |path: &BStr| {
        written.contains(path) || (!removed.contains(path) && index.entry_by_path(path).is_some())
    };
    for path in &written {
        let below_file = path
            .char_indices()
            .filter(|(_, _, c)| *c == '/')
            .any(|(start, _, _)| stays(path[..start].as_bstr()));
        if below_file {
            return Err(failure(
                HostErrorCode::Conflict,
                "a parent of the path is a file in git; name that file too",
            ));
        }
        let mut prefix = path.to_vec();
        prefix.push(b'/');
        let prefix = prefix.as_bstr();
        let indexed_below = index.prefixed_entries(prefix).is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| !removed.contains(entry.path(index)))
        });
        let written_below = written
            .range(prefix..)
            .next()
            .is_some_and(|below| below.starts_with(prefix));
        if indexed_below || written_below {
            return Err(failure(
                HostErrorCode::Conflict,
                "git tracks files below the path; name those files too",
            ));
        }
    }
    Ok(())
}
/// Whether the index has entries below `path`, a directory to git.
fn holds_entries(index: &gix::index::File, path: &BStr) -> bool {
    let mut prefix = path.to_owned();
    prefix.push(b'/');
    index
        .prefixed_entries(prefix.as_bstr())
        .is_some_and(|entries| !entries.is_empty())
}
fn directory() -> HostFailure {
    unsupported("directories and submodules cannot be staged or unstaged; name their files")
}

/// Index entries matching `HEAD` for the named paths, as `git reset --`.
fn from_head(
    repo: &gix::Repository,
    index: &gix::index::File,
    named: &BTreeSet<BString>,
    interrupt: &AtomicBool,
) -> Result<Vec<(BString, Change)>, HostFailure> {
    let tree = repo
        .head_tree_id_or_empty()
        .and_then(|id| id.object())
        .map_err(|e| git_failure("head", &e))?
        .into_tree();
    let mut changes = Vec::new();
    for path in named {
        check(interrupt)?;
        let path = path.as_bstr();
        let committed = tree
            .lookup_entry_by_path(path.to_str_lossy().as_ref())
            .map_err(|e| git_failure("tree", &e))?;
        let change = match committed {
            Some(entry) => {
                let kind = entry.mode().kind();
                if matches!(kind, EntryKind::Tree | EntryKind::Commit) {
                    return Err(directory());
                }
                let id = entry.object_id();
                let size = repo
                    .find_header(id)
                    .map_err(|e| git_failure("blob", &e))?
                    .size();
                Change::Write {
                    id,
                    mode: index_mode(kind),
                    size: u32::try_from(size).unwrap_or(u32::MAX),
                }
            }
            None if index.entry_by_path(path).is_some() => Change::Remove,
            None if holds_entries(index, path) => return Err(directory()),
            None => {
                return Err(failure(
                    HostErrorCode::NotFound,
                    "a named path is neither staged nor in HEAD",
                ));
            }
        };
        changes.push((path.to_owned(), change));
    }
    Ok(changes)
}
