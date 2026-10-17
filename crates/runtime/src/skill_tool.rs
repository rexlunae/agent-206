//! Skills for the native run engine: a [`ToolHost`] offering `skill_list` and
//! `skill_info`, as RustyClaw named them.
//!
//! Only skills the user approved, unchanged since that approval, are listed
//! or read; every call re-checks the pin against the file as it is now. The
//! tools declare no capability: a skill adds instructions, never authority,
//! and the model still works within its conversation's grant. Approval stays
//! a user operation (`SetSkillEnabled`); nothing here can enable a skill, and
//! the host reads the profile's pins without being able to change them.
//! Each conversation still denies both tools until the user permits them.
use crate::{
    store::Store,
    tools::{self, ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{ErrorKind, ErrorReport, ToolPermission, UnavailableTools};
use a206_skills::{Skill, SkillError};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Lists the approved skills.
pub const LIST: &str = "skill_list";
/// Reads one approved skill's instructions or a supporting file.
pub const INFO: &str = "skill_info";
/// The source named when skills cannot be listed.
pub const SOURCE: &str = "skills";

/// An approved skill the run engine may list for a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillOffer {
    pub name: String,
    pub description: Option<String>,
}

/// Why a skill tool call failed.
#[derive(Debug, thiserror::Error)]
pub enum SkillToolError {
    #[error(transparent)]
    Skill(#[from] SkillError),
    #[error("the gateway's settings are unavailable")]
    Poisoned,
    #[error("`{0}` must be a string")]
    NotAString(&'static str),
    #[error("`name` is required")]
    MissingName,
    #[error("unknown argument `{0}`")]
    Unknown(String),
    #[error("unknown skill tool `{0}`")]
    UnknownTool(String),
    #[error("the skill file is not UTF-8 text")]
    NotText(#[source] std::string::FromUtf8Error),
    #[error("the result exceeds the {limit}-byte tool output limit")]
    TooLarge { limit: usize },
    #[error("the skill reader stopped unexpectedly")]
    Worker(#[source] tokio::task::JoinError),
}
impl crate::error::Classified for SkillToolError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Skill(error) => return crate::error::classify(error),
            Self::Poisoned | Self::Worker(_) => ErrorKind::Internal,
            Self::NotAString(_)
            | Self::MissingName
            | Self::Unknown(_)
            | Self::NotText(_)
            | Self::TooLarge { .. } => ErrorKind::InvalidRequest,
            Self::UnknownTool(_) => ErrorKind::NotFound,
        })
    }
}

/// `name` loaded from `root`, only if the user approved exactly this version.
pub(crate) fn approved(
    root: &Path,
    pins: &BTreeMap<String, String>,
    name: &str,
) -> Result<Skill, SkillError> {
    let pinned = pins.get(name).ok_or(SkillError::Disabled)?;
    let (skill, _) = a206_skills::load(root, name)?;
    if &skill.digest != pinned {
        return Err(SkillError::Changed);
    }
    Ok(skill)
}

/// Approved, unchanged skills under `root`. Skills that changed or no longer
/// parse are left out; the Skills page shows why.
pub(crate) fn offers(
    root: &Path,
    pins: &BTreeMap<String, String>,
) -> Result<Vec<SkillOffer>, SkillError> {
    if pins.is_empty() {
        return Ok(vec![]);
    }
    Ok(a206_skills::scan(root)?
        .into_iter()
        .filter_map(|entry| entry.skill.ok())
        .filter(|skill| pins.get(&skill.name) == Some(&skill.digest))
        .map(|skill| SkillOffer {
            name: skill.name,
            description: skill.description,
        })
        .collect())
}

/// Offers approved skills to native runs.
pub struct SkillToolHost {
    source: Arc<Source>,
}
/// Where skills and their approvals are read from.
struct Source {
    store: Arc<Mutex<Store>>,
    root: PathBuf,
}
impl SkillToolHost {
    /// Skills under `<profile>/skills`, approved by the pins in `store`.
    pub fn new(store: Arc<Mutex<Store>>, profile: &Path) -> Self {
        Self {
            source: Arc::new(Source {
                store,
                root: profile.join("skills"),
            }),
        }
    }
    /// Run `read` on a blocking thread: skills are files, and a slow file
    /// system must not stall the gateway's async workers.
    ///
    /// Dropping the call detaches the read deliberately: blocking file I/O
    /// cannot be interrupted, and a read changes nothing, holds the settings
    /// lock only to copy the pins, is bounded by the skill crate's size
    /// limits, and its result is discarded. The call itself ends at once.
    async fn read<T: Send + 'static>(
        &self,
        read: impl FnOnce(&Source) -> Result<T, SkillToolError> + Send + 'static,
    ) -> Result<T, SkillToolError> {
        let source = self.source.clone();
        tokio::task::spawn_blocking(move || read(&source))
            .await
            .map_err(SkillToolError::Worker)?
    }
}
impl Source {
    fn pins(&self) -> Result<BTreeMap<String, String>, SkillToolError> {
        Ok(self
            .store
            .lock()
            .map_err(|_| SkillToolError::Poisoned)?
            .state
            .settings
            .enabled_skills
            .clone())
    }
}
fn specs() -> Vec<ToolSpec> {
    vec![
            ToolSpec {
                name: LIST.into(),
                description: "List the skills the user approved. A skill holds instructions for one kind of task; when exactly one clearly applies, read it with skill_info before acting and follow it. Skills never change your permissions.".into(),
                parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
                capabilities: Default::default(),
                permission: ToolPermission::Allow,
            },
            ToolSpec {
                name: INFO.into(),
                description: "Read an approved skill's instructions, or with `path` one of its supporting files.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "The skill's name, as skill_list shows it."},
                        "path": {"type": "string", "description": "A supporting file, relative to the skill's folder."}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
                capabilities: Default::default(),
                permission: ToolPermission::Allow,
            },
        ]
}
/// A cut list or skill would be taken as whole, so a result the run engine
/// would truncate is refused instead.
fn whole(text: String) -> Result<String, SkillToolError> {
    if text.len() > tools::MAX_OUTPUT {
        return Err(SkillToolError::TooLarge {
            limit: tools::MAX_OUTPUT,
        });
    }
    Ok(text)
}
impl Source {
    fn run(&self, request: &ToolRequest) -> Result<String, SkillToolError> {
        let pins = self.pins()?;
        match request.name.as_str() {
            LIST => {
                check_keys(&request.arguments, &[])?;
                let offers = offers(&self.root, &pins)?;
                whole(a206_skills::index(offers.iter().map(|offer| {
                    (offer.name.as_str(), offer.description.as_deref())
                })))
            }
            INFO => {
                check_keys(&request.arguments, &["name", "path"])?;
                let name = text(&request.arguments, "name")?.ok_or(SkillToolError::MissingName)?;
                let skill = approved(&self.root, &pins, &name)?;
                let text = match text(&request.arguments, "path")? {
                    None => skill.instructions,
                    Some(path) => {
                        String::from_utf8(a206_skills::read_file(&self.root, &name, &path)?)
                            .map_err(SkillToolError::NotText)?
                    }
                };
                whole(text)
            }
            other => Err(SkillToolError::UnknownTool(other.into())),
        }
    }
}
fn check_keys(arguments: &Map<String, Value>, known: &[&str]) -> Result<(), SkillToolError> {
    match arguments.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(SkillToolError::Unknown(key.clone())),
        None => Ok(()),
    }
}
fn text(
    arguments: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<String>, SkillToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(SkillToolError::NotAString(key)),
    }
}
impl ToolHost for SkillToolHost {
    /// The tools are offered only while at least one approved skill exists.
    fn list<'a>(&'a self, _scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            let offers = self
                .read(|source| Ok(offers(&source.root, &source.pins()?)?))
                .await;
            match offers {
                Ok(offers) if offers.is_empty() => ToolListing::default(),
                Ok(_) => ToolListing {
                    tools: specs(),
                    unavailable: vec![],
                },
                Err(error) => ToolListing {
                    tools: vec![],
                    unavailable: vec![UnavailableTools {
                        source: SOURCE.into(),
                        error: crate::error::handle("list_skills", &error),
                    }],
                },
            }
        })
    }
    fn call<'a>(
        &'a self,
        _scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            self.read(move |source| source.run(&request))
                .await
                .map(|text| ToolOutput {
                    text,
                    is_error: false,
                })
                .map_err(|error| crate::error::report(&error))
        })
    }
}

#[cfg(test)]
mod tests;
