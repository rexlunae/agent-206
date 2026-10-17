//! The three tools, over any [`Host`].
use crate::{
    bm25,
    corpus::{self, Corpus},
    error::MemoryError,
    host::Host,
    paths::{self, Scope},
};
use a206_extension_sdk::{Args, EntryKind, HostErrorCode, int_arg, string_arg};

pub const MAX_QUERY_BYTES: usize = 1024;
pub const MAX_QUERY_TERMS: usize = 32;
pub const MAX_RESULTS: i64 = 20;
pub const DEFAULT_RESULTS: i64 = 5;
/// Longest snippet one result carries.
pub const SNIPPET_BYTES: usize = 700;
/// Work one search may do loading files and ranking them, in approximate
/// fuel units (see [`bm25::Work`]): the invocation has 50,000,000, and
/// listing, parsing and output need the rest.
pub const LOAD_WORK: u64 = 14_000_000;
pub const RANK_WORK: u64 = 20_000_000;
pub const MAX_GET_LINES: i64 = 500;
pub const DEFAULT_GET_LINES: i64 = 200;
/// Longest text one save appends.
pub const MAX_NOTE_BYTES: usize = 16 * 1024;
/// Largest note a save grows. An append sends the old and new text to the
/// host in one edit, which this keeps well inside the request limit.
pub const MAX_NOTE_FILE: usize = 256 * 1024;

pub fn search(host: &mut impl Host, args: &Args) -> Result<String, MemoryError> {
    let query = required(args, "query")?;
    let limit = ranged(args, "max_results", 1, MAX_RESULTS, DEFAULT_RESULTS)?;
    run_search(host, query, limit as usize)
}

pub fn run_search(host: &mut impl Host, query: &str, limit: usize) -> Result<String, MemoryError> {
    if query.len() > MAX_QUERY_BYTES {
        return Err(MemoryError::QueryTooLong);
    }
    let terms = bm25::query_terms(query);
    if terms.is_empty() {
        return Err(MemoryError::EmptyQuery);
    }
    if terms.len() > MAX_QUERY_TERMS {
        return Err(MemoryError::TooManyTerms);
    }
    let corpus = corpus::load(host, &mut bm25::Work::new(LOAD_WORK))?;
    let ranking = bm25::rank(
        &corpus.documents,
        &terms,
        limit,
        &mut bm25::Work::new(RANK_WORK),
    );
    let mut out: Vec<String> = ranking
        .hits
        .iter()
        .map(|hit| {
            let document = &corpus.documents[hit.document];
            let section = &hit.section;
            format!(
                "{}:{}-{} (score {:.2})\n{}",
                document.path,
                section.first_line,
                section.last_line,
                hit.score,
                bm25::snippet(&document.text[section.bytes.clone()], SNIPPET_BYTES)
            )
        })
        .collect();
    if corpus.documents.is_empty() && !corpus.truncated {
        out.push("no memory files: none of MEMORY.md or memory/**/*.md is readable text".into());
    } else if ranking.hits.is_empty() {
        out.push(format!("no matches in {} memory files", ranking.ranked));
    }
    out.extend(notice(&corpus, ranking.ranked));
    Ok(out.join("\n\n"))
}

fn notice(corpus: &Corpus, ranked: usize) -> Vec<String> {
    let mut notes = Vec::new();
    if corpus.truncated || ranked < corpus.documents.len() {
        let bytes: usize = corpus.documents[..ranked]
            .iter()
            .map(|d| d.text.len())
            .sum();
        notes.push(format!(
            "[stopped at a bound after {ranked} files ({bytes} bytes); later memory files were not searched]"
        ));
    }
    if corpus.too_large > 0 {
        notes.push(format!(
            "[skipped {} files too large to read]",
            corpus.too_large
        ));
    }
    if corpus.no_room > 0 {
        notes.push(format!(
            "[skipped {} files that did not fit in the {}-byte search budget]",
            corpus.no_room,
            crate::corpus::MAX_BYTES
        ));
    }
    if corpus.too_deep > 0 {
        notes.push(format!(
            "[skipped {} folders nested more than {} levels inside memory/]",
            corpus.too_deep,
            crate::paths::MAX_DEPTH
        ));
    }
    if corpus.not_text > 0 {
        notes.push(format!(
            "[skipped {} files that are not UTF-8 text]",
            corpus.not_text
        ));
    }
    notes
}

pub fn get(host: &mut impl Host, args: &Args) -> Result<String, MemoryError> {
    let path = paths::check(required(args, "path")?, Scope::Memory)?;
    let from = ranged(args, "from", 1, i64::from(u32::MAX), 1)?;
    let lines = ranged(args, "lines", 1, MAX_GET_LINES, DEFAULT_GET_LINES)?;
    read_lines(host, path, from as u64, lines as u64)
}

pub fn read_lines(
    host: &mut impl Host,
    path: &str,
    from: u64,
    count: u64,
) -> Result<String, MemoryError> {
    plain_file(host, path)?;
    let bytes = match host.read_file(path) {
        Err(failure) if failure.code == HostErrorCode::NotFound => {
            return Err(MemoryError::NotFound(path.into()));
        }
        read => read?,
    };
    let text = String::from_utf8(bytes).map_err(|_| MemoryError::NotText(path.into()))?;
    let total = text.lines().count() as u64;
    if total == 0 {
        return Ok(format!("{path} is empty"));
    }
    if from > total {
        return Err(MemoryError::PastEnd {
            path: path.into(),
            from,
            total,
        });
    }
    let last = (from + count - 1).min(total);
    let selected: Vec<&str> = text
        .lines()
        .skip((from - 1) as usize)
        .take((last - from + 1) as usize)
        .collect();
    Ok(format!(
        "{path}:{from}-{last} of {total} lines\n{}",
        selected.join("\n")
    ))
}

/// Refuse a path that reaches its file through a symbolic link or is not a
/// regular file, by listing each folder from the root. The host would
/// follow a link that stays inside the project; this keeps `memory_get`
/// within `memory/` as the user sees it. Not atomic with the read that
/// follows: the grant, not this check, is the boundary.
fn plain_file(host: &mut impl Host, path: &str) -> Result<(), MemoryError> {
    let components: Vec<&str> = path.split('/').collect();
    let mut folder = String::from(".");
    for (index, name) in components.iter().enumerate() {
        let expected = if index + 1 == components.len() {
            EntryKind::File
        } else {
            EntryKind::Directory
        };
        let entries = match host.list_directory(&folder) {
            Err(failure) if failure.code == HostErrorCode::NotFound => {
                return Err(MemoryError::NotFound(path.into()));
            }
            listed => listed?,
        };
        if !entries
            .iter()
            .any(|entry| entry.name == *name && entry.kind == expected)
        {
            return Err(MemoryError::NotFound(path.into()));
        }
        folder = if index == 0 {
            (*name).into()
        } else {
            format!("{folder}/{name}")
        };
    }
    Ok(())
}

pub fn save(host: &mut impl Host, args: &Args) -> Result<String, MemoryError> {
    let path = paths::check(required(args, "path")?, Scope::Notes)?;
    append(host, path, required(args, "text")?)
}

pub fn append(host: &mut impl Host, path: &str, text: &str) -> Result<String, MemoryError> {
    if text.trim().is_empty() {
        return Err(MemoryError::EmptyNote);
    }
    if text.len() > MAX_NOTE_BYTES {
        return Err(MemoryError::NoteTooLong);
    }
    match host.read_file(path) {
        Ok(bytes) => {
            let existing =
                String::from_utf8(bytes).map_err(|_| MemoryError::NotText(path.into()))?;
            extend(host, path, &existing, text)
        }
        Err(failure) if failure.code == HostErrorCode::NotFound => {
            if let Some(parent) = paths::parent(path) {
                host.create_directory(parent)?;
            }
            let entry = entry(text);
            match host.write_file(path, entry.as_bytes(), false) {
                Ok(()) => Ok(format!("created {path}, lines 1-{}", entry.lines().count())),
                // Created meanwhile: append to what is there now.
                Err(failure) if failure.code == HostErrorCode::Exists => {
                    let bytes = host.read_file(path)?;
                    let existing =
                        String::from_utf8(bytes).map_err(|_| MemoryError::NotText(path.into()))?;
                    extend(host, path, &existing, text)
                }
                Err(failure) => Err(failure.into()),
            }
        }
        Err(failure) => Err(failure.into()),
    }
}

/// Append to `existing` through `edit_file` with its whole text as the
/// expected old text, so the host refuses the append with `conflict` if the
/// note changed since it was read.
fn extend(
    host: &mut impl Host,
    path: &str,
    existing: &str,
    text: &str,
) -> Result<String, MemoryError> {
    let mut updated = String::with_capacity(existing.len() + text.len() + 2);
    updated.push_str(existing);
    if !existing.is_empty() && !existing.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&entry(text));
    if updated.len() > MAX_NOTE_FILE {
        return Err(MemoryError::NoteFull(path.into()));
    }
    // `edit_file` needs old text to compare, and a plain overwrite could
    // replace text written since the read, so an empty note is refused.
    if existing.is_empty() {
        return Err(MemoryError::EmptyFile(path.into()));
    }
    host.edit_file(path, existing, &updated)?;
    Ok(format!(
        "appended to {path}, lines {}-{}",
        existing.lines().count() + 1,
        updated.lines().count()
    ))
}

/// The text as written: always ending in a newline.
fn entry(text: &str) -> String {
    if text.ends_with('\n') {
        text.into()
    } else {
        format!("{text}\n")
    }
}

fn required<'a>(args: &'a Args, name: &'static str) -> Result<&'a str, MemoryError> {
    string_arg(args, name)
        .map_err(MemoryError::Argument)?
        .ok_or(MemoryError::Missing(name))
}
fn ranged(
    args: &Args,
    name: &'static str,
    min: i64,
    max: i64,
    default: i64,
) -> Result<i64, MemoryError> {
    let value = int_arg(args, name)
        .map_err(MemoryError::Argument)?
        .unwrap_or(default);
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(MemoryError::OutOfRange { name, min, max })
    }
}
