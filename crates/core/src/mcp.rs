//! MCP client configuration and status, shared by clients and the gateway.
//!
//! A server entry is user configuration: nothing here enables a server, starts
//! it or grants a thread access to its tools. Imported servers stay disabled.
use crate::{Capability, ErrorReport, Id};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Whether a thread may call one MCP tool, before thread grants are applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// Callable without a per-call approval when the thread's grants allow it.
    Allow,
    /// Every call needs the user's approval.
    #[default]
    Ask,
    /// Never offered to a model and never callable.
    Deny,
}
impl ToolPolicy {
    pub const ALL: [Self; 3] = [Self::Allow, Self::Ask, Self::Deny];
    pub fn label(self) -> &'static str {
        match self {
            Self::Allow => "Allow",
            Self::Ask => "Ask",
            Self::Deny => "Deny",
        }
    }
}

/// A user-configured MCP server, run as an isolated child process over stdio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Lowercase ASCII letters, digits and single hyphens; unique per profile.
    pub name: String,
    /// Disabled servers never start. Off unless the user turns it on.
    #[serde(default)]
    pub enabled: bool,
    /// Absolute path of the server program, inside `runtime_root`.
    pub executable: String,
    /// Absolute installation directory the sandboxed server may read.
    pub runtime_root: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Non-secret environment for the server. The ambient environment is never
    /// inherited; credentials belong in the vault, not here.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// OS authority given to the server process. A thread may call its tools
    /// only when the thread holds every one of these capabilities.
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    /// Project whose directory a server with workspace capabilities may use.
    /// Only threads in this project may call such a server.
    #[serde(default)]
    pub project: Option<Id>,
    /// Policy for tools without an entry in `tools`.
    #[serde(default)]
    pub default_policy: ToolPolicy,
    /// Per-tool policy by the server's tool name.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolPolicy>,
    /// Seconds one tool call may run; the gateway default applies when absent.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}
impl McpServerConfig {
    pub fn policy(&self, tool: &str) -> ToolPolicy {
        self.tools.get(tool).copied().unwrap_or(self.default_policy)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Failed,
}
impl McpServerState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Failed => "Failed",
        }
    }
}

/// A tool a connected server advertised. Names, descriptions and schemas come
/// from the server and are untrusted display text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// The tool's JSON Schema input document, as the server sent it.
    pub input_schema: String,
}

/// Runtime state of one configured server, reported by the gateway.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub state: McpServerState,
    /// Name and version the server reported during initialization.
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub tools: Vec<McpToolInfo>,
    /// Why the last connection attempt or session failed.
    #[serde(default)]
    pub failure: Option<ErrorReport>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn servers_are_disabled_and_ask_unless_configured_otherwise() {
        let name = uuid::Uuid::new_v4().simple().to_string();
        let tool = uuid::Uuid::new_v4().to_string();
        let config: McpServerConfig = toml::from_str(&format!(
            "name = \"{name}\"\nexecutable = \"/opt/a/server\"\nruntime_root = \"/opt/a\"\n[tools]\n\"{tool}\" = \"allow\"\n"
        ))
        .unwrap();
        assert!(!config.enabled);
        assert!(config.capabilities.is_empty());
        assert_eq!(config.policy(&tool), ToolPolicy::Allow);
        assert_eq!(config.policy("other"), ToolPolicy::Ask);
    }
}
