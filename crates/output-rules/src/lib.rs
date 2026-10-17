//! Rule-driven compression of tool output before it reaches a model.
//!
//! A [`Rules`] set holds the rules shipped with the application
//! (`rules/builtin.toml`) and any the user adds as TOML files in one
//! directory. Each rule says which tool calls it applies to (by tool name,
//! the command's program and arguments, or substrings of the command line)
//! and how to reduce their output: strip terminal escapes, drop or keep lines
//! by pattern, fold blank runs, keep only the start and end of very long
//! output, and count lines of interest. The highest-priority matching rule
//! applies; at equal priority a user rule wins. A user rule with the id of a
//! shipped rule replaces it, and `enabled = false` turns a rule off.
//!
//! A rule never adds information and always says what it did: the
//! reduced text ends with a line naming the rule and how many lines it kept.
//!
//! The rule model and the shipped rules are adapted from RustyClaw's
//! `tokenjuice` crate (MIT, see NOTICE); rules here are TOML.
use std::path::{Path, PathBuf};

mod command;
mod reduce;
mod rule;

pub use command::program;
pub use rule::{MAX_COUNTERS, MAX_NAME, MAX_SUMMARY, Origin, PatternError, RulesError};

/// Largest rule file read, in bytes.
pub const MAX_RULE_FILE: u64 = 256 * 1024;
/// Most user rules, across all files.
pub const MAX_RULES: usize = 256;

/// One tool result to reduce.
#[derive(Debug, Clone, Copy)]
pub struct Input<'a> {
    /// The model-facing tool name.
    pub tool: &'a str,
    /// The shell command line the tool ran, when it ran one.
    pub command: Option<&'a str>,
    /// The tool's result text.
    pub output: &'a str,
}

/// Output a rule reduced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reduced {
    /// The reduced text, ending with a line that names the rule.
    pub text: String,
    /// The id of the rule that applied.
    pub rule: String,
    pub origin: Origin,
    /// The rule's counters over the lines the model is shown, in rule order.
    pub counts: Vec<Count>,
}

/// A named count of kept lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Count {
    pub name: String,
    pub count: usize,
}

/// A rule as listed for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleInfo<'a> {
    pub id: &'a str,
    pub family: &'a str,
    pub description: Option<&'a str>,
    pub priority: i32,
    pub origin: Origin,
    /// The file a user rule came from.
    pub source: Option<&'a Path>,
}

/// The rules in effect, highest priority first.
#[derive(Debug)]
pub struct Rules {
    rules: Vec<rule::Compiled>,
}

impl Rules {
    /// Only the rules shipped with the application.
    pub fn builtin() -> Self {
        // The shipped file is part of the source; the crate's tests compile
        // it, so failing here is a programmer error.
        Self::assemble(
            rule::builtin().expect("shipped output rules are valid"),
            vec![],
        )
        .expect("shipped output rules are valid")
    }

    /// The shipped rules overlaid with every `*.toml` file directly in
    /// `dir`, read in name order. A missing directory adds nothing.
    pub fn load(dir: &Path) -> Result<Self, RulesError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::builtin());
            }
            Err(source) => {
                return Err(RulesError::Directory {
                    path: dir.to_owned(),
                    source,
                });
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| RulesError::Directory {
                path: dir.to_owned(),
                source,
            })?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "toml")
            {
                paths.push(path);
            }
        }
        paths.sort();
        let files = paths
            .into_iter()
            .map(|path| rule::read(&path).map(|text| (path, text)))
            .collect::<Result<Vec<_>, _>>()?;
        Self::with_user_files(files)
    }

    /// The shipped rules overlaid with rule files already read, as
    /// `(path, text)` in the order they apply.
    pub fn with_user_files(files: Vec<(PathBuf, String)>) -> Result<Self, RulesError> {
        let builtin = rule::builtin().expect("shipped output rules are valid");
        let mut user = Vec::new();
        for (path, text) in files {
            user.extend(rule::parse(&text, Origin::User, Some(&path))?);
        }
        Self::assemble(builtin, user)
    }

    fn assemble(
        builtin: Vec<rule::Compiled>,
        user: Vec<rule::Compiled>,
    ) -> Result<Self, RulesError> {
        if user.len() > MAX_RULES {
            return Err(RulesError::TooMany);
        }
        for (index, rule) in user.iter().enumerate() {
            if let Some(first) = user[..index].iter().find(|other| other.id == rule.id) {
                return Err(RulesError::Duplicate {
                    id: rule.id.clone(),
                    first: first.source.clone().unwrap_or_default(),
                    second: rule.source.clone().unwrap_or_default(),
                });
            }
        }
        let mut rules: Vec<_> = builtin
            .into_iter()
            .filter(|shipped| user.iter().all(|rule| rule.id != shipped.id))
            .collect::<Vec<_>>()
            .into_iter()
            .chain(user)
            .filter(|rule| rule.enabled)
            .collect();
        // Stable: equal rules keep file order.
        rules.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| b.origin.cmp(&a.origin))
        });
        Ok(Self { rules })
    }

    /// The enabled rules, in the order they are tried.
    pub fn rules(&self) -> impl Iterator<Item = RuleInfo<'_>> {
        self.rules.iter().map(|rule| RuleInfo {
            id: &rule.id,
            family: &rule.family,
            description: rule.description.as_deref(),
            priority: rule.priority,
            origin: rule.origin,
            source: rule.source.as_deref(),
        })
    }

    /// The output reduced by the first rule that matches `input`, or `None`
    /// when no rule matches or the rule leaves the output as it was.
    pub fn reduce(&self, input: &Input<'_>) -> Option<Reduced> {
        let argv = input.command.map(program).unwrap_or_default();
        let rule = self
            .rules
            .iter()
            .find(|rule| rule.matching.matches(input, &argv))?;
        let (text, counts) = reduce::reduce(rule, input.output)?;
        Some(Reduced {
            text,
            rule: rule.id.clone(),
            origin: rule.origin,
            counts,
        })
    }
}

#[cfg(test)]
mod tests;
