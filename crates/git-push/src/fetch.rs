//! A fetch of one reference over `git-upload-pack` (protocol version 0 in
//! smart HTTP's stateless form): the request naming the wanted commit and
//! commits this repository has, the server's answer with its pack, and
//! storing and checking that pack.
use crate::{
    Advertisement, Limits, PackError, WireError, objects,
    pkt::{self, Reader},
    wire::{clean, object_id},
};
use gix::ObjectId;
use std::sync::atomic::AtomicBool;

/// Most commits a fetch tells the server it has.
pub const MAX_HAVES: usize = 256;

/// The request for `want`, telling the server about `haves` so it can leave
/// out what they already reach. Only capabilities the server advertised are
/// asked for: progress-free side-band framing, offset deltas, the object
/// format and the agent.
pub fn fetch_request(
    advertised: &Advertisement,
    want: ObjectId,
    haves: &[ObjectId],
) -> Result<Vec<u8>, WireError> {
    let mut capabilities = vec![];
    for wanted in ["side-band-64k", "ofs-delta"] {
        if advertised.capability(wanted) {
            capabilities.push(wanted.to_owned());
        }
    }
    if advertised.capability("side-band-64k") && advertised.capability("no-progress") {
        capabilities.push("no-progress".into());
    }
    if let Some(format) = advertised
        .capabilities
        .iter()
        .find(|c| c.starts_with("object-format="))
    {
        capabilities.push(format.clone());
    }
    if advertised
        .capabilities
        .iter()
        .any(|c| c.starts_with("agent="))
    {
        capabilities.push(concat!("agent=git/agent206-", env!("CARGO_PKG_VERSION")).into());
    }
    let mut out = vec![];
    let want = match capabilities.is_empty() {
        true => format!("want {want}\n"),
        false => format!("want {want} {}\n", capabilities.join(" ")),
    };
    pkt::data(&mut out, want.as_bytes())?;
    pkt::flush(&mut out);
    for have in haves.iter().take(MAX_HAVES) {
        pkt::data(&mut out, format!("have {have}\n").as_bytes())?;
    }
    pkt::data(&mut out, b"done\n")?;
    Ok(out)
}

/// The pack in a server's answer to [`fetch_request`]. `side_band` says
/// whether the request asked for side-band framing. A message the server
/// sends on the error band, or as `ERR`, is returned cleaned.
pub fn parse_fetch(
    body: &[u8],
    side_band: bool,
    hash: gix::hash::Kind,
) -> Result<Vec<u8>, WireError> {
    let mut reader = Reader::new(body);
    let mut acknowledged = false;
    // Acknowledgements first: `NAK`, or `ACK` and a commit both sides have.
    loop {
        if !side_band && reader.rest().starts_with(b"PACK") {
            break;
        }
        let Some(payload) = reader.next()? else {
            return Err(WireError::Malformed("the answer holds no pack"));
        };
        let line = pkt::line(payload);
        if let Some(message) = line.strip_prefix(b"ERR ") {
            return Err(WireError::Server(clean(message)));
        }
        if line == b"NAK" {
            acknowledged = true;
        } else if let Some(rest) = line.strip_prefix(b"ACK ") {
            let id = rest.split(|&b| b == b' ').next().unwrap_or_default();
            object_id(id, hash)?;
            acknowledged = true;
        } else if side_band && acknowledged {
            return demultiplex(payload, &mut reader);
        } else {
            return Err(WireError::Malformed(
                "the answer does not start with acknowledgements",
            ));
        }
    }
    if !acknowledged {
        return Err(WireError::Malformed(
            "the answer does not start with acknowledgements",
        ));
    }
    Ok(reader.rest().to_vec())
}

/// The pack carried on band 1, starting with the packet `first`, up to the
/// final flush. Progress on band 2 is dropped.
fn demultiplex(first: &[u8], reader: &mut Reader<'_>) -> Result<Vec<u8>, WireError> {
    let mut pack = vec![];
    let mut packet = Some(first);
    while let Some(payload) = packet {
        match payload.split_first() {
            Some((1, data)) => pack.extend_from_slice(data),
            Some((2, _)) => {}
            Some((3, message)) => return Err(WireError::Server(clean(message))),
            _ => return Err(WireError::Malformed("a side-band packet has no known band")),
        }
        packet = reader.next()?;
    }
    if !reader.is_empty() {
        return Err(WireError::Malformed("data follows the final flush"));
    }
    if pack.is_empty() {
        return Err(WireError::Malformed("the answer holds no pack"));
    }
    Ok(pack)
}

/// A pack written into a repository's object store.
#[derive(Debug)]
pub struct Stored {
    /// How many objects it holds.
    pub objects: usize,
    files: Vec<std::path::PathBuf>,
    keep: Option<std::path::PathBuf>,
}
impl Stored {
    /// Remove the pack again, for a fetch that turned out incomplete. A file
    /// that cannot be removed is returned with the error.
    pub fn discard(self) -> Result<(), (std::path::PathBuf, std::io::Error)> {
        for path in self.files.into_iter().chain(self.keep) {
            match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err((path, error));
                }
                _ => {}
            }
        }
        Ok(())
    }
    /// Let git collect the pack like any other, once a reference keeps its
    /// objects alive.
    pub fn release(self) -> Result<(), (std::path::PathBuf, std::io::Error)> {
        match self.keep.map(|path| (std::fs::remove_file(&path), path)) {
            Some((Err(error), path)) if error.kind() != std::io::ErrorKind::NotFound => {
                Err((path, error))
            }
            _ => Ok(()),
        }
    }
}

/// Index `pack` into `repo`'s object store. Refused before anything is
/// written when its header announces more than `limits.objects` objects;
/// no object or delta result may exceed `limits.object` bytes. The pack must
/// be complete in itself (no thin-pack bases). It stays protected by a
/// `.keep` file until [`Stored::release`].
pub fn store_pack(
    repo: &gix::Repository,
    pack: &[u8],
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<Stored, PackError> {
    if pack.len() > limits.bytes {
        return Err(PackError::TooLarge(limits.bytes));
    }
    let count = pack
        .get(8..12)
        .filter(|_| pack.starts_with(b"PACK"))
        .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or(PackError::NotAPack)?;
    if usize::try_from(count).map_or(true, |count| count > limits.objects) {
        return Err(PackError::TooManyObjects(limits.objects));
    }
    let directory = repo.objects.store_ref().path().join("pack");
    let options = gix_pack::bundle::write::Options {
        thread_limit: Some(1),
        alloc_limit_bytes: Some(limits.object),
        ..Default::default()
    };
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut std::io::BufReader::new(pack),
        Some(&directory),
        &mut gix::progress::Discard,
        interrupt,
        None::<gix::objs::find::Never>,
        repo.object_hash(),
        options,
    )
    .map_err(PackError::Unreadable)?;
    Ok(Stored {
        objects: outcome.index.num_objects as usize,
        files: outcome
            .data_path
            .into_iter()
            .chain(outcome.index_path)
            .collect(),
        keep: outcome.keep_path,
    })
}

/// Check that every object `tip` reaches is present, walking back until
/// commits reachable from `known` (which were complete before the fetch).
/// `repo` must have been opened after the pack was stored. A missing
/// object is [`PackError::Incomplete`].
pub fn check_complete(
    repo: &gix::Repository,
    tip: ObjectId,
    known: impl IntoIterator<Item = ObjectId>,
    limits: Limits,
    interrupt: &AtomicBool,
) -> Result<(), PackError> {
    if !repo.has_object(tip) {
        return Err(PackError::Incomplete);
    }
    let reached = match objects(repo, tip, known, limits, interrupt) {
        Ok(reached) => reached,
        // Reading a commit or tree that is not there.
        Err(PackError::Git(_)) => return Err(PackError::Incomplete),
        Err(error) => return Err(error),
    };
    if reached.ids.iter().all(|id| repo.has_object(id)) {
        Ok(())
    } else {
        Err(PackError::Incomplete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write_pack;
    use gix::{hash::Kind, objs::tree::EntryKind};

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
    fn id() -> ObjectId {
        let hex = format!("{}{}", text(), text()).replace('-', "");
        ObjectId::from_hex(&hex.as_bytes()[..40]).unwrap()
    }
    fn advertising(capabilities: &[&str]) -> Advertisement {
        Advertisement {
            refs: vec![],
            haves: vec![],
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
        }
    }
    fn packet(out: &mut Vec<u8>, payload: &[u8]) {
        pkt::data(out, payload).unwrap();
    }
    fn lines(body: &[u8]) -> Vec<Option<Vec<u8>>> {
        let mut reader = Reader::new(body);
        let mut out = vec![];
        while !reader.is_empty() {
            out.push(reader.next().unwrap().map(<[u8]>::to_vec));
        }
        out
    }
    /// A repository with a commit of one file, and that commit.
    fn repository() -> (tempfile::TempDir, gix::Repository, ObjectId) {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init_bare(dir.path()).unwrap();
        let blob = repo.write_blob(text().as_bytes()).unwrap().detach();
        let mut tree = repo.edit_tree(ObjectId::empty_tree(Kind::Sha1)).unwrap();
        tree.upsert("dir/file", EntryKind::Blob, blob).unwrap();
        let tree = tree.write().unwrap().detach();
        let signature = gix::actor::Signature {
            name: "n".into(),
            email: "n@example.invalid".into(),
            time: gix::date::Time::new(2_000_000, 0),
        };
        let commit = gix::objs::Commit {
            tree,
            parents: Default::default(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: text().into(),
            extra_headers: vec![],
        };
        let commit = repo.write_object(&commit).unwrap().detach();
        (dir, repo, commit)
    }

    #[test]
    fn requests_want_one_commit_and_say_what_is_here() {
        let (want, have) = (id(), id());
        let advertised = advertising(&[
            "side-band-64k",
            "ofs-delta",
            "no-progress",
            "multi_ack",
            "agent=x",
        ]);
        let request = fetch_request(&advertised, want, &[have]).unwrap();
        let agent = concat!("agent=git/agent206-", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            lines(&request),
            [
                Some(
                    format!("want {want} side-band-64k ofs-delta no-progress {agent}\n")
                        .into_bytes()
                ),
                None,
                Some(format!("have {have}\n").into_bytes()),
                Some(b"done\n".to_vec()),
            ]
        );
        // Nothing the server lacks is asked for, and at most MAX_HAVES haves.
        let haves: Vec<_> = (0..MAX_HAVES + 5).map(|_| id()).collect();
        let request = fetch_request(&advertising(&[]), want, &haves).unwrap();
        let lines = lines(&request);
        assert_eq!(lines[0], Some(format!("want {want}\n").into_bytes()));
        assert_eq!(lines.len(), 1 + 1 + MAX_HAVES + 1);
    }

    #[test]
    fn answers_give_their_pack_with_or_without_side_band() {
        let pack = format!("PACK{}", text()).into_bytes();
        let mut raw = vec![];
        packet(&mut raw, b"NAK\n");
        raw.extend_from_slice(&pack);
        assert_eq!(parse_fetch(&raw, false, Kind::Sha1).unwrap(), pack);

        let mut banded = vec![];
        packet(&mut banded, format!("ACK {}\n", id()).as_bytes());
        packet(&mut banded, &[&[2u8][..], b"counting objects\n"].concat());
        let (a, b) = pack.split_at(7);
        packet(&mut banded, &[&[1u8][..], a].concat());
        packet(&mut banded, &[&[1u8][..], b].concat());
        pkt::flush(&mut banded);
        assert_eq!(parse_fetch(&banded, true, Kind::Sha1).unwrap(), pack);

        let reason = text();
        let mut failed = vec![];
        packet(&mut failed, b"NAK\n");
        packet(
            &mut failed,
            &[&[3u8][..], reason.as_bytes(), b"\x1b[31m"].concat(),
        );
        assert!(matches!(
            parse_fetch(&failed, true, Kind::Sha1),
            Err(WireError::Server(message)) if message.starts_with(&reason) && !message.contains('\x1b')
        ));
        let mut refused = vec![];
        packet(&mut refused, format!("ERR {reason}\n").as_bytes());
        assert!(matches!(
            parse_fetch(&refused, false, Kind::Sha1),
            Err(WireError::Server(message)) if message == reason
        ));

        let malformed: Vec<Vec<u8>> = vec![
            // No acknowledgement before the pack.
            pack.clone(),
            // Nothing after the acknowledgement.
            {
                let mut body = vec![];
                packet(&mut body, b"NAK\n");
                pkt::flush(&mut body);
                body
            },
            // An unknown band.
            {
                let mut body = vec![];
                packet(&mut body, b"NAK\n");
                packet(&mut body, &[&[7u8][..], &pack].concat());
                pkt::flush(&mut body);
                body
            },
            // Data after the final flush.
            [banded.clone(), b"0000".to_vec()].concat(),
            // An acknowledgement of something that is not an object id.
            {
                let mut body = vec![];
                packet(&mut body, b"ACK nothing\n");
                body.extend_from_slice(&pack);
                body
            },
        ];
        for (index, body) in malformed.iter().enumerate() {
            let side_band = index >= 2;
            assert!(
                parse_fetch(body, side_band, Kind::Sha1).is_err(),
                "case {index}"
            );
        }
    }

    #[test]
    fn a_stored_pack_is_checked_for_everything_the_commit_reaches() {
        let (_source_dir, source, tip) = repository();
        let all = objects(&source, tip, [], ROOMY, &RUNNING).unwrap().ids;
        let pack = write_pack(&source, &all, ROOMY, &RUNNING).unwrap();

        let (_dir, target, _) = repository();
        let stored = store_pack(&target, &pack, ROOMY, &RUNNING).unwrap();
        assert_eq!(stored.objects, all.len());
        let target = gix::open(target.path()).unwrap();
        check_complete(&target, tip, [], ROOMY, &RUNNING).unwrap();
        stored.release().unwrap();

        // A pack without the file's blob is incomplete, and is removed.
        let (_dir, target, _) = repository();
        let blob = source
            .find_commit(tip)
            .unwrap()
            .tree()
            .unwrap()
            .peel_to_entry_by_path("dir/file")
            .unwrap()
            .unwrap()
            .object_id();
        let partial: Vec<_> = all.iter().copied().filter(|id| *id != blob).collect();
        let pack = write_pack(&source, &partial, ROOMY, &RUNNING).unwrap();
        let before = std::fs::read_dir(target.path().join("objects/pack"))
            .unwrap()
            .count();
        let stored = store_pack(&target, &pack, ROOMY, &RUNNING).unwrap();
        let reopened = gix::open(target.path()).unwrap();
        assert!(matches!(
            check_complete(&reopened, tip, [], ROOMY, &RUNNING),
            Err(PackError::Incomplete)
        ));
        stored.discard().unwrap();
        let after = std::fs::read_dir(target.path().join("objects/pack"))
            .unwrap()
            .count();
        assert_eq!(before, after);
        // Without the commit itself, nothing is complete.
        assert!(matches!(
            check_complete(&reopened, id(), [], ROOMY, &RUNNING),
            Err(PackError::Incomplete)
        ));
    }

    #[test]
    fn packs_beyond_the_limits_are_refused_before_they_are_written() {
        let (_source_dir, source, tip) = repository();
        let all = objects(&source, tip, [], ROOMY, &RUNNING).unwrap().ids;
        let pack = write_pack(&source, &all, ROOMY, &RUNNING).unwrap();
        let (_dir, target, _) = repository();
        let files = || {
            std::fs::read_dir(target.path().join("objects/pack"))
                .unwrap()
                .count()
        };
        let before = files();
        let few = Limits {
            objects: all.len() - 1,
            ..ROOMY
        };
        assert!(matches!(
            store_pack(&target, &pack, few, &RUNNING),
            Err(PackError::TooManyObjects(_))
        ));
        let small = Limits {
            bytes: pack.len() - 1,
            ..ROOMY
        };
        assert!(matches!(
            store_pack(&target, &pack, small, &RUNNING),
            Err(PackError::TooLarge(_))
        ));
        assert!(matches!(
            store_pack(&target, text().as_bytes(), ROOMY, &RUNNING),
            Err(PackError::NotAPack)
        ));
        // A corrupted pack fails its checksum.
        let mut corrupt = pack.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xff;
        assert!(store_pack(&target, &corrupt, ROOMY, &RUNNING).is_err());
        assert_eq!(files(), before);
    }
}
