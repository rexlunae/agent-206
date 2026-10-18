//! SQLite WAL snapshot, event journal and durable request deduplication.
use crate::provider::now;
use a206_core::{schedule::MAX_JOB_RUNS, *};
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
            CREATE TABLE IF NOT EXISTS extension_state(extension TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(extension, key));
            CREATE TABLE IF NOT EXISTS job_runs(seq INTEGER PRIMARY KEY AUTOINCREMENT, job TEXT NOT NULL, run TEXT NOT NULL UNIQUE, payload BLOB NOT NULL);
            CREATE INDEX IF NOT EXISTS job_runs_by_job ON job_runs(job, seq);")?;
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
                messengers: vec![],
                mcp_servers: vec![],
                jobs: vec![],
                extension_proposals: vec![],
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
        // Before interrupted runs are marked failed, which it reads.
        crate::schedule::recover(&mut store.state.jobs, &store.state.threads);
        for thread in &mut store.state.threads {
            // No run survives a restart, so neither do its approvals,
            // questions or calls.
            thread.approvals.clear();
            thread.prompts.clear();
            // Messages steered to the interrupted run stay in the
            // conversation, after it.
            let mut steered = std::mem::take(&mut thread.steering);
            thread.messages.append(&mut steered);
            // Nor does a pause, which held a run.
            thread.paused = false;
            thread.held = false;
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
            // An interrupted compaction is not a run: the previous run's
            // outcome stays.
            if let Some(prior) = thread.compacting.take() {
                thread.state = prior;
            } else if thread.state == RunState::Running {
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
        // Recovery settled interrupted runs; their history follows.
        let tx = store.db.transaction()?;
        for job in &store.state.jobs {
            record_run(&tx, &store.state, job.id)?;
        }
        unrecorded(&tx)?;
        tx.commit()?;
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
            let job = match kind {
                EventKind::JobChanged { id } => Some(id),
                _ => None,
            };
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
            // Every change to a job's latest run publishes this, so its
            // history commits with the snapshot that shows it.
            if let Some(job) = job {
                record_run(&tx, &self.state, job)?;
            }
        }
        tx.execute("INSERT INTO snapshot(id,payload) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", [rmp_serde::to_vec_named(&self.state)?])?;
        tx.commit()?;
        Ok(())
    }
    /// See [`Command::JobRuns`](a206_core::Command::JobRuns).
    pub fn job_runs(&self, job: Id) -> Result<Vec<JobRun>> {
        let mut stmt = self
            .db
            .prepare("SELECT payload FROM job_runs WHERE job=?1 ORDER BY seq DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![job.to_string(), MAX_JOB_RUNS], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        rows.map(|r| Ok(rmp_serde::from_slice(&r?)?)).collect()
    }
    /// Record how an earlier run, no longer its job's latest, ended.
    pub fn settle_run(&self, run: Id, outcome: JobOutcome, ended: u64) -> Result<()> {
        let key = run.to_string();
        let payload: Option<Vec<u8>> = self
            .db
            .query_row("SELECT payload FROM job_runs WHERE run=?1", [&key], |row| {
                row.get(0)
            })
            .optional()?;
        // Pruned from the history meanwhile.
        let Some(payload) = payload else {
            return Ok(());
        };
        let mut settled: JobRun = rmp_serde::from_slice(&payload)?;
        settled.outcome = outcome;
        settled.ended = Some(ended);
        self.db.execute(
            "UPDATE job_runs SET payload=?1 WHERE run=?2",
            params![rmp_serde::to_vec_named(&settled)?, key],
        )?;
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
    /// See [`Command::Activity`](a206_core::Command::Activity). Only the
    /// retained journal is read, so the oldest page ends where the journal does.
    pub fn activity(&self, before: Option<u64>, limit: u32) -> Result<ActivityPage> {
        // A page always holds at least one event, so a page that says older
        // activity remains also carries the cursor to reach it.
        let limit = usize::try_from(limit.clamp(1, MAX_ACTIVITY)).unwrap_or(1);
        let before = before.unwrap_or(self.state.last_event.saturating_add(1));
        let mut stmt = self
            .db
            .prepare("SELECT payload FROM events WHERE seq < ?1 ORDER BY seq DESC")?;
        let mut rows = stmt.query([before])?;
        let mut events = Vec::with_capacity(limit);
        let mut more = false;
        while let Some(row) = rows.next()? {
            let event: Event = rmp_serde::from_slice(&row.get::<_, Vec<u8>>(0)?)?;
            if event.kind.is_progress() {
                continue;
            }
            if events.len() == limit {
                more = true;
                break;
            }
            events.push(event);
        }
        Ok(ActivityPage { events, more })
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
    /// Forget everything an extension stored.
    pub fn clear_extension_state(&mut self, extension: &str) -> Result<()> {
        self.db.execute(
            "DELETE FROM extension_state WHERE extension=?1",
            params![extension],
        )?;
        Ok(())
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
/// inherits a newly requested capability. Installed extensions follow
/// unchanged, unless a bundled one now has their id.
fn bundled_plugins(previous: &[PluginSummary]) -> Result<Vec<PluginSummary>> {
    let builtins = a206_extensions::builtins().map_err(StoreError::Builtin)?;
    let mut plugins: Vec<PluginSummary> = builtins
        .iter()
        .map(|builtin| {
            let earlier = previous.iter().find(|p| {
                p.id == builtin.manifest.id && p.source == a206_core::PluginSource::Bundled
            });
            builtin.manifest.summary(
                earlier.is_none_or(|p| p.enabled),
                &earlier.map(|p| p.granted.clone()).unwrap_or_default(),
            )
        })
        .collect();
    let installed: Vec<PluginSummary> = previous
        .iter()
        .filter(|p| {
            matches!(p.source, a206_core::PluginSource::Installed { .. })
                && !plugins.iter().any(|b| b.id == p.id)
        })
        .cloned()
        .collect();
    plugins.extend(installed);
    Ok(plugins)
}

/// Mark every run the history still shows as running ended unrecorded: no
/// run survives a restart, and recovery has settled each job's latest.
fn unrecorded(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let mut stmt = tx.prepare("SELECT run, payload FROM job_runs")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (key, payload) in rows {
        let mut run: JobRun = rmp_serde::from_slice(&payload)?;
        if run.outcome == JobOutcome::Running {
            run.outcome = JobOutcome::Unrecorded;
            tx.execute(
                "UPDATE job_runs SET payload=?1 WHERE run=?2",
                params![rmp_serde::to_vec_named(&run)?, key],
            )?;
        }
    }
    Ok(())
}

/// Keep `job`'s latest run in its history, which holds its newest
/// [`MAX_JOB_RUNS`] runs; a removed job's history goes with it.
fn record_run(tx: &rusqlite::Transaction<'_>, state: &Snapshot, job: Id) -> Result<()> {
    let key = job.to_string();
    let Some(found) = state.jobs.iter().find(|j| j.id == job) else {
        tx.execute("DELETE FROM job_runs WHERE job=?1", [&key])?;
        return Ok(());
    };
    let Some(run) = &found.last else {
        return Ok(());
    };
    tx.execute(
        "INSERT INTO job_runs(job,run,payload) VALUES(?1,?2,?3) ON CONFLICT(run) DO UPDATE SET payload=excluded.payload",
        params![key, run.id.to_string(), rmp_serde::to_vec_named(run)?],
    )?;
    tx.execute(
        "DELETE FROM job_runs WHERE job=?1 AND seq NOT IN (SELECT seq FROM job_runs WHERE job=?1 ORDER BY seq DESC LIMIT ?2)",
        params![key, MAX_JOB_RUNS],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reopening_keeps_installed_extensions_but_not_over_bundled_ids() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path()).unwrap();
        let bundled = store.state.plugins[0].clone();
        let installed = |id: &str| PluginSummary {
            id: id.into(),
            enabled: true,
            source: a206_core::PluginSource::Installed {
                digest: Id::new_v4().simple().to_string(),
            },
            ..bundled.clone()
        };
        let kept = installed(&format!("test.{}", Id::new_v4().simple()));
        // A bundled extension that now has an installed extension's id wins.
        store.state.plugins.push(kept.clone());
        store.state.plugins.push(installed(&bundled.id));
        store.persist(None).unwrap();
        drop(store);
        let restored = Store::open(directory.path()).unwrap();
        let plugins = &restored.state.plugins;
        assert!(plugins.contains(&kept));
        assert_eq!(plugins.iter().filter(|p| p.id == bundled.id).count(), 1);
        assert!(plugins.contains(&bundled));
    }
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
    fn restart_keeps_messages_steered_to_an_interrupted_run_after_it_and_lifts_its_pause() {
        use a206_core::{CodingBackend, Message, Mode, Role, Thread};
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path()).unwrap();
        let mut thread = Thread::new(Id::new_v4().to_string(), Mode::Chat);
        thread.state = RunState::Running;
        let message = |text: String| Message::new(Role::User, text, 0, CodingBackend::Native);
        let earlier = message(Id::new_v4().to_string());
        let steered = message(Id::new_v4().to_string());
        thread.messages.push(earlier.clone());
        thread.steering.push(steered.clone());
        (thread.paused, thread.held) = (true, true);
        let id = thread.id;
        store.state.threads.push(thread);
        store.persist(None).unwrap();
        drop(store);
        let restored = Store::open(directory.path()).unwrap();
        let thread = restored.thread(id).unwrap();
        assert_eq!(thread.state, RunState::Failed);
        assert!(thread.steering.is_empty());
        assert_eq!(thread.messages, vec![earlier, steered]);
        assert!(!thread.paused && !thread.held);
    }
    #[test]
    fn restart_expires_approvals_and_questions_and_cancels_unfinished_tool_calls() {
        use a206_core::{
            ApprovalRequest, Message, Mode, Role, Thread, ToolCall, ToolCallState, UserPrompt,
        };
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
            reduction: None,
            redactions: Vec::new(),
            injection: Vec::new(),
            repeated: None,
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
        thread.prompts.push(UserPrompt {
            id: Id::new_v4(),
            call: Id::new_v4().to_string(),
            question: Id::new_v4().to_string(),
            choices: vec![],
            other: true,
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
        assert!(thread.prompts.is_empty());
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
    fn restart_during_a_compaction_restores_the_previous_run_state() {
        use a206_core::{Mode, Thread};
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path()).unwrap();
        let report =
            a206_core::ErrorReport::new(a206_core::ErrorKind::Storage, Id::new_v4().to_string());
        let mut ids = vec![];
        // A compaction of a completed and of a failed conversation, and a run.
        for (prior, compacting) in [
            (RunState::Complete, true),
            (RunState::Failed, true),
            (RunState::Complete, false),
        ] {
            let mut thread = Thread::new(Id::new_v4().to_string(), Mode::Chat);
            thread.state = RunState::Running;
            if compacting {
                thread.compacting = Some(prior.clone());
                thread.error = (prior == RunState::Failed).then(|| report.clone());
            }
            ids.push(thread.id);
            store.state.threads.push(thread);
        }
        store.persist(None).unwrap();
        drop(store);
        let restored = Store::open(directory.path()).unwrap();
        let state = |i: usize| restored.thread(ids[i]).unwrap();
        assert_eq!(state(0).state, RunState::Complete);
        assert_eq!(state(0).error, None);
        assert_eq!(state(0).compacting, None);
        assert_eq!(state(1).state, RunState::Failed);
        assert_eq!(state(1).error, Some(report));
        // A run in progress is still reported as interrupted.
        assert_eq!(state(2).state, RunState::Failed);
        assert_eq!(
            state(2).error.as_ref().unwrap().kind,
            a206_core::ErrorKind::Unavailable
        );
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
