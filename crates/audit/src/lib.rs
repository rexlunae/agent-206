//! Pre-install audit of skill and extension text: patterns a user should look
//! at before approving, such as a downloaded script piped into a shell or an
//! instruction to keep something from them. Findings advise the user's
//! review; they never approve or block, and a clean report proves nothing.
//! Scanning reads text only: it runs nothing and opens no files.
#![forbid(unsafe_code)]
use a206_core::{
    AuditBuilder, AuditField, AuditFinding, AuditLocation, AuditReport, AuditRule, PluginSummary,
};
use regex::RegexSet;
use std::sync::LazyLock;

/// Patterns per rule. Each matches within one line.
const PATTERNS: &[(AuditRule, &str)] = &[
    (
        AuditRule::RemoteScript,
        r"(?i)\b(curl|wget|fetch|iwr|invoke-webrequest|irm|invoke-restmethod)\b[^|\n]*\|\s*(sudo\s+)?(\S*/)?(ba|z|da|k|fi)?sh\b",
    ),
    (
        AuditRule::RemoteScript,
        r"(?i)\b(ba|z)?sh\s+(-c\s+)?['\x22]?\s*\$\(\s*(curl|wget)\b",
    ),
    (
        AuditRule::RemoteScript,
        r"(?i)\b(curl|wget)\b[^|\n]*\|\s*(sudo\s+)?(python3?|perl|ruby|node)\b",
    ),
    (
        AuditRule::RemoteScript,
        r"(?i)\b(iex|invoke-expression)\b[^\n]*\b(iwr|irm|invoke-webrequest|invoke-restmethod|downloadstring)\b",
    ),
    (AuditRule::ReverseShell, r"(?i)/dev/(tcp|udp)/"),
    (
        AuditRule::ReverseShell,
        r"(?i)\b(nc|ncat|netcat)\b[^\n]*\s-(e|c)\s",
    ),
    (AuditRule::ReverseShell, r"(?i)\b(ba)?sh\s+-i\b[^\n]*>&"),
    (AuditRule::ReverseShell, r"(?i)\bsocat\b[^\n]*\bexec:"),
    (
        AuditRule::CredentialFile,
        r"(?i)(\.ssh/|\bid_(rsa|dsa|ecdsa|ed25519)\b|\.aws/credentials|\.netrc\b|\.git-credentials|\.docker/config\.json|\.kube/config|\.gnupg/|/etc/shadow\b|library/keychains|login\.keychain|\.config/gh/hosts\.yml|\.npmrc\b|\.pypirc\b)",
    ),
    (
        AuditRule::InstructionOverride,
        r"(?i)\b(ignore|disregard|forget|override|bypass)\s+(all\s+|any\s+|every\s+)?(of\s+)?(the\s+|your\s+|these\s+|those\s+)?(previous|prior|above|earlier|preceding|original|system|developer|safety)\s+(instructions?|prompts?|rules|messages|guidelines|directions|policies)",
    ),
    (
        AuditRule::InstructionOverride,
        r"(?i)\b(do\s+not|don't|never)\s+(tell|inform|show|alert|notify|mention\s+(this|it)\s+to)\s+the\s+user\b",
    ),
    (
        AuditRule::InstructionOverride,
        r"(?i)\bwithout\s+(telling|asking|informing|notifying|alerting)\s+the\s+user\b",
    ),
    (
        AuditRule::InstructionOverride,
        r"(?i)\b(reveal|print|show|output|repeat)\s+(your|the)\s+(system|developer)\s+(prompt|instructions|message)",
    ),
    (AuditRule::SecretValue, r"\b(AKIA|ASIA)[0-9A-Z]{16}\b"),
    (
        AuditRule::SecretValue,
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
    ),
    (AuditRule::SecretValue, r"\bgh[pousr]_[A-Za-z0-9]{36,}\b"),
    (AuditRule::SecretValue, r"\bgithub_pat_[A-Za-z0-9_]{60,}\b"),
    (AuditRule::SecretValue, r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b"),
    (AuditRule::SecretValue, r"\bsk-[A-Za-z0-9_-]{32,}\b"),
    (AuditRule::SecretValue, r"\bAIza[0-9A-Za-z_-]{35}\b"),
    (
        AuditRule::EncodedCommand,
        r"(?i)\bbase64\s+(-d|-D|--decode)\b[^\n]*\|\s*(sudo\s+)?(ba|z|da)?sh\b",
    ),
    (
        AuditRule::EncodedCommand,
        r"(?i)\b(ba|z)?sh\s+(-c\s+)?['\x22]?\s*\$\(\s*(echo|printf)\b[^\n]*\|\s*base64\s+(-d|-D|--decode)\b",
    ),
    (
        AuditRule::EncodedCommand,
        r"(?i)\b(powershell|pwsh)(\.exe)?\b[^\n]*\s-(e|en|enc|encodedcommand)\s",
    ),
    (
        AuditRule::EncodedCommand,
        r"(?i)\b(eval|exec)\s*\(\s*(atob|base64\.b64decode|buffer\.from)\b",
    ),
];

struct Rules {
    set: RegexSet,
    rules: Vec<AuditRule>,
}

static RULES: LazyLock<Rules> = LazyLock::new(|| Rules {
    // The patterns are constants covered by this crate's tests.
    set: RegexSet::new(PATTERNS.iter().map(|(_, pattern)| *pattern))
        .expect("audit patterns are valid"),
    rules: PATTERNS.iter().map(|(rule, _)| *rule).collect(),
});

/// Characters that are invisible or reorder text, so what a reviewer reads
/// can differ from what a model receives.
fn hidden(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}'
        | '\u{FEFF}'
        | '\u{E0000}'..='\u{E007F}')
}

/// The rules one piece of text breaks, in [`AuditRule::ALL`] order.
fn rules_in(text: &str) -> Vec<AuditRule> {
    let rules = &*RULES;
    let mut found: Vec<AuditRule> = rules
        .set
        .matches(text)
        .into_iter()
        .map(|index| rules.rules[index])
        .collect();
    if text.chars().any(hidden) {
        found.push(AuditRule::HiddenText);
    }
    found.sort();
    found.dedup();
    found
}

/// Audit a text file line by line, such as a `SKILL.md`.
pub fn text(text: &str) -> AuditReport {
    let mut report = AuditBuilder::default();
    // A byte-order mark opening the file is an encoding marker, not content.
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    for (index, line) in text.lines().enumerate() {
        let line_number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        for rule in rules_in(line) {
            report.push(AuditFinding {
                rule,
                location: AuditLocation::Line(line_number),
            });
        }
    }
    report.finish()
}

/// Audit the text an extension's manifest gives the user and the model:
/// its name and description, and its tools', parameters' and panels' text.
pub fn plugin(summary: &PluginSummary) -> AuditReport {
    let mut report = AuditBuilder::default();
    let mut check = |item: Option<&str>, field: AuditField, text: &str| {
        for rule in rules_in(text) {
            report.push(AuditFinding {
                rule,
                location: AuditLocation::Field {
                    item: item.map(str::to_owned),
                    field,
                },
            });
        }
    };
    check(None, AuditField::Name, &summary.name);
    check(None, AuditField::Description, &summary.description);
    for tool in &summary.tools {
        check(Some(&tool.name), AuditField::ToolName, &tool.name);
        check(
            Some(&tool.name),
            AuditField::ToolDescription,
            &tool.description,
        );
        for param in &tool.params {
            let item = format!("{}.{}", tool.name, param.name);
            check(
                Some(&item),
                AuditField::ParameterDescription,
                &param.description,
            );
        }
    }
    for panel in &summary.panels {
        check(Some(&panel.id), AuditField::PanelTitle, &panel.title);
    }
    report.finish()
}

#[cfg(test)]
mod tests;
