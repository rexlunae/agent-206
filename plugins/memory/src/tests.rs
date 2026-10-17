//! The tools against an in-memory workspace that behaves like the gateway's
//! host calls, including following symbolic links on reads.
use crate::{
    corpus::{MAX_BYTES, MAX_DIRECTORIES, MAX_FILES},
    error::MemoryError,
    host::Host,
    paths::{MAX_DEPTH, PathError},
    tools::*,
};
use a206_extension_sdk::{
    Args, DirEntry, EntryKind, GuestError, HostErrorCode, HostFailure, Value,
};
use std::collections::BTreeMap;

/// A unique name, so no assertion can pass on text fixed in the guest.
pub fn unique(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}{}x{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Deterministic generated words (xorshift), so corpora vary in content but
/// every run is the same.
pub struct Words(u64);
impl Words {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    pub fn step(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// A lowercase word of 3 to 8 letters, never containing `q`, so words
    /// built with `q` can be planted without accidental matches.
    pub fn word(&mut self) -> String {
        let length = 3 + self.step() % 6;
        (0..length)
            .map(|_| {
                let letter = b"abcdefghijklmnoprstuvwxyz"[(self.step() % 25) as usize];
                letter as char
            })
            .collect()
    }
    pub fn line(&mut self, words: usize) -> String {
        (0..words)
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }
}
/// A term that generated filler never contains.
pub fn planted() -> String {
    unique("q")
}

#[derive(Debug, Clone)]
pub enum Node {
    File(Vec<u8>),
    Directory,
    Link(String),
    /// A file the host refuses to read as too large.
    Huge,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    List(String),
    Read(String),
    CreateDirectory(String),
    Write { path: String, overwrite: bool },
    Edit { path: String, old: String },
}

#[derive(Default)]
pub struct Fake {
    pub nodes: BTreeMap<String, Node>,
    pub requests: Vec<Request>,
}
fn fail(code: HostErrorCode) -> HostFailure {
    HostFailure {
        code,
        message: unique("host says "),
    }
}
impl Fake {
    pub fn file(mut self, path: &str, text: impl Into<Vec<u8>>) -> Self {
        self.add_parents(path);
        self.nodes.insert(path.into(), Node::File(text.into()));
        self
    }
    pub fn node(mut self, path: &str, node: Node) -> Self {
        self.add_parents(path);
        self.nodes.insert(path.into(), node);
        self
    }
    fn add_parents(&mut self, path: &str) {
        let mut at = 0;
        while let Some(slash) = path[at..].find('/') {
            let parent = &path[..at + slash];
            self.nodes.entry(parent.into()).or_insert(Node::Directory);
            at += slash + 1;
        }
    }
    pub fn text(&self, path: &str) -> Option<String> {
        match self.nodes.get(path) {
            Some(Node::File(bytes)) => Some(String::from_utf8(bytes.clone()).unwrap()),
            _ => None,
        }
    }
    pub fn writes(&self) -> usize {
        self.requests
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    Request::Write { .. } | Request::Edit { .. } | Request::CreateDirectory(_)
                )
            })
            .count()
    }
    /// Resolve links component by component, as cap-std does inside a root.
    fn resolve(&self, path: &str) -> String {
        let mut resolved = String::new();
        for name in path.split('/') {
            let candidate = if resolved.is_empty() {
                name.to_string()
            } else {
                format!("{resolved}/{name}")
            };
            resolved = match self.nodes.get(&candidate) {
                Some(Node::Link(target)) => target.clone(),
                _ => candidate,
            };
        }
        resolved
    }
}
impl Host for Fake {
    fn list_directory(&mut self, path: &str) -> Result<Vec<DirEntry>, HostFailure> {
        self.requests.push(Request::List(path.into()));
        let prefix = if path == "." {
            String::new()
        } else {
            let resolved = self.resolve(path);
            if !matches!(self.nodes.get(&resolved), Some(Node::Directory)) {
                return Err(fail(HostErrorCode::NotFound));
            }
            format!("{resolved}/")
        };
        Ok(self
            .nodes
            .iter()
            .filter_map(|(key, node)| {
                let name = key.strip_prefix(&prefix)?;
                (!name.contains('/')).then(|| DirEntry {
                    name: name.into(),
                    kind: match node {
                        Node::File(_) | Node::Huge => EntryKind::File,
                        Node::Directory => EntryKind::Directory,
                        Node::Link(_) => EntryKind::Symlink,
                    },
                    size: match node {
                        Node::File(bytes) => Some(bytes.len() as u64),
                        _ => None,
                    },
                })
            })
            .collect())
    }
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostFailure> {
        self.requests.push(Request::Read(path.into()));
        match self.nodes.get(&self.resolve(path)) {
            Some(Node::File(bytes)) => Ok(bytes.clone()),
            Some(Node::Huge) => Err(fail(HostErrorCode::TooLarge)),
            Some(_) => Err(fail(HostErrorCode::Invalid)),
            None => Err(fail(HostErrorCode::NotFound)),
        }
    }
    fn create_directory(&mut self, path: &str) -> Result<(), HostFailure> {
        self.requests.push(Request::CreateDirectory(path.into()));
        let mut at = String::new();
        for name in path.split('/') {
            at = if at.is_empty() {
                name.into()
            } else {
                format!("{at}/{name}")
            };
            match self.nodes.get(&at) {
                None => {
                    self.nodes.insert(at.clone(), Node::Directory);
                }
                Some(Node::Directory) => {}
                Some(Node::Link(_)) => return Err(fail(HostErrorCode::Invalid)),
                Some(_) => return Err(fail(HostErrorCode::Exists)),
            }
        }
        Ok(())
    }
    fn write_file(
        &mut self,
        path: &str,
        contents: &[u8],
        overwrite: bool,
    ) -> Result<(), HostFailure> {
        self.requests.push(Request::Write {
            path: path.into(),
            overwrite,
        });
        if let Some((parent, _)) = path.rsplit_once('/')
            && !matches!(self.nodes.get(parent), Some(Node::Directory))
        {
            return Err(fail(HostErrorCode::NotFound));
        }
        match self.nodes.get(path) {
            Some(Node::File(_)) if !overwrite => return Err(fail(HostErrorCode::Exists)),
            Some(Node::File(_)) | None => {}
            Some(_) => return Err(fail(HostErrorCode::Invalid)),
        }
        self.nodes.insert(path.into(), Node::File(contents.into()));
        Ok(())
    }
    fn edit_file(&mut self, path: &str, old: &str, new: &str) -> Result<(), HostFailure> {
        self.requests.push(Request::Edit {
            path: path.into(),
            old: old.into(),
        });
        let Some(Node::File(bytes)) = self.nodes.get(path) else {
            return Err(fail(HostErrorCode::NotFound));
        };
        let text = String::from_utf8(bytes.clone()).unwrap();
        if old.is_empty() || text.matches(old).count() != 1 {
            return Err(fail(HostErrorCode::Conflict));
        }
        let edited = text.replacen(old, new, 1);
        self.nodes.insert(path.into(), Node::File(edited.into()));
        Ok(())
    }
}

fn args(pairs: &[(&str, Value)]) -> Args {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}
fn s(text: &str) -> Value {
    Value::Str(text.into())
}
/// Filler notes: `files` notes of `lines` generated lines each.
fn filler(mut fake: Fake, words: &mut Words, files: usize, lines: usize) -> Fake {
    for _ in 0..files {
        let text = (0..lines)
            .map(|_| words.line(6))
            .collect::<Vec<_>>()
            .join("\n");
        fake = fake.file(&format!("memory/{}.md", unique("f")), text);
    }
    fake
}

// ── memory_search ──────────────────────────────────────────────────────────

#[test]
fn search_finds_the_section_holding_a_planted_term() {
    let mut words = Words::new(7);
    let term = planted();
    let target = format!("memory/{}/{}.md", unique("d"), unique("t"));
    let mut lines: Vec<String> = (0..30).map(|_| words.line(5)).collect();
    // Line 25 is in the second section (lines 21-30).
    lines[24] = format!("{} {} {}", words.word(), term.to_uppercase(), words.word());
    let mut fake = filler(Fake::default(), &mut words, 12, 25)
        .file("MEMORY.md", words.line(40))
        .file(&target, lines.join("\n"));
    let found = run_search(&mut fake, &term, 5).unwrap();
    let first = found.split("\n\n").next().unwrap();
    assert!(
        first.starts_with(&format!("{target}:21-30 (score ")),
        "{found}"
    );
    assert!(first.contains(&term.to_uppercase()), "{found}");
    assert_eq!(found.matches(" (score ").count(), 1, "{found}");
    // Searching never asks to change anything.
    assert_eq!(fake.writes(), 0);
    assert!(fake.requests.contains(&Request::Read("MEMORY.md".into())));
}

#[test]
fn search_reads_only_memory_notes() {
    let term = planted();
    let note = format!("memory/{}.md", unique("n"));
    let outside = format!("src/{}.md", unique("o"));
    let mut fake = Fake::default()
        .file(&note, unique("plain "))
        .file(&outside, term.clone())
        .file(&format!("{}.md", unique("root")), term.clone())
        .file(&format!("memory/{}.txt", unique("t")), term.clone())
        .file(&format!("memory/.{}.md", unique("h")), term.clone())
        .file(&format!("memory/.{}/n.md", unique("h")), term.clone())
        .node(
            &format!("memory/{}.md", unique("l")),
            Node::Link(outside.clone()),
        )
        .node(&format!("memory/{}", unique("l")), Node::Link("src".into()));
    let found = run_search(&mut fake, &term, 5).unwrap();
    assert_eq!(found, "no matches in 1 memory files");
    let reads: Vec<_> = fake
        .requests
        .iter()
        .filter_map(|r| match r {
            Request::Read(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(reads, vec![note]);
}

#[test]
fn a_memory_directory_reached_through_a_link_is_not_searched() {
    let term = planted();
    let mut fake = Fake::default()
        .file("notes/a.md", term.clone())
        .node("memory", Node::Link("notes".into()))
        .node("MEMORY.md", Node::Link("notes/a.md".into()));
    let found = run_search(&mut fake, &term, 5).unwrap();
    assert!(found.starts_with("no memory files"), "{found}");
    assert_eq!(fake.requests, vec![Request::List(".".into())]);
}

#[test]
fn search_stops_at_the_file_bound() {
    let mut words = Words::new(11);
    let term = planted();
    let mut fake = Fake::default();
    for index in 0..MAX_FILES + 9 {
        fake = fake.file(
            &format!("memory/{index:04}.md"),
            format!("{} {term}", words.line(3)),
        );
    }
    let found = run_search(&mut fake, &term, MAX_RESULTS as usize).unwrap();
    let reads = fake
        .requests
        .iter()
        .filter(|r| matches!(r, Request::Read(_)))
        .count();
    assert_eq!(reads, MAX_FILES);
    assert!(fake.requests.len() < 256, "{}", fake.requests.len());
    assert!(
        found.contains(&format!("[stopped at a bound after {MAX_FILES} files")),
        "{found}"
    );
    assert_eq!(found.matches(" (score ").count(), MAX_RESULTS as usize);
    // Files are read in name order, so the bound is deterministic.
    assert!(
        !fake
            .requests
            .contains(&Request::Read(format!("memory/{MAX_FILES:04}.md")))
    );
}

#[test]
fn search_stops_at_the_byte_bound() {
    let mut words = Words::new(13);
    let term = planted();
    let chunk = MAX_BYTES / 3 + 1;
    let mut fake = Fake::default();
    for index in 0..4 {
        let mut text = format!("{term}\n");
        // Long words, so the ranking's work bound is not what stops it.
        while text.len() < chunk {
            text.push_str(&words.word().repeat(16));
            text.push('\n');
        }
        fake = fake.file(&format!("memory/{index}.md"), text);
    }
    let found = run_search(&mut fake, &term, 10).unwrap();
    assert!(
        found.contains("[skipped 2 files that did not fit in the "),
        "{found}"
    );
    assert!(!found.contains("[stopped at a bound"), "{found}");
    // Listed sizes over the budget are not read.
    for skipped in ["memory/2.md", "memory/3.md"] {
        assert!(!fake.requests.contains(&Request::Read(skipped.into())));
    }
}

#[test]
fn a_large_root_file_does_not_hide_later_notes() {
    let mut words = Words::new(17);
    let term = planted();
    let mut big = String::new();
    while big.len() <= MAX_BYTES {
        big.push_str(&words.word().repeat(16));
        big.push('\n');
    }
    let note = format!("memory/{}.md", unique("n"));
    let mut fake = Fake::default()
        .file("MEMORY.md", big)
        .file(&note, format!("{} {term}", words.word()));
    let found = run_search(&mut fake, &term, 5).unwrap();
    assert!(found.starts_with(&format!("{note}:1-1 (score ")), "{found}");
    assert!(
        found.contains("[skipped 1 files that did not fit in the "),
        "{found}"
    );
}

#[test]
fn search_stops_when_its_ranking_work_runs_out() {
    let mut words = Words::new(41);
    let (first, last) = (planted(), planted());
    let files = 12;
    let mut fake = Fake::default();
    for index in 0..files {
        let term = match index {
            0 => first.as_str(),
            i if i == files - 1 => last.as_str(),
            _ => "",
        };
        // Two-letter words fill the byte bound with more words than the
        // ranking may count.
        let line = |words: &mut Words| -> String {
            (0..16)
                .map(|_| words.word()[..2].to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let text = (0..400)
            .map(|_| line(&mut words))
            .collect::<Vec<_>>()
            .join("\n");
        fake = fake.file(&format!("memory/{index:02}.md"), format!("{term}\n{text}"));
    }
    let found = run_search(&mut fake, &format!("{first} {last}"), 5).unwrap();
    // Every file was read; only the leading ones were ranked.
    let reads = fake
        .requests
        .iter()
        .filter(|r| matches!(r, Request::Read(_)))
        .count();
    assert_eq!(reads, files);
    assert!(found.starts_with("memory/00.md:1-20 (score "), "{found}");
    assert!(
        !found.contains(&format!("memory/{:02}.md", files - 1)),
        "{found}"
    );
    let ranked: usize = found
        .split("[stopped at a bound after ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .expect("the notice says how many files were searched");
    assert!((1..files).contains(&ranked), "{found}");
}

#[test]
fn search_stops_at_the_folder_bound_and_depth() {
    let term = planted();
    let mut fake = Fake::default();
    for index in 0..MAX_DIRECTORIES + 4 {
        fake = fake.file(&format!("memory/{index:03}/n.md"), term.clone());
    }
    let deep = format!("memory/{}n.md", "d/".repeat(MAX_DEPTH + 1));
    let mut deep_fake = Fake::default().file(&deep, term.clone());
    let found = run_search(&mut fake, &term, 20).unwrap();
    let lists = fake
        .requests
        .iter()
        .filter(|r| matches!(r, Request::List(_)))
        .count();
    // The root, then `memory/` and its folders up to the bound.
    assert_eq!(lists, 1 + MAX_DIRECTORIES);
    assert!(found.contains("[stopped at a bound"), "{found}");
    let found = run_search(&mut deep_fake, &term, 20).unwrap();
    assert!(found.starts_with("no memory files"), "{found}");
    assert!(
        found.contains(&format!(
            "[skipped 1 folders nested more than {MAX_DEPTH} levels"
        )),
        "{found}"
    );
    assert!(
        !deep_fake
            .requests
            .iter()
            .any(|r| matches!(r, Request::Read(_)))
    );
}

#[test]
fn unreadable_files_are_counted_not_hidden() {
    let term = planted();
    let mut fake = Fake::default()
        .file("memory/a.md", term.clone())
        .file("memory/b.md", vec![0xff, 0xfe, b'q'])
        .node("memory/c.md", Node::Huge);
    let found = run_search(&mut fake, &term, 5).unwrap();
    assert!(found.starts_with("memory/a.md:1-1 (score "), "{found}");
    assert!(
        found.contains("[skipped 1 files too large to read]"),
        "{found}"
    );
    assert!(
        found.contains("[skipped 1 files that are not UTF-8 text]"),
        "{found}"
    );
}

#[test]
fn search_arguments_are_bounded() {
    let mut fake = Fake::default().file("MEMORY.md", planted());
    let refused =
        |fake: &mut Fake, pairs: &[(&str, Value)]| search(fake, &args(pairs)).unwrap_err();
    for max in [0, MAX_RESULTS + 1, -1] {
        assert!(matches!(
            refused(
                &mut fake,
                &[("query", s("x y")), ("max_results", Value::Int(max))]
            ),
            MemoryError::OutOfRange {
                name: "max_results",
                ..
            }
        ));
    }
    assert!(matches!(
        refused(&mut fake, &[("query", s("- _ . a"))]),
        MemoryError::EmptyQuery
    ));
    assert!(matches!(
        refused(&mut fake, &[("query", s(&"ab ".repeat(MAX_QUERY_BYTES)))]),
        MemoryError::QueryTooLong
    ));
    let many = (0..=MAX_QUERY_TERMS)
        .map(|_| unique("w"))
        .collect::<Vec<_>>();
    assert!(matches!(
        refused(&mut fake, &[("query", s(&many.join(" ")))]),
        MemoryError::TooManyTerms
    ));
    assert!(matches!(
        refused(&mut fake, &[]),
        MemoryError::Missing("query")
    ));
    // Refused before any host call.
    assert!(fake.requests.is_empty());
}

#[test]
fn snippets_are_bounded_at_character_boundaries() {
    let term = planted();
    // Three-byte characters, so the snippet bound falls inside one.
    let long = format!("{term} {}", "語".repeat(SNIPPET_BYTES));
    let mut fake = Fake::default().file("MEMORY.md", long.clone());
    let found = run_search(&mut fake, &term, 1).unwrap();
    let snippet = found.split_once('\n').unwrap().1;
    let body = snippet.strip_suffix('…').expect("marked as cut");
    assert!(body.len() <= SNIPPET_BYTES && body.len() > SNIPPET_BYTES - 3);
    assert!(long.starts_with(body));
}

// ── memory_get ─────────────────────────────────────────────────────────────

#[test]
fn get_returns_the_requested_lines() {
    let mut words = Words::new(17);
    let lines: Vec<String> = (0..40)
        .map(|i| format!("{i} ü {}", words.line(3)))
        .collect();
    let path = format!("memory/{}.md", unique("g"));
    let mut fake = Fake::default().file(&path, lines.join("\r\n"));
    let got = get(
        &mut fake,
        &args(&[
            ("path", s(&path)),
            ("from", Value::Int(5)),
            ("lines", Value::Int(3)),
        ]),
    )
    .unwrap();
    assert_eq!(
        got,
        format!("{path}:5-7 of 40 lines\n{}", lines[4..7].join("\n"))
    );
    let tail = get(
        &mut fake,
        &args(&[("path", s(&path)), ("from", Value::Int(39))]),
    )
    .unwrap();
    assert_eq!(
        tail,
        format!("{path}:39-40 of 40 lines\n{}", lines[38..].join("\n"))
    );
    let past = get(
        &mut fake,
        &args(&[("path", s(&path)), ("from", Value::Int(41))]),
    );
    assert!(matches!(past, Err(MemoryError::PastEnd { total: 40, .. })));
}

#[test]
fn get_bounds_its_range() {
    let mut fake = Fake::default().file("MEMORY.md", "a\n".repeat(1000));
    for (name, value) in [
        ("from", 0),
        ("from", -3),
        ("lines", 0),
        ("lines", MAX_GET_LINES + 1),
    ] {
        let refused = get(
            &mut fake,
            &args(&[("path", s("MEMORY.md")), (name, Value::Int(value))]),
        );
        assert!(
            matches!(refused, Err(MemoryError::OutOfRange { name: n, .. }) if n == name),
            "{name}={value}"
        );
    }
    assert!(fake.requests.is_empty());
    let got = get(&mut fake, &args(&[("path", s("MEMORY.md"))])).unwrap();
    assert_eq!(got.lines().count(), 1 + DEFAULT_GET_LINES as usize);
    let got = get(
        &mut fake,
        &args(&[
            ("path", s("MEMORY.md")),
            ("lines", Value::Int(MAX_GET_LINES)),
        ]),
    )
    .unwrap();
    assert!(got.starts_with(&format!("MEMORY.md:1-{MAX_GET_LINES} of 1000 lines\n")));
}

#[test]
fn get_refuses_paths_outside_memory_without_reading() {
    let secret = planted();
    let outside = format!("src/{}.md", unique("s"));
    let mut fake = Fake::default().file(&outside, secret.clone());
    for path in [
        outside.clone(),
        format!("memory/../{outside}"),
        format!("/{outside}"),
        "memory/x.txt".into(),
    ] {
        let refused = get(&mut fake, &args(&[("path", s(&path))])).unwrap_err();
        assert!(matches!(refused, MemoryError::Path(_)), "{path}: {refused}");
        let wire = GuestError::from(refused);
        assert_eq!(wire.code, "not_memory_path");
        assert!(!wire.message.contains(&secret));
    }
    assert!(fake.requests.is_empty());
}

#[test]
fn get_refuses_links_the_host_would_follow() {
    let secret = planted();
    let outside = format!("src/{}.md", unique("s"));
    let mut fake = Fake::default()
        .file(&outside, secret.clone())
        .node("memory/leak.md", Node::Link(outside.clone()))
        .node("memory/dir", Node::Link("src".into()));
    let file_name = outside.rsplit_once('/').unwrap().1;
    for path in [
        "memory/leak.md".to_string(),
        format!("memory/dir/{file_name}"),
    ] {
        // The fake host would follow the link and return the secret.
        assert_eq!(fake.read_file(&path).unwrap(), secret.as_bytes());
        let refused = get(&mut fake, &args(&[("path", s(&path))])).unwrap_err();
        assert!(matches!(refused, MemoryError::NotFound(_)), "{refused}");
    }
}

#[test]
fn get_reports_missing_and_binary_files() {
    let mut fake = Fake::default().file("memory/bin.md", vec![0xc3, 0x28]);
    let missing = format!("memory/{}.md", unique("m"));
    assert!(matches!(
        get(&mut fake, &args(&[("path", s(&missing))])),
        Err(MemoryError::NotFound(_))
    ));
    let binary = get(&mut fake, &args(&[("path", s("memory/bin.md"))])).unwrap_err();
    assert_eq!(GuestError::from(binary).code, "not_text");
}

// ── memory_save ────────────────────────────────────────────────────────────

#[test]
fn save_creates_then_appends_a_note() {
    let path = format!("memory/{}/{}.md", unique("d"), unique("n"));
    let (first, second) = (
        unique("first "),
        format!("{}\n{}", unique("a"), unique("b")),
    );
    let mut fake = Fake::default();
    let created = save(&mut fake, &args(&[("path", s(&path)), ("text", s(&first))])).unwrap();
    assert_eq!(created, format!("created {path}, lines 1-1"));
    assert_eq!(fake.text(&path), Some(format!("{first}\n")));
    assert!(fake.requests.contains(&Request::Write {
        path: path.clone(),
        overwrite: false
    }));
    let appended = save(
        &mut fake,
        &args(&[("path", s(&path)), ("text", s(&second))]),
    )
    .unwrap();
    assert_eq!(appended, format!("appended to {path}, lines 2-3"));
    assert_eq!(fake.text(&path), Some(format!("{first}\n{second}\n")));
    // The append names the whole note it read, so a change since is refused.
    assert_eq!(
        fake.requests.last(),
        Some(&Request::Edit {
            path: path.clone(),
            old: format!("{first}\n")
        })
    );
}

#[test]
fn appending_after_a_concurrent_change_is_a_conflict() {
    /// Changes the note between the guest's read and its edit.
    struct Racing(Fake);
    impl Host for Racing {
        fn list_directory(&mut self, path: &str) -> Result<Vec<DirEntry>, HostFailure> {
            self.0.list_directory(path)
        }
        fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostFailure> {
            let read = self.0.read_file(path);
            self.0 = std::mem::take(&mut self.0).file(path, unique("changed "));
            read
        }
        fn create_directory(&mut self, path: &str) -> Result<(), HostFailure> {
            self.0.create_directory(path)
        }
        fn write_file(&mut self, path: &str, contents: &[u8], o: bool) -> Result<(), HostFailure> {
            self.0.write_file(path, contents, o)
        }
        fn edit_file(&mut self, path: &str, old: &str, new: &str) -> Result<(), HostFailure> {
            self.0.edit_file(path, old, new)
        }
    }
    let mut host = Racing(Fake::default().file("memory/n.md", unique("old ")));
    let refused = append(&mut host, "memory/n.md", &unique("new ")).unwrap_err();
    assert_eq!(GuestError::from(refused).code, "conflict");
}

#[test]
fn appending_to_an_empty_note_is_refused_without_writing() {
    let path = format!("memory/{}.md", unique("e"));
    let mut fake = Fake::default().file(&path, "");
    let refused = save(
        &mut fake,
        &args(&[("path", s(&path)), ("text", s(&unique("t ")))]),
    )
    .unwrap_err();
    assert_eq!(GuestError::from(refused).code, "empty_note_file");
    assert_eq!(fake.writes(), 0);
    assert_eq!(fake.text(&path), Some(String::new()));
}

#[test]
fn save_refuses_other_paths_without_any_host_call() {
    let mut fake = Fake::default().file("MEMORY.md", planted());
    for path in [
        "MEMORY.md".to_string(),
        format!("src/{}.md", unique("s")),
        format!("memory/../{}.md", unique("s")),
        format!("memory/.{}.md", unique("h")),
        format!("memory/{}.txt", unique("t")),
    ] {
        let refused =
            save(&mut fake, &args(&[("path", s(&path)), ("text", s("note"))])).unwrap_err();
        assert!(matches!(refused, MemoryError::Path(_)), "{path}: {refused}");
    }
    assert!(matches!(
        save(
            &mut fake,
            &args(&[("path", s("MEMORY.md")), ("text", s("x"))])
        ),
        Err(MemoryError::Path(PathError::RootFile(_)))
    ));
    assert!(fake.requests.is_empty());
}

#[test]
fn save_bounds_its_text_and_the_note() {
    let path = "memory/full.md";
    let mut fake = Fake::default().file(path, "x".repeat(MAX_NOTE_FILE - 4));
    for (text, code) in [
        ("  \n".to_string(), "invalid_argument"),
        ("y".repeat(MAX_NOTE_BYTES + 1), "invalid_argument"),
        ("abcd".into(), "note_full"),
    ] {
        let refused = save(&mut fake, &args(&[("path", s(path)), ("text", s(&text))])).unwrap_err();
        assert_eq!(GuestError::from(refused).code, code);
    }
    assert_eq!(fake.writes(), 0);
    // Exactly at the bound still fits: the old text, a newline and "ab\n".
    save(&mut fake, &args(&[("path", s(path)), ("text", s("ab"))])).unwrap();
    assert_eq!(fake.text(path).unwrap().len(), MAX_NOTE_FILE);
}

#[test]
fn host_refusals_keep_their_code() {
    struct Denying;
    impl Host for Denying {
        fn list_directory(&mut self, _: &str) -> Result<Vec<DirEntry>, HostFailure> {
            Err(fail(HostErrorCode::Denied))
        }
        fn read_file(&mut self, _: &str) -> Result<Vec<u8>, HostFailure> {
            Err(fail(HostErrorCode::Denied))
        }
        fn create_directory(&mut self, _: &str) -> Result<(), HostFailure> {
            Err(fail(HostErrorCode::Denied))
        }
        fn write_file(&mut self, _: &str, _: &[u8], _: bool) -> Result<(), HostFailure> {
            Err(fail(HostErrorCode::Denied))
        }
        fn edit_file(&mut self, _: &str, _: &str, _: &str) -> Result<(), HostFailure> {
            Err(fail(HostErrorCode::Denied))
        }
    }
    let searched = run_search(&mut Denying, "abc", 1).unwrap_err();
    let got = read_lines(&mut Denying, "MEMORY.md", 1, 1).unwrap_err();
    let saved = append(&mut Denying, "memory/a.md", "abc").unwrap_err();
    for error in [searched, got, saved] {
        assert!(matches!(error, MemoryError::Host(_)), "{error}");
        assert_eq!(GuestError::from(error).code, "denied");
    }
}
