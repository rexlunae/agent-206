//! The MCP [`ToolHost`]: offers the tools of enabled, connected MCP servers
//! to the native run engine and calls them through the gateway's MCP call
//! path ([`Gateway::call_mcp_tool`](crate::gateway::Gateway::call_mcp_tool)).
//!
//! Only servers the conversation may use are listed: enabled, not
//! disconnected by the user, bound to the conversation's project when they
//! hold workspace access, and holding no capability outside the scope's
//! capabilities (the conversation's grant within its mode ceiling,
//! intersected with every ancestor's). Such a server starts on first use.
//! Listing a tool makes it available to be permitted; the conversation's own
//! setting still defaults to deny. The host's policy is the server entry's
//! per-tool Allow/Ask/Deny, so the gateway enforces the most restrictive of
//! that and the conversation's settings.
//!
//! Every call is authorized again by the gateway's MCP call path against the
//! thread's current grant and the server's current entry, which stays the
//! single enforcement point for MCP authority; the scope's capabilities are
//! checked first as a ceiling.
//!
//! Model-facing names are `mcp__<server>__<tool>`, with every character of the
//! tool name outside `[A-Za-z0-9_-]` written as `_`. Server names never contain
//! `_`, so the server is unambiguous. A name longer than [`tools::MAX_NAME`],
//! or one that two tools of a server would share, is not offered at all and
//! is reported in [`ToolListing::unavailable`]; names are never truncated or
//! guessed.
use crate::{
    gateway::{GatewayError, McpClient, McpOffer, McpOffers},
    tools::{self, ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{ErrorKind, ErrorReport, ToolPermission, ToolPolicy, UnavailableTools};
use a206_mcp::{Approval, Content, McpError};
use futures_util::future::BoxFuture;
use std::collections::BTreeMap;

/// The prefix of every MCP tool's model-facing name.
pub const PREFIX: &str = "mcp__";
const SEPARATOR: &str = "__";
/// The source named when the MCP servers cannot be listed at all.
pub const SOURCE: &str = "mcp";

/// Why an MCP tool is not offered or a call could not reach it.
#[derive(Debug, thiserror::Error)]
pub enum McpToolError {
    #[error(
        "tool `{tool}` of MCP server `{server}` is not offered: its model-facing name `{name}` is longer than {} characters",
        tools::MAX_NAME
    )]
    NameTooLong {
        server: String,
        tool: String,
        name: String,
    },
    #[error("a tool of MCP server `{server}` is not offered: it has an empty name")]
    EmptyName { server: String },
    #[error(
        "tool `{tool}` of MCP server `{server}` is not offered: its model-facing name `{name}` is also derived for {others:?}"
    )]
    NameCollision {
        server: String,
        tool: String,
        name: String,
        others: Vec<String>,
    },
    #[error("tool `{tool}` of MCP server `{server}` has an input schema that is not JSON")]
    Schema {
        server: String,
        tool: String,
        #[source]
        cause: serde_json::Error,
    },
    #[error("MCP server `{server}` could not list its tools")]
    Server {
        server: String,
        #[source]
        cause: ErrorReport,
    },
    #[error("could not list the MCP servers")]
    Servers(#[source] GatewayError),
    #[error("no connected MCP server offers tool `{0}`")]
    NotOffered(String),
}
impl crate::error::Classified for McpToolError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::NameTooLong { .. } | Self::EmptyName { .. } => ErrorKind::InvalidRequest,
            Self::NameCollision { .. } => ErrorKind::Conflict,
            Self::Schema { .. } => ErrorKind::Protocol,
            Self::NotOffered(_) => ErrorKind::NotFound,
            Self::Server { .. } | Self::Servers(_) => return None,
        })
    }
}

/// The model-facing name of `tool` of `server`, before validation.
pub fn tool_name(server: &str, tool: &str) -> String {
    let tool: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{PREFIX}{server}{SEPARATOR}{tool}")
}
/// The server a model-facing name routes to. Server names contain no `_`,
/// so the first separator after the prefix ends it.
fn server_of(name: &str) -> Option<&str> {
    let (server, _) = name.strip_prefix(PREFIX)?.split_once(SEPARATOR)?;
    Some(server)
}

#[derive(Debug, Default)]
struct Offers {
    tools: BTreeMap<String, McpOffer>,
    unavailable: Vec<UnavailableTools>,
}
fn unavailable(server: &str, error: &McpToolError) -> UnavailableTools {
    UnavailableTools {
        source: format!("{PREFIX}{server}"),
        error: crate::error::report(error),
    }
}

/// Name every offered tool. Listing and calling both derive names here, so a
/// call reaches exactly the tool that was offered under its name.
fn offers(offers: McpOffers) -> Offers {
    let mut result = Offers::default();
    for (server, cause) in offers.unavailable {
        let error = McpToolError::Server {
            server: server.clone(),
            cause,
        };
        result.unavailable.push(unavailable(&server, &error));
    }
    let mut named: BTreeMap<String, Vec<McpOffer>> = BTreeMap::new();
    for offer in offers.tools {
        if offer.tool.name.is_empty() {
            let error = McpToolError::EmptyName {
                server: offer.server.clone(),
            };
            result.unavailable.push(unavailable(&offer.server, &error));
            continue;
        }
        let name = tool_name(&offer.server, &offer.tool.name);
        if name.len() > tools::MAX_NAME {
            let error = McpToolError::NameTooLong {
                server: offer.server.clone(),
                tool: offer.tool.name,
                name,
            };
            result.unavailable.push(unavailable(&offer.server, &error));
            continue;
        }
        named.entry(name).or_default().push(offer);
    }
    for (name, mut claimants) in named {
        if claimants.len() == 1 {
            if let Some(offer) = claimants.pop() {
                result.tools.insert(name, offer);
            }
            continue;
        }
        // Routing a shared name to either tool would be a guess: offer neither.
        for (index, claimant) in claimants.iter().enumerate() {
            let others = claimants
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .map(|(_, other)| other.tool.name.clone())
                .collect();
            let error = McpToolError::NameCollision {
                server: claimant.server.clone(),
                tool: claimant.tool.name.clone(),
                name: name.clone(),
                others,
            };
            result
                .unavailable
                .push(unavailable(&claimant.server, &error));
        }
    }
    result
}

fn permission(policy: ToolPolicy) -> ToolPermission {
    match policy {
        ToolPolicy::Allow => ToolPermission::Allow,
        ToolPolicy::Ask => ToolPermission::Ask,
        ToolPolicy::Deny => ToolPermission::Deny,
    }
}

/// The spec of an offered tool. Its description and schema are untrusted
/// server text, bounded by the gateway's catalog checks.
fn spec(name: String, offer: McpOffer) -> Result<ToolSpec, McpToolError> {
    let parameters =
        serde_json::from_str(&offer.tool.input_schema).map_err(|cause| McpToolError::Schema {
            server: offer.server.clone(),
            tool: offer.tool.name.clone(),
            cause,
        })?;
    Ok(ToolSpec {
        name,
        description: offer
            .tool
            .description
            .or(offer.tool.title)
            .unwrap_or_default(),
        parameters,
        capabilities: offer.capabilities,
        permission: permission(offer.policy),
        whole_output: false,
    })
}

/// The model-facing text of an MCP result. Non-text content is described,
/// never inlined: binary data would only fill the model's context.
pub fn output_text(output: a206_mcp::ToolOutput) -> ToolOutput {
    let mut parts: Vec<String> = output
        .content
        .into_iter()
        .map(|content| match content {
            Content::Text(text) => text,
            Content::Image { mime_type, data } => {
                format!(
                    "[image content ({mime_type}, {} base64 bytes) not shown]",
                    data.len()
                )
            }
            Content::Audio { mime_type, data } => {
                format!(
                    "[audio content ({mime_type}, {} base64 bytes) not shown]",
                    data.len()
                )
            }
            Content::Resource {
                uri,
                text: Some(text),
                ..
            } => format!("[resource {uri}]\n{text}"),
            Content::Resource {
                uri, text: None, ..
            } => format!("[binary resource {uri} not shown]"),
            Content::Link {
                uri,
                name,
                description,
            } => match description {
                Some(description) => format!("[link {name}: {uri}] {description}"),
                None => format!("[link {name}: {uri}]"),
            },
            Content::Unsupported => "[content of a type this client does not support]".into(),
        })
        .collect();
    // Servers should repeat structured results as text; use it only when
    // they did not.
    if parts.is_empty()
        && let Some(structured) = output.structured
    {
        parts.push(structured.to_string());
    }
    ToolOutput {
        text: parts.join("\n"),
        is_error: output.is_error,
    }
}

/// Offers enabled MCP servers' tools to native runs. Registered by default.
#[derive(Clone)]
pub struct McpToolHost {
    client: McpClient,
}
impl McpToolHost {
    pub(crate) fn new(client: McpClient) -> Self {
        Self { client }
    }
}
impl ToolHost for McpToolHost {
    fn list<'a>(&'a self, scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            let listed = match self
                .client
                .offers(scope.thread, Some(&scope.capabilities))
                .await
            {
                Ok(listed) => listed,
                Err(cause) => {
                    let error = McpToolError::Servers(cause);
                    return ToolListing {
                        tools: vec![],
                        unavailable: vec![UnavailableTools {
                            source: SOURCE.into(),
                            error: crate::error::report(&error),
                        }],
                    };
                }
            };
            let mut offers = offers(listed);
            let mut tools = vec![];
            for (name, offer) in offers.tools {
                let server = offer.server.clone();
                match spec(name, offer) {
                    Ok(spec) => tools.push(spec),
                    Err(error) => offers.unavailable.push(unavailable(&server, &error)),
                }
            }
            ToolListing {
                tools,
                unavailable: offers.unavailable,
            }
        })
    }

    /// Routes by the server's live session, without starting it, so a call
    /// reaches only a tool its server still offers under that name. The
    /// gateway's MCP call path then authorizes the call against current
    /// state, and cancels it if that authority is withdrawn while it runs.
    /// Dropping the future cancels the call at the server.
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            let report = |error: &(dyn std::error::Error + 'static)| crate::error::report(error);
            let not_offered = || report(&McpToolError::NotOffered(request.name.clone()));
            let server = server_of(&request.name).ok_or_else(not_offered)?;
            let connected = self
                .client
                .connected_offers(server)
                .map_err(|e| report(&e))?;
            let routed = offers(McpOffers {
                tools: connected,
                unavailable: vec![],
            });
            let offer = routed.tools.get(&request.name).ok_or_else(not_offered)?;
            // The gateway refused a tool whose capabilities exceed the scope
            // when it decided this call; refuse it here too rather than rely
            // on the thread's own grant alone.
            if let Some(&capability) = offer
                .capabilities
                .iter()
                .find(|c| !scope.capabilities.contains(c))
            {
                return Err(report(&McpError::Denied { capability }));
            }
            let approval = if request.approved {
                Approval::Approved
            } else {
                Approval::NotRequested
            };
            let output = self
                .client
                .call(
                    scope.thread,
                    &offer.server,
                    &offer.tool.name,
                    request.arguments,
                    approval,
                )
                .await
                .map_err(|e| report(&e))?;
            Ok(output_text(output))
        })
    }
}

#[cfg(test)]
mod tests;
