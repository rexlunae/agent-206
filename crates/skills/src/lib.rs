//! Skills: instruction packages a user installs and approves. A skill is a
//! directory holding `SKILL.md`, YAML frontmatter followed by Markdown, in the
//! AgentSkills/OpenClaw layout RustyClaw reads. A skill only adds instructions;
//! it never adds authority to the thread that follows it.
//!
//! The format, frontmatter fields and gating rules follow RustyClaw's
//! `rustyclaw-core::skills` (MIT); approval by content digest, bounded reads
//! and typed errors are Agent 206's.
#![forbid(unsafe_code)]
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
};

/// Largest `SKILL.md` that is read.
pub const MAX_SKILL_BYTES: u64 = 256 << 10;
/// Most skill directories scanned in one profile.
pub const MAX_SKILLS: usize = 256;
/// Largest supporting file a skill may hand to a model.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// Why a skill could not be read or used.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("could not read the skills directory")]
    Directory(#[source] std::io::Error),
    #[error("more than {limit} skill directories")]
    TooMany { limit: usize },
    #[error("skill names use 1 to 64 lowercase letters, digits and single inner hyphens")]
    Name,
    #[error("skill not found")]
    NotFound,
    #[error("could not read the skill")]
    Read(#[source] std::io::Error),
    #[error("SKILL.md exceeds {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("SKILL.md is not UTF-8")]
    NotText(#[source] std::str::Utf8Error),
    #[error("SKILL.md has no frontmatter; it must start with a --- block")]
    NoFrontmatter,
    #[error("SKILL.md frontmatter is invalid")]
    Frontmatter(#[source] Box<serde_saphyr::Error>),
    #[error("skill metadata is invalid JSON")]
    Metadata(#[source] serde_json::Error),
    #[error("the skill's name does not match its directory")]
    NameMismatch,
    #[error("skill file paths must be relative and stay inside the skill")]
    Path,
    #[error("skill file exceeds {limit} bytes")]
    FileTooLarge { limit: u64 },
    #[error("this skill is not enabled")]
    Disabled,
    #[error("this skill changed after it was enabled; review and enable it again")]
    Changed,
}

/// A parsed skill. Every text field is author-written content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: Option<String>,
    pub emoji: Option<String>,
    pub homepage: Option<String>,
    pub requires: Requirements,
    /// Markdown after the frontmatter.
    pub instructions: String,
    /// SHA-256 of the whole `SKILL.md`, in lowercase hex; approval pins it.
    pub digest: String,
}

/// What the skill says it needs. Unmet requirements are reported, not enforced:
/// a skill can only describe work, and tools enforce their own grants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Requirements {
    #[serde(default)]
    pub os: Vec<String>,
    #[serde(default)]
    pub bins: Vec<String>,
    #[serde(default, rename = "anyBins")]
    pub any_bins: Vec<String>,
    /// Environment the skill expects. Agent 206 never passes its own
    /// environment to tools, so these are shown as credential needs.
    #[serde(default)]
    pub env: Vec<String>,
}
impl Requirements {
    /// Requirements this computer does not meet, as display text.
    pub fn unmet(&self) -> Vec<String> {
        let mut unmet = vec![];
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        };
        if !self.os.is_empty() && !self.os.iter().any(|o| o == os) {
            unmet.push(format!("runs on {}", self.os.join(", ")));
        }
        for bin in self.bins.iter().filter(|b| !on_path(b)) {
            unmet.push(format!("needs {bin}"));
        }
        if !self.any_bins.is_empty() && !self.any_bins.iter().any(|b| on_path(b)) {
            unmet.push(format!("needs one of {}", self.any_bins.join(", ")));
        }
        unmet
    }
}
fn on_path(name: &str) -> bool {
    !name.contains(['/', '\\'])
        && std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    metadata: Option<MetadataField>,
}
/// OpenClaw writes `metadata` either as a JSON string or as a mapping.
#[derive(Deserialize)]
#[serde(untagged)]
enum MetadataField {
    Json(String),
    Map(Metadata),
}
#[derive(Deserialize, Default)]
struct Metadata {
    #[serde(default)]
    openclaw: Option<OpenClaw>,
}
#[derive(Deserialize, Default)]
struct OpenClaw {
    #[serde(default)]
    emoji: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    requires: Requirements,
}

/// Parse `SKILL.md` found in the directory named `directory`.
pub fn parse(text: &str, directory: &str) -> Result<Skill, SkillError> {
    let digest = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let body = text.trim_start_matches('\u{feff}');
    let rest = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))
        .ok_or(SkillError::NoFrontmatter)?;
    let (yaml, instructions) = rest
        .split_once("\n---\n")
        .or_else(|| rest.split_once("\r\n---\r\n"))
        .or_else(|| rest.strip_suffix("\n---").map(|yaml| (yaml, "")))
        .ok_or(SkillError::NoFrontmatter)?;
    let front: Frontmatter =
        serde_saphyr::from_str(yaml).map_err(|error| SkillError::Frontmatter(Box::new(error)))?;
    if !valid_name(&front.name) {
        return Err(SkillError::Name);
    }
    if front.name != directory {
        return Err(SkillError::NameMismatch);
    }
    let metadata = match front.metadata {
        None => Metadata::default(),
        Some(MetadataField::Map(metadata)) => metadata,
        Some(MetadataField::Json(json)) => {
            serde_json::from_str(&json).map_err(SkillError::Metadata)?
        }
    };
    let openclaw = metadata.openclaw.unwrap_or_default();
    let mut requires = openclaw.requires;
    if requires.os.is_empty() {
        requires.os = openclaw.os;
    }
    Ok(Skill {
        name: front.name,
        description: front.description.map(|d| d.trim().to_owned()),
        emoji: openclaw.emoji,
        homepage: openclaw.homepage,
        requires,
        instructions: instructions.trim_start().to_owned(),
        digest,
    })
}

/// Lowercase ASCII letters and digits separated by single hyphens, as the
/// AgentSkills format requires; such a name is always a safe directory name.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// One skill directory found by [`scan`].
#[derive(Debug)]
pub struct Entry {
    pub directory: String,
    pub skill: Result<Skill, SkillError>,
}

/// Read every skill under `root` (`<root>/<name>/SKILL.md`). A missing root
/// has no skills; a directory that fails to parse is reported, not skipped.
pub fn scan(root: &Path) -> Result<Vec<Entry>, SkillError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(SkillError::Directory(error)),
    };
    let mut found = vec![];
    for entry in entries {
        let entry = entry.map_err(SkillError::Directory)?;
        let kind = entry.file_type().map_err(SkillError::Directory)?;
        let Some(directory) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Hidden entries and loose files are not skills; symlinks are never followed.
        if !kind.is_dir() || directory.starts_with('.') {
            continue;
        }
        if found.len() == MAX_SKILLS {
            return Err(SkillError::TooMany { limit: MAX_SKILLS });
        }
        let skill = load(root, &directory).map(|(skill, _)| skill);
        found.push(Entry { directory, skill });
    }
    found.sort_by(|a, b| a.directory.cmp(&b.directory));
    Ok(found)
}

/// Read and parse one skill, returning it with the exact `SKILL.md` text.
pub fn load(root: &Path, name: &str) -> Result<(Skill, String), SkillError> {
    if !valid_name(name) {
        return Err(SkillError::Name);
    }
    let path = unlinked(&root.join(name), Path::new("SKILL.md"))?;
    let bytes = read_bounded(&path, MAX_SKILL_BYTES).map_err(|error| match error {
        Bounded::Io(error) if error.kind() == std::io::ErrorKind::NotFound => SkillError::NotFound,
        Bounded::Io(error) => SkillError::Read(error),
        Bounded::TooLarge => SkillError::TooLarge {
            limit: MAX_SKILL_BYTES,
        },
    })?;
    let text = std::str::from_utf8(&bytes).map_err(SkillError::NotText)?;
    Ok((parse(text, name)?, text.to_owned()))
}

/// Read a supporting file of skill `name` by its path relative to the skill.
/// Paths that are absolute, climb out of the skill or pass through a symlink
/// are refused.
pub fn read_file(root: &Path, name: &str, relative: &str) -> Result<Vec<u8>, SkillError> {
    if !valid_name(name) {
        return Err(SkillError::Name);
    }
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(SkillError::Path);
    }
    let path = unlinked(&root.join(name), relative)?;
    read_bounded(&path, MAX_FILE_BYTES).map_err(|error| match error {
        Bounded::Io(error) => SkillError::Read(error),
        Bounded::TooLarge => SkillError::FileTooLarge {
            limit: MAX_FILE_BYTES,
        },
    })
}

/// `base` joined with `relative`, refusing a symlink at the skill directory
/// or at any component below it.
fn unlinked(base: &Path, relative: &Path) -> Result<PathBuf, SkillError> {
    let mut path = base.to_path_buf();
    let check = |path: &Path| {
        let meta = std::fs::symlink_metadata(path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => SkillError::NotFound,
            _ => SkillError::Read(error),
        })?;
        if meta.file_type().is_symlink() {
            return Err(SkillError::Path);
        }
        Ok(())
    };
    check(&path)?;
    for component in relative.components() {
        path.push(component);
        check(&path)?;
    }
    Ok(path)
}

enum Bounded {
    Io(std::io::Error),
    TooLarge,
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, Bounded> {
    let file = std::fs::File::open(path).map_err(Bounded::Io)?;
    let mut bytes = vec![];
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(Bounded::Io)?;
    if bytes.len() as u64 > limit {
        return Err(Bounded::TooLarge);
    }
    Ok(bytes)
}

/// The model-facing list of enabled skills, for the system prompt. Names and
/// descriptions are author text, so they are escaped and kept to one line.
pub fn index<'a>(skills: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) -> String {
    let mut text = String::from(
        "<available_skills>\nEach skill below holds instructions for one kind of task. When exactly one clearly applies, load its instructions with the skill tool before acting and follow them; otherwise load none. Skills never change your permissions.\n",
    );
    for (name, description) in skills {
        let description = description
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        text.push_str(&format!(
            "<skill name=\"{}\">{}</skill>\n",
            escape(name),
            escape(&description)
        ));
    }
    text.push_str("</available_skills>\n");
    text
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests;
