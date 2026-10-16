//! SQLite WAL snapshot, event journal and durable request deduplication.
use crate::provider::now;
use a206_core::*;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
pub struct Store {
    _lock: std::fs::File,
    db: Connection,
    pub state: Snapshot,
    pub root: PathBuf,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("gateway.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("a gateway already owns this data directory")?;
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
                toml::from_str(&std::fs::read_to_string(root.join("settings.toml"))?)?
            } else {
                Settings::default()
            };
            Snapshot {
                settings,
                threads: vec![],
                projects: vec![],
                catalogs: vec![],
                downloads: vec![],
                plugins: vec![a206_extensions::builtin()?.0.summary()],
                last_event: 0,
            }
        };
        let mut store = Self {
            _lock: lock,
            db,
            state,
            root: root.to_owned(),
        };
        crate::provider::validate_presets(&store.state.settings.provider_presets)?;
        for thread in &mut store.state.threads {
            if thread.state == RunState::Running {
                thread.state = RunState::Failed;
            }
        }
        for download in &mut store.state.downloads {
            if matches!(
                download.state,
                DownloadState::Running | DownloadState::Queued
            ) {
                download.state = DownloadState::Failed;
                download.error = Some(
                    "Gateway restarted; partial file retained. Start a new transfer to retry."
                        .into(),
                );
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
        anyhow::ensure!(
            rmp_serde::to_vec_named(&self.state)?.len() < a206_protocol::MAX_FRAME - 65_536,
            "snapshot capacity reached; paged history is not implemented yet"
        );
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
            bail!("event cursor unavailable; request a fresh snapshot");
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
                bail!("request id reused with a different command");
            }
            return Ok(Some(if let Some(reply) = reply {
                rmp_serde::from_slice(&reply)?
            } else {
                Reply::Error { message: "request was already accepted; outcome is pending or unknown; refresh state before retrying".into() }
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
            .context("thread not found")
    }
    pub fn thread_mut(&mut self, id: Id) -> Result<&mut Thread> {
        self.state
            .threads
            .iter_mut()
            .find(|t| t.id == id)
            .context("thread not found")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
