//! Adding an entry to a changelog's `## Unreleased` section, where Literate
//! AI keeps release-visible outcomes as work items close (`litai release
//! prepare` only promotes that section). A pure text transformation.
use crate::{error::QueueError, queue::MAX_FIELD};

/// The default changelog, relative to the project root.
pub const CHANGELOG: &str = "CHANGELOG.md";
/// Longest entry, in bytes.
pub const MAX_ENTRY: usize = 4 * MAX_FIELD;

/// Lines with their start offsets and, for a heading outside a fenced code
/// block, its level.
fn headings(text: &str) -> Vec<(usize, &str, Option<usize>)> {
    crate::queue::lines(text)
        .into_iter()
        .map(|(offset, line, fenced)| {
            let level = line.bytes().take_while(|b| *b == b'#').count();
            let heading = (!fenced && (1..=6).contains(&level))
                .then(|| line[level..].starts_with(' ').then_some(level))
                .flatten();
            (offset, line, heading)
        })
        .collect()
}

/// The entry as a Markdown list item: `- ` before its first line and two
/// spaces before each following non-blank line.
fn item(entry: &str) -> Result<String, QueueError> {
    let lines: Vec<&str> = entry
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let Some((first, rest)) = lines.split_first() else {
        return Err(QueueError::EmptyField("text"));
    };
    if entry.len() > MAX_ENTRY {
        return Err(QueueError::LongField("text"));
    }
    let first = first.strip_prefix("- ").unwrap_or(first);
    let mut out = format!("- {first}");
    for line in rest {
        out.push_str("\n  ");
        out.push_str(line);
    }
    Ok(out)
}

fn is_unreleased(line: &str) -> bool {
    line.trim_start_matches('#')
        .trim()
        .trim_start_matches('[')
        .to_lowercase()
        .starts_with("unreleased")
}

/// The changelog with `entry` added under `## Unreleased` (and under
/// `### <category>` within it, when given), creating what is missing.
/// `existing` is `None` when there is no changelog yet.
pub fn add(
    existing: Option<&str>,
    entry: &str,
    category: Option<&str>,
) -> Result<String, QueueError> {
    let item = item(entry)?;
    let category = category
        .map(|c| crate::queue::line_field("category", c))
        .transpose()?;
    let fresh = |item: &str| match category {
        Some(c) => format!("## Unreleased\n\n### {c}\n\n{item}\n"),
        None => format!("## Unreleased\n\n{item}\n"),
    };
    let Some(text) = existing.filter(|t| !t.trim().is_empty()) else {
        return Ok(format!("# Changelog\n\n{}", fresh(&item)));
    };
    let lines = headings(text);
    let Some(index) = lines
        .iter()
        .position(|&(_, line, level)| level == Some(2) && is_unreleased(line))
    else {
        // Before the first release section, or at the end.
        return Ok(
            match lines.iter().find(|&&(_, _, level)| level == Some(2)) {
                Some(&(at, _, _)) => format!("{}{}\n{}", &text[..at], fresh(&item), &text[at..]),
                None => format!("{}\n\n{}", text.trim_end(), fresh(&item)),
            },
        );
    };
    let section_end = lines[index + 1..]
        .iter()
        .find(|&&(_, _, level)| level.is_some_and(|l| l <= 2))
        .map_or(text.len(), |&(at, _, _)| at);
    let Some(category) = category else {
        // Entries directly under Unreleased go before its first subsection.
        let end = lines[index + 1..]
            .iter()
            .find(|&&(at, _, level)| at < section_end && level.is_some_and(|l| l <= 3))
            .map_or(section_end, |&(at, _, _)| at);
        return Ok(insert(text, lines[index].0, end, &item));
    };
    let sub = lines[index + 1..].iter().position(|&(at, line, level)| {
        at < section_end && level == Some(3) && line[3..].trim().eq_ignore_ascii_case(category)
    });
    Ok(match sub {
        Some(offset) => {
            let sub = index + 1 + offset;
            let end = lines[sub + 1..]
                .iter()
                .find(|&&(_, _, level)| level.is_some_and(|l| l <= 3))
                .map_or(text.len(), |&(at, _, _)| at)
                .min(section_end);
            insert(text, lines[sub].0, end, &item)
        }
        None => insert(
            text,
            lines[index].0,
            section_end,
            &format!("### {category}\n\n{item}"),
        ),
    })
}

/// `text` with `block` after the content of the region from `start` (a
/// heading line) to `end`, matching the region's spacing between items.
fn insert(text: &str, start: usize, end: usize, block: &str) -> String {
    let region = text[start..end].trim_end();
    let content_end = start + region.len();
    let items: Vec<usize> = region.match_indices("\n- ").map(|(at, _)| at).collect();
    // Blank lines between entries when the region already separates them so.
    let blank_separated = items.len() >= 2 && region[..items[1]].ends_with('\n');
    let separator = if items.is_empty() || block.starts_with('#') || blank_separated {
        "\n\n"
    } else {
        "\n"
    };
    let rest = &text[end..];
    let tail = if rest.is_empty() { "\n" } else { "\n\n" };
    format!("{}{separator}{block}{tail}{rest}", &text[..content_end])
}
