//! SQLite WAL snapshot, event journal and durable request deduplication.
use crate::provider::now;
use a206_core::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

/// Why durable gateway state could not be opened, read or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not prepare the data directory")]
    Directory(#[source] std::io::Error),
    #[error("a gateway already owns this data directory")]
    Owned(#[source] std::io::Error),
    #[error("state database failed")]
    Database(#[from] rusqlite::Error),
    #[error("stored state is unreadable")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("state could not be encoded")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("could not read settings.toml")]
    ReadSettings(#[source] std::io::Error),
    #[error("settings.toml is invalid")]
    Settings(#[source] toml::de::Error),
    #[error("the bundled extensions are invalid")]
    Builtin(#[source] a206_extensions::ManifestError),
    #[error("stored provider presets are invalid")]
    Presets(#[source] crate::provider::PresetError),
    #[error("snapshot capacity reached; paged history is not implemented yet")]
    Full,
    #[error("event cursor unavailable; request a fresh snapshot")]
    Cursor,
    #[error("request id reused with a different command")]
    ReusedId,
    #[error("thread not found")]
    ThreadNotFound,
}
impl crate::error::Classified for StoreError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Owned(_) | Self::ReusedId | Self::Cursor => ErrorKind::Conflict,
            Self::Settings(_) | Self::Presets(_) => ErrorKind::InvalidRequest,
            Self::Encode(_) | Self::Builtin(_) => ErrorKind::Internal,
            Self::ThreadNotFound => ErrorKind::NotFound,
            Self::Directory(_)
            | Self::Database(_)
            | Self::Decode(_)
            | Self::ReadSettings(_)
            | Self::Full => ErrorKind::Storage,
        })
    }
}
type Result<T, E = StoreError> = std::result::Result<T, E>;
/// Largest snapshot kept: one frame, less room for the envelope it travels in.
const LIMIT: usize = a206_protocol::MAX_FRAME - 65_536;

/// Open the single profile lock used by both startup and offline stop intent.
pub(crate) fn profile_lock(root: &Path) -> std::io::Result<std::fs::File> {
    lock_file(root, "gateway.lock")
}
/// Internal fixed lock filenames only; these never come from a command or plugin.
pub(crate) fn lock_file(root: &Path, name: &str) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(name))
}
pub struct Store {
    _lock: std::fs::File,
    db: Connection,
    pub state: Snapshot,
    pub root: PathBuf,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let lock = profile_lock(root).map_err(StoreError::Directory)?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(StoreError::Owned)?;
        let db = Connection::open(root.join("state.sqlite3"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS snapshot(id INTEGER PRIMARY KEY CHECK(id=1), payload BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT, payload BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS requests(id TEXT PRIMARY KEY, request BLOB NOT NULL, reply BLOB);
            CREATE TABLE IF NOT EXISTS extension_state(extension TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(extension, key));")?;
        let data: Option<Vec<u8>> = db
            .query_row("SELECT payload FROM snapshot WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()?;
        let state = if let Some(data) = data {
            rmp_serde::from_slice(&data)?
        } else {
            let settings = if root.join("settings.toml").exists() {
                toml::from_str(
                    &std::fs::read_to_string(root.join("settings.toml"))
                        .map_err(StoreError::ReadSettings)?,
                )
                .map_err(StoreError::Settings)?
            } else {
                Settings::default()
            };
            Snapshot {
                settings,
                threads: vec![],
                projects: vec![],
                catalogs: vec![],
                downloads: vec![],
                plugins: vec![],
                last_event: 0,
                managed_joshua_provider: None,
                mcp_servers: vec![],
            }
        };
        let mut store = Self {
            _lock: lock,
            db,
            state,
            root: root.to_owned(),
        };
        crate::provider::validate_presets(&store.state.settings.provider_presets)
            .map_err(StoreError::Presets)?;
        store.state.plugins = bundled_plugins(&store.state.plugins)?;
        // A fresh gateway has no owned child. Retire its persisted connection, not
        // arbitrary user-configured endpoints that happen to have the same URL.
        store.state.clear_managed_joshua();
        // A fresh gateway has no MCP sessions: every configured server starts
        // disconnected, and nothing from an earlier session counts as connected.
        store.state.mcp_servers = store
            .state
            .settings
            .mcp_servers
            .iter()
            .map(|server| McpServerStatus {
                name: server.name.clone(),
                ..Default::default()
            })
            .collect();
        for thread in &mut store.state.threads {
            // No run survives a restart, so neither do its approvals or calls.
            thread.approvals.clear();
            for call in thread.messages.iter_mut().flat_map(|m| &mut m.tool_calls) {
                if !call.state.is_final() {
                    let report = a206_core::ErrorReport::new(
                        a206_core::ErrorKind::Unavailable,
                        "The gateway restarted before this tool call finished.",
                    );
                    call.state = a206_core::ToolCallState::Cancelled;
                    call.output = Some(report.detail());
                    call.error = Some(report);
                }
            }
            if thread.state == RunState::Running {
                thread.state = RunState::Failed;
                thread.error = Some(a206_core::ErrorReport::new(
                    a206_core::ErrorKind::Unavailable,
                    "The gateway restarted while this run was in progress.",
                ));
            }
        }
        for download in &mut store.state.downloads {
            if matches!(
                download.state,
                DownloadState::Running | DownloadState::Queued
            ) {
                download.state = DownloadState::Failed;
                download.fail(a206_core::ErrorReport::new(
                    a206_core::ErrorKind::Cancelled,
                    "Gateway restarted; partial file retained. Start a new transfer to retry.",
                ));
            }
        }
        store.persist(None)?;
        Ok(store)
    }
    pub fn persist(&mut self, kind: Option<EventKind>) -> Result<()> {
        let result = self.persist_inner(kind);
        if result.is_err() {
            // Restore the committed snapshot after a rejected or failed mutation.
            let data: Option<Vec<u8>> = self
                .db
                .query_row("SELECT payload FROM snapshot WHERE id=1", [], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(data) = data {
                self.state = rmp_serde::from_slice(&data)?;
            }
        }
        result
    }
    /// Bytes the snapshot can still grow by before a persist is refused as
    /// [`StoreError::Full`].
    pub fn remaining(&self) -> Result<usize> {
        Ok(LIMIT.saturating_sub(rmp_serde::to_vec_named(&self.state)?.len() + 1))
    }
    fn persist_inner(&mut self, kind: Option<EventKind>) -> Result<()> {
        if rmp_serde::to_vec_named(&self.state)?.len() >= LIMIT {
            return Err(StoreError::Full);
        }
        let tx = self.db.transaction()?;
        if let Some(kind) = kind {
            let seq = self.state.last_event + 1;
            let event = Event {
                sequence: seq,
                at: now(),
                kind,
            };
            tx.execute(
                "INSERT INTO events(seq,payload) VALUES(?1,?2)",
                params![seq, rmp_serde::to_vec_named(&event)?],
            )?;
            self.state.last_event = seq;
            tx.execute(
                "DELETE FROM events WHERE seq < ?1",
                [seq.saturating_sub(10_000)],
            )?;
        }
        tx.execute("INSERT INTO snapshot(id,payload) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", [rmp_serde::to_vec_named(&self.state)?])?;
        tx.commit()?;
        Ok(())
    }
    pub fn events(&self, after: u64) -> Result<Vec<Event>> {
        if after > self.state.last_event || self.state.last_event.saturating_sub(after) > 10_000 {
            return Err(StoreError::Cursor);
        }
        let mut stmt = self
            .db
            .prepare("SELECT payload FROM events WHERE seq > ?1 ORDER BY seq LIMIT 512")?;
        let rows = stmt.query_map([after], |row| row.get::<_, Vec<u8>>(0))?;
        rows.map(|r| Ok(rmp_serde::from_slice(&r?)?)).collect()
    }
    /// A duplicate pending request is indeterminate, never silently executed a second time.
    pub fn begin(&mut self, id: Id, command: &Command) -> Result<Option<Reply>> {
        let body = rmp_serde::to_vec_named(command)?;
        let previous: Option<(Vec<u8>, Option<Vec<u8>>)> = self
            .db
            .query_row(
                "SELECT request,reply FROM requests WHERE id=?1",
                [id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((request, reply)) = previous {
            if request != body {
                return Err(StoreError::ReusedId);
            }
            return Ok(Some(if let Some(reply) = reply {
                rmp_serde::from_slice(&reply)?
            } else {
                let report = a206_core::ErrorReport::new(
                    a206_core::ErrorKind::Conflict,
                    "request was already accepted; outcome is pending or unknown; refresh state before retrying",
                )
                .with_correlation(id);
                crate::error::record("replay", &report);
                Reply::Error(report)
            }));
        }
        self.db.execute(
            "INSERT INTO requests(id,request) VALUES(?1,?2)",
            params![id.to_string(), body],
        )?;
        Ok(None)
    }
    pub fn finish(&self, id: Id, reply: &Reply) -> Result<()> {
        self.db.execute(
            "UPDATE requests SET reply=?1 WHERE id=?2",
            params![rmp_serde::to_vec_named(reply)?, id.to_string()],
        )?;
        Ok(())
    }
    /// This extension's persisted value for `key`.
    pub fn extension_state(&self, extension: &str, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row(
                "SELECT value FROM extension_state WHERE extension=?1 AND key=?2",
                params![extension, key],
                |row| row.get(0),
            )
            .optional()?)
    }
    /// Replace or delete one entry, refusing writes past `quota`.
    pub fn put_extension_state(
        &mut self,
        extension: &str,
        key: &str,
        value: Option<&[u8]>,
        quota: StateQuota,
    ) -> Result<StatePut> {
        let tx = self.db.transaction()?;
        tx.execute(
            "DELETE FROM extension_state WHERE extension=?1 AND key=?2",
            params![extension, key],
        )?;
        if let Some(value) = value {
            let (entries, bytes): (u64, u64) = tx.query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(key) + LENGTH(value)), 0) FROM extension_state WHERE extension=?1",
                [extension],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if entries >= quota.entries || bytes + (key.len() + value.len()) as u64 > quota.bytes {
                // Dropping the transaction keeps the previous value.
                return Ok(StatePut::Quota);
            }
            tx.execute(
                "INSERT INTO extension_state(extension,key,value) VALUES(?1,?2,?3)",
                params![extension, key, value],
            )?;
        }
        tx.commit()?;
        Ok(StatePut::Stored)
    }
    pub fn thread(&self, id: Id) -> Result<&Thread> {
        self.state
            .threads
            .iter()
            .find(|t| t.id == id)
            .ok_or(StoreError::ThreadNotFound)
    }
    pub fn thread_mut(&mut self, id: Id) -> Result<&mut Thread> {
        self.state
            .threads
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(StoreError::ThreadNotFound)
    }
}
/// Bounds on one extension's persisted state.
#[derive(Debug, Clone, Copy)]
pub struct StateQuota {
    pub entries: u64,
    pub bytes: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatePut {
    Stored,
    Quota,
}

/// Summaries of the bundled extensions, keeping the user's enable choice and
/// only those grants the current manifest still requests: an update never
/// inherits a newly requested capability.
fn bundled_plugins(previous: &[PluginSummary]) -> Result<Vec<PluginSummary>> {
    Ok(a206_extensions::builtins()
        .map_err(StoreError::Builtin)?
        .iter()
        .map(|builtin| {
            let earlier = previous.iter().find(|p| p.id == builtin.manifest.id);
            builtin.manifest.summary(
                earlier.is_none_or(|p| p.enabled),
                &earlier.map(|p| p.granted.clone()).unwrap_or_default(),
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_retires_only_the_owned_joshua_connection() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path()).unwrap();
        let make_provider = || ProviderConfig {
            id: Id::new_v4(),
            label: Id::new_v4().to_string(),
            base_url: "http://127.0.0.1:9876/v1".into(),
            credential: None,
            api: Default::default(),
        };
        let owned = make_provider();
        let user = make_provider();
        store.state.settings.providers = vec![owned.clone(), user.clone()];
        store.state.managed_joshua_provider = Some(owned.id);
        store.persist(None).unwrap();
        drop(store);
        let restored = Store::open(directory.path()).unwrap();
        assert_eq!(restored.state.settings.providers, vec![user]);
        assert!(restored.state.managed_joshua_provider.is_none());
        assert_eq!(restored.state.settings.revision, 1);
    }
    #[test]
    fn restart_expires_approvals_and_cancels_unfinished_tool_calls() {
        use a206_core::{ApprovalRequest, Message, Mode, Role, Thread, ToolCall, ToolCallState};
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path()).unwrap();
        let mut thread = Thread::new(Id::new_v4().to_string(), Mode::Chat);
        let mut message = Message::new(Role::Assistant, String::new(), 0, thread.backend);
        let call = |state| ToolCall {
            id: Id::new_v4().to_string(),
            generated_id: false,
            tool: Id::new_v4().simple().to_string(),
            arguments: "{}".into(),
            digest: Id::new_v4().to_string(),
            healed: false,
            state,
            output: None,
            error: None,
            signature: None,
        };
        message.tool_calls = vec![
            call(ToolCallState::AwaitingApproval),
            call(ToolCallState::Succeeded),
        ];
        let pending = &message.tool_calls[0];
        thread.approvals.push(ApprovalRequest {
            id: Id::new_v4(),
            message: message.id,
            call: pending.id.clone(),
            tool: pending.tool.clone(),
            arguments: pending.arguments.clone(),
            digest: pending.digest.clone(),
            capabilities: Default::default(),
            requested_at: 0,
        });
        thread.messages.push(message);
        thread.state = RunState::Running;
        let id = thread.id;
        store.state.threads.push(thread);
        store.persist(None).unwrap();
        drop(store);
        let restored = Store::open(directory.path()).unwrap();
        let thread = restored.thread(id).unwrap();
        assert!(thread.approvals.is_empty());
        let states: Vec<_> = thread.messages[0]
            .tool_calls
            .iter()
            .map(|c| c.state)
            .collect();
        assert_eq!(
            states,
            vec![ToolCallState::Cancelled, ToolCallState::Succeeded]
        );
        assert!(thread.messages[0].tool_calls[0].error.is_some());
    }
    #[test]
    fn pending_replays_reference_their_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let id = Id::new_v4();
        assert!(store.begin(id, &Command::Shutdown).unwrap().is_none());
        let Some(Reply::Error(report)) = store.begin(id, &Command::Shutdown).unwrap() else {
            panic!("a pending request must replay as an error");
        };
        assert_eq!(report.kind, a206_core::ErrorKind::Conflict);
        assert_eq!(report.correlation, Some(id));
    }
    #[test]
    fn extension_grants_survive_restart_within_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let builtin = a206_extensions::builtins().unwrap().remove(0);
        let requested = builtin.manifest.capabilities.clone();
        let outside = [
            Capability::Execute,
            Capability::Network,
            Capability::UseSecret,
        ]
        .into_iter()
        .find(|c| !requested.contains(c))
        .unwrap();
        {
            let mut store = Store::open(dir.path()).unwrap();
            let plugin = store
                .state
                .plugins
                .iter_mut()
                .find(|p| p.id == builtin.manifest.id)
                .unwrap();
            assert!(plugin.granted.is_empty(), "nothing is granted by default");
            plugin.enabled = false;
            plugin.granted = requested.iter().copied().chain([outside]).collect();
            store.persist(None).unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        let plugin = store
            .state
            .plugins
            .iter()
            .find(|p| p.id == builtin.manifest.id)
            .unwrap();
        assert!(!plugin.enabled);
        assert_eq!(plugin.granted, requested);
    }
    #[test]
    fn extension_state_is_scoped_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let quota = StateQuota {
            entries: 2,
            bytes: 64,
        };
        let (a, b) = (Id::new_v4().to_string(), Id::new_v4().to_string());
        let put = |store: &mut Store, ext: &str, key: &str, value: Option<&[u8]>| {
            store.put_extension_state(ext, key, value, quota).unwrap()
        };
        assert_eq!(put(&mut store, &a, "k", Some(b"1")), StatePut::Stored);
        assert_eq!(store.extension_state(&b, "k").unwrap(), None);
        assert_eq!(put(&mut store, &a, "k", Some(b"2")), StatePut::Stored);
        assert_eq!(put(&mut store, &a, "j", Some(b"3")), StatePut::Stored);
        assert_eq!(put(&mut store, &a, "i", Some(b"4")), StatePut::Quota);
        assert_eq!(put(&mut store, &a, "k", Some(&[0; 64])), StatePut::Quota);
        // A refused write keeps the earlier value.
        assert_eq!(store.extension_state(&a, "k").unwrap(), Some(b"2".to_vec()));
        assert_eq!(put(&mut store, &a, "k", None), StatePut::Stored);
        assert_eq!(store.extension_state(&a, "k").unwrap(), None);
    }
    #[test]
    fn journal_and_dedup_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let id = Id::new_v4();
        {
            let mut s = Store::open(dir.path()).unwrap();
            assert!(s.begin(id, &Command::Shutdown).unwrap().is_none());
            s.finish(id, &Reply::Ok).unwrap();
            s.persist(Some(EventKind::SettingsChanged)).unwrap();
        }
        let mut s = Store::open(dir.path()).unwrap();
        assert_eq!(s.begin(id, &Command::Shutdown).unwrap(), Some(Reply::Ok));
        assert!(s.begin(id, &Command::Snapshot).is_err());
        assert_eq!(s.events(0).unwrap().len(), 1);
        assert!(s.events(u64::MAX).is_err());
    }
}
