//! Applying a rule to output.
use crate::{Count, rule::Compiled};

/// The output as `rule` reduces it, with a closing line naming the rule, or
/// `None` when the rule changes nothing.
pub(crate) fn reduce(rule: &Compiled, output: &str) -> Option<(String, Vec<Count>)> {
    let transforms = rule.transforms;
    let stripped = transforms.strip_ansi.then(|| strip_ansi(output));
    let text = stripped.as_deref().unwrap_or(output);
    let total = text.lines().count();
    let mut lines: Vec<&str> = text
        .lines()
        .filter(|line| !rule.skip.iter().any(|pattern| pattern.is_match(line)))
        .filter(|line| rule.keep.is_empty() || rule.keep.iter().any(|p| p.is_match(line)))
        .collect();
    if transforms.dedupe_adjacent {
        lines.dedup();
    }
    let blank = |line: &&str| line.trim().is_empty();
    if transforms.fold_blank_runs {
        let mut previous_blank = false;
        lines.retain(|line| {
            let keep = !(blank(line) && previous_blank);
            previous_blank = blank(line);
            keep
        });
    }
    if transforms.trim_empty_edges {
        let start = lines
            .iter()
            .position(|line| !blank(line))
            .unwrap_or(lines.len());
        let end = lines
            .iter()
            .rposition(|line| !blank(line))
            .map_or(start, |at| at + 1);
        lines = lines[start..end].to_vec();
    }
    let counts = rule
        .counters
        .iter()
        .map(|(name, pattern)| Count {
            name: name.clone(),
            count: lines.iter().filter(|line| pattern.is_match(line)).count(),
        })
        .collect::<Vec<_>>();
    let kept = lines.len();
    let (head, tail) = (rule.summarize.head, rule.summarize.tail);
    let summarized = (head > 0 || tail > 0) && head + tail < kept;
    let mut reduced = if summarized {
        let marker = format!("… [{} lines left out]", kept - head - tail);
        let mut out = lines[..head].to_vec();
        out.push(&marker);
        out.extend_from_slice(&lines[kept - tail..]);
        out.join("\n")
    } else {
        lines.join("\n")
    };
    if reduced == output || Some(reduced.as_str()) == output.strip_suffix('\n') {
        return None;
    }
    let shown = if summarized { head + tail } else { kept };
    reduced.push_str(&format!(
        "\n[output rule `{}` kept {shown} of {total} lines{}]",
        rule.id,
        counts
            .iter()
            .map(|count| format!("; {}: {}", count.name, count.count))
            .collect::<String>()
    ));
    Some((reduced, counts))
}

/// `text` without terminal control sequences (CSI and OSC, and other
/// two-character escapes).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then a final byte in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: ends with BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}
