//! Read-only git host calls through gitoxide. Nothing the repository
//! configures can run: no hooks, filter, diff or credential programs, and no
//! system, global or `GIT_*` environment configuration is read.
use super::{MAX_FILE, failure};
use a206_extensions::abi::{
    GitArea, GitChange, GitCommit, GitDiff, GitEntry, GitStatus, HostErrorCode, HostFailure,
    HostReply,
};
use gix::bstr::{BString, ByteSlice};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

/// Most changed paths one status lists.
pub const MAX_STATUS_ENTRIES: usize = 1_000;
/// Most commits one log returns.
pub const MAX_COMMITS: u32 = 200;
/// Longest diff text returned.
pub const MAX_DIFF: usize = 192 * 1024;
const CONTEXT_LINES: u32 = 3;

/// Record a gitoxide failure where it is handled; the guest gets a generic
/// code because the message can name host paths.
fn git_failure(operation: &'static str, error: &(dyn std::error::Error + 'static)) -> HostFailure {
    tracing::warn!(error, operation, "extension git call failed");
    failure(HostErrorCode::Io, "the repository could not be read")
}

/// Stop once the call's deadline has passed or its caller has gone.
fn check(interrupt: &AtomicBool) -> Result<(), HostFailure> {
    if interrupt.load(std::sync::atomic::Ordering::Relaxed) {
        Err(failure(HostErrorCode::Budget, "the git call took too long"))
    } else {
        Ok(())
    }
}

fn unsupported(message: &'static str) -> HostFailure {
    failure(HostErrorCode::Unsupported, message)
}

/// Most entries a repository's metadata may hold for it to be checked.
const MAX_METADATA_ENTRIES: usize = 200_000;

/// Refuse a repository whose metadata holds a symbolic link anywhere, such
/// as a pack directory or loose object pointing outside the project.
pub(super) fn contained(
    git_dir: &std::path::Path,
    interrupt: &AtomicBool,
) -> Result<(), HostFailure> {
    let mut pending = vec![git_dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|e| git_failure("open", &e))?;
        for entry in entries {
            let entry = entry.map_err(|e| git_failure("open", &e))?;
            seen += 1;
            check(interrupt)?;
            if seen > MAX_METADATA_ENTRIES {
                return Err(unsupported(
                    "the repository has too many loose files to check; pack it with `git gc`",
                ));
            }
            // `DirEntry::file_type` describes the entry itself, not a link target.
            let kind = entry.file_type().map_err(|e| git_failure("open", &e))?;
            if kind.is_symlink() {
                return Err(unsupported(
                    "repository metadata that is a symbolic link is not supported",
                ));
            }
            if kind.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Open the repository at exactly the workspace root, never a parent: either
/// a `.git` directory there or a bare repository that is the root itself.
/// Every piece of repository metadata must live inside the project, so a
/// `.git` file or symlink, a linked worktree (`commondir`), an alternate
/// object store or a configured worktree elsewhere is refused.
fn open(root: PathBuf, interrupt: &AtomicBool) -> Result<gix::Repository, HostFailure> {
    let dot_git = root.join(".git");
    let git_dir = match std::fs::symlink_metadata(&dot_git) {
        Ok(metadata) if metadata.is_dir() => dot_git,
        Ok(_) => {
            return Err(unsupported(
                "`.git` must be a directory in the project; linked worktrees and \
                 submodule checkouts are not supported",
            ));
        }
        Err(_) if root.join("HEAD").is_file() && root.join("objects").is_dir() => root.clone(),
        Err(_) => {
            return Err(failure(
                HostErrorCode::NotFound,
                "the workspace is not a git repository",
            ));
        }
    };
    contained(&git_dir, interrupt)?;
    let shared = ["commondir", "objects/info/alternates"]
        .iter()
        .any(|name| std::fs::symlink_metadata(git_dir.join(name)).is_ok());
    if shared {
        return Err(unsupported(
            "repositories that share objects or metadata outside the project are not supported",
        ));
    }
    // `isolated` drops system and global configuration and the environment.
    // Refusing every section to the "trusted values" filter disables the
    // repository's own filter, diff and credential programs and URL rewrites.
    let options = gix::open::Options::isolated().filter_config_section(|_| false);
    let repo = gix::open_opts(&git_dir, options).map_err(|e| git_failure("open", &e))?;
    let same = |a: &std::path::Path, b: &std::path::Path| match (
        std::fs::canonicalize(a),
        std::fs::canonicalize(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    let inside = same(repo.git_dir(), &git_dir)
        && same(repo.common_dir(), &git_dir)
        && match repo.workdir() {
            Some(workdir) => git_dir != root && same(workdir, &root),
            None => true,
        };
    if !inside {
        return Err(unsupported(
            "the repository's working tree or metadata is outside the project",
        ));
    }
    Ok(repo)
}

pub(super) fn status(root: PathBuf, interrupt: Arc<AtomicBool>) -> Result<HostReply, HostFailure> {
    let repo = open(root, &interrupt)?;
    let head = repo.head().map_err(|e| git_failure("head", &e))?;
    let mut status = GitStatus {
        branch: head
            .referent_name()
            .map(|name| name.shorten().to_str_lossy().into_owned()),
        head: head.id().map(|id| id.to_string()),
        ..GitStatus::default()
    };
    if repo.workdir().is_none() {
        check(&interrupt)?;
        return Ok(HostReply::GitStatus(status));
    }
    let changes = repo
        .status(gix::progress::Discard)
        .map_err(|e| git_failure("status", &e))?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .index_worktree_rewrites(None)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .index_worktree_submodules(gix::status::Submodule::Given {
            ignore: gix::submodule::config::Ignore::All,
            check_dirty: false,
        })
        .should_interrupt_owned(Arc::clone(&interrupt))
        .into_iter(None::<BString>)
        .map_err(|e| git_failure("status", &e))?;
    for item in changes {
        let item = item.map_err(|e| git_failure("status", &e))?;
        let Some(entry) = entry(&item) else {
            continue;
        };
        if status.entries.len() == MAX_STATUS_ENTRIES {
            status.truncated = true;
            break;
        }
        status.entries.push(entry);
    }
    check(&interrupt)?;
    status
        .entries
        .sort_by(|a, b| (&a.path, a.area).cmp(&(&b.path, b.area)));
    Ok(HostReply::GitStatus(status))
}

fn entry(item: &gix::status::Item) -> Option<GitEntry> {
    use gix::status::index_worktree::iter::Summary;
    match item {
        gix::status::Item::IndexWorktree(item) => {
            let (area, change) = match item.summary()? {
                Summary::Added => (GitArea::Untracked, GitChange::Added),
                Summary::Removed => (GitArea::Unstaged, GitChange::Removed),
                Summary::Modified => (GitArea::Unstaged, GitChange::Modified),
                Summary::TypeChange => (GitArea::Unstaged, GitChange::TypeChange),
                Summary::Conflict => (GitArea::Unstaged, GitChange::Conflict),
                Summary::IntentToAdd => (GitArea::Unstaged, GitChange::Added),
                Summary::Renamed | Summary::Copied => (GitArea::Unstaged, GitChange::Other),
            };
            Some(GitEntry {
                path: item.rela_path().to_str_lossy().into_owned(),
                area,
                change,
            })
        }
        gix::status::Item::TreeIndex(change) => {
            use gix::diff::index::ChangeRef;
            let change_kind = match change {
                ChangeRef::Addition { .. } => GitChange::Added,
                ChangeRef::Deletion { .. } => GitChange::Removed,
                ChangeRef::Modification { .. } => GitChange::Modified,
                ChangeRef::Rewrite { .. } => GitChange::Other,
            };
            Some(GitEntry {
                path: change.location().to_str_lossy().into_owned(),
                area: GitArea::Staged,
                change: change_kind,
            })
        }
    }
}

pub(super) fn log(
    root: PathBuf,
    max: u32,
    interrupt: &AtomicBool,
) -> Result<HostReply, HostFailure> {
    let repo = open(root, interrupt)?;
    let head = repo.head().map_err(|e| git_failure("head", &e))?;
    let Some(id) = head.id() else {
        check(interrupt)?;
        return Ok(HostReply::Commits(Vec::new()));
    };
    let walk = id.ancestors().all().map_err(|e| git_failure("log", &e))?;
    let mut commits = Vec::new();
    for info in walk.take(max.clamp(1, MAX_COMMITS) as usize) {
        check(interrupt)?;
        let info = info.map_err(|e| git_failure("log", &e))?;
        let commit = info.object().map_err(|e| git_failure("log", &e))?;
        let message = commit.message().map_err(|e| git_failure("log", &e))?;
        let author = commit.author().map_err(|e| git_failure("log", &e))?;
        commits.push(GitCommit {
            id: commit.id.to_string(),
            summary: message.summary().to_str_lossy().into_owned(),
            author: author.name.to_str_lossy().into_owned(),
            time: author.time().map_err(|e| git_failure("log", &e))?.seconds,
        });
    }
    check(interrupt)?;
    Ok(HostReply::Commits(commits))
}

pub(super) fn diff(
    root: PathBuf,
    path: &str,
    staged: bool,
    interrupt: &AtomicBool,
) -> Result<HostReply, HostFailure> {
    super::workspace::check_relative(path)?;
    let repo = open(root.clone(), interrupt)?;
    let index = repo
        .index_or_empty()
        .map_err(|e| git_failure("index", &e))?;
    let indexed = index
        .entry_by_path(path.as_bytes().as_bstr())
        .map(|entry| blob(&repo, entry.id))
        .transpose()?;
    let (before, after) = if staged {
        let committed = match repo.head_tree() {
            Ok(tree) => tree
                .lookup_entry_by_path(path)
                .map_err(|e| git_failure("diff", &e))?
                .map(|entry| blob(&repo, entry.object_id()))
                .transpose()?,
            // An unborn `HEAD` has no tree: every staged file is new.
            Err(_) if repo.head().is_ok_and(|head| head.is_unborn()) => None,
            Err(error) => return Err(git_failure("diff", &error)),
        };
        (committed, indexed)
    } else {
        if repo.workdir().is_none() {
            return Err(unsupported("a bare repository has no working tree"));
        }
        (indexed, super::workspace::read_worktree(root, path)?)
    };
    let (before, after) = (before.unwrap_or_default(), after.unwrap_or_default());
    check(interrupt)?;
    if before == after {
        return Ok(HostReply::Diff(GitDiff::default()));
    }
    if before.contains(&0) || after.contains(&0) {
        return Ok(HostReply::Diff(GitDiff {
            binary: true,
            ..GitDiff::default()
        }));
    }
    check(interrupt)?;
    let text = unified(&before, &after)?;
    check(interrupt)?;
    let truncated = text.len() > MAX_DIFF;
    Ok(HostReply::Diff(GitDiff {
        text: super::bounded(&text, MAX_DIFF).to_string(),
        binary: false,
        truncated,
    }))
}

fn blob(repo: &gix::Repository, id: impl Into<gix::ObjectId>) -> Result<Vec<u8>, HostFailure> {
    let id = id.into();
    // Check the stored size first so an oversized blob is never inflated.
    let header = repo.find_header(id).map_err(|e| git_failure("diff", &e))?;
    if header.size() > MAX_FILE {
        return Err(failure(HostErrorCode::TooLarge, "the file exceeds 512 KiB"));
    }
    let object = repo.find_object(id).map_err(|e| git_failure("diff", &e))?;
    Ok(object.detach().data)
}

fn unified(before: &[u8], after: &[u8]) -> Result<String, HostFailure> {
    use gix::diff::blob::{
        Algorithm, Diff, InternedInput, UnifiedDiff,
        unified_diff::{ConsumeBinaryHunk, ContextSize},
    };
    let input = InternedInput::new(before, after);
    // gitoxide's diff/postprocessing have no cancellation API. Bound their
    // worst-case work before entering them, in addition to the byte limit.
    // Reject rather than keep a blocking worker alive on pathological input.
    if input.before.len().saturating_mul(input.after.len()) > 1_000_000
        || input.before.len().saturating_add(input.after.len()) > 8_192
    {
        return Err(failure(
            HostErrorCode::Budget,
            "the diff exceeds the line comparison budget",
        ));
    }
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    UnifiedDiff::new(
        &diff,
        &input,
        ConsumeBinaryHunk::new(String::new(), "\n"),
        ContextSize::symmetrical(CONTEXT_LINES),
    )
    .consume()
    .map_err(|_| failure(HostErrorCode::Invalid, "the file is not UTF-8 text"))
}
