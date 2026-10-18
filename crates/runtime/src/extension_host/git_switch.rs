//! Switching to a local branch, like `git switch <branch>`: the working
//! tree and the index are changed to the branch's commit, then `HEAD` names
//! the branch. Only a clean working tree switches, so nothing is carried
//! over, and a file the switch would put where an untracked or ignored one
//! lies refuses it instead of replacing it. Where git would run something
//! or change content on the way out (hooks, smudge filters, line-ending
//! conversion), and for submodules and symbolic links, the switch is
//! refused rather than made differently.
use super::{
    MAX_FILE, failure,
    git::{check, git_failure, open, unsupported},
    git_branch::branch_name,
    git_commit::{CONVERSIONS, Settings, UserGit, has_hooks, lock_index, write_index},
    workspace,
};
use a206_extensions::abi::{GitBranch, HostErrorCode, HostFailure, HostReply};
use gix::{
    ObjectId,
    bstr::{BStr, ByteSlice},
    index::entry::{Flags, Mode, Stat},
    refs::{
        Target,
        transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    },
};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

/// Most files one switch writes or removes.
pub const MAX_SWITCH_FILES: usize = 1_000;
/// Most file bytes one switch reads and writes.
pub const MAX_SWITCH_BYTES: u64 = 32 * 1024 * 1024;
/// Hooks git runs while switching. Their presence refuses the switch.
const SWITCH_HOOKS: [&str; 2] = ["post-checkout", "reference-transaction"];

/// One working-tree path a switch changes.
enum Step {
    /// A tracked file or link that the branch does not have.
    Remove { link: bool, held: Vec<u8> },
    /// A file the branch has, new here or replacing a tracked file.
    Write {
        contents: Vec<u8>,
        executable: bool,
        held: Option<Vec<u8>>,
    },
}

pub(super) fn switch(
    root: PathBuf,
    name: &str,
    user: impl FnOnce() -> Result<UserGit, HostFailure>,
    interrupt: Arc<AtomicBool>,
) -> Result<HostReply, HostFailure> {
    let full = branch_name(name).ok_or_else(|| {
        failure(
            HostErrorCode::Invalid,
            "the branch name is not one git accepts",
        )
    })?;
    // No extension write can change a file between its check and its switch.
    let _writing = workspace::write_lock();
    let repo = open(root.clone(), &interrupt)?;
    if repo.workdir().is_none() {
        return Err(unsupported("a bare repository has no working tree"));
    }
    if repo.state().is_some() {
        return Err(unsupported(
            "finish or abort the merge, rebase or other operation in progress first",
        ));
    }
    let user = user()?;
    let settings = Settings::resolve(&repo, &user)?;
    if has_hooks(&repo, &settings, &SWITCH_HOOKS) {
        return Err(unsupported(
            "the repository has a post-checkout or reference-transaction hook, \
             which extensions never run",
        ));
    }
    if converts_line_endings(&settings) {
        return Err(unsupported(
            "the repository converts line endings when files are checked out \
             (core.autocrlf or core.eol), which extensions do not do",
        ));
    }
    let (committer, email) = settings.identity()?;
    let mut target = repo
        .try_find_reference(&full)
        .map_err(|e| git_failure("switch", &e))?
        .ok_or_else(|| failure(HostErrorCode::NotFound, "no local branch has that name"))?;
    let commit = target
        .peel_to_commit()
        .map_err(|e| git_failure("switch", &e))?;
    let reply = GitBranch {
        name: name.into(),
        commit: commit.id.to_string(),
        current: true,
    };
    let head = repo.head().map_err(|e| git_failure("head", &e))?;
    if head.referent_name() == Some(full.as_ref()) {
        return Ok(HostReply::Branch(reply));
    }
    let (previous, from) = match &head.kind {
        gix::head::Kind::Symbolic(reference) => (
            Target::Symbolic(reference.name.clone()),
            reference.name.shorten().to_string(),
        ),
        gix::head::Kind::Unborn(name) => {
            (Target::Symbolic(name.clone()), name.shorten().to_string())
        }
        gix::head::Kind::Detached { target, .. } => (Target::Object(*target), target.to_string()),
    };
    let left = head.id().map(gix::Id::detach);
    let tree = commit.tree_id().map_err(|e| git_failure("switch", &e))?;

    let (lock, index) = lock_index(&repo)?;
    if index
        .entries()
        .iter()
        .any(|entry| entry.flags.contains(Flags::SKIP_WORKTREE))
    {
        return Err(unsupported("sparse checkouts are not supported"));
    }
    clean(&repo, &interrupt)?;
    let mut next = repo
        .index_from_tree(&tree)
        .map_err(|e| git_failure("switch", &e))?;
    let steps = plan(&repo, &index, &mut next, &interrupt)?;
    convertible(&repo, &next, &steps)?;
    let workspace = workspace::open_workspace(root)?;
    let leaving: BTreeSet<&str> = steps
        .iter()
        .filter(|(_, step)| matches!(step, Step::Remove { .. }))
        .map(|(path, _)| path.as_str())
        .collect();
    for (path, step) in &steps {
        check(&interrupt)?;
        if matches!(step, Step::Write { held: None, .. })
            && workspace::in_the_way(&workspace, path, &leaving)?
        {
            return Err(failure(
                HostErrorCode::Conflict,
                &format!(
                    "an untracked or ignored file is where the branch has {path}; \
                     move it first"
                ),
            ));
        }
    }

    // The last point at which the switch can stop with nothing changed.
    check(&interrupt)?;
    let removals = steps
        .iter()
        .filter(|(_, step)| matches!(step, Step::Remove { .. }));
    let writes = steps
        .iter()
        .filter(|(_, step)| matches!(step, Step::Write { .. }));
    for (path, step) in removals.chain(writes) {
        let done = match step {
            Step::Remove { link, held } => {
                workspace::remove_checked_out(&workspace, path, *link, held)
            }
            Step::Write {
                contents,
                executable,
                held,
            } => {
                workspace::check_out_file(&workspace, path, contents, held.as_deref(), *executable)
            }
        };
        if let Err(cause) = done {
            return Err(failure(
                cause.code,
                &format!(
                    "switching stopped at {path} ({}); some files now match {name} while \
                     the index and HEAD still name {from}",
                    cause.message
                ),
            ));
        }
    }
    if !write_index(&next, lock) {
        return Err(failure(
            HostErrorCode::Io,
            &format!(
                "the files now match {name}, but the index could not be written; \
                 HEAD still names {from}"
            ),
        ));
    }
    let signature = gix::actor::Signature {
        name: committer,
        email,
        time: gix::date::Time::now_local_or_utc(),
    };
    let mut time = Default::default();
    let message = format!("checkout: moving from {from} to {name}");
    let edit = RefEdit {
        change: Change::Update {
            // gitoxide records no reflog entry for a symbolic change; it is
            // appended below, as git writes it.
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: message.as_str().into(),
            },
            expected: PreviousValue::MustExistAndMatch(previous),
            new: Target::Symbolic(full),
        },
        name: "HEAD".try_into().map_err(|e| git_failure("switch", &e))?,
        deref: false,
    };
    if let Err(error) = repo.edit_references_as([edit], Some(signature.to_ref(&mut time))) {
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            "extension branch switch could not move HEAD"
        );
        return Err(failure(
            HostErrorCode::Io,
            &format!(
                "the files and index now match {name}, but HEAD could not be changed \
                 and still names {from}"
            ),
        ));
    }
    let entry = signature
        .to_ref(&mut time)
        .to_owned()
        .map_err(|e| git_failure("switch", &e))?;
    if let Err(error) = log_head(&repo, left, commit.id, &entry, &message) {
        // HEAD has moved and the switch is done; only the record of the
        // move for `git reflog` and `git switch -` is missing.
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            "could not record the branch switch in HEAD's reflog"
        );
    }
    tracing::info!(commit = %reply.commit, "extension switched git branch");
    Ok(HostReply::Branch(reply))
}

/// Append the line git writes to HEAD's reflog on a checkout.
fn log_head(
    repo: &gix::Repository,
    left: Option<ObjectId>,
    now: ObjectId,
    signature: &gix::actor::Signature,
    message: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    let left = left.unwrap_or_else(|| ObjectId::null(repo.object_hash()));
    let mut line = format!("{left} {now} ").into_bytes();
    signature.write_to(&mut line)?;
    line.push(b'\t');
    line.extend_from_slice(message.as_bytes());
    line.push(b'\n');
    let logs = repo.git_dir().join("logs");
    std::fs::create_dir_all(&logs)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("HEAD"))?
        .write_all(&line)
}

/// Whether git would convert line endings when writing files out.
fn converts_line_endings(settings: &Settings) -> bool {
    let autocrlf = settings.string("core.autocrlf").is_some_and(|value| {
        !["false", "input", "no", "off", "0"]
            .iter()
            .any(|off| value.eq_ignore_ascii_case(off.as_bytes()))
    });
    let eol = settings
        .string("core.eol")
        .is_some_and(|value| value.eq_ignore_ascii_case(b"crlf"));
    autocrlf || eol
}

/// Refuse a working tree or index that differs from `HEAD` in any tracked
/// path. Untracked files are left to [`workspace::in_the_way`].
fn clean(repo: &gix::Repository, interrupt: &Arc<AtomicBool>) -> Result<(), HostFailure> {
    let mut changes = repo
        .status(gix::progress::Discard)
        .map_err(|e| git_failure("status", &e))?
        .untracked_files(gix::status::UntrackedFiles::None)
        .index_worktree_rewrites(None)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .index_worktree_submodules(gix::status::Submodule::Given {
            ignore: gix::submodule::config::Ignore::All,
            check_dirty: false,
        })
        .should_interrupt_owned(Arc::clone(interrupt))
        .into_iter(None::<gix::bstr::BString>)
        .map_err(|e| git_failure("status", &e))?;
    for item in changes.by_ref() {
        let changed = match item.map_err(|e| git_failure("status", &e))? {
            gix::status::Item::IndexWorktree(item) => item.summary().is_some(),
            gix::status::Item::TreeIndex(_) => true,
        };
        if changed {
            return Err(failure(
                HostErrorCode::Conflict,
                "the working tree or index has changes; commit them first",
            ));
        }
    }
    check(interrupt)
}

/// The working-tree steps from the clean `index` to `next`, in path order.
/// Entries both hold unchanged keep their file-system data in `next`, so
/// git does not read those files again.
fn plan(
    repo: &gix::Repository,
    index: &gix::index::File,
    next: &mut gix::index::File,
    interrupt: &AtomicBool,
) -> Result<Vec<(String, Step)>, HostFailure> {
    let (old, new) = (index.entries(), next.entries());
    let mut steps = Vec::new();
    let mut kept = Vec::new();
    let mut bytes = 0u64;
    let (mut i, mut j) = (0, 0);
    loop {
        check(interrupt)?;
        let before = old.get(i).map(|e| (e.path(index), e));
        let after = new.get(j).map(|e| (e.path(next), e));
        let (path, from, to) = match (before, after) {
            (Some((a, from)), Some((b, to))) if a == b => {
                i += 1;
                j += 1;
                if from.id == to.id && from.mode == to.mode {
                    kept.push((j - 1, from.stat));
                    continue;
                }
                (a, Some(from), Some(to))
            }
            (Some((a, from)), Some((b, _))) if a < b => {
                i += 1;
                (a, Some(from), None)
            }
            (Some((a, from)), None) => {
                i += 1;
                (a, Some(from), None)
            }
            (_, Some((b, to))) => {
                j += 1;
                (b, None, Some(to))
            }
            (None, None) => break,
        };
        for entry in from.iter().chain(to.iter()) {
            if entry.mode == Mode::COMMIT {
                return Err(unsupported(
                    "repositories with submodules are not supported",
                ));
            }
        }
        if to.is_some_and(|entry| entry.mode == Mode::SYMLINK) {
            return Err(unsupported(
                "the branch has a symbolic link here that this one lacks, \
                 which extensions do not create",
            ));
        }
        let path = path
            .to_str()
            .map_err(|_| unsupported("a changed path is not UTF-8"))?
            .to_owned();
        if steps.len() == MAX_SWITCH_FILES {
            return Err(failure(
                HostErrorCode::TooLarge,
                "the switch changes more than 1,000 files; switch with git itself",
            ));
        }
        let mut read = |id: ObjectId| -> Result<Vec<u8>, HostFailure> {
            let size = repo
                .find_header(id)
                .map_err(|e| git_failure("switch", &e))?
                .size();
            bytes += size;
            if size > MAX_FILE || bytes > MAX_SWITCH_BYTES {
                return Err(failure(
                    HostErrorCode::TooLarge,
                    "the switch changes a file over 512 KiB or 32 MiB of files; \
                     switch with git itself",
                ));
            }
            Ok(repo
                .find_blob(id)
                .map_err(|e| git_failure("switch", &e))?
                .detach()
                .data)
        };
        let held = from.map(|entry| read(entry.id)).transpose()?;
        let link = from.is_some_and(|entry| entry.mode == Mode::SYMLINK);
        let step = match (to, held) {
            (None, Some(held)) => Step::Remove { link, held },
            // Never: every path comes from one side or both.
            (None, None) => continue,
            (Some(to), held) => {
                let contents = read(to.id)?;
                let executable = to.mode == Mode::FILE_EXECUTABLE;
                let held = match held {
                    // A link the branch replaces with a file is removed first.
                    Some(held) if link => {
                        steps.push((path.clone(), Step::Remove { link, held }));
                        None
                    }
                    held => held,
                };
                Step::Write {
                    contents,
                    executable,
                    held,
                }
            }
        };
        steps.push((path, step));
    }
    let entries = next.entries_mut();
    for (at, stat) in kept {
        entries[at].stat = stat;
    }
    // A size with no times makes git compare written files' content again.
    for (path, step) in &steps {
        if let Step::Write { contents, .. } = step
            && let Some(entry) = next.entry_mut_by_path_and_stage(
                BStr::new(path.as_bytes()),
                gix::index::entry::Stage::Unconflicted,
            )
        {
            entry.stat = Stat {
                size: u32::try_from(contents.len()).unwrap_or(u32::MAX),
                ..Stat::default()
            };
        }
    }
    Ok(steps)
}

/// Refuse a written file that git would change on the way out: a smudge
/// filter, ident or encoding attribute, or `eol=crlf`. Attributes are read
/// from the branch's own `.gitattributes` files, as git reads them after
/// checking those out.
fn convertible(
    repo: &gix::Repository,
    next: &gix::index::File,
    steps: &[(String, Step)],
) -> Result<(), HostFailure> {
    let mut rules = repo
        .attributes_only(
            next,
            gix::worktree::stack::state::attributes::Source::IdMapping,
        )
        .map_err(|e| git_failure("attributes", &e))?;
    let mut conversions = rules.selected_attribute_matches(CONVERSIONS);
    for (path, step) in steps {
        let Step::Write { executable, .. } = step else {
            continue;
        };
        let mode = if *executable {
            Mode::FILE_EXECUTABLE
        } else {
            Mode::FILE
        };
        rules
            .at_entry(BStr::new(path.as_bytes()), Some(mode))
            .map_err(|e| git_failure("attributes", &e))?
            .matching_attributes(&mut conversions);
        for matched in conversions.iter_selected() {
            use gix::attrs::StateRef;
            let state = matched.assignment.state;
            if matches!(state, StateRef::Unspecified | StateRef::Unset) {
                continue;
            }
            match matched.assignment.name.as_str() {
                "text" | "crlf" => {}
                "eol" if !matches!(state, StateRef::Value(v) if v.as_bstr() == "crlf") => {}
                _ => {
                    return Err(unsupported(
                        "a file the branch changes has a git filter, ident, encoding or \
                         eol=crlf attribute, which extensions do not apply",
                    ));
                }
            }
        }
    }
    Ok(())
}
