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
    #[error("the bundled extension is invalid")]
    Builtin(#[source] a206_extensions::ExtensionError),
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
            CREATE TABLE IF NOT EXISTS requests(id TEXT PRIMARY KEY, request BLOB NOT NULL, reply BLOB);")?;
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
                plugins: vec![
                    a206_extensions::builtin()
                        .map_err(StoreError::Builtin)?
                        .0
                        .summary(),
                ],
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
    fn persist_inner(&mut self, kind: Option<EventKind>) -> Result<()> {
        if rmp_serde::to_vec_named(&self.state)?.len() >= a206_protocol::MAX_FRAME - 65_536 {
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
