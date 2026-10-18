//! Legacy model-facing skill names, backed by the bundled WASM extension.
//! Approval pins remain gateway-owned; the guest has no filesystem or settings
//! access. Reads use the same worker timeout and policy checks as other builtins.
use crate::{
    extension_host::{ExtensionHost, Scope},
    store::Store,
    tools::{self, ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{ErrorKind, ErrorReport, ToolPermission, UnavailableTools};
use a206_skills::{Skill, SkillError};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
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
pub(crate) fn offers(
    root: &Path,
    pins: &BTreeMap<String, String>,
) -> Result<Vec<SkillOffer>, SkillError> {
    // Scan even without pins: a broken directory is unavailable, not empty.
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
/// Complete text only; neither the index nor instructions may be truncated.
pub(crate) fn read(
    root: &Path,
    pins: &BTreeMap<String, String>,
    request: &ToolRequest,
) -> Result<String, SkillToolError> {
    let result = match request.name.as_str() {
        LIST => {
            check_keys(&request.arguments, &[])?;
            let offers = offers(root, pins)?;
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
                None => approved(root, pins, &name)?.instructions,
                Some(path) => String::from_utf8(a206_skills::read_approved_file(
                    root,
                    &name,
                    &path,
                    pins.get(&name).ok_or(SkillError::Disabled)?,
                )?)
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
    fn list<'a>(&'a self, _: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            let load = async {
                if !self.enabled()? {
                    return Ok(vec![]);
                }
                let root = self.root.clone();
                let pins = self.pins()?;
                let available = io(move || Ok(offers(&root, &pins)?)).await?;
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
            let output = self
                .extensions
                .call_tool(
                    EXTENSION,
                    &request.name,
                    args,
                    Scope {
                        workspace: None,
                        ceiling: Some(scope.capabilities.clone()),
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
