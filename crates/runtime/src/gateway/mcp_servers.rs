//! Gateway side of the MCP client. The gateway owns every server session and
//! offers a server's tools to a thread only within that thread's grants; see
//! [`a206_mcp::authorize`]. Sessions are runtime state: a gateway restart
//! leaves every server disconnected until it is used or connected again.
use super::{Gateway, GatewayError, Result, poisoned};
use crate::error::check;
use crate::store::Store;
use a206_core::*;
use a206_mcp::{Approval, McpError, Session, ToolOutput};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::watch;

pub(super) struct McpServers {
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    /// Servers starting now. Each start holds the sender, so waiters learn
    /// when it ends, and its failure if it failed.
    starting: Mutex<BTreeMap<String, watch::Receiver<Option<ErrorReport>>>>,
    /// Servers the user disconnected; they start again only on request.
    stopped: Mutex<BTreeSet<String>>,
    /// Signalled after every state change, so in-flight calls re-check
    /// whether their thread may still make them.
    changes: watch::Sender<()>,
}
impl Default for McpServers {
    fn default() -> Self {
        Self {
            sessions: Default::default(),
            starting: Default::default(),
            stopped: Default::default(),
            changes: watch::Sender::new(()),
        }
    }
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
    /// Grants, settings or threads may have changed.
    pub(super) fn state_changed(&self) {
        self.changes.send_replace(());
    }
    fn is_stopped(&self, name: &str) -> Result<bool> {
        Ok(self
            .stopped
            .lock()
            .map_err(poisoned("MCP sessions"))?
            .contains(name))
    }
    /// Claim the right to start `name`, or learn of the start under way.
    fn claim(&self, name: &str) -> Result<Claim<'_>> {
        let mut starting = self.starting.lock().map_err(poisoned("MCP sessions"))?;
        if let Some(done) = starting.get(name) {
            return Ok(Claim::Wait(done.clone()));
        }
        let (outcome, done) = watch::channel(None);
        starting.insert(name.to_owned(), done);
        Ok(Claim::Start(Starting {
            servers: self,
            name: name.to_owned(),
            outcome,
        }))
    }
}

enum Claim<'a> {
    Start(Starting<'a>),
    /// Carries the other start's failure, or closes when it succeeds.
    Wait(watch::Receiver<Option<ErrorReport>>),
}

/// Releases a server's start claim however the start ends.
struct Starting<'a> {
    servers: &'a McpServers,
    name: String,
    outcome: watch::Sender<Option<ErrorReport>>,
}
impl Drop for Starting<'_> {
    fn drop(&mut self) {
        if let Ok(mut starting) = self.servers.starting.lock() {
            starting.remove(&self.name);
        }
    }
}

/// What discovery found for one server.
enum Discovered {
    Session(Arc<Session>),
    /// The user disconnected it.
    Stopped,
    /// A start this discovery waited for failed; its caller recorded why.
    Failed(ErrorReport),
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
        set_status(&self.store, name, update)
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

    /// Start an enabled server in its sandbox at the user's request.
    pub(super) async fn connect_mcp(&self, name: &str) -> Result<()> {
        self.mcp_config(name)?;
        let Claim::Start(claim) = self.mcp.claim(name)? else {
            return Err(GatewayError::Conflict(
                "this MCP server is already starting",
            ));
        };
        self.mcp
            .stopped
            .lock()
            .map_err(poisoned("MCP sessions"))?
            .remove(name);
        self.start_mcp(name, claim).await
    }

    /// Start a claimed server and record its tools. Callers waiting on the
    /// claim learn of a failure.
    async fn start_mcp(&self, name: &str, claim: Starting<'_>) -> Result<()> {
        let result = self.start_claimed(name).await;
        if let Err(error) = &result {
            claim
                .outcome
                .send_replace(Some(crate::error::report(error)));
        }
        result
    }

    async fn start_claimed(&self, name: &str) -> Result<()> {
        let config = self.mcp_config(name)?;
        check(config.enabled, McpError::Disabled)?;
        let project_root = config.project.map(|id| self.project_root(id)).transpose()?;
        let state_root = self.lock()?.root.clone();
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
        let server = session.server().map(str::to_owned);
        let tools = session.tools().to_vec();
        let session = Arc::new(session);
        {
            // Under the state lock, which settings saves, disconnects and
            // session-end handling also take, so none interleaves with this.
            let mut store = self.lock()?;
            let current = store
                .state
                .settings
                .mcp_servers
                .iter()
                .find(|s| s.name == name);
            // The user may have changed or disconnected the server while it started.
            if self.mcp.is_stopped(name)? || current != Some(&config) {
                drop(store);
                session.stop();
                self.set_mcp_status(name, |status| status.state = McpServerState::Disconnected)
                    .ignore_missing()?;
                return Err(GatewayError::Conflict(
                    "the MCP server was changed or disconnected while it started; connect it again",
                ));
            }
            self.mcp
                .sessions
                .lock()
                .map_err(poisoned("MCP sessions"))?
                .insert(name.to_owned(), session.clone());
            update_status(&mut store, name, |status| {
                status.state = McpServerState::Connected;
                status.server = server;
                status.tools = tools;
                status.failure = None;
            })?;
        }
        self.watch_session(name, &session);
        Ok(())
    }

    /// Mark the server failed if its session ends without the gateway
    /// stopping it: the process exited or closed its pipe.
    fn watch_session(&self, name: &str, session: &Arc<Session>) {
        let closed = session.closed();
        let session = Arc::downgrade(session);
        // Weak, so a dropped gateway does not stay alive for its sessions.
        let servers = Arc::downgrade(&self.mcp);
        let store = self.store.clone();
        let name = name.to_owned();
        // Detached on purpose: it ends when the session does, and every
        // session ends when stopped, replaced or dropped with the gateway.
        tokio::spawn(async move {
            closed.await;
            if let Err(error) = session_ended(&servers, &store, &name, &session) {
                crate::error::handle("mcp_session_ended", &error);
            }
        });
    }

    /// A live session for an enabled server, starting it if needed.
    /// Concurrent callers share one start and its outcome.
    async fn ensure_session(&self, name: &str) -> Result<Discovered> {
        let mut attempted = false;
        loop {
            if let Some(session) = self.session(name)? {
                return Ok(Discovered::Session(session));
            }
            if self.mcp.is_stopped(name)? {
                return Ok(Discovered::Stopped);
            }
            match self.mcp.claim(name)? {
                Claim::Wait(mut done) => {
                    let entry = self.mcp_config(name).ok();
                    // A failure is sent; a success just closes the channel.
                    if done.changed().await.is_ok()
                        && let Some(report) = done.borrow_and_update().clone()
                    {
                        // A start the user cut short by disconnecting or
                        // changing the server is not this server's failure.
                        if self.mcp.is_stopped(name)? {
                            return Ok(Discovered::Stopped);
                        }
                        if self.mcp_config(name).ok() != entry {
                            continue;
                        }
                        return Ok(Discovered::Failed(report));
                    }
                    attempted = true;
                }
                // Started, but the session was gone by the time we looked.
                Claim::Start(_) if attempted => return Err(McpError::Exited.into()),
                Claim::Start(claim) => {
                    attempted = true;
                    self.start_mcp(name, claim).await?;
                }
            }
        }
    }

    /// Stop a server; it stays stopped until the user connects it again.
    pub(super) fn disconnect_mcp(&self, name: &str) -> Result<()> {
        self.mcp_config(name)?;
        {
            // Under the state lock, so a start in progress either sees this
            // or registers its session before the removal below.
            let _store = self.lock()?;
            self.mcp
                .stopped
                .lock()
                .map_err(poisoned("MCP sessions"))?
                .insert(name.to_owned());
        }
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
            let session = match self.ensure_session(&server.name).await {
                Ok(Discovered::Session(session)) => session,
                Ok(Discovered::Stopped) => continue,
                Ok(Discovered::Failed(report)) => {
                    offers.unavailable.push((server.name.clone(), report));
                    continue;
                }
                Err(error) => {
                    let report = crate::error::handle("connect_mcp_server", &error);
                    offers.unavailable.push((server.name.clone(), report));
                    continue;
                }
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

    /// Whether `thread` may call `tool` now, under its current grant and the
    /// server's current entry; returns the entry.
    fn authorize_mcp(
        &self,
        thread: Id,
        server: &str,
        tool: &str,
        approval: Approval,
    ) -> Result<McpServerConfig> {
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
        Ok(config)
    }

    /// Call one MCP tool for `thread`. Authorization uses the thread's current
    /// grant and the server's current entry, never what was offered earlier,
    /// and is checked again after every state change while the call runs: a
    /// call the thread may no longer make is cancelled with
    /// [`McpError::Revoked`].
    pub async fn call_mcp_tool(
        &self,
        thread: Id,
        server: &str,
        tool: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
        approval: Approval,
    ) -> Result<ToolOutput> {
        // Subscribe first, so no change between the check and the call is missed.
        let mut changes = self.mcp.changes.subscribe();
        let config = self.authorize_mcp(thread, server, tool, approval)?;
        let session = self.session(server)?.ok_or(McpError::NotConnected)?;
        let timeout = config.timeout_seconds.map(Duration::from_secs);
        let revoked = async {
            while changes.changed().await.is_ok() {
                // The running call has the authority of the entry it started
                // under; any change to that entry revokes it.
                let current = self
                    .authorize_mcp(thread, server, tool, approval)
                    .and_then(|now| check(now == config, GatewayError::Mcp(McpError::Revoked)));
                if let Err(error) = current {
                    tracing::info!(
                        error = &error as &dyn std::error::Error,
                        %thread,
                        server,
                        tool,
                        "cancelling MCP call"
                    );
                    return;
                }
            }
            // The gateway is gone; its sessions end with it.
            std::future::pending().await
        };
        match session.call_until(tool, arguments, timeout, revoked).await {
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

    /// The server's live session. A session that ended is dropped here too,
    /// in case its watcher has not run yet.
    fn session(&self, name: &str) -> Result<Option<Arc<Session>>> {
        let session = self
            .mcp
            .sessions
            .lock()
            .map_err(poisoned("MCP sessions"))?
            .get(name)
            .cloned();
        match session {
            Some(session) if session.is_closed() => {
                session_ended(
                    &Arc::downgrade(&self.mcp),
                    &self.store,
                    name,
                    &Arc::downgrade(&session),
                )?;
                Ok(None)
            }
            session => Ok(session),
        }
    }
}

fn set_status(
    store: &Mutex<Store>,
    name: &str,
    update: impl FnOnce(&mut McpServerStatus),
) -> Result<()> {
    update_status(&mut *store.lock().map_err(poisoned("state"))?, name, update)
}

fn update_status(
    store: &mut Store,
    name: &str,
    update: impl FnOnce(&mut McpServerStatus),
) -> Result<()> {
    let Some(status) = store.state.mcp_servers.iter_mut().find(|s| s.name == name) else {
        return Err(GatewayError::NotFound("MCP server is not configured"));
    };
    update(status);
    store.persist(Some(EventKind::McpServerChanged { name: name.into() }))?;
    Ok(())
}

/// Forget `session` and mark its server failed, unless the gateway already
/// stopped or replaced it.
fn session_ended(
    servers: &Weak<McpServers>,
    store: &Mutex<Store>,
    name: &str,
    session: &Weak<Session>,
) -> Result<()> {
    let Some(servers) = servers.upgrade() else {
        return Ok(());
    };
    // The state lock first, as in `start_claimed`, so a replacement session
    // cannot be recorded between this removal and the status written for it.
    let mut store = store.lock().map_err(poisoned("state"))?;
    {
        let mut sessions = servers.sessions.lock().map_err(poisoned("MCP sessions"))?;
        match sessions.get(name) {
            Some(current) if std::ptr::eq(Arc::as_ptr(current), session.as_ptr()) => {
                sessions.remove(name);
            }
            _ => return Ok(()),
        }
    }
    let report = crate::error::report(&GatewayError::from(McpError::Exited));
    update_status(&mut store, name, |status| {
        status.state = McpServerState::Failed;
        status.failure = Some(report);
    })
    .ignore_missing()
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

    /// Fails before any sandbox starts, so it runs on every platform; this
    /// test binary cannot serve as the Linux sandbox helper.
    #[tokio::test]
    async fn a_server_that_cannot_start_fails_visibly() {
        let state = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let g = Gateway::open(state.path()).unwrap();
        let mut server = entry(install.path());
        server.enabled = true;
        assert_eq!(save(&g, vec![server.clone()]).await, Reply::Ok);
        std::fs::remove_file(&server.executable).unwrap();
        let reply = send(
            &g,
            Command::ConnectMcpServer {
                name: "tools".into(),
            },
        )
        .await;
        assert_eq!(kind(reply), ErrorKind::NotFound);
        let status = g.snapshot().unwrap().mcp_servers.remove(0);
        assert_eq!(status.state, McpServerState::Failed);
        assert_eq!(status.failure.unwrap().kind, ErrorKind::NotFound);

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

    /// A minimal in-memory MCP server offering one tool, `wait`, that never
    /// answers. Cancellations it receives are forwarded; dropping `close`
    /// ends the connection as if the server exited.
    async fn fake_session() -> (
        Session,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (client, server) = tokio::io::duplex(1 << 16);
        let (cancelled_tx, cancelled) = tokio::sync::mpsc::unbounded_channel();
        let (close, closed) = tokio::sync::oneshot::channel::<()>();
        // Ends when the test drops `close` or the client hangs up.
        tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = tokio::io::BufReader::new(read).lines();
            let serve = async {
                while let Ok(Some(line)) = lines.next_line().await {
                    let message: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let result = match message["method"].as_str() {
                        Some("initialize") => serde_json::json!({
                            "protocolVersion": message["params"]["protocolVersion"],
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "fake", "version": "1" },
                        }),
                        Some("tools/list") => serde_json::json!({
                            "tools": [{ "name": "wait", "inputSchema": { "type": "object" } }],
                        }),
                        Some("notifications/cancelled") => {
                            cancelled_tx.send(message["params"].clone()).unwrap();
                            continue;
                        }
                        _ => continue,
                    };
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0", "id": message["id"], "result": result,
                    });
                    write
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                }
            };
            tokio::select! { () = serve => {}, _closed = closed => {} }
        });
        let (read, write) = tokio::io::split(client);
        let session = Session::start((read, write), None, a206_mcp::Limits::default())
            .await
            .unwrap();
        (session, cancelled, close)
    }

    /// A gateway with an enabled `tools` server connected to [`fake_session`]
    /// and a thread allowed to call its `wait` tool.
    async fn connected() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Gateway,
        Thread,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let state = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let g = Gateway::open(state.path()).unwrap();
        let mut server = entry(install.path());
        server.enabled = true;
        server.capabilities = [Capability::Network].into();
        server.default_policy = ToolPolicy::Allow;
        assert_eq!(save(&g, vec![server]).await, Reply::Ok);
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
        g.lock()
            .unwrap()
            .thread_mut(thread.id)
            .unwrap()
            .grant
            .capabilities = [Capability::Network].into();
        let (session, cancelled, close) = fake_session().await;
        let claim = match g.mcp.claim("tools").unwrap() {
            Claim::Start(claim) => claim,
            Claim::Wait(_) => panic!("nothing else is starting"),
        };
        let session = Arc::new(session);
        g.mcp
            .sessions
            .lock()
            .unwrap()
            .insert("tools".into(), session.clone());
        g.set_mcp_status("tools", |status| status.state = McpServerState::Connected)
            .unwrap();
        g.watch_session("tools", &session);
        drop(claim);
        (state, install, g, *thread, cancelled, close)
    }

    #[tokio::test]
    async fn revoking_a_grant_cancels_a_running_call() {
        let (_state, _install, g, thread, mut cancelled, _close) = connected().await;
        let call = g.call_mcp_tool(
            thread.id,
            "tools",
            "wait",
            Default::default(),
            Approval::NotRequested,
        );
        let revoke = async {
            // Unrelated changes leave the call running.
            let renamed = send(
                &g,
                Command::RenameThread {
                    id: thread.id,
                    title: "Renamed".into(),
                },
            )
            .await;
            assert!(!matches!(renamed, Reply::Error(_)), "{renamed:?}");
            // Chat mode cannot hold Network, so the thread loses it.
            send(
                &g,
                Command::ConfigureThread {
                    id: thread.id,
                    mode: Mode::Chat,
                    model: None,
                    subagent_models: Default::default(),
                },
            )
            .await
        };
        let (result, configured) = tokio::join!(call, revoke);
        assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
        assert!(matches!(result, Err(GatewayError::Mcp(McpError::Revoked))));
        // The server is told to stop, and its session stays up for others.
        let notice = cancelled.recv().await.unwrap();
        assert_eq!(notice["reason"], McpError::Revoked.to_string());
        assert_eq!(
            g.snapshot().unwrap().mcp_servers[0].state,
            McpServerState::Connected
        );
    }

    #[tokio::test]
    async fn a_server_that_exits_is_marked_failed() {
        let (_state, _install, g, thread, _cancelled, close) = connected().await;
        let session = g.session("tools").unwrap().unwrap();
        drop(close);
        session.closed().await;
        drop(session);
        // Offers do not use the dead session, whether or not its watcher ran.
        let offers = g.mcp_offers(thread.id).await.unwrap();
        assert!(offers.tools.is_empty());
        assert!(g.mcp.sessions.lock().unwrap().is_empty());
        let status = g.snapshot().unwrap().mcp_servers.remove(0);
        assert_eq!(status.state, McpServerState::Failed);
        assert!(status.failure.is_some());
    }

    #[tokio::test]
    async fn concurrent_discovery_shares_one_start() {
        let state = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let g = Gateway::open(state.path()).unwrap();
        let mut server = entry(install.path());
        server.enabled = true;
        assert_eq!(save(&g, vec![server]).await, Reply::Ok);
        let Reply::Thread(thread) = send(
            &g,
            Command::CreateThread {
                title: "Chat".into(),
                mode: Mode::Chat,
            },
        )
        .await
        else {
            panic!("thread created");
        };
        // Hold a start open so both callers must wait for it, then fail it.
        let Claim::Start(claim) = g.mcp.claim("tools").unwrap() else {
            panic!("nothing else is starting");
        };
        let failure = crate::error::report(&GatewayError::from(McpError::Unsupported))
            .with_correlation(Id::new_v4());
        let release = async {
            tokio::task::yield_now().await;
            claim.outcome.send_replace(Some(failure.clone()));
            drop(claim);
        };
        let (first, second, ()) =
            tokio::join!(g.mcp_offers(thread.id), g.mcp_offers(thread.id), release);
        // Both callers report that start's failure; neither starts the server again.
        for offers in [first.unwrap(), second.unwrap()] {
            assert_eq!(offers.unavailable, [("tools".to_owned(), failure.clone())]);
        }
        assert_eq!(
            g.snapshot().unwrap().mcp_servers[0].state,
            McpServerState::Disconnected
        );

        // A start cut short by a disconnect leaves the server out, not failed.
        let Claim::Start(claim) = g.mcp.claim("tools").unwrap() else {
            panic!("nothing else is starting");
        };
        let disconnect = async {
            tokio::task::yield_now().await;
            g.disconnect_mcp("tools").unwrap();
            claim.outcome.send_replace(Some(failure.clone()));
            drop(claim);
        };
        let (offers, ()) = tokio::join!(g.mcp_offers(thread.id), disconnect);
        assert_eq!(offers.unwrap(), McpOffers::default());
    }
}
