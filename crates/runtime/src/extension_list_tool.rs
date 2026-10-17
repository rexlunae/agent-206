//! The native `extension_list` tool: what extensions the user has, which of
//! their tools this conversation can call, and what stands in the way of the
//! rest (RustyClaw's `plugin_list`).
//!
//! It reads the same records as the Extensions page and changes nothing, so
//! it uses no capability; like every tool it is denied until the user
//! permits it in the conversation. An
//! agent learns what to ask the user for (enable an extension, grant it a
//! permission, allow one in this conversation), but enabling and granting
//! stay user operations. Extension state is not listed: it is the
//! extension's private storage, and agents change it only through the
//! extension's own tools, which validate what they store.
use crate::{
    extension_host::{ExtensionError, ExtensionHost},
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    Capability, ErrorKind, ErrorReport, ExtensionProposal, PluginSource, PluginSummary,
    ToolPermission, UnavailableTools,
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const NAME: &str = "extension_list";

/// Why the extensions could not be listed.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    #[error("unexpected argument `{0}`")]
    Unexpected(String),
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error(transparent)]
    Extension(#[from] ExtensionError),
}
impl crate::error::Classified for ListError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Unexpected(_) => ErrorKind::InvalidRequest,
            Self::UnknownTool(_) => ErrorKind::NotFound,
            Self::Extension(error) => return crate::error::classify(error),
        })
    }
}

/// Offers `extension_list` to native runs.
pub struct ListHost {
    extensions: ExtensionHost,
}
impl ListHost {
    pub fn new(extensions: ExtensionHost) -> Self {
        Self { extensions }
    }
    fn spec() -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: "List the user's extensions with their tools and panels. For each \
                tool it gives the name to call it by in this conversation, or why it cannot \
                be called: the extension is disabled, the user has not granted it a \
                permission, or this conversation does not allow one. Only the user can \
                enable extensions and grant permissions; ask them to on the Extensions page. \
                Also lists packages waiting for the user's review."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            capabilities: BTreeSet::new(),
            permission: ToolPermission::Allow,
            whole_output: true,
        }
    }
    fn list_extensions(
        &self,
        scope: &ToolScope,
        arguments: &Map<String, Value>,
    ) -> Result<ToolOutput, ListError> {
        if let Some(name) = arguments.keys().next() {
            return Err(ListError::Unexpected(name.clone()));
        }
        let (plugins, proposals) = self.extensions.catalog()?;
        let (names, unavailable) = crate::extension_tools::offered_names(&self.extensions);
        Ok(ToolOutput {
            text: listing(scope, &plugins, &proposals, &names, &unavailable).to_string(),
            is_error: false,
        })
    }
}

/// The listing the model receives. Names and descriptions are written by each
/// extension's author.
fn listing(
    scope: &ToolScope,
    plugins: &[PluginSummary],
    proposals: &[ExtensionProposal],
    names: &BTreeMap<(String, String), String>,
    unavailable: &[UnavailableTools],
) -> Value {
    let extensions: Vec<Value> = plugins
        .iter()
        .map(|plugin| {
            let failures: Vec<String> = unavailable
                .iter()
                .filter(|u| u.source == plugin.id)
                .map(|u| display(&u.error))
                .collect();
            let tools: Vec<Value> = plugin
                .tools
                .iter()
                .map(|tool| {
                    let call_as = if plugin.id == crate::skill_tool::EXTENSION {
                        // Offered under its stable names while a skill is approved.
                        (plugin.enabled && tool.capabilities.is_subset(&plugin.granted))
                            .then(|| tool.name.clone())
                    } else {
                        names.get(&(plugin.id.clone(), tool.name.clone())).cloned()
                    };
                    let mut blocked = blocked(plugin, &tool.capabilities, scope);
                    if call_as.is_none() && blocked.is_empty() && failures.is_empty() {
                        blocked.push(
                            "Not offered: its name is too long or shared with another \
                             extension's tool."
                                .into(),
                        );
                    }
                    json!({
                        "tool": tool.name,
                        "description": tool.description,
                        "permissions": labels(&tool.capabilities),
                        "call_as": call_as,
                        "blocked_by": blocked,
                    })
                })
                .collect();
            let panels: Vec<Value> = plugin
                .panels
                .iter()
                .map(|panel| json!({"id": panel.id, "title": panel.title}))
                .collect();
            let mut entry = json!({
                "id": plugin.id,
                "name": plugin.name,
                "description": plugin.description,
                "version": plugin.version,
                "source": match plugin.source {
                    PluginSource::Bundled => "bundled",
                    PluginSource::Installed { .. } => "installed by the user",
                },
                "enabled": plugin.enabled,
                "permissions_requested": labels(&plugin.capabilities),
                "permissions_granted": labels(&plugin.granted),
                "tools": tools,
                "panels": panels,
            });
            if !failures.is_empty() {
                entry["errors"] = json!(failures);
            }
            entry
        })
        .collect();
    let proposals: Vec<Value> = proposals
        .iter()
        .map(|proposal| {
            json!({
                "id": proposal.summary.id,
                "version": proposal.summary.version,
                "digest": proposal.digest,
                "prepared_in_this_conversation": proposal.thread == scope.thread,
            })
        })
        .collect();
    json!({
        "extensions": extensions,
        "waiting_for_review": proposals,
        "note": "Only the user can install, enable or grant permissions to extensions. \
            A tool whose call_as is set may still need the user's permission in this \
            conversation.",
    })
}

/// What keeps a tool needing `needs` from being called in `scope`.
fn blocked(plugin: &PluginSummary, needs: &BTreeSet<Capability>, scope: &ToolScope) -> Vec<String> {
    let mut reasons = vec![];
    if !plugin.enabled {
        reasons.push("The user has not enabled this extension.".into());
    }
    let ungranted: BTreeSet<_> = needs.difference(&plugin.granted).copied().collect();
    if !ungranted.is_empty() {
        reasons.push(format!(
            "The user has not granted this extension: {}.",
            labels(&ungranted).join(", ")
        ));
    }
    let disallowed: BTreeSet<_> = needs.difference(&scope.capabilities).copied().collect();
    if !disallowed.is_empty() {
        reasons.push(format!(
            "This conversation does not allow: {}.",
            labels(&disallowed).join(", ")
        ));
    }
    reasons
}
fn labels(capabilities: &BTreeSet<Capability>) -> Vec<&'static str> {
    capabilities.iter().map(|c| c.label()).collect()
}
/// A report and its causes, as the model reads them.
fn display(report: &ErrorReport) -> String {
    let mut text = report.message.clone();
    let mut cause = report.source.as_deref();
    while let Some(report) = cause {
        text.push_str(": ");
        text.push_str(&report.message);
        cause = report.source.as_deref();
    }
    text
}

impl ToolHost for ListHost {
    fn list<'a>(&'a self, _scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            ToolListing {
                tools: vec![Self::spec()],
                unavailable: vec![],
            }
        })
    }
    /// Reads the records once and returns; there is nothing to cancel.
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            let result = if request.name == NAME {
                self.list_extensions(scope, &request.arguments)
            } else {
                Err(ListError::UnknownTool(request.name))
            };
            result.map_err(|error| crate::error::report(&error))
        })
    }
}

#[cfg(test)]
mod tests;
