//! Detection of text in tool results that reads as instructions to the
//! model rather than data for it.
//!
//! A web page, a file or a server's reply can carry text written for the
//! model that reads it ("ignore your previous instructions"), turn markers
//! of a chat template, tool-call markup, or characters the user cannot see.
//! [`find`] reports which of these kinds a result contains and how often,
//! and [`remove_hidden`] removes the invisible Unicode tag characters, which
//! carry text only a model reads. Everything else is kept: the gateway
//! marks the result for the model and shows the kinds beside it, so the
//! user can tell what the tool returned. A result is never blocked for what
//! it says.
//!
//! Matching is by phrase and is advisory: wording that avoids the phrases
//! is not found, and quoted examples are found too. The phrases address the
//! reader ("your instructions", "from now on you"), so ordinary text about
//! instructions is left alone. Every pattern is linear-time (the `regex`
//! crate).
//!
//! The kinds are adapted from RustyClaw's `PromptGuard` (MIT, see NOTICE).
//! Unlike it, nothing is scored or blocked.
use std::sync::LazyLock;

use regex::Regex;

/// Open and close tags of tool-call blocks some models write as text.
pub const TOOL_CALL_TAGS: [(&str, &str); 6] = [
    ("<tool_call>", "</tool_call>"),
    ("<tool_calls>", "</tool_calls>"),
    ("<function_call>", "</function_call>"),
    ("<function_calls>", "</function_calls>"),
    ("<tool_use>", "</tool_use>"),
    ("<|tool_call|>", "<|/tool_call|>"),
];

/// Invisible Unicode tag characters were removed.
pub const HIDDEN_TEXT: &str = "hidden_text";
/// Tool-call markup, as a model would write it.
pub const TOOL_CALL_MARKUP: &str = "tool_call_markup";

/// The phrases [`find`] reports, by kind. Several patterns may share one.
const PATTERNS: [(&str, &str); 6] = [
    (
        "instruction_override",
        r"(?i)\b(?:ignore|disregard|forget|override)\s+(?:all\s+|any\s+)?(?:of\s+)?(?:the\s+|your\s+|my\s+)?(?:previous|prior|above|earlier|preceding|original|system)\s+(?:instructions?|prompts?|directions?|rules|guidelines|messages?)\b",
    ),
    (
        "role_reassignment",
        r"(?i)\bfrom\s+now\s+on,?\s+you\s+(?:are|will|must|should|shall)\b",
    ),
    (
        "role_reassignment",
        r"(?i)\byour\s+new\s+(?:role|instructions?|task|objective|goal|persona)\s+(?:is|are)\b",
    ),
    (
        "chat_template",
        r"<\|(?:im_start|im_end|start_header_id|end_header_id|eot_id|system|user|assistant)\|>|\[/?INST\]|<</?SYS>>",
    ),
    (
        "secret_request",
        r"(?i)\b(?:reveal|print|output|send|repeat|leak|exfiltrate|dump|share|post|upload)\s+(?:me\s+|us\s+)?(?:all\s+(?:of\s+)?)?your\s+(?:full\s+|entire\s+|original\s+|hidden\s+)?(?:system\s+prompt|instructions|api\s+keys?|credentials|secrets|passwords?|tokens|environment\s+variables)\b",
    ),
    (
        TOOL_CALL_MARKUP,
        r"<\|?/?(?:tool_calls?|function_calls?|tool_use)\|?>",
    ),
];

static KINDS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    PATTERNS
        .iter()
        .map(|(name, pattern)| {
            // The patterns are constants covered by the tests.
            (
                *name,
                Regex::new(pattern).expect("shipped pattern is valid"),
            )
        })
        .collect()
});

/// How often one kind was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub kind: &'static str,
    pub count: u64,
}

/// The names of the kinds that can be reported, in name order.
pub fn kinds() -> Vec<&'static str> {
    let mut names: Vec<_> = PATTERNS.iter().map(|(name, _)| *name).collect();
    names.push(HIDDEN_TEXT);
    names.sort_unstable();
    names.dedup();
    names
}

/// The phrases in `text` that address the model, by kind in name order.
/// Run it on the text the model will read, after [`remove_hidden`]: hidden
/// characters can split a phrase.
pub fn find(text: &str) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    for (kind, pattern) in KINDS.iter() {
        let count = pattern.find_iter(text).count() as u64;
        if count == 0 {
            continue;
        }
        match found.iter_mut().find(|found| found.kind == *kind) {
            Some(found) => found.count += count,
            None => found.push(Found { kind, count }),
        }
    }
    found.sort_by_key(|found| found.kind);
    found
}

/// The waving black flag that begins an emoji tag sequence.
const BLACK_FLAG: char = '\u{1F3F4}';
/// Ends an emoji tag sequence.
const CANCEL_TAG: char = '\u{E007F}';

fn is_tag(c: char) -> bool {
    ('\u{E0000}'..='\u{E007F}').contains(&c)
}

/// `text` without runs of Unicode tag characters, and how many runs went.
/// Tag characters are invisible and spell out ASCII a model reads; they
/// can also split a credential or a phrase so neither is recognised, so
/// they go before anything else looks at a result. Their one visible use,
/// the subdivision flags (a black flag, tag letters, a cancel tag), is
/// kept, and tags right after a flag's cancel tag are a run of their own.
pub fn remove_hidden(text: String) -> (String, u64) {
    if !text.chars().any(is_tag) {
        return (text, 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut runs = 0;
    let mut chars = text.chars().peekable();
    let mut previous = None;
    while let Some(c) = chars.next() {
        if !is_tag(c) {
            out.push(c);
            previous = Some(c);
            continue;
        }
        let mut run = vec![c];
        while let Some(next) = chars.next_if(|next| is_tag(*next)) {
            run.push(next);
        }
        let mut rest = &run[..];
        if previous == Some(BLACK_FLAG)
            && let Some(cancel) = run.iter().position(|c| *c == CANCEL_TAG)
            && cancel > 0
            && run[..cancel]
                .iter()
                .all(|c| ('\u{E0020}'..='\u{E007E}').contains(c))
        {
            out.extend(&run[..=cancel]);
            rest = &run[cancel + 1..];
        }
        if !rest.is_empty() {
            runs += 1;
        }
        previous = None;
    }
    (out, runs)
}

#[cfg(test)]
mod tests;
