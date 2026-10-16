//! MCP client: user-configured MCP servers run as isolated child processes,
//! and their tools are offered to a thread only within its grants.
//!
//! The configuration, connection and tool-result types follow RustyClaw's
//! `rustyclaw-core::mcp` (MIT); process isolation, bounded framing, typed
//! errors and the grant check are Agent 206's.
#![forbid(unsafe_code)]
mod bounded;
mod config;
mod policy;
mod sandbox;
mod session;

pub use config::validate;
pub use policy::{Approval, authorize, server_callable};
pub use sandbox::{Launch, spawn};
pub use session::{Content, Session, ToolOutput};

use a206_core::Capability;
use std::time::Duration;

/// Resource bounds for one server session. These are local safety limits,
/// not facts about any model or provider.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Longest single JSON-RPC line the server may send.
    pub max_line_bytes: usize,
    pub max_tools: usize,
    /// Combined size of every advertised tool's schema and description.
    pub max_listing_bytes: usize,
    pub max_output_bytes: usize,
    /// Initialization and tool listing.
    pub connect_timeout: Duration,
    pub call_timeout: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_line_bytes: 8 << 20,
            max_tools: 256,
            max_listing_bytes: 512 << 10,
            max_output_bytes: 1 << 20,
            connect_timeout: Duration::from_secs(30),
            call_timeout: Duration::from_secs(120),
        }
    }
}

/// Why an MCP server could not be configured, started or called.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP server names use 1 to 32 lowercase letters, digits and single inner hyphens")]
    Name,
    #[error("another MCP server already uses this name")]
    DuplicateName,
    #[error("MCP server executable and installation directory must be absolute paths")]
    RelativePath,
    #[error("MCP server arguments and environment must not contain NUL characters")]
    Nul,
    #[error("environment variable names must be non-empty and must not contain '='")]
    Environment,
    #[error("the sandbox sets {key} for every MCP server; an entry cannot replace it")]
    ReservedEnvironment { key: String },
    #[error("MCP servers cannot hold {capability:?} authority")]
    Capability { capability: Capability },
    #[error("workspace write access for an MCP server also requires read access")]
    WriteWithoutRead,
    #[error("an MCP server with workspace access must be bound to a project")]
    ProjectRequired,
    #[error("only an MCP server with workspace access may be bound to a project")]
    ProjectUnused,
    #[error("MCP tool timeout must be between 1 and {max} seconds")]
    TimeoutRange { max: u64 },
    #[error("{what} is missing or inaccessible")]
    Missing {
        what: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Placement(&'static str),
    #[error("MCP server paths must be UTF-8")]
    NonUtf8Path,
    #[error("could not prepare the MCP server sandbox")]
    Setup(#[source] std::io::Error),
    #[error(
        "MCP servers are disabled on this platform until equivalent OS filesystem and network isolation is implemented"
    )]
    Unsupported,
    #[error("could not start the MCP server")]
    Spawn(#[source] std::io::Error),
    #[error("MCP server has no usable process ID")]
    ProcessId,
    #[error("MCP server did not complete initialization")]
    Initialize(#[source] Box<rmcp::service::ClientInitializeError>),
    #[error("MCP request failed")]
    Service(#[source] rmcp::ServiceError),
    #[error("MCP server did not respond in time")]
    Timeout(#[source] tokio::time::error::Elapsed),
    #[error("MCP server advertised more than {limit} tools")]
    TooManyTools { limit: usize },
    #[error("MCP server's tool listing exceeds {limit} bytes")]
    ListingTooLarge { limit: usize },
    #[error("MCP tool result exceeds its {limit}-byte limit")]
    OutputTooLarge { limit: usize },
    #[error("MCP server sent an invalid tool schema")]
    Schema(#[source] serde_json::Error),
    #[error("MCP server is disabled")]
    Disabled,
    #[error("MCP server is not connected")]
    NotConnected,
    #[error("MCP server exited or closed its connection")]
    Exited,
    #[error("MCP server does not offer this tool")]
    UnknownTool,
    #[error("this MCP tool is denied by policy")]
    ToolDenied,
    #[error("this MCP tool needs the user's approval for each call")]
    ApprovalRequired,
    #[error("this conversation lacks {capability:?}, which the MCP server holds")]
    Denied { capability: Capability },
    #[error("this MCP server is bound to a different project")]
    ProjectMismatch,
    #[error(
        "the thread lost permission to use this MCP tool while the call ran; the call was cancelled"
    )]
    Revoked,
}

/// The authority an MCP server process may hold. Anything else (secrets, git,
/// agents, browser) is reached through gateway-brokered tools instead.
pub const SERVER_CAPABILITIES: [Capability; 4] = [
    Capability::ReadWorkspace,
    Capability::WriteWorkspace,
    Capability::Execute,
    Capability::Network,
];
