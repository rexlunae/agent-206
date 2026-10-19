//! Legacy model-facing skill names, backed by the bundled WASM extension.
//! Approval pins remain gateway-owned; the guest has no filesystem or settings
//! access. Reads use the same worker timeout and policy checks as other builtins.
use crate::{
    extension_host::{ExtensionHost, Scope},
    store::Store,
    tools::{self, ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    Capability, ErrorKind, ErrorReport, Id, Settings, ToolPermission, UnavailableTools,
};
use a206_skills::{Skill, SkillError, SkillsDir};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const LIST: &str = "skill_list";
pub const INFO: &str = "skill_info";
pub const SOURCE: &str = "skills";
pub const EXTENSION: &str = "agent206.skills";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillOffer {
    pub name: String,
    pub description: Option<String>,
}
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
    #[error("the bundled skill manifest is invalid")]
    Manifest(#[source] a206_extensions::ManifestError),
    #[error("the skill read worker failed")]
    Worker(#[source] tokio::task::JoinError),
    #[error("the skill read queue is unavailable")]
    Queue(#[source] tokio::sync::AcquireError),
    #[error("the skill read timed out")]
    Timeout(#[source] tokio::time::error::Elapsed),
}
impl crate::error::Classified for SkillToolError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Skill(error) => return crate::error::classify(error),
            Self::Poisoned | Self::Manifest(_) | Self::Worker(_) | Self::Queue(_) => {
                ErrorKind::Internal
            }
            Self::Timeout(_) => ErrorKind::Timeout,
            Self::NotAString(_)
            | Self::MissingName
            | Self::Unknown(_)
            | Self::NotText(_)
            | Self::TooLarge { .. } => ErrorKind::InvalidRequest,
            Self::UnknownTool(_) => ErrorKind::NotFound,
        })
    }
}
/// The model-facing name of a project skill starts with this prefix. Profile
/// and project skills never shadow each other: `notes` is always the profile
/// skill and `project:notes` the conversation's project skill of that name.
pub const PROJECT_PREFIX: &str = "project:";

/// The project whose approved skills a conversation may read: set only for
/// a conversation in a project whose effective grant includes `ReadWorkspace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSkills {
    pub project: Id,
    /// The project root; its skills are in `.agent206/skills`.
    pub root: PathBuf,
}

/// The project skills conversation `thread` may read, if any. A missing
/// thread or project has none: project skills are offered only when the
/// conversation is in the project and may read it, as for its instruction
/// files ([`crate::instructions::permitted`]).
pub(crate) fn project_scope(
    store: &Store,
    thread: Id,
    capabilities: &BTreeSet<Capability>,
) -> Option<ProjectSkills> {
    if !crate::instructions::permitted(capabilities) {
        return None;
    }
    let project = store.thread(thread).ok()?.project?;
    let root = &store.state.projects.iter().find(|p| p.id == project)?.root;
    Some(ProjectSkills {
        project,
        root: PathBuf::from(root),
    })
}

/// The skill folders one read uses and their approval pins, taken together
/// from the settings so a read checks one consistent approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sources {
    profile: SkillsDir,
    pins: BTreeMap<String, String>,
    project: Option<(SkillsDir, BTreeMap<String, String>)>,
}
impl Sources {
    pub(crate) fn new(
        settings: &Settings,
        profile: &Path,
        project: Option<&ProjectSkills>,
    ) -> Self {
        Self {
            profile: SkillsDir::profile(profile),
            pins: settings.enabled_skills.clone(),
            project: project.map(|project| {
                (
                    SkillsDir::project(&project.root),
                    settings
                        .enabled_project_skills
                        .get(&project.project)
                        .cloned()
                        .unwrap_or_default(),
                )
            }),
        }
    }
    /// The folder, pins and plain name a model-facing name refers to.
    fn locate<'a>(
        &'a self,
        name: &'a str,
    ) -> Result<(&'a SkillsDir, &'a BTreeMap<String, String>, &'a str), SkillError> {
        match name.strip_prefix(PROJECT_PREFIX) {
            None => Ok((&self.profile, &self.pins, name)),
            Some(name) => match &self.project {
                Some((dir, pins)) => Ok((dir, pins, name)),
                // Not in a project the conversation may read.
                None => Err(SkillError::Disabled),
            },
        }
    }
}

/// Load a skill only if the user approved exactly this version of it.
pub(crate) fn approved(sources: &Sources, name: &str) -> Result<Skill, SkillError> {
    let (dir, pins, name) = sources.locate(name)?;
    let pinned = pins.get(name).ok_or(SkillError::Disabled)?;
    let (skill, _) = dir.load(name)?;
    if &skill.digest != pinned {
        return Err(SkillError::Changed);
    }
    Ok(skill)
}
/// A supporting file of an approved skill, from the approved snapshot.
pub(crate) fn approved_file(
    sources: &Sources,
    name: &str,
    path: &str,
) -> Result<Vec<u8>, SkillError> {
    let (dir, pins, name) = sources.locate(name)?;
    dir.read_approved_file(name, path, pins.get(name).ok_or(SkillError::Disabled)?)
}
/// Approved, unchanged skills: the profile's, then the project's under
/// [`PROJECT_PREFIX`].
pub(crate) fn offers(sources: &Sources) -> Result<Vec<SkillOffer>, SkillError> {
    // Scan even without pins: a broken directory is unavailable, not empty.
    let mut offers = approved_offers(&sources.profile, &sources.pins, "")?;
    // A project's folder is the project's business until the user approves
    // one of its skills; only then is it read for the conversation.
    if let Some((dir, pins)) = &sources.project
        && !pins.is_empty()
    {
        offers.extend(approved_offers(dir, pins, PROJECT_PREFIX)?);
    }
    Ok(offers)
}
fn approved_offers(
    dir: &SkillsDir,
    pins: &BTreeMap<String, String>,
    prefix: &str,
) -> Result<Vec<SkillOffer>, SkillError> {
    Ok(dir
        .scan()?
        .into_iter()
        .filter_map(|entry| entry.skill.ok())
        .filter(|skill| pins.get(&skill.name) == Some(&skill.digest))
        .map(|skill| SkillOffer {
            name: format!("{prefix}{}", skill.name),
            description: skill.description,
        })
        .collect())
}
/// Complete text only; neither the index nor instructions may be truncated.
pub(crate) fn read(sources: &Sources, request: &ToolRequest) -> Result<String, SkillToolError> {
    let result = match request.name.as_str() {
        LIST => {
            check_keys(&request.arguments, &[])?;
            let offers = offers(sources)?;
            a206_skills::index(
                offers
                    .iter()
                    .map(|offer| (offer.name.as_str(), offer.description.as_deref())),
            )
        }
        INFO => {
            check_keys(&request.arguments, &["name", "path"])?;
            let name = text(&request.arguments, "name")?.ok_or(SkillToolError::MissingName)?;
            match text(&request.arguments, "path")? {
                None => approved(sources, &name)?.instructions,
                Some(path) => String::from_utf8(approved_file(sources, &name, &path)?)
                    .map_err(SkillToolError::NotText)?,
            }
        }
        other => return Err(SkillToolError::UnknownTool(other.into())),
    };
    if result.len() > tools::MAX_OUTPUT {
        return Err(SkillToolError::TooLarge {
            limit: tools::MAX_OUTPUT,
        });
    }
    Ok(result)
}
/// A stalled filesystem syscall cannot be interrupted by Rust. Keep it off
/// Tokio's executor and bound outstanding reads to two, including after a
/// caller cancels. A cancelled/expired caller discards the result; its WASM
/// worker is killed by ExtensionHost. The permit remains with the OS read.
pub(crate) async fn io<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, SkillToolError> + Send + 'static,
) -> Result<T, SkillToolError> {
    static READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    tokio::time::timeout(crate::extension_host::TIMEOUT, async move {
        let permit = READS.acquire().await.map_err(SkillToolError::Queue)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(SkillToolError::Worker)?
    })
    .await
    .map_err(SkillToolError::Timeout)?
}
pub struct SkillToolHost {
    store: Arc<Mutex<Store>>,
    root: PathBuf,
    extensions: ExtensionHost,
}
impl SkillToolHost {
    pub fn new(store: Arc<Mutex<Store>>, profile: &Path) -> Self {
        Self {
            extensions: ExtensionHost::new(store.clone(), profile.to_owned()),
            store,
            root: profile.join("skills"),
        }
    }
    /// The project skills `scope`'s conversation may read.
    fn project(&self, scope: &ToolScope) -> Result<Option<ProjectSkills>, SkillToolError> {
        let store = self.store.lock().map_err(|_| SkillToolError::Poisoned)?;
        Ok(project_scope(&store, scope.thread, &scope.capabilities))
    }
    fn sources(&self, scope: &ToolScope) -> Result<Sources, SkillToolError> {
        let store = self.store.lock().map_err(|_| SkillToolError::Poisoned)?;
        let project = project_scope(&store, scope.thread, &scope.capabilities);
        Ok(Sources::new(
            &store.state.settings,
            &self.root,
            project.as_ref(),
        ))
    }
    fn enabled(&self) -> Result<bool, SkillToolError> {
        Ok(self
            .store
            .lock()
            .map_err(|_| SkillToolError::Poisoned)?
            .state
            .plugins
            .iter()
            .any(|p| p.id == EXTENSION && p.enabled))
    }
    fn specs() -> Result<Vec<ToolSpec>, SkillToolError> {
        let builtin = a206_extensions::builtin(EXTENSION)
            .map_err(SkillToolError::Manifest)?
            .ok_or_else(|| SkillToolError::UnknownTool(EXTENSION.into()))?;
        Ok(builtin
            .manifest
            .tools
            .into_iter()
            .map(|spec| ToolSpec {
                parameters: crate::extension_tools::parameters_schema(&spec),
                name: spec.name,
                description: spec.description,
                capabilities: spec.capabilities,
                permission: ToolPermission::Allow,
                // Skill instructions are atomic model input.
                whole_output: true,
            })
            .collect())
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
    fn list<'a>(&'a self, scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            let load = async {
                if !self.enabled()? {
                    return Ok(vec![]);
                }
                let sources = self.sources(scope)?;
                let available = io(move || Ok(offers(&sources)?)).await?;
                if available.is_empty() {
                    Ok(vec![])
                } else {
                    Self::specs()
                }
            };
            match load.await {
                Ok(tools) => ToolListing {
                    tools,
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
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            let args = crate::extension_tools::arguments(request.arguments)
                .map_err(|e| crate::error::report(&e))?;
            let project_skills = self.project(scope).map_err(|e| crate::error::report(&e))?;
            let output = self
                .extensions
                .call_tool(
                    EXTENSION,
                    &request.name,
                    args,
                    Scope {
                        workspace: None,
                        ceiling: Some(scope.capabilities.clone()),
                        current: None,
                        project_skills,
                    },
                )
                .await
                .map_err(|e| crate::error::report(&e))?;
            Ok(ToolOutput {
                text: output.text,
                is_error: false,
            })
        })
    }
}
#[cfg(test)]
mod tests;
