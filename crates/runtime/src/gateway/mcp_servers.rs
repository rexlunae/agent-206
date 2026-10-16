//! Gateway side of the MCP client. The gateway owns every server session and
//! offers a server's tools to a thread only within that thread's grants; see
//! [`a206_mcp::authorize`]. Sessions are runtime state: a gateway restart
//! leaves every server disconnected until it is used or connected again.
use super::{Gateway, GatewayError, Result, poisoned};
use crate::error::check;
use a206_core::*;
use a206_mcp::{Approval, McpError, Session, ToolOutput};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
pub(super) struct McpServers {
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    /// Servers starting now; a second start of the same server is refused.
    starting: Mutex<BTreeSet<String>>,
    /// Servers the user disconnected; they start again only on request.
    stopped: Mutex<BTreeSet<String>>,
}
impl McpServers {
    fn remove(&self, name: &str) -> Result<()> {
        if let Some(session) = self
            .sessions
            .lock()
            .map_err(poisoned("MCP sessions"))?
            .remove(name)
        {
            session.stop();
        }
        Ok(())
    }
    pub(super) fn stop_all(&self) -> Result<()> {
        let sessions =
            std::mem::take(&mut *self.sessions.lock().map_err(poisoned("MCP sessions"))?);
        for session in sessions.values() {
            session.stop();
        }
        Ok(())
    }
}

/// Releases a server's start claim however the start ends.
struct Starting<'a> {
    servers: &'a McpServers,
    name: String,
}
impl Drop for Starting<'_> {
    fn drop(&mut self) {
        if let Ok(mut starting) = self.servers.starting.lock() {
            starting.remove(&self.name);
        }
    }
}

/// A tool a thread may be offered, with the policy its calls follow.
#[derive(Debug, Clone, PartialEq)]
pub struct McpOffer {
    pub server: String,
    pub tool: McpToolInfo,
    /// [`ToolPolicy::Ask`] tools need [`Approval::Approved`] for every call.
    pub policy: ToolPolicy,
}
/// Tools a thread may be offered, and servers it may use that failed to start.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpOffers {
    pub tools: Vec<McpOffer>,
    pub unavailable: Vec<(String, ErrorReport)>,
}

impl Gateway {
    /// Check new MCP entries against the current state and build their status:
    /// an entry that changed, or is newly enabled, starts disconnected.
    pub(super) fn mcp_statuses(
        state: &Snapshot,
        servers: &[McpServerConfig],
    ) -> Result<Vec<McpServerStatus>> {
        a206_mcp::validate(servers)?;
        for server in servers {
            if let Some(project) = server.project {
                check(
                    state.projects.iter().any(|p| p.id == project),
                    GatewayError::Invalid("an MCP server is bound to an unknown project"),
                )?;
            }
        }
        Ok(servers
            .iter()
            .map(|server| {
                let unchanged = server.enabled && state.settings.mcp_servers.contains(server);
                state
                    .mcp_servers
                    .iter()
                    .find(|status| unchanged && status.name == server.name)
                    .cloned()
                    .unwrap_or_else(|| McpServerStatus {
                        name: server.name.clone(),
                        ..Default::default()
                    })
            })
            .collect())
    }

    /// After settings are saved, stop sessions whose entry changed, was removed
    /// or was disabled. Their next start uses the new entry.
    pub(super) fn stop_changed_mcp(
        &self,
        previous: &[McpServerConfig],
        servers: &[McpServerConfig],
    ) -> Result<()> {
        for old in previous {
            if !servers.iter().any(|s| s == old && s.enabled) {
                self.mcp.remove(&old.name)?;
                self.mcp
                    .stopped
                    .lock()
                    .map_err(poisoned("MCP sessions"))?
                    .remove(&old.name);
            }
        }
        Ok(())
    }

    fn set_mcp_status(&self, name: &str, update: impl FnOnce(&mut McpServerStatus)) -> Result<()> {
        let mut store = self.lock()?;
        let Some(status) = store.state.mcp_servers.iter_mut().find(|s| s.name == name) else {
            return Err(GatewayError::NotFound("MCP server is not configured"));
        };
        update(status);
        store.persist(Some(EventKind::McpServerChanged { name: name.into() }))?;
        Ok(())
    }

    fn mcp_config(&self, name: &str) -> Result<McpServerConfig> {
        self.lock()?
            .state
            .settings
            .mcp_servers
            .iter()
            .find(|s| s.name == name)
            .cloned()
            .ok_or(GatewayError::NotFound("MCP server is not configured"))
    }

    /// Start an enabled server in its sandbox and record its tools.
    pub(super) async fn connect_mcp(&self, name: &str) -> Result<()> {
        let config = self.mcp_config(name)?;
        check(config.enabled, McpError::Disabled)?;
        let project_root = config.project.map(|id| self.project_root(id)).transpose()?;
        let state_root = self.lock()?.root.clone();
        check(
            self.mcp
                .starting
                .lock()
                .map_err(poisoned("MCP sessions"))?
                .insert(name.to_owned()),
            GatewayError::Conflict("this MCP server is already starting"),
        )?;
        let _starting = Starting {
            servers: &self.mcp,
            name: name.to_owned(),
        };
        self.mcp
            .stopped
            .lock()
            .map_err(poisoned("MCP sessions"))?
            .remove(name);
        self.mcp.remove(name)?;
        self.set_mcp_status(name, |status| {
            *status = McpServerStatus {
                name: name.into(),
                state: McpServerState::Connecting,
                ..Default::default()
            }
        })?;
        let session = match a206_mcp::spawn(
            &config,
            project_root.as_deref(),
            &state_root,
            a206_mcp::Limits::default(),
        )
        .await
        {
            Ok(session) => session,
            Err(error) => {
                let error = GatewayError::from(error);
                let report = crate::error::report(&error);
                self.set_mcp_status(name, |status| {
                    status.state = McpServerState::Failed;
                    status.failure = Some(report);
                })?;
                return Err(error);
            }
        };
        // The user may have changed or disconnected the server while it started.
        let current = self.mcp_config(name).ok();
        let stopped = self
            .mcp
            .stopped
            .lock()
            .map_err(poisoned("MCP sessions"))?
            .contains(name);
        if stopped || current.as_ref() != Some(&config) {
            drop(session);
            self.set_mcp_status(name, |status| status.state = McpServerState::Disconnected)
                .ignore_missing()?;
            return Err(GatewayError::Conflict(
                "the MCP server was changed or disconnected while it started; connect it again",
            ));
        }
        let server = session.server().map(str::to_owned);
        let tools = session.tools().to_vec();
        self.mcp
            .sessions
            .lock()
            .map_err(poisoned("MCP sessions"))?
            .insert(name.to_owned(), Arc::new(session));
        self.set_mcp_status(name, |status| {
            status.state = McpServerState::Connected;
            status.server = server;
            status.tools = tools;
            status.failure = None;
        })
    }

    /// Stop a server; it stays stopped until the user connects it again.
    pub(super) fn disconnect_mcp(&self, name: &str) -> Result<()> {
        self.mcp_config(name)?;
        self.mcp
            .stopped
            .lock()
            .map_err(poisoned("MCP sessions"))?
            .insert(name.to_owned());
        self.mcp.remove(name)?;
        self.set_mcp_status(name, |status| {
            status.state = McpServerState::Disconnected;
            status.failure = None;
        })
    }

    /// MCP tools `thread` may be offered, for the run engine to present to a
    /// model. Enabled servers the user has not disconnected start on first use;
    /// a server that fails to start is reported in `unavailable`, never dropped.
    pub async fn mcp_offers(&self, thread: Id) -> Result<McpOffers> {
        let (caller, servers) = {
            let store = self.lock()?;
            (
                store.thread(thread)?.clone(),
                store.state.settings.mcp_servers.clone(),
            )
        };
        let mut offers = McpOffers::default();
        for server in servers {
            if a206_mcp::server_callable(&server, &caller).is_err() {
                continue;
            }
            let session = self.session(&server.name)?;
            let stopped = self
                .mcp
                .stopped
                .lock()
                .map_err(poisoned("MCP sessions"))?
                .contains(&server.name);
            let session = match session {
                Some(session) => session,
                None if stopped => continue,
                None => match self.connect_mcp(&server.name).await {
                    Ok(()) => match self.session(&server.name)? {
                        Some(session) => session,
                        None => continue,
                    },
                    Err(error) => {
                        let report = crate::error::handle("connect_mcp_server", &error);
                        offers.unavailable.push((server.name.clone(), report));
                        continue;
                    }
                },
            };
            offers.tools.extend(
                session
                    .tools()
                    .iter()
                    .filter(|tool| server.policy(&tool.name) != ToolPolicy::Deny)
                    .map(|tool| McpOffer {
                        server: server.name.clone(),
                        tool: tool.clone(),
                        policy: server.policy(&tool.name),
                    }),
            );
        }
        Ok(offers)
    }

    /// Call one MCP tool for `thread`. Authorization uses the thread's current
    /// grant and the server's current entry, never what was offered earlier.
    pub async fn call_mcp_tool(
        &self,
        thread: Id,
        server: &str,
        tool: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
        approval: Approval,
    ) -> Result<ToolOutput> {
        let (caller, config) = {
            let store = self.lock()?;
            let config = store
                .state
                .settings
                .mcp_servers
                .iter()
                .find(|s| s.name == server)
                .cloned()
                .ok_or(GatewayError::NotFound("MCP server is not configured"))?;
            (store.thread(thread)?.clone(), config)
        };
        a206_mcp::authorize(&config, &caller, tool, approval)?;
        let session = self.session(server)?.ok_or(McpError::NotConnected)?;
        let timeout = config.timeout_seconds.map(Duration::from_secs);
        match session.call(tool, arguments, timeout).await {
            Err(McpError::Service(
                error
                @ (rmcp::ServiceError::TransportClosed | rmcp::ServiceError::TransportSend(_)),
            )) => {
                // The session is gone: the server exited or its pipe broke.
                let error = GatewayError::from(McpError::Service(error));
                let report = crate::error::report(&error);
                self.mcp.remove(server)?;
                self.set_mcp_status(server, |status| {
                    status.state = McpServerState::Failed;
                    status.failure = Some(report);
                })
                .ignore_missing()?;
                Err(error)
            }
            result => Ok(result?),
        }
    }

    fn session(&self, name: &str) -> Result<Option<Arc<Session>>> {
        Ok(self
            .mcp
            .sessions
            .lock()
            .map_err(poisoned("MCP sessions"))?
            .get(name)
            .cloned())
    }
}

trait IgnoreMissing {
    /// A server removed from settings meanwhile has no status left to update.
    fn ignore_missing(self) -> Result<()>;
}
impl IgnoreMissing for Result<()> {
    fn ignore_missing(self) -> Result<()> {
        match self {
            Err(GatewayError::NotFound(_)) => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_protocol::Request;

    fn entry(root: &std::path::Path) -> McpServerConfig {
        let executable = root.join("server");
        std::fs::write(&executable, b"").unwrap();
        McpServerConfig {
            name: "tools".into(),
            enabled: false,
            executable: executable.to_str().unwrap().into(),
            runtime_root: root.to_str().unwrap().into(),
            args: vec![],
            env: Default::default(),
            capabilities: Default::default(),
            project: None,
            default_policy: ToolPolicy::Ask,
            tools: Default::default(),
            timeout_seconds: None,
        }
    }
    async fn send(g: &Gateway, body: Command) -> Reply {
        g.request(Request {
            id: Id::new_v4(),
            body,
        })
        .await
        .body
    }
    fn kind(reply: Reply) -> ErrorKind {
        match reply {
            Reply::Error(report) => report.kind,
            other => panic!("expected a failure, got {other:?}"),
        }
    }
    async fn save(g: &Gateway, servers: Vec<McpServerConfig>) -> Reply {
        let mut settings = g.snapshot().unwrap().settings;
        settings.mcp_servers = servers;
        send(g, Command::SaveSettings(settings)).await
    }

    #[tokio::test]
    async fn entries_are_validated_and_disabled_servers_never_start() {
        let state = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let g = Gateway::open(state.path()).unwrap();
        let mut bad = entry(install.path());
        bad.name = "Bad_Name".into();
        assert_eq!(kind(save(&g, vec![bad]).await), ErrorKind::InvalidRequest);
        let mut unbound = entry(install.path());
        unbound.capabilities = [Capability::ReadWorkspace].into();
        unbound.project = Some(Id::new_v4());
        assert_eq!(
            kind(save(&g, vec![unbound]).await),
            ErrorKind::InvalidRequest
        );

        assert_eq!(save(&g, vec![entry(install.path())]).await, Reply::Ok);
        let status = &g.snapshot().unwrap().mcp_servers;
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].state, McpServerState::Disconnected);
        let connect = |name: &str| Command::ConnectMcpServer { name: name.into() };
        assert_eq!(
            kind(send(&g, connect("tools")).await),
            ErrorKind::PermissionDenied
        );
        assert_eq!(kind(send(&g, connect("other")).await), ErrorKind::NotFound);
        assert_eq!(g.snapshot().unwrap().mcp_servers[0].failure, None);
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn a_server_that_cannot_be_isolated_fails_visibly() {
        let state = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let g = Gateway::open(state.path()).unwrap();
        let mut server = entry(install.path());
        server.enabled = true;
        assert_eq!(save(&g, vec![server]).await, Reply::Ok);
        let reply = send(
            &g,
            Command::ConnectMcpServer {
                name: "tools".into(),
            },
        )
        .await;
        assert_eq!(kind(reply), ErrorKind::Unavailable);
        let status = g.snapshot().unwrap().mcp_servers.remove(0);
        assert_eq!(status.state, McpServerState::Failed);
        assert_eq!(status.failure.unwrap().kind, ErrorKind::Unavailable);

        // A restarted gateway reports no session from before.
        drop(g);
        let g = Gateway::open(state.path()).unwrap();
        let status = g.snapshot().unwrap().mcp_servers.remove(0);
        assert_eq!(status.state, McpServerState::Disconnected);
    }

    #[tokio::test]
    async fn calls_and_offers_follow_the_threads_grant() {
        let state = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let g = Gateway::open(state.path()).unwrap();
        let tool = Id::new_v4().to_string();
        let mut server = entry(install.path());
        server.enabled = true;
        server.capabilities = [Capability::Network].into();
        assert_eq!(save(&g, vec![server.clone()]).await, Reply::Ok);
        let Reply::Thread(thread) = send(
            &g,
            Command::CreateThread {
                title: "Research".into(),
                mode: Mode::Research,
            },
        )
        .await
        else {
            panic!("thread created");
        };
        let call =
            |approval| g.call_mcp_tool(thread.id, "tools", &tool, Default::default(), approval);
        assert!(matches!(
            call(Approval::Approved).await,
            Err(GatewayError::Mcp(McpError::Denied {
                capability: Capability::Network
            }))
        ));
        // Without the server's authority, nothing is offered and nothing starts.
        assert_eq!(g.mcp_offers(thread.id).await.unwrap(), McpOffers::default());
        assert_eq!(
            g.snapshot().unwrap().mcp_servers[0].state,
            McpServerState::Disconnected
        );

        g.lock()
            .unwrap()
            .thread_mut(thread.id)
            .unwrap()
            .grant
            .capabilities = [Capability::Network].into();
        assert!(matches!(
            call(Approval::NotRequested).await,
            Err(GatewayError::Mcp(McpError::ApprovalRequired))
        ));
        assert!(matches!(
            call(Approval::Approved).await,
            Err(GatewayError::Mcp(McpError::NotConnected))
        ));
        server.tools.insert(tool.clone(), ToolPolicy::Deny);
        assert_eq!(save(&g, vec![server]).await, Reply::Ok);
        assert!(matches!(
            call(Approval::Approved).await,
            Err(GatewayError::Mcp(McpError::ToolDenied))
        ));
    }
}
