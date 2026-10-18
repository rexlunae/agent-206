//! Choosing the objects a push sends and writing them as a pack.
use gix::{ObjectId, objs::tree::EntryKind};
use std::collections::HashSet;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

/// How much one push may send.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub commits: usize,
    pub objects: usize,
    /// Pack bytes, header and checksum included.
    pub bytes: usize,
    /// The largest object, before compression. Each object is read whole.
    pub object: usize,
}

/// Why the objects of a push could not be chosen or packed.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("the push or fetch would carry more than {0} commits")]
    TooManyCommits(usize),
    #[error("the push or fetch would carry more than {0} objects")]
    TooManyObjects(usize),
    #[error("the push or fetch would carry more than {0} bytes")]
    TooLarge(usize),
    #[error("the push or fetch includes an object larger than {0} bytes")]
    ObjectTooLarge(usize),
    #[error("the push or fetch was cancelled")]
    Interrupted,
    #[error("the server's answer is not a git pack")]
    NotAPack,
    #[error("the server's pack could not be read and stored")]
    Unreadable(#[source] gix::Error),
    #[error("the fetched objects do not include everything the fetched commit reaches")]
    Incomplete,
    #[error("the commits include a submodule, which cannot be pushed or fetched with them")]
    Submodule,
    #[error("the repository could not be read")]
    Git(#[from] gix::Error),
    #[error("the pack could not be compressed")]
    Compress(#[source] std::io::Error),
}

/// The objects a push sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Objects {
    /// Commits first, then the trees and blobs they add.
    pub ids: Vec<ObjectId>,
    /// How many of `ids` are commits.
    pub commits: usize,
}

/// The objects reachable from `tip` that a server holding `known` may lack,
/// commits first. Commits reachable from a known commit present here are
/// skipped; of the others, each tree entry is skipped where it is unchanged
/// from a parent, so a push of a few commits reads only what they changed.
/// Objects the server already has elsewhere may still be sent.
/// `interrupt` stops the walk when raised.
pub fn objects(
    repo: &gix::Repository,
    tip: ObjectId,
    known: impl IntoIterator<Item = ObjectId>,
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<Objects, PackError> {
    let hidden: Vec<ObjectId> = known.into_iter().filter(|id| repo.has_object(id)).collect();
    let walk = repo.rev_walk([tip]).with_hidden(hidden).all()?;
    let mut commits = vec![];
    for info in walk {
        check(interrupt)?;
        let info = info?;
        if commits.len() == limits.commits {
            return Err(PackError::TooManyCommits(limits.commits));
        }
        commits.push(info.id);
    }
    let mut chosen = Chosen {
        ids: commits.clone(),
        seen: commits.iter().copied().collect(),
        limit: limits.objects,
        interrupt,
    };
    if chosen.ids.len() > chosen.limit {
        return Err(PackError::TooManyObjects(chosen.limit));
    }
    let count = commits.len();
    for id in commits {
        let commit = repo.find_commit(id)?;
        let tree = commit.tree_id()?.detach();
        let mut bases = vec![];
        for parent in commit.parent_ids() {
            let parent = repo.find_commit(parent)?;
            bases.push(parent.tree_id()?.detach());
        }
        added(repo, tree, &bases, &mut chosen)?;
    }
    Ok(Objects {
        ids: chosen.ids,
        commits: count,
    })
}

struct Chosen<'a> {
    ids: Vec<ObjectId>,
    seen: HashSet<ObjectId>,
    limit: usize,
    interrupt: &'a AtomicBool,
}
impl Chosen<'_> {
    /// Add `id` unless already chosen; `true` if it was new.
    fn add(&mut self, id: ObjectId) -> Result<bool, PackError> {
        if !self.seen.insert(id) {
            return Ok(false);
        }
        if self.ids.len() == self.limit {
            return Err(PackError::TooManyObjects(self.limit));
        }
        self.ids.push(id);
        Ok(true)
    }
}

/// Choose `tree` and what in it differs from every one of `bases`, the
/// trees at the same path in the commit's parents.
fn added(
    repo: &gix::Repository,
    tree: ObjectId,
    bases: &[ObjectId],
    chosen: &mut Chosen<'_>,
) -> Result<(), PackError> {
    check(chosen.interrupt)?;
    if bases.contains(&tree) || !chosen.add(tree)? {
        return Ok(());
    }
    let base_entries = bases
        .iter()
        .map(|base| entries(repo, *base))
        .collect::<Result<Vec<_>, _>>()?;
    for (name, kind, id) in entries(repo, tree)? {
        let at_name = || {
            base_entries
                .iter()
                .filter_map(|entries| entries.iter().find(|(n, _, _)| *n == name))
        };
        if at_name().any(|(_, _, base)| *base == id) {
            continue;
        }
        match kind {
            EntryKind::Tree => {
                let bases: Vec<ObjectId> = at_name()
                    .filter(|(_, kind, _)| *kind == EntryKind::Tree)
                    .map(|(_, _, id)| *id)
                    .collect();
                added(repo, id, &bases, chosen)?;
            }
            EntryKind::Commit => return Err(PackError::Submodule),
            EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
                chosen.add(id)?;
            }
        }
    }
    Ok(())
}

fn check(interrupt: &AtomicBool) -> Result<(), PackError> {
    if interrupt.load(Ordering::Relaxed) {
        return Err(PackError::Interrupted);
    }
    Ok(())
}

type Entries = Vec<(Vec<u8>, EntryKind, ObjectId)>;

fn entries(repo: &gix::Repository, tree: ObjectId) -> Result<Entries, PackError> {
    let tree = repo.find_tree(tree)?;
    let decoded = tree.decode()?;
    Ok(decoded
        .entries
        .iter()
        .map(|entry| {
            (
                entry.filename.to_vec(),
                entry.mode.kind(),
                entry.oid.to_owned(),
            )
        })
        .collect())
}

/// A version 2 pack of `ids`, each object whole and compressed, within
/// `limits.bytes` and `limits.object`. An object's size is checked before it
/// is read, and compression stops as soon as the pack would be too large.
/// `interrupt` stops the writing when raised.
pub fn write_pack(
    repo: &gix::Repository,
    ids: &[ObjectId],
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<Vec<u8>, PackError> {
    let max = limits.bytes;
    let too_large = || PackError::TooLarge(max);
    let count = u32::try_from(ids.len()).map_err(|_| too_large())?;
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&count.to_be_bytes());
    let checksum = repo.object_hash().len_in_bytes();
    let room = max.checked_sub(checksum).ok_or_else(too_large)?;
    for id in ids {
        check(interrupt)?;
        let size = usize::try_from(repo.find_header(*id)?.size()).unwrap_or(usize::MAX);
        if size > limits.object {
            return Err(PackError::ObjectTooLarge(limits.object));
        }
        let object = repo.find_object(*id)?;
        let kind = match object.kind {
            gix::object::Kind::Commit => 1u8,
            gix::object::Kind::Tree => 2,
            gix::object::Kind::Blob => 3,
            gix::object::Kind::Tag => 4,
        };
        // Type and size: four bits of size in the first byte, then seven
        // per byte, least significant first.
        let mut size = object.data.len();
        let mut byte = (kind << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            pack.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        pack.push(byte);
        if pack.len() > room {
            return Err(too_large());
        }
        let mut deflate = gix::zlib::stream::deflate::Write::new(
            Bounded {
                out: &mut pack,
                room,
                full: false,
            },
            gix::zlib::Compression::DEFAULT,
        );
        let written = deflate
            .write_all(&object.data)
            .and_then(|()| deflate.flush());
        let full = deflate.into_inner().full;
        match written {
            Err(_) if full => return Err(too_large()),
            written => written.map_err(PackError::Compress)?,
        }
    }
    let mut hasher = gix::hash::hasher(repo.object_hash());
    hasher.update(&pack);
    let digest = hasher.try_finalize()?;
    pack.extend_from_slice(digest.as_bytes());
    Ok(pack)
}

/// Appends to a pack, refusing any write past `room` bytes.
struct Bounded<'a> {
    out: &'a mut Vec<u8>,
    room: usize,
    /// Set when a write was refused for lack of room.
    full: bool,
}
impl std::io::Write for Bounded<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.out.len().saturating_add(buf.len()) > self.room {
            self.full = true;
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::objs::tree::EntryKind;

    const ROOMY: Limits = Limits {
        commits: 100,
        objects: 1_000,
        bytes: 1 << 20,
        object: 1 << 20,
    };
    static RUNNING: AtomicBool = AtomicBool::new(false);

    fn text() -> String {
        uuid::Uuid::new_v4().to_string()
    }
    fn repository() -> (tempfile::TempDir, gix::Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init_bare(dir.path()).unwrap();
        (dir, repo)
    }
    /// A tree of `files`, which may name folders with `/`.
    fn tree(repo: &gix::Repository, files: &[(&str, &str, EntryKind)]) -> ObjectId {
        let mut editor = repo
            .edit_tree(ObjectId::empty_tree(repo.object_hash()))
            .unwrap();
        for (path, content, kind) in files {
            let id = if *kind == EntryKind::Commit {
                ObjectId::from_hex(content.as_bytes()).unwrap()
            } else {
                repo.write_blob(content.as_bytes()).unwrap().detach()
            };
            editor.upsert(*path, *kind, id).unwrap();
        }
        editor.write().unwrap().detach()
    }
    fn commit(repo: &gix::Repository, tree: ObjectId, parents: &[ObjectId]) -> ObjectId {
        let signature = gix::actor::Signature {
            name: "n".into(),
            email: "n@example.invalid".into(),
            time: gix::date::Time::new(2_000_000, 0),
        };
        let commit = gix::objs::Commit {
            tree,
            parents: parents.iter().copied().collect(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: text().into(),
            extra_headers: vec![],
        };
        repo.write_object(&commit).unwrap().detach()
    }
    fn entry(repo: &gix::Repository, tree: ObjectId, path: &str) -> ObjectId {
        repo.find_tree(tree)
            .unwrap()
            .peel_to_entry_by_path(path)
            .unwrap()
            .unwrap()
            .object_id()
    }
    fn set(ids: &[ObjectId]) -> HashSet<ObjectId> {
        ids.iter().copied().collect()
    }

    #[test]
    fn only_commits_the_server_lacks_and_what_they_changed_are_chosen() {
        let (_dir, repo) = repository();
        let (kept, old, new, added) = (text(), text(), text(), text());
        let first_tree = tree(
            &repo,
            &[
                ("kept", &kept, EntryKind::Blob),
                ("dir/file", &old, EntryKind::Blob),
            ],
        );
        let first = commit(&repo, first_tree, &[]);
        let second_tree = tree(
            &repo,
            &[
                ("kept", &kept, EntryKind::Blob),
                ("dir/file", &new, EntryKind::BlobExecutable),
                ("added", &added, EntryKind::Blob),
            ],
        );
        let second = commit(&repo, second_tree, &[first]);
        let unknown = commit(&repo, first_tree, &[]);

        let chosen = objects(&repo, second, [first], ROOMY, &RUNNING).unwrap();
        assert_eq!(chosen.commits, 1);
        let chosen = chosen.ids;
        assert_eq!(chosen[0], second, "commits come first");
        assert_eq!(
            set(&chosen),
            set(&[
                second,
                second_tree,
                entry(&repo, second_tree, "dir"),
                entry(&repo, second_tree, "dir/file"),
                entry(&repo, second_tree, "added"),
            ])
        );
        // An object the server has but this repository lacks is ignored, so
        // everything is sent.
        let hex = format!("{}{}", text(), text()).replace('-', "");
        let missing = ObjectId::from_hex(&hex.as_bytes()[..40]).unwrap();
        let everything = objects(&repo, second, [missing], ROOMY, &RUNNING)
            .unwrap()
            .ids;
        assert_eq!(
            set(&everything),
            set(&[
                second,
                first,
                second_tree,
                first_tree,
                entry(&repo, second_tree, "dir"),
                entry(&repo, first_tree, "dir"),
                entry(&repo, second_tree, "dir/file"),
                entry(&repo, first_tree, "dir/file"),
                entry(&repo, second_tree, "added"),
                entry(&repo, first_tree, "kept"),
            ])
        );
        // Nothing is new once the server has the tip.
        assert!(
            objects(&repo, second, [second], ROOMY, &RUNNING)
                .unwrap()
                .ids
                .is_empty()
        );
        assert!(
            !objects(&repo, unknown, [second], ROOMY, &RUNNING)
                .unwrap()
                .ids
                .is_empty()
        );
    }

    #[test]
    fn a_written_pack_indexes_and_holds_every_object() {
        let (_dir, repo) = repository();
        let big = "x".repeat(70_000);
        let base = tree(
            &repo,
            &[
                ("big", &big, EntryKind::Blob),
                ("link", "target", EntryKind::Link),
            ],
        );
        let first = commit(&repo, base, &[]);
        let next = tree(&repo, &[("big", &text(), EntryKind::Blob)]);
        let second = commit(&repo, next, &[first]);
        let chosen = objects(&repo, second, [], ROOMY, &RUNNING).unwrap().ids;
        let pack = write_pack(&repo, &chosen, ROOMY, &RUNNING).unwrap();

        let (_server_dir, server) = repository();
        let directory = server.objects.store_ref().path().join("pack");
        let outcome = gix_pack::Bundle::write_to_directory(
            &mut std::io::BufReader::new(&pack[..]),
            Some(&directory),
            &mut gix::progress::Discard,
            &std::sync::atomic::AtomicBool::new(false),
            None::<gix::objs::find::Never>,
            repo.object_hash(),
            Default::default(),
        )
        .unwrap();
        assert_eq!(outcome.index.num_objects as usize, chosen.len());
        let server = gix::open(server.path()).unwrap();
        for id in &chosen {
            let sent = server.find_object(*id).unwrap();
            let original = repo.find_object(*id).unwrap();
            assert_eq!((sent.kind, &sent.data), (original.kind, &original.data));
        }
        // An empty pack is still a valid pack.
        let empty = write_pack(&repo, &[], ROOMY, &RUNNING).unwrap();
        assert_eq!(&empty[..12], b"PACK\0\0\0\x02\0\0\0\0");
        assert_eq!(empty.len(), 12 + repo.object_hash().len_in_bytes());
    }

    #[test]
    fn limits_and_submodules_refuse_a_push() {
        let (_dir, repo) = repository();
        let first = commit(&repo, tree(&repo, &[("a", &text(), EntryKind::Blob)]), &[]);
        let second = commit(
            &repo,
            tree(&repo, &[("a", &text(), EntryKind::Blob)]),
            &[first],
        );
        let limits = |commits, objects, bytes| Limits {
            commits,
            objects,
            bytes,
            object: 1 << 20,
        };
        assert!(matches!(
            objects(&repo, second, [], limits(1, 100, 1 << 20), &RUNNING),
            Err(PackError::TooManyCommits(1))
        ));
        assert!(objects(&repo, second, [], limits(2, 100, 1 << 20), &RUNNING).is_ok());
        assert!(matches!(
            objects(&repo, second, [], limits(2, 5, 1 << 20), &RUNNING),
            Err(PackError::TooManyObjects(5))
        ));
        let all = objects(&repo, second, [], limits(2, 6, 1 << 20), &RUNNING)
            .unwrap()
            .ids;
        let size = write_pack(&repo, &all, ROOMY, &RUNNING).unwrap().len();
        assert!(write_pack(&repo, &all, limits(2, 6, size), &RUNNING).is_ok());
        assert!(matches!(
            write_pack(&repo, &all, limits(2, 6, size - 1), &RUNNING),
            Err(PackError::TooLarge(_))
        ));

        // An object larger than allowed is refused before it is read, and
        // one that fits is refused once its compressed form would not.
        let big = text().repeat(100);
        let large = commit(
            &repo,
            tree(&repo, &[("a", &big, EntryKind::Blob)]),
            &[second],
        );
        let sent = objects(&repo, large, [second], ROOMY, &RUNNING)
            .unwrap()
            .ids;
        let small_objects = Limits {
            object: big.len() - 1,
            ..ROOMY
        };
        assert!(matches!(
            write_pack(&repo, &sent, small_objects, &RUNNING),
            Err(PackError::ObjectTooLarge(_))
        ));
        let exact = Limits {
            object: big.len(),
            ..ROOMY
        };
        assert!(write_pack(&repo, &sent, exact, &RUNNING).is_ok());
        assert!(matches!(
            write_pack(&repo, &sent, limits(3, 100, 64), &RUNNING),
            Err(PackError::TooLarge(64))
        ));

        let module = tree(&repo, &[("module", &first.to_string(), EntryKind::Commit)]);
        let with_module = commit(&repo, module, &[second]);
        assert!(matches!(
            objects(&repo, with_module, [second], ROOMY, &RUNNING),
            Err(PackError::Submodule)
        ));
    }

    #[test]
    fn a_raised_interrupt_stops_choosing_and_packing() {
        let (_dir, repo) = repository();
        let tip = commit(&repo, tree(&repo, &[("a", &text(), EntryKind::Blob)]), &[]);
        let stopped = AtomicBool::new(true);
        assert!(matches!(
            objects(&repo, tip, [], ROOMY, &stopped),
            Err(PackError::Interrupted)
        ));
        let ids = objects(&repo, tip, [], ROOMY, &RUNNING).unwrap().ids;
        assert!(matches!(
            write_pack(&repo, &ids, ROOMY, &stopped),
            Err(PackError::Interrupted)
        ));
    }
}
