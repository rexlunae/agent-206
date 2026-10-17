//! Searching the text of every conversation's messages.
//!
//! The gateway answers [`Command::SearchMessages`](crate::Command::SearchMessages)
//! with [`search`]; clients show the hits and open the conversation.
use crate::{Id, Role, Thread};
use serde::{Deserialize, Serialize};

/// Longest query, in characters.
pub const MAX_QUERY: usize = 200;
/// Most hits one search returns.
pub const MAX_HITS: u32 = 100;
/// Characters of context kept on each side of a match.
const CONTEXT: usize = 60;
/// Characters of a conversation title copied into each hit. Titles are not
/// otherwise bounded, and a reply repeats one per hit, so this keeps a full
/// reply well inside a protocol frame.
pub const MAX_TITLE: usize = 120;

/// One message that contains every term of the query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHit {
    pub thread: Id,
    /// The conversation's title, cut to [`MAX_TITLE`] characters.
    pub title: String,
    pub message: Id,
    pub role: Role,
    pub at: u64,
    /// The first match with up to [`CONTEXT`] characters around it; `before`
    /// and `after` are empty at the ends of the message.
    pub before: String,
    pub matched: String,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    #[error("enter something to search for")]
    Empty,
    #[error("searches are limited to {MAX_QUERY} characters")]
    TooLong,
}

/// Lowercased text, with final sigma folded to sigma as Unicode case folding
/// does (`Σ`, `σ` and `ς` are the same letter), and the byte offset in the original text of the
/// character each lowercased byte came from.
struct Folded {
    text: String,
    origin: Vec<usize>,
}
fn fold(text: &str) -> Folded {
    let mut folded = Folded {
        text: String::with_capacity(text.len()),
        origin: Vec::with_capacity(text.len()),
    };
    for (offset, character) in text.char_indices() {
        for lower in character.to_lowercase() {
            let lower = if lower == 'ς' { 'σ' } else { lower };
            folded.text.push(lower);
            folded
                .origin
                .extend(std::iter::repeat_n(offset, lower.len_utf8()));
        }
    }
    folded
}
/// The original byte range of the first case-insensitive match of `term`.
fn find(text: &str, folded: &Folded, term: &str) -> Option<(usize, usize)> {
    let start = folded.text.find(term)?;
    let last = start + term.len() - 1;
    let end = folded.origin[last] + text[folded.origin[last]..].chars().next()?.len_utf8();
    Some((folded.origin[start], end))
}
fn tail(text: &str, count: usize) -> &str {
    let skip = text.chars().count().saturating_sub(count);
    text.char_indices()
        .nth(skip)
        .map_or("", |(at, _)| &text[at..])
}
fn head(text: &str, count: usize) -> &str {
    text.char_indices()
        .nth(count)
        .map_or(text, |(at, _)| &text[..at])
}

/// Messages containing every whitespace-separated term of `query`, ignoring
/// case, newest first and at most `limit` (capped at [`MAX_HITS`]). Messages
/// stamped in the same second keep their order within a conversation, newest
/// first.
pub fn search(threads: &[Thread], query: &str, limit: u32) -> Result<Vec<MessageHit>, SearchError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(SearchError::Empty);
    }
    if query.chars().count() > MAX_QUERY {
        return Err(SearchError::TooLong);
    }
    let terms: Vec<String> = query.split_whitespace().map(|t| fold(t).text).collect();
    let limit = usize::try_from(limit.min(MAX_HITS)).unwrap_or(usize::MAX);
    let mut hits: Vec<MessageHit> = threads
        .iter()
        .flat_map(|thread| {
            thread
                .messages
                .iter()
                .rev()
                .map(move |message| (thread, message))
        })
        .filter_map(|(thread, message)| {
            let folded = fold(&message.text);
            let mut first = None;
            for term in &terms {
                let found = find(&message.text, &folded, term)?;
                first = first.or(Some(found));
            }
            let (start, end) = first?;
            Some(MessageHit {
                thread: thread.id,
                title: head(&thread.title, MAX_TITLE).to_owned(),
                message: message.id,
                role: message.role.clone(),
                at: message.at,
                before: tail(&message.text[..start], CONTEXT).to_owned(),
                matched: message.text[start..end].to_owned(),
                after: head(&message.text[end..], CONTEXT).to_owned(),
            })
        })
        .collect();
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.at));
    hits.truncate(limit);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CodingBackend, Message, Mode};

    fn word() -> String {
        Id::new_v4().simple().to_string()
    }
    fn thread(texts: &[(String, u64)]) -> Thread {
        let mut thread = Thread::new(word(), Mode::Chat);
        thread.messages = texts
            .iter()
            .map(|(text, at)| Message::new(Role::User, text.clone(), *at, CodingBackend::default()))
            .collect();
        thread
    }

    #[test]
    fn every_term_must_match_and_newest_hits_come_first() {
        let (a, b, other) = (word(), word(), word());
        let threads = [
            thread(&[(format!("{a} then {b}"), 1), (format!("{a} only"), 2)]),
            thread(&[
                (format!("{} and {}", b.to_uppercase(), a), 3),
                (other.clone(), 4),
            ]),
        ];
        let hits = search(&threads, &format!("  {a}   {b} "), MAX_HITS).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].at, hits[1].at), (3, 1));
        assert_eq!(hits[0].thread, threads[1].id);
        assert_eq!(hits[0].title, threads[1].title);
        assert_eq!(hits[0].message, threads[1].messages[0].id);
        // The snippet shows the first term as written in the message.
        assert_eq!(hits[1].matched, a);
        assert_eq!(hits[1].after, format!(" then {b}"));
        assert!(search(&threads, &word(), MAX_HITS).unwrap().is_empty());
        assert_eq!(search(&threads, &a, 1).unwrap().len(), 1);
    }

    #[test]
    fn matching_ignores_case_and_keeps_snippets_on_character_boundaries() {
        let key = word();
        let long = "é".repeat(CONTEXT * 2);
        // `İ` lowercases to two characters, shifting every later offset.
        let text = format!("{long}İ ÉCOLE {key}{long}");
        let threads = [thread(&[(text, 1)])];
        let hits = search(&threads, "école", MAX_HITS).unwrap();
        assert_eq!(hits[0].matched, "ÉCOLE");
        assert_eq!(hits[0].before, format!("{}İ ", "é".repeat(CONTEXT - 2)));
        assert_eq!(hits[0].after.chars().count(), CONTEXT);
        let hits = search(&threads, &key.to_uppercase(), MAX_HITS).unwrap();
        assert_eq!(hits[0].matched, key);
        assert!(hits[0].after.chars().all(|c| c == 'é'));
    }

    #[test]
    fn same_second_messages_come_newest_first() {
        let key = word();
        let threads = [thread(&[(format!("{key} 1"), 5), (format!("{key} 2"), 5)])];
        let hits = search(&threads, &key, MAX_HITS).unwrap();
        assert_eq!(hits[0].message, threads[0].messages[1].id);
        assert_eq!(hits[1].message, threads[0].messages[0].id);
    }

    #[test]
    fn final_sigma_matches_either_sigma() {
        let threads = [thread(&[("ΟΔΟΣ".to_owned(), 1), ("οδός".to_owned(), 2)])];
        assert_eq!(search(&threads, "Σ", MAX_HITS).unwrap().len(), 2);
        let hits = search(&threads, "οδοσ", MAX_HITS).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matched, "ΟΔΟΣ");
    }

    #[test]
    fn hits_carry_a_bounded_title() {
        let key = word();
        let mut long = thread(&[(key.clone(), 1)]);
        long.title = "é".repeat(MAX_TITLE * 50);
        let hits = search(std::slice::from_ref(&long), &key, MAX_HITS).unwrap();
        assert_eq!(hits[0].title, "é".repeat(MAX_TITLE));
    }

    #[test]
    fn empty_and_oversized_queries_are_refused() {
        assert_eq!(search(&[], " \t", MAX_HITS), Err(SearchError::Empty));
        assert_eq!(
            search(&[], &"x".repeat(MAX_QUERY + 1), MAX_HITS),
            Err(SearchError::TooLong)
        );
    }
}
