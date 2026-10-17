//! Loading the memory files a search ranks, within fixed bounds.
//!
//! The walk lists directories itself instead of using the host's search
//! call: BM25 needs every section's length and is case-insensitive, while
//! the host search matches literal, case-sensitive lines. Symbolic links,
//! hidden entries and anything [`paths::check`] would refuse are skipped, so
//! a search only reads what `memory_get` would accept.
use crate::{
    bm25::{Document, Work},
    error::MemoryError,
    host::Host,
    paths::{self, Scope},
};
use a206_extension_sdk::{EntryKind, HostErrorCode};

/// Most files one search reads.
pub const MAX_FILES: usize = 128;
/// Most folders one search lists, `memory/` included.
pub const MAX_DIRECTORIES: usize = 32;
/// Most text one search reads, summed over its files. With the file and
/// folder bounds and the ranking's own work meter this keeps a search
/// inside the invocation's fuel and host-call limits (see the memory tests
/// in `crates/extensions`).
pub const MAX_BYTES: usize = 256 * 1024;
/// [`Work`] to read and check one file, besides its bytes.
pub const WORK_PER_FILE: u64 = 60_000;
/// [`Work`] per byte read, and extra per non-ASCII byte (UTF-8 checks and
/// scans cost more there).
pub const WORK_PER_BYTE: u64 = 45;
pub const WORK_PER_WIDE_BYTE: u64 = 110;

#[derive(Debug, Default)]
pub struct Corpus {
    pub documents: Vec<Document>,
    /// A bound stopped the walk; files after that point were not read.
    pub truncated: bool,
    /// Files the host refused as too large.
    pub too_large: usize,
    /// Files that are not UTF-8 text.
    pub not_text: usize,
    /// Files that did not fit in what was left of [`MAX_BYTES`]; the walk
    /// goes on, so smaller files after them are still searched.
    pub no_room: usize,
    /// Folders nested deeper than [`paths::MAX_DEPTH`], outside the scope.
    pub too_deep: usize,
}

impl Corpus {
    pub fn bytes(&self) -> usize {
        self.documents.iter().map(|d| d.text.len()).sum()
    }
}

/// `MEMORY.md`, then `memory/**/*.md` depth-first in name order, keeping
/// a file only while `work` covers checking it.
pub fn load(host: &mut impl Host, work: &mut Work) -> Result<Corpus, MemoryError> {
    let mut walk = Walk {
        host,
        work,
        corpus: Corpus::default(),
        directories: 0,
    };
    let root = walk.host.list_directory(".")?;
    let entry = |name: &str| root.iter().find(|e| e.name == name);
    if let Some(file) = entry(paths::ROOT_FILE).filter(|e| e.kind == EntryKind::File) {
        walk.file(paths::ROOT_FILE.into(), file.size)?;
    }
    if !walk.corpus.truncated
        && entry(paths::DIRECTORY).is_some_and(|e| e.kind == EntryKind::Directory)
    {
        walk.directory(paths::DIRECTORY.into(), 0)?;
    }
    Ok(walk.corpus)
}

struct Walk<'a, H> {
    host: &'a mut H,
    work: &'a mut Work,
    corpus: Corpus,
    directories: usize,
}
impl<H: Host> Walk<'_, H> {
    /// Returns once the walk is complete or a bound stopped it.
    fn directory(&mut self, path: String, depth: usize) -> Result<(), MemoryError> {
        if self.directories == MAX_DIRECTORIES {
            self.corpus.truncated = true;
            return Ok(());
        }
        self.directories += 1;
        let mut entries = match self.host.list_directory(&path) {
            Ok(entries) => entries,
            // Removed since its parent was listed.
            Err(failure) if failure.code == HostErrorCode::NotFound => return Ok(()),
            Err(failure) => return Err(failure.into()),
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in entries {
            if !paths::plain(&entry.name) {
                continue;
            }
            let child = format!("{path}/{}", entry.name);
            match entry.kind {
                EntryKind::Directory if depth < paths::MAX_DEPTH => {
                    self.directory(child, depth + 1)?
                }
                EntryKind::Directory => self.corpus.too_deep += 1,
                EntryKind::File if paths::check(&child, Scope::Memory).is_ok() => {
                    self.file(child, entry.size)?
                }
                _ => {}
            }
            if self.corpus.truncated {
                break;
            }
        }
        Ok(())
    }

    fn file(&mut self, path: String, size: Option<u64>) -> Result<(), MemoryError> {
        if self.corpus.documents.len() == MAX_FILES || self.work.left() < WORK_PER_FILE {
            self.corpus.truncated = true;
            return Ok(());
        }
        // A listed size over what is left is not read at all.
        let over = |size: u64| self.corpus.bytes() as u64 + size > MAX_BYTES as u64;
        if size.is_some_and(over) {
            self.corpus.no_room += 1;
            return Ok(());
        }
        let bytes = match self.host.read_file(&path) {
            Ok(bytes) => bytes,
            Err(failure) if failure.code == HostErrorCode::NotFound => return Ok(()),
            Err(failure) if failure.code == HostErrorCode::TooLarge => {
                self.corpus.too_large += 1;
                return Ok(());
            }
            Err(failure) => return Err(failure.into()),
        };
        if self.corpus.bytes() + bytes.len() > MAX_BYTES {
            // The read itself was work, even though the file is dropped.
            if !self
                .work
                .spend(WORK_PER_FILE + bytes.len() as u64 * WORK_PER_BYTE)
            {
                self.corpus.truncated = true;
            } else {
                self.corpus.no_room += 1;
            }
            return Ok(());
        }
        // Charged before the UTF-8 check and everything after it; the read
        // and this count are cheap and bounded by `MAX_BYTES`.
        let wide = bytes.iter().filter(|b| !b.is_ascii()).count() as u64;
        if !self
            .work
            .spend(WORK_PER_FILE + bytes.len() as u64 * WORK_PER_BYTE + wide * WORK_PER_WIDE_BYTE)
        {
            self.corpus.truncated = true;
            return Ok(());
        }
        match String::from_utf8(bytes) {
            Ok(text) => self.corpus.documents.push(Document { path, text }),
            Err(_) => self.corpus.not_text += 1,
        }
        Ok(())
    }
}
