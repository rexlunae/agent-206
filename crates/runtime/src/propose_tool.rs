//! The native `extension_propose` tool: an agent hands the user an extension
//! package it built in the conversation's project, for review.
//!
//! The package is validated and stored as a proposal
//! ([`ExtensionHost::propose`]). That grants nothing and runs nothing: only
//! the user installs a proposal, and it then arrives disabled with no
//! permissions, so enabling it and granting each permission stay user
//! operations. The tool reads the two files under `read_workspace`, like
//! every other project read, and is denied until the user permits it in a
//! conversation.
use crate::{
    extension_host::{ExtensionError, ExtensionHost},
    provider::now,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    Capability, ErrorKind, ErrorReport, MAX_EXTENSION_MANIFEST, MAX_EXTENSION_MODULE,
    ToolPermission,
};
use a206_extensions::abi::HostFailure;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::path::PathBuf;

pub const NAME: &str = "extension_propose";

/// Why a package could not be proposed.
#[derive(Debug, thiserror::Error)]
pub enum ProposeError {
    #[error("unexpected argument `{0}`")]
    Unexpected(String),
    #[error("`{0}` must be a path inside the project")]
    Path(&'static str),
    #[error("choose a project: extension packages are read from project files")]
    NoProject,
    #[error("this conversation does not allow \"{}\"", Capability::ReadWorkspace.label())]
    OutOfScope,
    #[error("could not read the {0}")]
    Read(&'static str, #[source] HostFailure),
    #[error("the {0} is larger than {1} KiB")]
    TooLarge(&'static str, usize),
    #[error("the manifest is not UTF-8 text")]
    NotText(#[source] std::string::FromUtf8Error),
    #[error("the package read task failed")]
    Join(#[source] tokio::task::JoinError),
    #[error(transparent)]
    Extension(#[from] ExtensionError),
}
impl crate::error::Classified for ProposeError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Unexpected(_)
            | Self::Path(_)
            | Self::NoProject
            | Self::TooLarge(..)
            | Self::NotText(_) => ErrorKind::InvalidRequest,
            Self::OutOfScope => ErrorKind::PermissionDenied,
            Self::Read(_, failure) => return crate::error::classify(failure),
            Self::Join(_) => ErrorKind::Internal,
            Self::Extension(error) => return crate::error::classify(error),
        })
    }
}

/// Offers `extension_propose` to native runs.
pub struct ProposeHost {
    extensions: ExtensionHost,
}
impl ProposeHost {
    pub fn new(extensions: ExtensionHost) -> Self {
        Self { extensions }
    }
    fn spec() -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: "Offer the user an extension package you built in this project: its \
                plugin.toml manifest and compiled WebAssembly module. The user reviews it on \
                the Extensions page. You cannot install, enable or grant it; if the user \
                installs it, it starts disabled with no permissions."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "manifest": {
                        "type": "string",
                        "description": "Path of the plugin.toml, relative to the project root."
                    },
                    "module": {
                        "type": "string",
                        "description": "Path of the compiled .wasm module, relative to the project root."
                    }
                },
                "required": ["manifest", "module"],
                "additionalProperties": false
            }),
            capabilities: [Capability::ReadWorkspace].into(),
            permission: ToolPermission::Allow,
        }
    }
    async fn propose(
        &self,
        scope: &ToolScope,
        arguments: &Map<String, Value>,
    ) -> Result<ToolOutput, ProposeError> {
        if let Some(name) = arguments
            .keys()
            .find(|name| !matches!(name.as_str(), "manifest" | "module"))
        {
            return Err(ProposeError::Unexpected(name.clone()));
        }
        let path = |name: &'static str| {
            arguments
                .get(name)
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .ok_or(ProposeError::Path(name))
        };
        let (manifest_path, module_path) = (path("manifest")?, path("module")?);
        if !scope.capabilities.contains(&Capability::ReadWorkspace) {
            return Err(ProposeError::OutOfScope);
        }
        let root = scope.workspace.clone().ok_or(ProposeError::NoProject)?;
        // Both reads are bounded; a cancelled call discards their result.
        let (manifest, module) = tokio::task::spawn_blocking(move || {
            Ok::<_, ProposeError>((
                read(&root, &manifest_path, "manifest", MAX_EXTENSION_MANIFEST)?,
                read(&root, &module_path, "module", MAX_EXTENSION_MODULE)?,
            ))
        })
        .await
        .map_err(ProposeError::Join)??;
        let manifest = String::from_utf8(manifest).map_err(ProposeError::NotText)?;
        let proposal = self
            .extensions
            .propose(scope.thread, manifest, module, now())
            .await?;
        Ok(ToolOutput {
            text: json!({
                "proposed": proposal.summary.id,
                "version": proposal.summary.version,
                "digest": proposal.digest,
                "note": "The user reviews this package on the Extensions page. It does not run \
                    unless they install it, enable it and grant its permissions.",
            })
            .to_string(),
            is_error: false,
        })
    }
}
fn read(
    root: &std::path::Path,
    path: &str,
    file: &'static str,
    limit: usize,
) -> Result<Vec<u8>, ProposeError> {
    crate::extension_host::read_limited(PathBuf::from(root), path, limit as u64)
        .map_err(|failure| ProposeError::Read(file, failure))?
        .ok_or(ProposeError::TooLarge(file, limit / 1024))
}
impl ToolHost for ProposeHost {
    fn list<'a>(&'a self, _scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            ToolListing {
                tools: vec![Self::spec()],
                unavailable: vec![],
            }
        })
    }
    /// Dropping the future stops the call before its files are read in
    /// full; once recording has begun it finishes, and the user sees the
    /// proposal like any other.
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            let result = if request.name == NAME {
                self.propose(scope, &request.arguments).await
            } else {
                Err(ProposeError::Unexpected(request.name))
            };
            result.map_err(|error| crate::error::report(&error))
        })
    }
}
