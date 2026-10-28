//! The active-work queue's Markdown, in the shape Literate AI's
//! `litai work record` writes and `litai work close` checks
//! (`literate_ai/application/roadmap_work.py`), so a project's queue reads
//! the same whichever tool kept it:
//!
//! ```markdown
//! ### [ ] AREA-001 — Title
//!
//! - **Priority:** P1
//! - **Owner:** the owning component
//! - **Direction:** what the user asked for
//! - **Conclusion:** the decided scope
//! - **Depends on:** none
//! - **Implementation:**
//!   - [ ] a concrete change
//! - **Evidence:**
//!   - [ ] the proof that must pass
//! ```
//!
//! A heading's box is ` ` (open), `~` (partly done) or `x` (closed). Every
//! function here is a pure text transformation; the tools read and write.
use crate::error::QueueError;

/// The queue, relative to the project root.
pub const QUEUE: &str = "docs/roadmap/active-work.md";
/// Longest single-line field, in bytes.
pub const MAX_FIELD: usize = 2000;
/// Most entries in one checklist or dependency list.
pub const MAX_ENTRIES: usize = 50;

/// The text a new queue starts with.
pub const NEW_QUEUE: &str = "# Active work\n\n\
This is the durable resumption queue for user-directed and discovered work, beneath the \
goals in `PROJECT.md`. Record an item before implementing it, check a box only when its \
evidence is recorded, and close an item only when every box is checked. Git preserves \
prior states.\n";

/// A heading or checkbox state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Partial,
    Done,
}
impl State {
    fn parse(mark: u8) -> Option<Self> {
        match mark {
            b' ' => Some(Self::Open),
            b'~' => Some(Self::Partial),
            b'x' | b'X' => Some(Self::Done),
            _ => None,
        }
    }
    pub fn mark(self) -> char {
        match self {
            Self::Open => ' ',
            Self::Partial => '~',
            Self::Done => 'x',
        }
    }
}

/// One `### [s] ID — title` item and the byte ranges it covers.
#[derive(Debug, Clone)]
pub struct Item<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub state: State,
    /// Offset of the heading's state mark.
    pub mark_at: usize,
    /// The item from its heading to the next heading of level 1 to 3.
    pub start: usize,
    pub end: usize,
    pub tasks: Vec<Task<'a>>,
    /// Labels of `- **Label:**` lines, lowercased, with the offset just past
    /// the last line of each group's checklist.
    pub groups: Vec<(String, usize)>,
}

/// One indented `- [s] text` checkbox of an item.
#[derive(Debug, Clone)]
pub struct Task<'a> {
    /// The lowercased label of the `- **Label:**` line above it.
    pub group: String,
    pub state: State,
    pub text: &'a str,
    pub mark_at: usize,
    /// Offset just past the line's text, before its newline.
    pub line_end: usize,
}

/// A fence line's delimiter character and run length: up to three spaces
/// of indentation, then at least three backticks or tildes.
fn fence(line: &str) -> Option<(u8, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let mark = *rest.as_bytes().first()?;
    if mark != b'`' && mark != b'~' {
        return None;
    }
    let run = rest.bytes().take_while(|b| *b == mark).count();
    (run >= 3).then_some((mark, run))
}

/// Lines with their start offsets, the newline excluded, and whether each
/// is inside a fenced code block (fence lines included). As in CommonMark, a
/// block closes only at a fence of its own character, at least as long as
/// the opening one and with nothing after it, so a shorter fence shown
/// inside a longer one stays part of the block.
pub(crate) fn lines(text: &str) -> Vec<(usize, &str, bool)> {
    let mut out = Vec::new();
    let mut open: Option<(u8, usize)> = None;
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let found = fence(line);
        match (open, found) {
            (None, Some((mark, run))) => {
                // A backtick fence's info string may not contain backticks.
                let info = &line.trim_start()[run..];
                if mark != b'`' || !info.contains('`') {
                    open = Some((mark, run));
                }
                out.push((offset, line, open.is_some()));
            }
            (Some((mark, run)), Some((closing, length)))
                if closing == mark
                    && length >= run
                    && line.trim_start()[length..].trim().is_empty() =>
            {
                open = None;
                out.push((offset, line, true));
            }
            (state, _) => out.push((offset, line, state.is_some())),
        }
        offset += raw.len();
    }
    out
}

/// The level of a Markdown ATX heading line, if it is one.
fn heading_level(line: &str) -> Option<usize> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    ((1..=6).contains(&level) && line[level..].starts_with(' ')).then_some(level)
}

/// Whether `id` is a work ID: an uppercase letter, then 2 to 63 uppercase
/// letters, digits or hyphens (Literate AI's `^[A-Z][A-Z0-9-]{2,63}$`).
pub fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (3..=64).contains(&bytes.len())
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'-')
}

/// Every item of the queue, in order.
pub fn items(text: &str) -> Vec<Item<'_>> {
    let lines = lines(text);
    let mut items: Vec<Item<'_>> = Vec::new();
    let mut open: Option<Item<'_>> = None;
    let mut group = String::new();
    for &(start, line, fenced) in &lines {
        let level = if fenced { None } else { heading_level(line) };
        if level.is_some_and(|level| level <= 3) {
            if let Some(mut item) = open.take() {
                item.end = start;
                items.push(item);
            }
            if let Some(item) = heading(line, start) {
                open = Some(item);
                group.clear();
            }
            continue;
        }
        let Some(item) = open.as_mut() else { continue };
        if fenced {
            continue;
        }
        if let Some(label) = line
            .strip_prefix("- **")
            .and_then(|rest| rest.split_once(":**"))
            .map(|(label, _)| label.trim().to_lowercase())
        {
            item.groups.push((label.clone(), start + line.len()));
            group = label;
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = line.trim_start();
        if indent > 0
            && let Some(rest) = body.strip_prefix("- [")
            && rest.len() >= 2
            && rest.as_bytes()[1] == b']'
            && let Some(state) = State::parse(rest.as_bytes()[0])
        {
            let mark_at = start + indent + 3;
            let text_part = rest[2..].trim();
            item.tasks.push(Task {
                group: group.clone(),
                state,
                text: text_part,
                mark_at,
                line_end: start + line.len(),
            });
            if let Some(last) = item.groups.iter_mut().rev().find(|(g, _)| *g == group) {
                last.1 = start + line.len();
            }
        }
    }
    if let Some(mut item) = open.take() {
        item.end = text.len();
        items.push(item);
    }
    items
}

fn heading(line: &str, start: usize) -> Option<Item<'_>> {
    let rest = line.strip_prefix("### [")?;
    let state = State::parse(*rest.as_bytes().first()?)?;
    let rest = rest.get(1..)?.strip_prefix("] ")?;
    let id_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'-')
        .count();
    let id = &rest[..id_len];
    if !valid_id(id) {
        return None;
    }
    let after = &rest[id_len..];
    // Literate AI matches the ID followed by a word boundary.
    if after
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let title = after
        .trim_start()
        .trim_start_matches(['—', '–', '-', ':'])
        .trim();
    Some(Item {
        id,
        title,
        state,
        mark_at: start + "### [".len(),
        start,
        end: start,
        tasks: Vec::new(),
        groups: Vec::new(),
    })
}

fn find<'a>(items: &'a [Item<'a>], id: &str) -> Result<&'a Item<'a>, QueueError> {
    if !valid_id(id) {
        return Err(QueueError::InvalidId(id.into()));
    }
    items
        .iter()
        .find(|item| item.id == id)
        .ok_or_else(|| QueueError::NoItem(id.into()))
}

/// A new item's fields. Lists hold one entry per element.
#[derive(Debug, Clone, Default)]
pub struct Record<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub priority: &'a str,
    pub owner: &'a str,
    pub direction: &'a str,
    pub conclusion: &'a str,
    pub depends_on: Vec<&'a str>,
    pub implementation: Vec<&'a str>,
    pub evidence: Vec<&'a str>,
    pub release_target: Option<&'a str>,
    pub issue: Option<&'a str>,
}

/// A field must be one non-empty line within [`MAX_FIELD`] bytes.
pub fn line_field<'a>(name: &'static str, value: &'a str) -> Result<&'a str, QueueError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(QueueError::EmptyField(name));
    }
    if value.contains(['\n', '\r']) {
        return Err(QueueError::MultiLine(name));
    }
    if value.len() > MAX_FIELD {
        return Err(QueueError::LongField(name));
    }
    Ok(value)
}

/// One entry per non-blank line, with any leading `- ` or `- [ ] ` the
/// model wrote as Markdown removed.
pub fn entries<'a>(name: &'static str, value: &'a str) -> Result<Vec<&'a str>, QueueError> {
    let entries: Vec<&str> = value
        .lines()
        .map(|line| {
            let line = line.trim();
            let line = match line {
                "-" => "",
                _ => line.strip_prefix("- ").unwrap_or(line).trim_start(),
            };
            ["[ ]", "[x]", "[~]"]
                .iter()
                .find_map(|mark| line.strip_prefix(mark))
                .unwrap_or(line)
                .trim()
        })
        .filter(|line| !line.is_empty())
        .collect();
    if entries.len() > MAX_ENTRIES {
        return Err(QueueError::TooManyEntries(name));
    }
    for entry in &entries {
        line_field(name, entry)?;
    }
    Ok(entries)
}

/// The next unused `AREA-NNN` ID: one more than the highest number used
/// with that area, at least three digits.
pub fn next_id(text: &str, area: &str) -> Result<String, QueueError> {
    let area = area.trim().trim_end_matches('-');
    let prefix = format!("{area}-");
    if !valid_id(&format!("{prefix}001")) {
        return Err(QueueError::InvalidArea(area.into()));
    }
    let highest = items(text)
        .iter()
        .filter_map(|item| item.id.strip_prefix(&prefix)?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    let id = format!("{prefix}{:03}", highest + 1);
    if !valid_id(&id) {
        return Err(QueueError::InvalidArea(area.into()));
    }
    Ok(id)
}

/// The queue with `record` added: at the end of the `## <priority>`
/// section when the queue has one for the priority's first word, else at
/// the end.
pub fn record(text: &str, record: &Record<'_>) -> Result<String, QueueError> {
    if !valid_id(record.id) {
        return Err(QueueError::InvalidId(record.id.into()));
    }
    if items(text).iter().any(|item| item.id == record.id) {
        return Err(QueueError::Duplicate(record.id.into()));
    }
    let title = line_field("title", record.title)?;
    let priority = line_field("priority", record.priority)?;
    let owner = line_field("owner", record.owner)?;
    let direction = line_field("direction", record.direction)?;
    let conclusion = line_field("conclusion", record.conclusion)?;
    if record.implementation.is_empty() {
        return Err(QueueError::EmptyChecklist("implementation"));
    }
    if record.evidence.is_empty() {
        return Err(QueueError::EmptyChecklist("evidence"));
    }
    let mut lines = vec![
        format!("### [ ] {} — {title}", record.id),
        String::new(),
        format!("- **Priority:** {priority}"),
    ];
    if let Some(target) = record.release_target {
        lines.push(format!(
            "- **Release target:** {}",
            line_field("release_target", target)?
        ));
    }
    lines.push(format!("- **Owner:** {owner}"));
    if let Some(issue) = record.issue {
        lines.push(issue_line(line_field("issue", issue)?));
    }
    lines.push(format!("- **Direction:** {direction}"));
    lines.push(format!("- **Conclusion:** {conclusion}"));
    let dependencies = if record.depends_on.is_empty() {
        "none".to_string()
    } else {
        record.depends_on.join(", ")
    };
    lines.push(format!("- **Depends on:** {dependencies}"));
    lines.push("- **Implementation:**".into());
    lines.extend(record.implementation.iter().map(|t| format!("  - [ ] {t}")));
    lines.push("- **Evidence:**".into());
    lines.extend(record.evidence.iter().map(|t| format!("  - [ ] {t}")));
    let block = lines.join("\n");

    let token = priority.split_whitespace().next().unwrap_or(priority);
    let section = lines_of_section(text, token);
    Ok(match section {
        Some(end) => {
            let (before, after) = text.split_at(end);
            if after.trim().is_empty() {
                format!("{}\n\n{block}\n", before.trim_end())
            } else {
                format!("{}\n\n{block}\n\n{}", before.trim_end(), after.trim_start())
            }
        }
        None => format!("{}\n\n{block}\n", text.trim_end()),
    })
}

/// The end offset of the `## <name>` section, if the queue has one.
fn lines_of_section(text: &str, name: &str) -> Option<usize> {
    let lines = lines(text);
    let index = lines.iter().position(|&(_, line, fenced)| {
        !fenced
            && line
                .strip_prefix("## ")
                .is_some_and(|rest| rest.trim() == name)
    })?;
    Some(
        lines[index + 1..]
            .iter()
            .find(|&&(_, line, fenced)| !fenced && heading_level(line).is_some_and(|l| l <= 2))
            .map_or(text.len(), |&(start, _, _)| start),
    )
}

/// The issue as Literate AI cites it: `**GitHub issue:** [#NN](url)`.
fn issue_line(issue: &str) -> String {
    let forge = if issue.contains("github") {
        "GitHub issue"
    } else if issue.contains("gitlab") {
        "GitLab issue"
    } else {
        "Issue"
    };
    let number = issue
        .trim_end_matches('/')
        .rsplit_once("/issues/")
        .map(|(_, n)| n)
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    match number {
        Some(n) if issue.starts_with("https://") => format!("- **{forge}:** [#{n}]({issue})"),
        _ => format!("- **{forge}:** {issue}"),
    }
}

fn set_mark(text: &mut String, at: usize, state: State) {
    text.replace_range(at..at + 1, &state.mark().to_string());
}

/// The state an open item's heading should show for its tasks.
fn progress(tasks: &[Task<'_>]) -> State {
    if tasks.iter().any(|t| t.state == State::Done) {
        State::Partial
    } else {
        State::Open
    }
}

/// The queue with a task added to the end of `group` in item `id`, and
/// whether a closed item was reopened by it.
pub fn add(text: &str, id: &str, group: &str, task: &str) -> Result<(String, bool), QueueError> {
    let task = line_field("text", task)?;
    let group = line_field("section", group)?.to_lowercase();
    let items = items(text);
    let item = find(&items, id)?;
    let line = format!("\n  - [ ] {task}");
    let mut updated = text.to_string();
    match item.groups.iter().rev().find(|(g, _)| *g == group) {
        Some(&(_, at)) => updated.insert_str(at, &line),
        None => {
            // A new `- **Label:**` group at the end of the item's own lines.
            let at = text[..item.end].trim_end().len().max(item.mark_at);
            let label = capitalize(&group);
            updated.insert_str(at, &format!("\n- **{label}:**{line}"));
        }
    }
    let reopened = item.state == State::Done;
    if reopened {
        set_mark(&mut updated, item.mark_at, progress(&item.tasks));
    }
    Ok((updated, reopened))
}

fn capitalize(label: &str) -> String {
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The queue with task `number` (from 1) of `group` in item `id` checked,
/// `result` appended to it. Evidence needs a result: a box is checked only
/// with its proof recorded.
pub fn check(
    text: &str,
    id: &str,
    group: &str,
    number: usize,
    result: Option<&str>,
) -> Result<String, QueueError> {
    let group = line_field("section", group)?.to_lowercase();
    let result = result.map(|r| line_field("result", r)).transpose()?;
    let items = items(text);
    let item = find(&items, id)?;
    let tasks: Vec<&Task<'_>> = item.tasks.iter().filter(|t| t.group == group).collect();
    if tasks.is_empty() {
        return Err(QueueError::NoGroup {
            id: id.into(),
            group,
        });
    }
    let task = *tasks
        .get(number.wrapping_sub(1))
        .ok_or_else(|| QueueError::NoTask {
            id: id.into(),
            group: group.clone(),
            count: tasks.len(),
        })?;
    if task.state == State::Done {
        return Err(QueueError::AlreadyChecked {
            id: id.into(),
            group,
            number,
        });
    }
    if group == "evidence" && result.is_none() {
        return Err(QueueError::EvidenceNeedsResult);
    }
    let mut updated = text.to_string();
    // Insert after the line first, so the mark's offset stays valid.
    if let Some(result) = result {
        updated.insert_str(task.line_end, &format!(" — {result}"));
    }
    set_mark(&mut updated, task.mark_at, State::Done);
    if item.state == State::Open {
        set_mark(&mut updated, item.mark_at, State::Partial);
    }
    Ok(updated)
}

/// The queue with item `id` closed, refused while any of its boxes is
/// unchecked. `None` when it was already closed.
pub fn close(text: &str, id: &str) -> Result<Option<String>, QueueError> {
    let items = items(text);
    let item = find(&items, id)?;
    if item.state == State::Done {
        return Ok(None);
    }
    let open: Vec<String> = item
        .tasks
        .iter()
        .filter(|t| t.state != State::Done)
        .map(|t| format!("{}: {}", t.group, t.text))
        .collect();
    if !open.is_empty() {
        return Err(QueueError::Incomplete {
            id: id.into(),
            open,
        });
    }
    let mut updated = text.to_string();
    set_mark(&mut updated, item.mark_at, State::Done);
    Ok(Some(updated))
}

/// One line per item: its state, ID, title, checklist counts and next
/// unchecked task. Closed items only when asked for.
pub fn list(text: &str, include_closed: bool) -> String {
    let items = items(text);
    let mut out = Vec::new();
    let mut closed = 0;
    for item in &items {
        if item.state == State::Done && !include_closed {
            closed += 1;
            continue;
        }
        let mut line = format!("[{}] {} — {}", item.state.mark(), item.id, item.title);
        let mut groups: Vec<&str> = Vec::new();
        for task in &item.tasks {
            if !groups.contains(&task.group.as_str()) {
                groups.push(&task.group);
            }
        }
        let counts: Vec<String> = groups
            .iter()
            .map(|g| {
                let all = item.tasks.iter().filter(|t| t.group == *g);
                let done = all.clone().filter(|t| t.state == State::Done).count();
                format!("{g} {done}/{}", all.count())
            })
            .collect();
        if !counts.is_empty() {
            line.push_str(&format!(" ({})", counts.join(", ")));
        }
        if item.state != State::Done
            && let Some(next) = item.tasks.iter().find(|t| t.state != State::Done)
        {
            line.push_str(&format!("\n    next: {}: {}", next.group, next.text));
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push("no open work items".into());
    }
    if closed > 0 {
        out.push(format!("[{closed} closed items not shown]"));
    }
    out.join("\n")
}

/// The full Markdown of item `id`.
pub fn show<'a>(text: &'a str, id: &str) -> Result<&'a str, QueueError> {
    let items = items(text);
    let item = find(&items, id)?;
    Ok(text[item.start..item.end].trim_end())
}
