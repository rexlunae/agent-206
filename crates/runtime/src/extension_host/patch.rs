//! Applying a [`Hunk`] list to a file's text. Pure: the workspace module
//! reads, checks and writes the files.
use a206_extensions::abi::{Hunk, PatchLine};

/// Why a file's hunks could not be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("hunk {hunk} {kind}")]
pub(super) struct Mismatch {
    /// 1-based.
    pub hunk: usize,
    pub kind: MismatchKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum MismatchKind {
    #[error("does not match the file after the hunks before it")]
    NotFound,
    #[error("has no line number and matches more than one place")]
    Ambiguous,
    #[error("adds lines past the end of the file")]
    PastEnd,
    #[error("has no lines")]
    Empty,
}

/// `text` with `hunks` applied in order. Each hunk's old lines (context and
/// removals) must appear after the previous hunk's, and the match nearest to
/// its `old_start` wins. Unchanged lines keep their exact bytes; added lines
/// take the file's line ending.
pub(super) fn apply(text: &str, hunks: &[Hunk]) -> Result<String, Mismatch> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    // Editors decide by the first line ending, and so does this.
    let ending = if lines.first().is_some_and(|line| line.ends_with("\r\n")) {
        "\r\n"
    } else {
        "\n"
    };
    let mut out = Output {
        text: String::with_capacity(text.len()),
        ending,
    };
    // Lines before `cursor` are already copied or replaced.
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let mismatch = |kind| Mismatch {
            hunk: index + 1,
            kind,
        };
        if hunk.lines.is_empty() {
            return Err(mismatch(MismatchKind::Empty));
        }
        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                PatchLine::Context(text) | PatchLine::Remove(text) => Some(content(text)),
                PatchLine::Add(_) => None,
            })
            .collect();
        let at = locate(&lines, &old, cursor, hunk.old_start).map_err(mismatch)?;
        for line in &lines[cursor..at] {
            out.push(line);
        }
        let mut old_line = at;
        for line in &hunk.lines {
            match line {
                PatchLine::Context(_) => {
                    out.push(lines[old_line]);
                    old_line += 1;
                }
                PatchLine::Remove(_) => old_line += 1,
                PatchLine::Add(text) => out.add(text),
            }
        }
        cursor = old_line;
    }
    for line in &lines[cursor..] {
        out.push(line);
    }
    Ok(out.text)
}

/// Where `old` starts at or after `from`. With a 1-based `hint`, the match
/// nearest to it (the earlier one on a tie); without, the only match. A hunk
/// with no old lines inserts after line `hint`, as `-N,0` means.
fn locate(lines: &[&str], old: &[&str], from: usize, hint: u32) -> Result<usize, MismatchKind> {
    let hint = usize::try_from(hint).unwrap_or(usize::MAX);
    if old.is_empty() {
        return match hint {
            _ if hint > lines.len() => Err(MismatchKind::PastEnd),
            _ if hint < from => Err(MismatchKind::NotFound),
            _ => Ok(hint),
        };
    }
    let last = lines
        .len()
        .checked_sub(old.len())
        .ok_or(MismatchKind::NotFound)?;
    let fits = |start: usize| {
        lines[start..start + old.len()]
            .iter()
            .zip(old)
            .all(|(line, old)| content(line) == *old)
    };
    if hint == 0 {
        let mut matches = (from..=last).filter(|&start| fits(start));
        let first = matches.next().ok_or(MismatchKind::NotFound)?;
        return match matches.next() {
            Some(_) => Err(MismatchKind::Ambiguous),
            None => Ok(first),
        };
    }
    // Outward from the hint, earlier first, so the usual exact or nearly
    // exact position is found without scanning the file.
    let target = (hint - 1).clamp(from, last.max(from));
    (0..=last.saturating_sub(from).max(target - from))
        .flat_map(|distance| [target.checked_sub(distance), target.checked_add(distance)])
        .flatten()
        .filter(|&start| (from..=last).contains(&start))
        .find(|&start| fits(start))
        .ok_or(MismatchKind::NotFound)
}

/// A line without its terminator, so `\r\n` and `\n` files compare alike.
fn content(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

struct Output {
    text: String,
    ending: &'static str,
}
impl Output {
    /// Append a line, ending the previous one first if it had no
    /// terminator: only a file's last line lacks one.
    fn push(&mut self, line: &str) {
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push_str(self.ending);
        }
        self.text.push_str(line);
    }
    fn add(&mut self, line: &str) {
        self.push(content(line));
        if line.ends_with('\n') {
            self.text.push_str(self.ending);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old_start: u32, lines: &[(char, &str)]) -> Hunk {
        Hunk {
            old_start,
            lines: lines
                .iter()
                .map(|&(kind, text)| {
                    let text = format!("{text}\n");
                    match kind {
                        ' ' => PatchLine::Context(text),
                        '-' => PatchLine::Remove(text),
                        _ => PatchLine::Add(text),
                    }
                })
                .collect(),
        }
    }
    fn mismatch(hunk: usize, kind: MismatchKind) -> Result<String, Mismatch> {
        Err(Mismatch { hunk, kind })
    }

    #[test]
    fn hunks_apply_in_order_even_when_lines_moved() {
        let text = "a\nb\nc\nd\ne\nf\n";
        let hunks = [
            hunk(1, &[(' ', "a"), ('-', "b"), ('+', "B")]),
            // Says line 3, but `e` is at line 5: the nearest match wins.
            hunk(3, &[(' ', "d"), ('-', "e"), ('+', "E1"), ('+', "E2")]),
        ];
        assert_eq!(apply(text, &hunks).unwrap(), "a\nB\nc\nd\nE1\nE2\nf\n");
    }

    #[test]
    fn the_match_nearest_the_line_number_wins_and_no_number_needs_one_match() {
        let text = "x\ny\nx\ny\nx\ny\n";
        let change = [(' ', "x"), ('-', "y"), ('+', "Y")];
        assert_eq!(
            apply(text, &[hunk(3, &change)]).unwrap(),
            "x\ny\nx\nY\nx\ny\n"
        );
        assert_eq!(
            apply(text, &[hunk(6, &change)]).unwrap(),
            "x\ny\nx\ny\nx\nY\n"
        );
        assert_eq!(
            apply(text, &[hunk(0, &change)]),
            mismatch(1, MismatchKind::Ambiguous)
        );
        assert_eq!(
            apply("x\ny\n", &[hunk(0, &change)]).unwrap(),
            "x\nY\n",
            "one match needs no line number"
        );
    }

    #[test]
    fn a_hunk_that_does_not_match_or_overlaps_is_refused() {
        let text = "a\nb\nc\n";
        assert_eq!(
            apply(text, &[hunk(2, &[('-', "missing")])]),
            mismatch(1, MismatchKind::NotFound)
        );
        // The second hunk's lines lie before the first's end.
        let overlapping = [hunk(2, &[('-', "b"), ('-', "c")]), hunk(1, &[('-', "a")])];
        assert_eq!(
            apply(text, &overlapping),
            mismatch(2, MismatchKind::NotFound)
        );
        assert_eq!(
            apply(text, &[hunk(9, &[('+', "z")])]),
            mismatch(1, MismatchKind::PastEnd)
        );
        assert_eq!(
            apply(text, &[hunk(1, &[])]),
            mismatch(1, MismatchKind::Empty)
        );
    }

    #[test]
    fn additions_go_after_their_line_and_a_new_file_starts_empty() {
        assert_eq!(
            apply("a\nb\n", &[hunk(1, &[('+', "x")])]).unwrap(),
            "a\nx\nb\n"
        );
        assert_eq!(apply("a\n", &[hunk(0, &[('+', "x")])]).unwrap(), "x\na\n");
        assert_eq!(
            apply("", &[hunk(0, &[('+', "x"), ('+', "y")])]).unwrap(),
            "x\ny\n"
        );
    }

    #[test]
    fn line_endings_and_a_missing_final_newline_are_kept() {
        let crlf = "a\r\nb\r\nc\r\n";
        let change = [(' ', "a"), ('-', "b"), ('+', "B")];
        assert_eq!(apply(crlf, &[hunk(1, &change)]).unwrap(), "a\r\nB\r\nc\r\n");
        // Unchanged lines keep their bytes, including a last line with no
        // newline; a line added after it ends that line first.
        assert_eq!(apply("a\nb\nc", &[hunk(1, &change)]).unwrap(), "a\nB\nc");
        assert_eq!(
            apply("a\nb", &[hunk(2, &[(' ', "b"), ('+', "z")])]).unwrap(),
            "a\nb\nz\n"
        );
        // An added line marked as the last without a newline gets none.
        let mut last = hunk(2, &[('-', "b")]);
        last.lines.push(PatchLine::Add("B".into()));
        assert_eq!(apply("a\nb\n", &[last]).unwrap(), "a\nB");
    }
}
