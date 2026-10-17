//! Reading a unified diff, as `diff -u` or `git diff` writes it, into the
//! host's [`FilePatch`] list. The host finds where each hunk applies and
//! writes the files; this only parses.
use a206_extension_sdk::{FileChange, FilePatch, Hunk, PatchLine};

/// Why a patch could not be read. Line numbers are 1-based, in the patch.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("the patch has no file changes; write a unified diff with ---, +++ and @@ lines")]
    Empty,
    #[error("line {0}: a hunk comes before any --- and +++ file header")]
    HunkWithoutFile(usize),
    #[error("line {0}: a --- header is not followed by a +++ header")]
    MissingNewPath(usize),
    #[error("line {0}: the hunk header is not of the form @@ -start,count +start,count @@")]
    BadHeader(usize),
    #[error(
        "line {0}: this line is outside every hunk; check the line counts in the @@ header before it"
    )]
    OutsideHunk(usize),
    #[error("line {line}: {what} is not supported; patch file contents only")]
    Unsupported { line: usize, what: &'static str },
    #[error("line {0}: the patch moves a file, which is not supported; delete and create it")]
    Rename(usize),
    #[error("line {0}: the file header has neither an old nor a new path")]
    NoPath(usize),
    #[error(
        "line {0}: the hunk has no line numbers and only adds lines, so where it goes is unknown"
    )]
    Unplaced(usize),
    #[error("line {0}: the hunk has fewer lines than the counts in its @@ header")]
    ShortHunk(usize),
    #[error("{0} has a file header but no hunks")]
    NoHunks(String),
}

/// Headers `git diff` writes for changes other than file contents.
const UNSUPPORTED: &[(&str, &str)] = &[
    ("rename from ", "renaming a file"),
    ("rename to ", "renaming a file"),
    ("copy from ", "copying a file"),
    ("copy to ", "copying a file"),
    ("old mode ", "changing a file's mode"),
    ("new mode ", "changing a file's mode"),
    ("Binary files ", "a binary change"),
    ("GIT binary patch", "a binary change"),
    ("*** Begin Patch", "this patch format; write a unified diff"),
];

pub fn parse(patch: &str) -> Result<Vec<FilePatch>, ParseError> {
    let mut lines = patch.split_inclusive('\n').enumerate().peekable();
    let mut files: Vec<FilePatch> = Vec::new();
    while let Some((index, line)) = lines.next() {
        let number = index + 1;
        if let Some(&(_, what)) = UNSUPPORTED
            .iter()
            .find(|(start, _)| line.starts_with(start))
        {
            return Err(ParseError::Unsupported { line: number, what });
        }
        if let Some(old) = line.strip_prefix("--- ") {
            let Some(new) = lines.next_if(|(_, next)| next.starts_with("+++ ")) else {
                return Err(ParseError::MissingNewPath(number));
            };
            finish(&files)?;
            files.push(file(number, old, &new.1[4..])?);
            continue;
        }
        if line.starts_with("@@") {
            let file = files
                .last_mut()
                .ok_or(ParseError::HunkWithoutFile(number))?;
            let header = range(line).ok_or(ParseError::BadHeader(number))?;
            let hunk = hunk(&mut lines, header).ok_or(ParseError::ShortHunk(number))?;
            let adds_only = hunk
                .lines
                .iter()
                .all(|line| matches!(line, PatchLine::Add(_)));
            if header.counts.is_none() && adds_only && file.change == FileChange::Modify {
                return Err(ParseError::Unplaced(number));
            }
            file.hunks.push(hunk);
            continue;
        }
        // Git and diff headers, and commit text around a patch, are skipped;
        // a hunk line here means the header before it undercounted.
        let hunk_like = line.starts_with([' ', '+', '-']);
        if hunk_like && !files.is_empty() {
            return Err(ParseError::OutsideHunk(number));
        }
    }
    finish(&files)?;
    if files.is_empty() {
        return Err(ParseError::Empty);
    }
    Ok(files)
}

/// The previous file must have had at least one hunk.
fn finish(files: &[FilePatch]) -> Result<(), ParseError> {
    match files.last() {
        Some(file) if file.hunks.is_empty() => Err(ParseError::NoHunks(file.path.clone())),
        _ => Ok(()),
    }
}

const NULL: &str = "/dev/null";

/// A file section from its `---` and `+++` paths.
fn file(number: usize, old: &str, new: &str) -> Result<FilePatch, ParseError> {
    let (old, new) = (path(number, old)?, path(number, new)?);
    let (old, new) = ((old != NULL).then_some(old), (new != NULL).then_some(new));
    // Git writes `a/` and `b/` prefixes; plain `diff -u` writes none.
    let prefixed =
        old.is_none_or(|old| old.starts_with("a/")) && new.is_none_or(|new| new.starts_with("b/"));
    let strip = |path: &str| {
        if prefixed {
            path[2..].to_string()
        } else {
            path.to_string()
        }
    };
    let (change, path) = match (old.map(strip), new.map(strip)) {
        (None, None) => return Err(ParseError::NoPath(number)),
        (None, Some(new)) => (FileChange::Create, new),
        (Some(old), None) => (FileChange::Delete, old),
        (Some(old), Some(new)) if old != new => return Err(ParseError::Rename(number)),
        (Some(_), Some(new)) => (FileChange::Modify, new),
    };
    Ok(FilePatch {
        path,
        change,
        hunks: Vec::new(),
    })
}

/// A header path, without the timestamp `diff -u` puts after a tab.
fn path(number: usize, text: &str) -> Result<&str, ParseError> {
    let text = text.trim_end_matches(['\r', '\n']);
    let text = text
        .split_once('\t')
        .map_or(text, |(path, _)| path)
        .trim_end();
    if text.starts_with('"') {
        return Err(ParseError::Unsupported {
            line: number,
            what: "a quoted path",
        });
    }
    Ok(text)
}

/// The numbers of an `@@ -a,b +c,d @@` header: the old start and both line
/// counts. A bare `@@` (some tools write one) has none.
#[derive(Debug, Clone, Copy)]
struct Header {
    old_start: u32,
    counts: Option<(u32, u32)>,
}
fn range(line: &str) -> Option<Header> {
    let rest = line.strip_prefix("@@")?.trim_end_matches(['\r', '\n']);
    let inner = rest.split("@@").next()?.trim();
    if inner.is_empty() {
        return Some(Header {
            old_start: 0,
            counts: None,
        });
    }
    let (old, new) = inner.split_once(' ')?;
    let side = |text: &str, sign: char| -> Option<(u32, u32)> {
        let text = text.trim().strip_prefix(sign)?;
        match text.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = side(old, '-')?;
    let (_, new_count) = side(new, '+')?;
    Some(Header {
        old_start,
        counts: Some((old_count, new_count)),
    })
}

/// The lines of one hunk. With counts, exactly that many old and new lines,
/// or `None` if the patch ends or another header starts first: applying the
/// lines that are there would make only part of the change. Without counts,
/// every line until something that is not a hunk line.
fn hunk<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = (usize, &'a str)>>,
    header: Header,
) -> Option<Hunk> {
    let (mut old, mut new) = header.counts.unwrap_or((u32::MAX, u32::MAX));
    let mut out: Vec<PatchLine> = Vec::new();
    while let Some(&(_, line)) = lines.peek() {
        if line.starts_with('\\') {
            // `\ No newline at end of file`: the line before has none.
            lines.next();
            if let Some(PatchLine::Context(text) | PatchLine::Remove(text) | PatchLine::Add(text)) =
                out.last_mut()
            {
                let trimmed = text.trim_end_matches(['\r', '\n']).len();
                text.truncate(trimmed);
            }
            continue;
        }
        if old == 0 && new == 0 {
            break;
        }
        let ends = line.starts_with("@@")
            || line.starts_with("diff ")
            || (line.starts_with("--- ") && header.counts.is_none());
        if ends {
            break;
        }
        // A line with its terminator; the patch's last line may lack one.
        let text = |body: &str| {
            let body = body.strip_suffix('\n').unwrap_or(body);
            let body = body.strip_suffix('\r').unwrap_or(body);
            format!("{body}\n")
        };
        let parsed = match line.as_bytes().first() {
            Some(b' ') => (PatchLine::Context(text(&line[1..])), 1, 1),
            Some(b'-') => (PatchLine::Remove(text(&line[1..])), 1, 0),
            Some(b'+') => (PatchLine::Add(text(&line[1..])), 0, 1),
            // An empty line in a hunk is an empty context line whose
            // leading space was trimmed.
            Some(b'\n' | b'\r') | None => (PatchLine::Context("\n".into()), 1, 1),
            Some(_) => break,
        };
        let (line, old_used, new_used) = parsed;
        if old < old_used || new < new_used {
            // More lines than the header counts: they belong to whatever
            // comes next, which reports them if they are stray.
            break;
        }
        old -= old_used;
        new -= new_used;
        out.push(line);
        lines.next();
    }
    if header.counts.is_some() && (old, new) != (0, 0) {
        return None;
    }
    Some(Hunk {
        old_start: header.old_start,
        lines: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(text: &str) -> PatchLine {
        PatchLine::Add(format!("{text}\n"))
    }
    fn remove(text: &str) -> PatchLine {
        PatchLine::Remove(format!("{text}\n"))
    }
    fn context(text: &str) -> PatchLine {
        PatchLine::Context(format!("{text}\n"))
    }

    #[test]
    fn a_git_diff_reads_as_files_with_their_hunks() {
        let patch = "\
diff --git a/src/one.rs b/src/one.rs
index 1111111..2222222 100644
--- a/src/one.rs
+++ b/src/one.rs
@@ -1,3 +1,3 @@ fn main() {
 a
-b
+B
 c
@@ -10,2 +10,3 @@
 j
+k
 l
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+x
+y
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-z
";
        let files = parse(patch).unwrap();
        assert_eq!(
            files,
            vec![
                FilePatch {
                    path: "src/one.rs".into(),
                    change: FileChange::Modify,
                    hunks: vec![
                        Hunk {
                            old_start: 1,
                            lines: vec![context("a"), remove("b"), add("B"), context("c")],
                        },
                        Hunk {
                            old_start: 10,
                            lines: vec![context("j"), add("k"), context("l")],
                        },
                    ],
                },
                FilePatch {
                    path: "new.txt".into(),
                    change: FileChange::Create,
                    hunks: vec![Hunk {
                        old_start: 0,
                        lines: vec![add("x"), add("y")],
                    }],
                },
                FilePatch {
                    path: "gone.txt".into(),
                    change: FileChange::Delete,
                    hunks: vec![Hunk {
                        old_start: 1,
                        lines: vec![remove("z")],
                    }],
                },
            ]
        );
    }

    #[test]
    fn counts_decide_where_a_hunk_ends() {
        // `--- x` inside the counted hunk is a removed line, and the blank
        // line is an empty context line with its space trimmed.
        let patch = "--- f\t2026-01-01\n+++ f\t2026-01-02\n@@ -1,3 +1,2 @@\n--- x\n\n z\n";
        let files = parse(patch).unwrap();
        assert_eq!(files[0].path, "f", "plain diff -u paths keep no prefix");
        assert_eq!(
            files[0].hunks[0].lines,
            vec![remove("-- x"), context(""), context("z")]
        );
        // One line more than the header counts is reported, not dropped.
        let over = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n+c\n";
        assert_eq!(parse(over), Err(ParseError::OutsideHunk(6)));
        // So is one line less: the hunk would make only part of its change.
        let short = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n+b\n";
        assert_eq!(parse(short), Err(ParseError::ShortHunk(3)));
        let cut = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n+b\n@@ -5 +5 @@\n-e\n+E\n";
        assert_eq!(parse(cut), Err(ParseError::ShortHunk(3)));
    }

    #[test]
    fn a_no_newline_marker_ends_the_line_before_it() {
        let patch = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n";
        assert_eq!(
            parse(patch).unwrap()[0].hunks[0].lines,
            vec![PatchLine::Remove("a".into()), PatchLine::Add("b".into())]
        );
    }

    #[test]
    fn a_bare_hunk_header_needs_old_lines_to_place_it() {
        let placed = "--- a/f\n+++ b/f\n@@\n x\n+y\n";
        let hunk = &parse(placed).unwrap()[0].hunks[0];
        assert_eq!(hunk.old_start, 0);
        assert_eq!(hunk.lines, vec![context("x"), add("y")]);
        assert_eq!(
            parse("--- a/f\n+++ b/f\n@@ @@\n+y\n"),
            Err(ParseError::Unplaced(3))
        );
    }

    #[test]
    fn unsupported_and_malformed_patches_are_refused() {
        assert_eq!(parse("just text\n"), Err(ParseError::Empty));
        assert_eq!(
            parse("@@ -1 +1 @@\n-a\n+b\n"),
            Err(ParseError::HunkWithoutFile(1))
        );
        assert_eq!(
            parse("--- a/f\n@@ -1 +1 @@\n"),
            Err(ParseError::MissingNewPath(1))
        );
        assert_eq!(
            parse("--- a/f\n+++ b/f\n@@ -x +1 @@\n"),
            Err(ParseError::BadHeader(3))
        );
        assert_eq!(
            parse("--- a/f\n+++ b/g\n@@ -1 +1 @@\n-a\n+b\n"),
            Err(ParseError::Rename(1))
        );
        assert_eq!(
            parse("--- /dev/null\n+++ /dev/null\n"),
            Err(ParseError::NoPath(1))
        );
        assert_eq!(
            parse("--- a/f\n+++ b/f\n--- a/g\n+++ b/g\n@@ -1 +1 @@\n-a\n+b\n"),
            Err(ParseError::NoHunks("f".into()))
        );
        assert!(matches!(
            parse("diff --git a/f b/g\nrename from f\n"),
            Err(ParseError::Unsupported { line: 2, .. })
        ));
        assert!(matches!(
            parse("*** Begin Patch\n"),
            Err(ParseError::Unsupported { line: 1, .. })
        ));
        assert!(matches!(
            parse("--- \"a/x y\"\n+++ \"b/x y\"\n"),
            Err(ParseError::Unsupported { line: 1, .. })
        ));
    }
}
