//! BM25 ranking of memory sections, adapted from RustyClaw's `memory.rs`
//! (MIT, see NOTICE): files are cut into sections at `#`/`##` headings or
//! every [`SECTION_LINES`] lines, words are lowercase runs of letters,
//! digits, `-` and `_`, and sections are scored with k1 = 1.2, b = 0.75.
//!
//! Only query words are counted, so indexing needs no vocabulary and its
//! cost is linear in the corpus. That cost is also metered with [`Work`],
//! so a search ends with what it ranked instead of running out of fuel.
use std::ops::Range;

pub const SECTION_LINES: usize = 20;
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// One memory file's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub path: String,
    pub text: String,
}

/// A run of lines in a document, without leading or trailing blank lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// 1-based, inclusive.
    pub first_line: u32,
    pub last_line: u32,
    /// Byte range of those lines in the document's text.
    pub bytes: Range<usize>,
}

/// Interpreter work a search may still do, in approximate fuel units.
///
/// The host meters the guest's instructions and fails the call when they
/// run out, so loading and ranking meter themselves in the same units and
/// stop first.
/// The costs below are measured upper bounds for this guest's loops (see
/// the memory tests in `crates/extensions`), not facts about any model.
#[derive(Debug, Clone, Copy)]
pub struct Work {
    left: u64,
}
/// Per line of a ranked file, blank lines included.
pub const WORK_PER_LINE: u64 = 80;
/// Per word.
pub const WORK_PER_WORD: u64 = 500;
/// Per run of text holding non-ASCII characters.
pub const WORK_PER_WIDE_RUN: u64 = 4_000;
/// Per non-ASCII character classified by a known range or the cache.
pub const WORK_PER_WIDE: u64 = 1_800;
/// Per non-ASCII character classified by the Unicode tables.
pub const WORK_PER_LOOKUP: u64 = 12_000;
impl Work {
    pub fn new(limit: u64) -> Self {
        Self { left: limit }
    }
    pub fn unlimited() -> Self {
        Self::new(u64::MAX)
    }
    pub fn left(&self) -> u64 {
        self.left
    }
    /// Spend `cost`, or nothing and `false` when it is more than remains.
    pub fn spend(&mut self, cost: u64) -> bool {
        match self.left.checked_sub(cost) {
            Some(left) => {
                self.left = left;
                true
            }
            None => false,
        }
    }
}

/// A ranked section.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub document: usize,
    pub section: Section,
    pub score: f64,
}

// The guest runs under an instruction budget that charges every operation
// of a loop body on each pass, branches not taken included. So the loops
// that touch every byte are single slice-iterator scans, and anything
// rarer (non-ASCII text, a new section) happens outside them.

/// The sections of `text`, or `None` when `work` ran out first.
pub fn sections(text: &str, work: &mut Work) -> Option<Vec<Section>> {
    let bytes = text.as_bytes();
    let mut sections = Vec::new();
    let mut open: Option<Section> = None;
    // Lines since `open` began, blank ones included.
    let mut since = 0;
    let mut number = 0u32;
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        let length = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        let (start, line) = (at, &rest[..length]);
        at += length + 1;
        if !work.spend(WORK_PER_LINE) {
            return None;
        }
        number = number.saturating_add(1);
        // Other blank characters only make a section with no words.
        if line.iter().all(u8::is_ascii_whitespace) {
            since += 1;
            continue;
        }
        let heading = line.starts_with(b"# ") || line.starts_with(b"## ");
        let end = start + length - usize::from(line.last() == Some(&b'\r'));
        match &mut open {
            Some(section) if !heading && since < SECTION_LINES => {
                section.last_line = number;
                section.bytes.end = end;
                since += 1;
            }
            _ => {
                sections.extend(open.take());
                open = Some(Section {
                    first_line: number,
                    last_line: number,
                    bytes: start..end,
                });
                since = 1;
            }
        }
    }
    sections.extend(open);
    Some(sections)
}

/// Call `each` with every word of `text` as written, and whether it is
/// ASCII. Words are runs of letters, digits, `-` and `_`, trimmed of `-`
/// and `_`; a lone ASCII character is not a word. Returns `false`, having
/// stopped, when `work` ran out.
pub fn raw_words(text: &str, work: &mut Work, mut each: impl FnMut(&str, bool)) -> bool {
    let bytes = text.as_bytes();
    let mut at = 0;
    loop {
        let rest = bytes.get(at..).unwrap_or_default();
        at += rest
            .iter()
            .take_while(|&&b| CLASS[usize::from(b)] == SEPARATOR)
            .count();
        let rest = bytes.get(at..).unwrap_or_default();
        // A run of ASCII word bytes and non-ASCII characters, ended by an
        // ASCII separator or the end, so both ends are char boundaries.
        let length = rest
            .iter()
            .take_while(|&&b| CLASS[usize::from(b)] != SEPARATOR)
            .count();
        if length == 0 {
            return true;
        }
        if let Some(run) = text.get(at..at + length)
            && !split(run, work, &mut each)
        {
            return false;
        }
        at += length;
    }
}
#[inline(never)]
fn split(run: &str, work: &mut Work, each: &mut impl FnMut(&str, bool)) -> bool {
    if run.is_ascii() {
        word(run, true, work, each)
    } else {
        mixed(run, work, each)
    }
}

/// Words of a run holding non-ASCII characters, split exactly.
#[inline(never)]
fn mixed(run: &str, work: &mut Work, each: &mut impl FnMut(&str, bool)) -> bool {
    if !work.spend(WORK_PER_WIDE_RUN) {
        return false;
    }
    // Every ASCII character of the run is a word byte; only non-ASCII
    // characters that are not letters or digits split it.
    let (mut start, mut ascii) = (0, true);
    for (index, c) in run.char_indices() {
        if c.is_ascii() {
            continue;
        }
        let (alphanumeric, cost) = alphanumeric(c);
        if !work.spend(cost) {
            return false;
        }
        if alphanumeric {
            ascii = false;
        } else {
            if !word(&run[start..index], ascii, work, each) {
                return false;
            }
            (start, ascii) = (index + c.len_utf8(), true);
        }
    }
    word(&run[start..], ascii, work, each)
}
#[inline(never)]
fn word(piece: &str, ascii: bool, work: &mut Work, each: &mut impl FnMut(&str, bool)) -> bool {
    let bytes = piece.as_bytes();
    let joiner = |b: Option<&u8>| matches!(b, Some(b'-' | b'_'));
    let word = if joiner(bytes.first()) || joiner(bytes.last()) {
        trim(piece)
    } else {
        piece
    };
    if word.len() > 1 || (!word.is_empty() && !ascii) {
        if !work.spend(WORK_PER_WORD) {
            return false;
        }
        each(word, ascii);
    }
    true
}
#[inline(never)]
fn trim(piece: &str) -> &str {
    // Only ASCII bytes are trimmed, so both ends stay char boundaries.
    piece.trim_matches(['-', '_'])
}

// Largest ranges first: they hold most characters of the texts they cover.
/// Ranges in which every character is alphanumeric.
const LETTERS: &[(char, char)] = &[
    ('\u{4E00}', '\u{9FFF}'),
    ('\u{AC00}', '\u{D7A3}'),
    ('\u{3400}', '\u{4DBF}'),
    ('\u{00C0}', '\u{00D6}'),
    ('\u{00D8}', '\u{00F6}'),
    ('\u{00F8}', '\u{02C1}'),
    ('\u{0388}', '\u{038A}'),
    ('\u{038E}', '\u{03A1}'),
    ('\u{03A3}', '\u{03F5}'),
    ('\u{03F7}', '\u{0481}'),
    ('\u{048A}', '\u{052F}'),
    ('\u{05D0}', '\u{05EA}'),
    ('\u{0620}', '\u{064A}'),
    ('\u{0E01}', '\u{0E30}'),
    ('\u{3041}', '\u{3096}'),
    ('\u{30A1}', '\u{30FA}'),
];

/// Whether `c` is a letter or digit, exactly as `char::is_alphanumeric`,
/// and the work that took. The Unicode tables cost thousands of
/// instructions per lookup in the interpreter, so whole ranges that are
/// letters (checked against the tables by a test) and a cache of recent
/// answers come first.
#[inline(never)]
fn alphanumeric(c: char) -> (bool, u64) {
    if LETTERS
        .iter()
        .any(|&(first, last)| (first..=last).contains(&c))
    {
        return (true, WORK_PER_WIDE);
    }
    let code = u32::from(c);
    let slot = ((code ^ (code >> 10)) & (CACHE_SLOTS as u32 - 1)) as usize;
    CACHE.with_borrow_mut(|cache| {
        let entry = cache[slot];
        if entry >> 1 == code {
            return (entry & 1 == 1, WORK_PER_WIDE);
        }
        let answer = lookup(c);
        cache[slot] = code << 1 | u32::from(answer);
        (answer, WORK_PER_LOOKUP)
    })
}
#[inline(never)]
fn lookup(c: char) -> bool {
    c.is_alphanumeric()
}
const CACHE_SLOTS: usize = 1024;
thread_local! {
    /// `code << 1 | answer` per slot; `u32::MAX` is no character.
    static CACHE: std::cell::RefCell<[u32; CACHE_SLOTS]> =
        const { std::cell::RefCell::new([u32::MAX; CACHE_SLOTS]) };
}

const SEPARATOR: u8 = 0;
const WORD: u8 = 1;
const WIDE: u8 = 2;
/// Each byte's role: an ASCII letter, digit, `-` or `_` is part of a word,
/// other ASCII is a separator, and a non-ASCII byte belongs to a character
/// [`mixed`] classifies.
const CLASS: [u8; 256] = {
    let mut table = [WIDE; 256];
    let mut byte = 0;
    while byte < 128 {
        let b = byte as u8;
        table[byte] = if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            WORD
        } else {
            SEPARATOR
        };
        byte += 1;
    }
    table
};

/// Call `each` with every word of `text`, lowercased.
pub fn words(text: &str, mut each: impl FnMut(&str)) {
    let mut lower = String::new();
    raw_words(text, &mut Work::unlimited(), |word, _| {
        lower.clear();
        lower.extend(word.chars().flat_map(char::to_lowercase));
        each(&lower);
    });
}

/// Finds which lowercase query term a word is. Most words match no term,
/// so a word whose length no term has is ruled out before any comparison
/// or lowercasing.
struct Matcher<'a> {
    terms: &'a [String],
    /// Bit `n` is set when some term is `n` bytes long (`n` < 64).
    bytes: u64,
    /// Bit `n` is set when some term is `n` characters long (`n` < 64).
    chars: u64,
    lower: String,
}
/// Bit `n` of a length mask; lengths of 64 or more share the top bit.
fn bit(length: usize) -> u64 {
    1 << length.min(63)
}
impl<'a> Matcher<'a> {
    fn new(terms: &'a [String]) -> Self {
        let mask = |length: fn(&String) -> usize| terms.iter().fold(0, |m, t| m | bit(length(t)));
        Self {
            terms,
            bytes: mask(String::len),
            chars: mask(|t| t.chars().count()),
            lower: String::new(),
        }
    }
    fn position(&mut self, word: &str, ascii: bool) -> Option<usize> {
        if ascii {
            // Lowercasing ASCII keeps the length.
            if self.bytes & bit(word.len()) == 0 {
                return None;
            }
            return self.ascii(word);
        }
        self.wide(word)
    }
    #[inline(never)]
    fn ascii(&self, word: &str) -> Option<usize> {
        self.terms.iter().position(|term| {
            term.len() == word.len() && term.as_bytes().eq_ignore_ascii_case(word.as_bytes())
        })
    }
    #[inline(never)]
    fn wide(&mut self, word: &str) -> Option<usize> {
        let bytes = word.as_bytes();
        // Lowercasing keeps the character count, except for U+0130 (İ),
        // which becomes two characters.
        let chars = bytes.iter().filter(|&&b| b & 0xc0 != 0x80).count();
        if self.chars & bit(chars) == 0 && !bytes.windows(2).any(|pair| pair == [0xc4, 0xb0]) {
            return None;
        }
        self.lower.clear();
        self.lower.extend(word.chars().flat_map(char::to_lowercase));
        self.terms.iter().position(|term| *term == self.lower)
    }
}

/// The distinct words of a query, in order of first appearance.
pub fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    words(query, |word| {
        if !terms.iter().any(|term| term == word) {
            terms.push(word.into());
        }
    });
    terms
}

/// The outcome of [`rank`].
#[derive(Debug, Clone, PartialEq)]
pub struct Ranking {
    pub hits: Vec<Hit>,
    /// The leading documents that were ranked; the rest were not, because
    /// `work` ran out. The statistics cover exactly these documents.
    pub ranked: usize,
}

/// The best `limit` sections for `terms` in the leading documents `work`
/// allows, highest score first; ties go to the earlier document, then the
/// earlier line. Sections without any term are not returned.
pub fn rank(documents: &[Document], terms: &[String], limit: usize, work: &mut Work) -> Ranking {
    struct Counted {
        document: usize,
        section: Section,
        length: u32,
        frequencies: Vec<u32>,
    }
    let mut counted = Vec::new();
    let mut matcher = Matcher::new(terms);
    let mut ranked = 0;
    'documents: for (index, document) in documents.iter().enumerate() {
        let Some(sections) = sections(&document.text, work) else {
            break;
        };
        let mut sections_counted = Vec::with_capacity(sections.len());
        for section in sections {
            let mut length = 0u32;
            let mut frequencies = vec![0u32; terms.len()];
            let finished = raw_words(
                &document.text[section.bytes.clone()],
                work,
                |word, ascii| {
                    length = length.saturating_add(1);
                    if let Some(at) = matcher.position(word, ascii) {
                        frequencies[at] = frequencies[at].saturating_add(1);
                    }
                },
            );
            if !finished {
                // A partly counted document would skew every statistic.
                break 'documents;
            }
            sections_counted.push(Counted {
                document: index,
                section,
                length,
                frequencies,
            });
        }
        counted.append(&mut sections_counted);
        ranked = index + 1;
    }
    if counted.is_empty() {
        return Ranking {
            hits: Vec::new(),
            ranked,
        };
    }
    let total = counted.len() as f64;
    let average = counted.iter().map(|c| f64::from(c.length)).sum::<f64>() / total;
    let idf: Vec<f64> = (0..terms.len())
        .map(|at| {
            let frequency = counted.iter().filter(|c| c.frequencies[at] > 0).count() as f64;
            ((total - frequency + 0.5) / (frequency + 0.5) + 1.0).ln()
        })
        .collect();
    let mut hits: Vec<Hit> = counted
        .into_iter()
        .filter(|c| c.frequencies.iter().any(|&f| f > 0))
        .map(|c| {
            // Matching sections have at least one word, so `average` > 0.
            let norm = K1 * (1.0 - B + B * f64::from(c.length) / average);
            let score = c
                .frequencies
                .iter()
                .zip(&idf)
                .map(|(&tf, idf)| {
                    let tf = f64::from(tf);
                    idf * tf * (K1 + 1.0) / (tf + norm)
                })
                .sum();
            Hit {
                document: c.document,
                section: c.section,
                score,
            }
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.document.cmp(&b.document))
            .then(a.section.first_line.cmp(&b.section.first_line))
    });
    hits.truncate(limit);
    Ranking { hits, ranked }
}

/// At most `limit` bytes of `text`, cut at a character boundary and marked.
pub fn snippet(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests;
