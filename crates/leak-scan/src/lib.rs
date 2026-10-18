//! Removal of credential-shaped text from tool results before a model
//! reads them.
//!
//! Tool output is untrusted and can carry credentials the user never meant
//! to share: a `cat .env`, a configuration file, a server's error message.
//! [`redact`] finds text shaped like a known kind of credential (provider
//! API keys, forge, messenger and cloud tokens, private keys, bearer tokens,
//! JWTs and passwords in URLs) and replaces it with `[redacted <kind>]`.
//! It reports which kinds it removed and how often, never the text.
//!
//! Matching is by shape only, so it is defense in depth: secrets that look
//! like ordinary text are not found. Every pattern is linear-time (the
//! `regex` crate) and requires the credential's own prefix or structure,
//! so prose and hashes are left alone.
//!
//! The kinds are adapted from RustyClaw's `LeakDetector` (MIT, see NOTICE).
//! Unlike it, nothing here blocks a result or only warns: a match is always
//! removed, and the rest of the result still reaches the model.
use std::sync::LazyLock;

use regex::Regex;

/// A kind of credential and the text shape that identifies it. When the
/// pattern has a capture group, only that group is the credential and the
/// rest of the match (such as `Bearer `) is kept.
struct Kind {
    name: &'static str,
    pattern: Regex,
}

/// The shapes [`redact`] removes, most specific first: where two overlap,
/// the one starting first wins, then the longer.
const PATTERNS: [(&str, &str); 17] = [
    (
        "private_key",
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|\z)",
    ),
    ("anthropic_api_key", r"\bsk-ant-[A-Za-z0-9_-]{32,}"),
    (
        "openai_api_key",
        r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}",
    ),
    ("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
    ("github_token", r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
    ("github_token", r"\bgithub_pat_[A-Za-z0-9_]{60,}\b"),
    ("gitlab_token", r"\bglpat-[A-Za-z0-9_-]{20,}"),
    ("stripe_key", r"\b[rs]k_(?:live|test)_[A-Za-z0-9]{24,}\b"),
    ("google_api_key", r"\bAIza[0-9A-Za-z_-]{35}"),
    ("slack_token", r"\bxox[abposr]-[0-9A-Za-z-]{10,}"),
    ("slack_token", r"\bxapp-\d-[0-9A-Za-z-]{10,}"),
    (
        "sendgrid_api_key",
        r"\bSG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}",
    ),
    ("telegram_bot_token", r"\b\d{8,10}:AA[A-Za-z0-9_-]{33}\b"),
    (
        "discord_bot_token",
        r"\b[MNO][A-Za-z0-9_-]{23,27}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,}",
    ),
    (
        "jwt",
        r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
    ),
    ("bearer_token", r"(?i)\bbearer\s+([A-Za-z0-9._~+/-]{20,}=*)"),
    (
        "url_password",
        r"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@]+:([^\s/@]+)@",
    ),
];

static KINDS: LazyLock<Vec<Kind>> = LazyLock::new(|| {
    PATTERNS
        .iter()
        .map(|(name, pattern)| Kind {
            name,
            // The patterns are constants covered by the tests.
            pattern: Regex::new(pattern).expect("shipped pattern is valid"),
        })
        .collect()
});

/// How often one kind of credential was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub kind: &'static str,
    pub count: u64,
}

/// A result with its credentials removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    /// The kinds removed, in name order, each once.
    pub found: Vec<Found>,
}

/// The names of the kinds [`redact`] can report, in name order.
pub fn kinds() -> Vec<&'static str> {
    let mut names: Vec<_> = PATTERNS.iter().map(|(name, _)| *name).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// `text` without the credentials it contains, or `None` when it contains
/// none.
pub fn redact(text: &str) -> Option<Redacted> {
    let mut spans: Vec<(usize, usize, &'static str)> = Vec::new();
    for kind in KINDS.iter() {
        for captures in kind.pattern.captures_iter(text) {
            let span = captures.get(1).or_else(|| captures.get(0));
            if let Some(span) = span.filter(|span| !span.is_empty()) {
                spans.push((span.start(), span.end(), kind.name));
            }
        }
    }
    if spans.is_empty() {
        return None;
    }
    // Earliest first, then longest; a span overlapping a kept one goes.
    spans.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut out = String::with_capacity(text.len());
    let mut found: Vec<Found> = Vec::new();
    let mut end = 0;
    for (start, stop, name) in spans {
        if start < end {
            continue;
        }
        out.push_str(&text[end..start]);
        out.push_str("[redacted ");
        out.push_str(name);
        out.push(']');
        end = stop;
        match found.iter_mut().find(|found| found.kind == name) {
            Some(found) => found.count += 1,
            None => found.push(Found {
                kind: name,
                count: 1,
            }),
        }
    }
    out.push_str(&text[end..]);
    found.sort_by_key(|found| found.kind);
    Some(Redacted { text: out, found })
}

#[cfg(test)]
mod tests;
