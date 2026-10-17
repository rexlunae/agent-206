//! Whether a thread may call an MCP tool. The effective authority is the
//! intersection of the server entry (enabled, tool policy, capabilities,
//! project) and the thread's own grant; an MCP server never adds authority.
use crate::McpError;
use a206_core::{Capability, McpServerConfig, Thread, ToolPolicy};

/// Whether the user approved this specific call. Only the gateway's trusted
/// approval surface may supply [`Approval::Approved`]; a model's request to
/// call a tool is never an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    NotRequested,
    Approved,
}

/// Whether any of the server's tools could be offered to `thread`. Fails with
/// the first reason the thread may not use the server at all.
pub fn server_callable(server: &McpServerConfig, thread: &Thread) -> Result<(), McpError> {
    if !server.enabled {
        return Err(McpError::Disabled);
    }
    if let Some(&capability) = server
        .capabilities
        .iter()
        .find(|c| !thread.grant.capabilities.contains(c))
    {
        return Err(McpError::Denied { capability });
    }
    let workspace = server.capabilities.contains(&Capability::ReadWorkspace);
    if workspace && (server.project.is_none() || thread.project != server.project) {
        return Err(McpError::ProjectMismatch);
    }
    Ok(())
}

/// Authorize one call of `tool` on `server` from `thread`.
pub fn authorize(
    server: &McpServerConfig,
    thread: &Thread,
    tool: &str,
    approval: Approval,
) -> Result<(), McpError> {
    server_callable(server, thread)?;
    match (server.policy(tool), approval) {
        (ToolPolicy::Deny, _) => Err(McpError::ToolDenied),
        (ToolPolicy::Ask, Approval::NotRequested) => Err(McpError::ApprovalRequired),
        (ToolPolicy::Ask, Approval::Approved) | (ToolPolicy::Allow, _) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{Id, Mode};
    fn server() -> McpServerConfig {
        McpServerConfig {
            name: "a".into(),
            enabled: true,
            executable: "/opt/a/server".into(),
            runtime_root: "/opt/a".into(),
            args: vec![],
            env: Default::default(),
            capabilities: [Capability::Network].into(),
            project: None,
            default_policy: ToolPolicy::Ask,
            tools: Default::default(),
            timeout_seconds: None,
        }
    }
    fn thread(capabilities: &[Capability]) -> Thread {
        let mut thread = Thread::new("t".into(), Mode::Research);
        thread.grant.capabilities = capabilities.iter().copied().collect();
        thread
    }
    #[test]
    fn a_thread_needs_every_capability_the_server_holds() {
        let tool = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            authorize(&server(), &thread(&[]), &tool, Approval::Approved),
            Err(McpError::Denied {
                capability: Capability::Network
            })
        ));
        authorize(
            &server(),
            &thread(&[Capability::Network]),
            &tool,
            Approval::Approved,
        )
        .unwrap();
    }
    #[test]
    fn ask_needs_approval_and_deny_ignores_it() {
        let tool = uuid::Uuid::new_v4().to_string();
        let thread = thread(&[Capability::Network]);
        let mut entry = server();
        assert!(matches!(
            authorize(&entry, &thread, &tool, Approval::NotRequested),
            Err(McpError::ApprovalRequired)
        ));
        entry.tools.insert(tool.clone(), ToolPolicy::Deny);
        assert!(matches!(
            authorize(&entry, &thread, &tool, Approval::Approved),
            Err(McpError::ToolDenied)
        ));
        entry.tools.insert(tool.clone(), ToolPolicy::Allow);
        authorize(&entry, &thread, &tool, Approval::NotRequested).unwrap();
        entry.enabled = false;
        assert!(matches!(
            authorize(&entry, &thread, &tool, Approval::Approved),
            Err(McpError::Disabled)
        ));
    }
    #[test]
    fn workspace_servers_serve_only_their_project() {
        let project = Id::new_v4();
        let mut entry = server();
        entry.capabilities = [Capability::ReadWorkspace].into();
        entry.project = Some(project);
        let mut caller = thread(&[Capability::ReadWorkspace]);
        assert!(matches!(
            server_callable(&entry, &caller),
            Err(McpError::ProjectMismatch)
        ));
        caller.project = Some(Id::new_v4());
        assert!(matches!(
            server_callable(&entry, &caller),
            Err(McpError::ProjectMismatch)
        ));
        caller.project = Some(project);
        server_callable(&entry, &caller).unwrap();
    }
}
