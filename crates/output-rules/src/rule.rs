//! The rule file format and compiling rules into matchers.
use crate::{Input, MAX_RULE_FILE};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The shipped rules, compiled into the crate.
const BUILTIN: &str = include_str!("../rules/builtin.toml");
/// Largest compiled pattern, so a rule cannot make matching expensive.
const MAX_PATTERN_SIZE: usize = 1 << 20;
/// Longest rule id or counter name, in bytes. Both are recorded on every
/// call a rule reduces.
pub const MAX_NAME: usize = 128;
/// Most counters one rule keeps.
pub const MAX_COUNTERS: usize = 16;

/// Where a rule came from. User rules win ties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    Builtin,
    User,
}

/// Why the output rules could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("could not read the output rules directory {}", path.display())]
    Directory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read the output rule file {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the output rule file {} is larger than {MAX_RULE_FILE} bytes", path.display())]
    TooLarge { path: PathBuf },
    #[error("the output rule file {} is not valid", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("an output rule in {} has no id", path.display())]
    MissingId { path: PathBuf },
    #[error(
        "an output rule in {} is too large: ids and counter names are at most {MAX_NAME} bytes, with at most {MAX_COUNTERS} counters",
        path.display()
    )]
    TooLong { path: PathBuf },
    #[error("output rule `{id}` in {} has an invalid pattern", path.display())]
    Pattern {
        path: PathBuf,
        id: String,
        #[source]
        source: PatternError,
    },
    #[error("output rule `{id}` is defined in both {} and {}", first.display(), second.display())]
    Duplicate {
        id: String,
        first: PathBuf,
        second: PathBuf,
    },
    #[error("there are more than {} user output rules", crate::MAX_RULES)]
    TooMany,
}

/// Why one pattern could not be compiled.
#[derive(Debug, thiserror::Error)]
pub enum PatternError {
    #[error("unsupported pattern flag `{0}`; use i, m, s or x")]
    Flag(char),
    #[error("`{pattern}` is not a valid regular expression")]
    Regex {
        pattern: String,
        #[source]
        source: regex::Error,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    rule: Vec<Rule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    id: String,
    #[serde(default)]
    family: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    priority: i32,
    #[serde(default = "enabled")]
    enabled: bool,
    #[serde(default, rename = "match")]
    matching: Match,
    #[serde(default)]
    filters: Filters,
    #[serde(default)]
    transforms: Transforms,
    #[serde(default)]
    summarize: Summarize,
    #[serde(default)]
    counters: Vec<Counter>,
}
fn enabled() -> bool {
    true
}

/// When a rule applies. Every condition given must hold; an empty rule
/// matches every call.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Match {
    /// Any of these tool names.
    #[serde(default)]
    tools: Vec<String>,
    /// Any of these programs (the command's first word, without its path).
    #[serde(default)]
    argv0: Vec<String>,
    /// `git` with any of these subcommands.
    #[serde(default)]
    git_subcommands: Vec<String>,
    /// Every group's words all appear among the arguments.
    #[serde(default)]
    argv_all: Vec<Vec<String>>,
    /// Some group's words all appear among the arguments.
    #[serde(default)]
    argv_any: Vec<Vec<String>>,
    /// The command line contains all of these.
    #[serde(default)]
    command_all: Vec<String>,
    /// The command line contains one of these.
    #[serde(default)]
    command_any: Vec<String>,
}
impl Match {
    pub(crate) fn matches(&self, input: &Input<'_>, argv: &[String]) -> bool {
        let has = |group: &Vec<String>| group.iter().all(|word| argv.contains(word));
        let command = input.command.unwrap_or_default();
        let needs_command = !(self.command_all.is_empty() && self.command_any.is_empty());
        (self.tools.is_empty() || self.tools.iter().any(|tool| tool == input.tool))
            && (self.argv0.is_empty() || argv.first().is_some_and(|a| self.argv0.contains(a)))
            && (self.git_subcommands.is_empty()
                || crate::command::git_subcommand(argv)
                    .is_some_and(|sub| self.git_subcommands.iter().any(|s| s == sub)))
            && self.argv_all.iter().all(has)
            && (self.argv_any.is_empty() || self.argv_any.iter().any(has))
            && (!needs_command || input.command.is_some())
            && self
                .command_all
                .iter()
                .all(|part| command.contains(part.as_str()))
            && (self.command_any.is_empty()
                || self
                    .command_any
                    .iter()
                    .any(|part| command.contains(part.as_str())))
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filters {
    /// Lines matching any of these are dropped.
    #[serde(default)]
    skip: Vec<String>,
    /// When given, only lines matching one of these are kept (after `skip`).
    #[serde(default)]
    keep: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transforms {
    #[serde(default)]
    pub strip_ansi: bool,
    #[serde(default)]
    pub dedupe_adjacent: bool,
    #[serde(default)]
    pub fold_blank_runs: bool,
    #[serde(default)]
    pub trim_empty_edges: bool,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Summarize {
    /// Leading lines kept when the output is longer than `head + tail`.
    #[serde(default)]
    pub head: usize,
    /// Trailing lines kept when the output is longer than `head + tail`.
    #[serde(default)]
    pub tail: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Counter {
    name: String,
    pattern: String,
    #[serde(default)]
    flags: String,
}

/// A rule ready to apply.
#[derive(Debug)]
pub(crate) struct Compiled {
    pub id: String,
    pub family: String,
    pub description: Option<String>,
    pub priority: i32,
    pub enabled: bool,
    pub origin: Origin,
    pub source: Option<PathBuf>,
    pub matching: Match,
    pub skip: Vec<Regex>,
    pub keep: Vec<Regex>,
    pub transforms: Transforms,
    pub summarize: Summarize,
    pub counters: Vec<(String, Regex)>,
}

pub(crate) fn builtin() -> Result<Vec<Compiled>, RulesError> {
    parse(BUILTIN, Origin::Builtin, None)
}

/// Read one rule file, refusing one too large to be a rule file.
pub(crate) fn read(path: &Path) -> Result<String, RulesError> {
    use std::io::Read;
    let read = |source| RulesError::Read {
        path: path.to_owned(),
        source,
    };
    let file = std::fs::File::open(path).map_err(read)?;
    let mut text = String::new();
    file.take(MAX_RULE_FILE + 1)
        .read_to_string(&mut text)
        .map_err(read)?;
    if text.len() as u64 > MAX_RULE_FILE {
        return Err(RulesError::TooLarge {
            path: path.to_owned(),
        });
    }
    Ok(text)
}

pub(crate) fn parse(
    text: &str,
    origin: Origin,
    source: Option<&Path>,
) -> Result<Vec<Compiled>, RulesError> {
    let path = || {
        source
            .map(Path::to_owned)
            .unwrap_or_else(|| "builtin.toml".into())
    };
    let file: File = toml::from_str(text).map_err(|source| RulesError::Parse {
        path: path(),
        source,
    })?;
    file.rule
        .into_iter()
        .map(|rule| {
            if rule.id.trim().is_empty() {
                return Err(RulesError::MissingId { path: path() });
            }
            if rule.id.len() > MAX_NAME
                || rule.counters.len() > MAX_COUNTERS
                || rule
                    .counters
                    .iter()
                    .any(|counter| counter.name.len() > MAX_NAME)
            {
                return Err(RulesError::TooLong { path: path() });
            }
            let invalid = |source| RulesError::Pattern {
                path: path(),
                id: rule.id.clone(),
                source,
            };
            let patterns = |patterns: &[String]| {
                patterns
                    .iter()
                    .map(|pattern| compile(pattern, ""))
                    .collect::<Result<Vec<_>, _>>()
            };
            let skip = patterns(&rule.filters.skip).map_err(invalid)?;
            let keep = patterns(&rule.filters.keep).map_err(invalid)?;
            let counters = rule
                .counters
                .iter()
                .map(|counter| {
                    compile(&counter.pattern, &counter.flags)
                        .map(|regex| (counter.name.clone(), regex))
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(invalid)?;
            Ok(Compiled {
                family: rule.family,
                description: rule.description,
                priority: rule.priority,
                enabled: rule.enabled,
                origin,
                source: source.map(Path::to_owned),
                matching: rule.matching,
                skip,
                keep,
                transforms: rule.transforms,
                summarize: rule.summarize,
                counters,
                id: rule.id,
            })
        })
        .collect()
}

fn compile(pattern: &str, flags: &str) -> Result<Regex, PatternError> {
    let mut builder = RegexBuilder::new(pattern);
    builder.size_limit(MAX_PATTERN_SIZE);
    for flag in flags.chars() {
        match flag {
            'i' => builder.case_insensitive(true),
            'm' => builder.multi_line(true),
            's' => builder.dot_matches_new_line(true),
            'x' => builder.ignore_whitespace(true),
            other => return Err(PatternError::Flag(other)),
        };
    }
    builder.build().map_err(|source| PatternError::Regex {
        pattern: pattern.to_owned(),
        source,
    })
}
