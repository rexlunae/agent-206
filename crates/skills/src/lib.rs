//! Skills: instruction packages a user installs and approves. A skill is a
//! directory holding `SKILL.md`, YAML frontmatter followed by Markdown, in the
//! AgentSkills/OpenClaw layout RustyClaw reads. A skill only adds instructions;
//! it never adds authority to the thread that follows it.
//!
//! The format, frontmatter fields and gating rules follow RustyClaw's
//! `rustyclaw-core::skills` (MIT); approval by content digest, bounded reads
//! and typed errors are Agent 206's.
#![forbid(unsafe_code)]
use a206_core::AuditReport;
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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
/// Where a project keeps its skills, below its root.
pub const PROJECT_SKILLS: [&str; 2] = [".agent206", "skills"];

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
    #[error("skill packages allow at most 128 entries, 16 directory levels and 2 MiB total")]
    PackageLimit,
    #[error("approve a skill by the digest of the text you reviewed")]
    Unreviewed,
    #[error("this skill is not enabled")]
    Disabled,
    #[error("this skill changed after it was enabled; review and enable it again")]
    Changed,
    #[error("the project's `{component}` directory is a symbolic link, which is not followed")]
    Linked { component: &'static str },
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
    /// Package content digest in lowercase hex; approval pins it.
    pub digest: String,
    /// What the pre-install audit found in the package. Advice for
    /// the user's review; it neither approves nor blocks the skill.
    pub audit: AuditReport,
    /// Supporting files from the same bounded snapshot as the approved digest.
    pub files: BTreeMap<String, Vec<u8>>,
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
    std::env::var_os("PATH").is_some_and(|path| in_search_path(name, &path))
}
fn in_search_path(name: &str, path: &std::ffi::OsStr) -> bool {
    #[cfg(windows)]
    let extensions = std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    #[cfg(windows)]
    let extensions = Some(extensions.as_os_str());
    #[cfg(not(windows))]
    let extensions = None;
    search_path(name, path, extensions)
}
fn search_path(name: &str, path: &std::ffi::OsStr, extensions: Option<&std::ffi::OsStr>) -> bool {
    !name.contains(['/', '\\'])
        && std::env::split_paths(path).any(|dir| {
            executable(&dir.join(name))
                || extensions.is_some_and(|extensions| {
                    extensions
                        .to_string_lossy()
                        .split(';')
                        .map(str::trim)
                        .filter(|extension| {
                            extension.starts_with('.') && !extension.contains(['/', '\\'])
                        })
                        .any(|extension| executable(&dir.join(format!("{name}{extension}"))))
                })
        })
}
/// A regular file this process may run; Windows has no execute bit.
fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| {
        #[cfg(unix)]
        let runnable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
        #[cfg(not(unix))]
        let runnable = true;
        meta.is_file() && runnable
    })
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
        instructions: without_leading_blank_lines(instructions).to_owned(),
        digest,
        audit: a206_audit::text(text),
        files: BTreeMap::new(),
    })
}

/// `text` from its first line with content, keeping that line's indentation
/// (an indented first line is a Markdown code block).
fn without_leading_blank_lines(text: &str) -> &str {
    let mut rest = text;
    while let Some((line, next)) = rest.split_once('\n') {
        if !line.trim().is_empty() {
            break;
        }
        rest = next;
    }
    if rest.trim().is_empty() { "" } else { rest }
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

/// A directory of skill folders: the gateway profile's `skills` directory, or
/// a project's `.agent206/skills`. A project's `.agent206` and `skills`
/// directories are opened without following symlinks, so a project cannot
/// point its skills anywhere else; every skill folder and file below either
/// root is opened the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsDir {
    base: PathBuf,
    project: bool,
}

impl SkillsDir {
    /// The skills directory at `root`, such as `<profile>/skills`.
    pub fn profile(root: impl Into<PathBuf>) -> Self {
        Self {
            base: root.into(),
            project: false,
        }
    }
    /// The skills of the project at `root`: `<root>/.agent206/skills`.
    pub fn project(root: impl Into<PathBuf>) -> Self {
        Self {
            base: root.into(),
            project: true,
        }
    }
    /// The directory's path, for display.
    pub fn path(&self) -> PathBuf {
        let mut path = self.base.clone();
        if self.project {
            path.extend(PROJECT_SKILLS);
        }
        path
    }
    /// The open directory, or `None` when it does not exist.
    fn open(&self) -> Result<Option<Dir>, SkillError> {
        let missing = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
        let mut dir = match Dir::open_ambient_dir(&self.base, cap_std::ambient_authority()) {
            Ok(dir) => dir,
            Err(error) if missing(&error) => return Ok(None),
            Err(error) => return Err(SkillError::Directory(error)),
        };
        if self.project {
            for component in PROJECT_SKILLS {
                // A typed refusal first; opening without following a link
                // also refuses one swapped in after this check.
                match dir.symlink_metadata(component) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(SkillError::Linked { component });
                    }
                    Ok(_) => {}
                    Err(error) if missing(&error) => return Ok(None),
                    Err(error) => return Err(SkillError::Directory(error)),
                }
                dir = dir
                    .open_dir_nofollow(component)
                    .map_err(SkillError::Directory)?;
            }
        }
        Ok(Some(dir))
    }

    /// Read every skill's metadata and audit. Supporting bytes are discarded
    /// after auditing; use [`SkillsDir::load`] for a complete review snapshot.
    /// A missing directory has no skills; a parse failure is reported, not
    /// skipped.
    pub fn scan(&self) -> Result<Vec<Entry>, SkillError> {
        let Some(root) = self.open()? else {
            return Ok(vec![]);
        };
        let mut found = vec![];
        for entry in root.entries().map_err(SkillError::Directory)? {
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
            let skill = load_in(&root, &directory).map(|(mut skill, _)| {
                // Listings must not retain up to 256 complete 2-MiB packages.
                skill.files.clear();
                skill
            });
            found.push(Entry { directory, skill });
        }
        found.sort_by(|a, b| a.directory.cmp(&b.directory));
        Ok(found)
    }

    /// Read and parse one skill, returning it with the exact `SKILL.md` text.
    pub fn load(&self, name: &str) -> Result<(Skill, String), SkillError> {
        if !valid_name(name) {
            return Err(SkillError::Name);
        }
        load_in(&self.open()?.ok_or(SkillError::NotFound)?, name)
    }

    /// Read a supporting file of skill `name` by its path relative to the
    /// skill. Paths that are absolute, climb out of the skill or pass through
    /// a symlink are refused.
    pub fn read_file(&self, name: &str, relative: &str) -> Result<Vec<u8>, SkillError> {
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
        let root = self.open()?.ok_or(SkillError::NotFound)?;
        unlinked(&root, name, relative)?;
        read_bounded(&root, name, relative, MAX_FILE_BYTES).map_err(|error| match error {
            Bounded::Io(error) => SkillError::Read(error),
            Bounded::TooLarge => SkillError::FileTooLarge {
                limit: MAX_FILE_BYTES,
            },
        })
    }

    /// Serve bytes from the package snapshot whose digest the user approved.
    /// Never re-open a supporting file after checking a different snapshot.
    pub fn read_approved_file(
        &self,
        name: &str,
        relative: &str,
        digest: &str,
    ) -> Result<Vec<u8>, SkillError> {
        if relative.is_empty()
            || relative.contains('\\')
            || !Path::new(relative)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(SkillError::Path);
        }
        // std::path accepts internal `.` and redundant separators. Use those
        // validated components for the snapshot key, not the original spelling.
        let key = Path::new(relative)
            .components()
            .map(|component| component.as_os_str().to_str().ok_or(SkillError::Path))
            .collect::<Result<Vec<_>, _>>()?
            .join("/");
        let (mut skill, text) = self.load(name)?;
        if skill.digest != digest {
            return Err(SkillError::Changed);
        }
        if key == "SKILL.md" {
            return Ok(text.into_bytes());
        }
        skill.files.remove(&key).ok_or(SkillError::NotFound)
    }
}

/// One skill directory found by [`scan`].
#[derive(Debug)]
pub struct Entry {
    pub directory: String,
    pub skill: Result<Skill, SkillError>,
}

/// [`SkillsDir::scan`] of the profile skills directory `root`.
pub fn scan(root: &Path) -> Result<Vec<Entry>, SkillError> {
    SkillsDir::profile(root).scan()
}

/// [`SkillsDir::load`] from the profile skills directory `root`.
pub fn load(root: &Path, name: &str) -> Result<(Skill, String), SkillError> {
    SkillsDir::profile(root).load(name)
}

/// [`SkillsDir::read_file`] from the profile skills directory `root`.
pub fn read_file(root: &Path, name: &str, relative: &str) -> Result<Vec<u8>, SkillError> {
    SkillsDir::profile(root).read_file(name, relative)
}

/// [`SkillsDir::read_approved_file`] from the profile skills directory `root`.
pub fn read_approved_file(
    root: &Path,
    name: &str,
    relative: &str,
    digest: &str,
) -> Result<Vec<u8>, SkillError> {
    SkillsDir::profile(root).read_approved_file(name, relative, digest)
}

fn load_in(root: &Dir, name: &str) -> Result<(Skill, String), SkillError> {
    if !valid_name(name) {
        return Err(SkillError::Name);
    }
    let relative = Path::new("SKILL.md");
    unlinked(root, name, relative)?;
    let bytes =
        read_bounded(root, name, relative, MAX_SKILL_BYTES).map_err(|error| match error {
            Bounded::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
                SkillError::NotFound
            }
            Bounded::Io(error) => SkillError::Read(error),
            Bounded::TooLarge => SkillError::TooLarge {
                limit: MAX_SKILL_BYTES,
            },
        })?;
    let text = std::str::from_utf8(&bytes).map_err(SkillError::NotText)?;
    let mut skill = parse(text, name)?;
    package::inspect(root, name, &bytes, &mut skill)?;
    Ok((skill, text.to_owned()))
}

/// Refuse a symlink at the skill directory or any component of `relative`
/// below it, with a typed error. [`read_bounded`] opens the file again without
/// following symlinks, so a link swapped in after this check fails to open.
fn unlinked(root: &Dir, name: &str, relative: &Path) -> Result<(), SkillError> {
    let fail = |error: std::io::Error| match error.kind() {
        std::io::ErrorKind::NotFound => SkillError::NotFound,
        _ => SkillError::Read(error),
    };
    let check = |dir: &Dir, entry: &Path| {
        let meta = dir.symlink_metadata(entry).map_err(fail)?;
        if meta.file_type().is_symlink() {
            return Err(SkillError::Path);
        }
        Ok(())
    };
    check(root, Path::new(name))?;
    let mut dir = root.open_dir_nofollow(name).map_err(fail)?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        check(&dir, component.as_ref())?;
        if components.peek().is_some() {
            dir = dir.open_dir_nofollow(component).map_err(fail)?;
        }
    }
    Ok(())
}

enum Bounded {
    Io(std::io::Error),
    TooLarge,
}
/// Read `<root>/<name>/<relative>` without following a symlink at any
/// component, so nothing outside the skill can be read even if the tree
/// changes while it is read.
fn read_bounded(root: &Dir, name: &str, relative: &Path, limit: u64) -> Result<Vec<u8>, Bounded> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
    let open = || -> std::io::Result<std::fs::File> {
        let mut dir = root.open_dir_nofollow(name)?;
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                let mut options = cap_std::fs::OpenOptions::new();
                options.read(true).follow(FollowSymlinks::No).nonblock(true);
                return Ok(dir.open_with(component, &options)?.into_std());
            }
            dir = dir.open_dir_nofollow(component)?;
        }
        Err(std::io::ErrorKind::InvalidInput.into())
    };
    let file = open().map_err(Bounded::Io)?;
    if !file.metadata().map_err(Bounded::Io)?.is_file() {
        return Err(Bounded::Io(std::io::ErrorKind::InvalidInput.into()));
    }
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

mod package;
#[cfg(test)]
mod tests;
