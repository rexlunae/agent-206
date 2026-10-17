//! The native `extension_list` tool: what extensions the user has, which of
//! their tools this conversation can call, and what stands in the way of the
//! rest (RustyClaw's `plugin_list`). The tools it may call are in the model's
//! own tool list; this names each tool as it is offered there and gives the
//! reasons one is missing, without claiming more than the gateway offers.
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
    extension_host::{Catalog, ExtensionError, ExtensionHost, PROJECT_CAPABILITIES},
    tools::{self, ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    Capability, ErrorKind, ErrorReport, PluginSource, PluginSummary, ToolPermission,
    UnavailableTools,
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub const NAME: &str = "extension_list";

/// Why the extensions could not be listed.
#[derive(Debug, thiserror::Error)]
pub enum ListError {
    #[error("unexpected argument `{0}`")]
    Unexpected(String),
    #[error("`{0}` must be a string")]
    NotAString(&'static str),
    #[error("no extension `{0}` is installed")]
    NotInstalled(String),
    #[error("the listing of extension `{0}` does not fit in one tool result")]
    TooLarge(String),
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error(transparent)]
    Extension(#[from] ExtensionError),
}
impl crate::error::Classified for ListError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Unexpected(_) | Self::NotAString(_) => ErrorKind::InvalidRequest,
            Self::UnknownTool(_) | Self::NotInstalled(_) => ErrorKind::NotFound,
            Self::TooLarge(_) => ErrorKind::InvalidRequest,
            Self::Extension(error) => return crate::error::classify(error),
        })
    }
}

/// Offers `extension_list` to native runs.
pub struct ListHost {
    extensions: ExtensionHost,
    /// The host offering the skills builtin's tools under their stable
    /// names; asked what it offers rather than second-guessed.
    skills: Arc<dyn ToolHost>,
}
impl ListHost {
    pub fn new(extensions: ExtensionHost, skills: Arc<dyn ToolHost>) -> Self {
        Self { extensions, skills }
    }
    fn spec() -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: "List the user's extensions with their tools and panels, and for each \
                tool what keeps you from calling it: the extension is disabled, the user has \
                not granted it a permission, this conversation does not allow one or has no \
                project. Tools you can call are in your tool list under the name given here. \
                Only the user can enable extensions and grant permissions; ask them to on the \
                Extensions page. Also lists packages waiting for the user's review. Pass \
                `extension` for one extension with its tool descriptions."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "extension": {
                        "type": "string",
                        "description": "An extension id, to list only that extension in full."
                    }
                },
                "additionalProperties": false
            }),
            capabilities: BTreeSet::new(),
            permission: ToolPermission::Allow,
            whole_output: true,
        }
    }
    async fn list_extensions(
        &self,
        scope: &ToolScope,
        arguments: &Map<String, Value>,
    ) -> Result<ToolOutput, ListError> {
        if let Some(name) = arguments.keys().find(|name| name.as_str() != "extension") {
            return Err(ListError::Unexpected(name.clone()));
        }
        let only = match arguments.get("extension") {
            None => None,
            Some(Value::String(id)) => Some(id.as_str()),
            Some(_) => return Err(ListError::NotAString("extension")),
        };
        let mut catalog = self.extensions.catalog()?;
        if let Some(id) = only {
            catalog.plugins.retain(|plugin| plugin.id == id);
            catalog
                .proposals
                .retain(|proposal| proposal.summary.id == id);
            if catalog.plugins.is_empty() {
                return Err(ListError::NotInstalled(id.into()));
            }
        }
        let (names, unavailable) = crate::extension_tools::offered_names(&self.extensions);
        // Only a listing that includes the skills builtin waits on its scan.
        let skills = if catalog
            .plugins
            .iter()
            .any(|plugin| plugin.id == crate::skill_tool::EXTENSION)
        {
            self.skills.list(scope).await
        } else {
            ToolListing::default()
        };
        let context = Context {
            scope,
            names: &names,
            unavailable: &unavailable,
            skills: &skills,
        };
        let listing = listing(&context, &catalog, only.is_some());
        if let Some(id) = only
            && listing["extensions"].as_array().is_none_or(Vec::is_empty)
        {
            return Err(ListError::TooLarge(id.into()));
        }
        Ok(ToolOutput {
            text: listing.to_string(),
            is_error: false,
        })
    }
}

/// Room for the extension entries in one result; the rest is the proposals
/// (at most [`a206_core::MAX_EXTENSION_PROPOSALS`]) and the note.
const BUDGET: usize = tools::MAX_OUTPUT - 8 * 1024;

/// What a tool's availability depends on besides its extension's record.
struct Context<'a> {
    scope: &'a ToolScope,
    names: &'a BTreeMap<(String, String), String>,
    unavailable: &'a [UnavailableTools],
    /// What the skill tool host offers this conversation.
    skills: &'a ToolListing,
}

/// The listing the model receives. Names and descriptions are written by each
/// extension's author. The full listing leaves out tool descriptions and stops
/// before [`BUDGET`], naming the extensions it left out; a listing of one
/// extension includes them when they fit.
fn listing(context: &Context<'_>, catalog: &Catalog, detailed: bool) -> Value {
    let mut extensions = vec![];
    let mut omitted = vec![];
    let mut used = 0;
    for plugin in &catalog.plugins {
        let mut entry = extension(context, plugin, detailed);
        let mut size = entry.to_string().len();
        if detailed && size > BUDGET {
            entry = extension(context, plugin, false);
            entry["descriptions_shortened"] = json!(true);
            size = entry.to_string().len();
        }
        if used + size > BUDGET {
            omitted.push(plugin.id.clone());
            continue;
        }
        used += size;
        extensions.push(entry);
    }
    let proposals: Vec<Value> = catalog
        .proposals
        .iter()
        .map(|proposal| {
            json!({
                "id": proposal.summary.id,
                "version": proposal.summary.version,
                "digest": proposal.digest,
                "prepared_in_this_conversation": proposal.thread == context.scope.thread,
            })
        })
        .collect();
    let mut listing = json!({
        "extensions": extensions,
        "waiting_for_review": proposals,
        "note": "Only the user can install, enable or grant permissions to extensions. A tool \
            with nothing in blocked_by can be called if it is in your tool list; if it is not, \
            the user may still need to permit it in this conversation.",
    });
    if !omitted.is_empty() {
        listing["omitted"] = json!(omitted);
        listing["omitted_note"] =
            json!("These extensions did not fit; pass one as `extension` to list it on its own.");
    }
    listing
}

fn extension(context: &Context<'_>, plugin: &PluginSummary, detailed: bool) -> Value {
    let failures: Vec<String> = context
        .unavailable
        .iter()
        .filter(|u| u.source == plugin.id)
        .map(|u| display(&u.error))
        .collect();
    let skills = plugin.id == crate::skill_tool::EXTENSION;
    let tools: Vec<Value> = plugin
        .tools
        .iter()
        .map(|tool| {
            let mut blocked = blocked(plugin, &tool.capabilities, context.scope);
            let name = if skills {
                // Offered under its stable names, only while an approved
                // skill is unchanged; the skill host decides.
                let offered = context.skills.tools.iter().any(|t| t.name == tool.name);
                if !offered && blocked.is_empty() {
                    blocked.extend(context.skills.unavailable.iter().map(|u| display(&u.error)));
                    if blocked.is_empty() {
                        blocked.push(
                            "The user has no approved skill that is unchanged since approval."
                                .into(),
                        );
                    }
                }
                // Otherwise the skill host has not looked at approvals, so
                // the listing makes no claim about them.
                offered.then(|| tool.name.clone())
            } else {
                context
                    .names
                    .get(&(plugin.id.clone(), tool.name.clone()))
                    .cloned()
            };
            if name.is_none() && blocked.is_empty() && failures.is_empty() {
                blocked.push(
                    "Not offered: its name is too long or shared with another extension's tool."
                        .into(),
                );
            }
            let mut entry = json!({
                "tool": tool.name,
                "name": name,
                "permissions": labels(&tool.capabilities),
                "blocked_by": blocked,
            });
            if detailed {
                entry["description"] = json!(tool.description);
            }
            entry
        })
        .collect();
    let panels: Vec<Value> = plugin
        .panels
        .iter()
        .map(|panel| json!({"id": panel.id, "title": panel.title}))
        .collect();
    // The compact form keeps an author's long description from crowding out
    // the tools; a listing of one extension gives it whole when it fits.
    let description = if detailed {
        plugin.description.as_str()
    } else {
        shortened(&plugin.description, SHORT_DESCRIPTION)
    };
    let mut entry = json!({
        "id": plugin.id,
        "name": plugin.name,
        "description": description,
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
}

/// Longest extension description in the compact form.
const SHORT_DESCRIPTION: usize = 1024;
/// `text` cut to at most `limit` bytes at a character boundary, marked when cut.
fn shortened(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
    if scope.workspace.is_none() && PROJECT_CAPABILITIES.iter().any(|c| needs.contains(c)) {
        reasons.push("This conversation has no project.".into());
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
                self.list_extensions(scope, &request.arguments).await
            } else {
                Err(ListError::UnknownTool(request.name))
            };
            result.map_err(|error| crate::error::report(&error))
        })
    }
}

#[cfg(test)]
mod tests;
