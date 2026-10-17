//! Scheduled messages. A due job sends its text to its conversation through
//! `Send`, so it is admitted, journaled and run like a message the user typed,
//! under the conversation's grants and permissions as they are at that time.
//! A job that comes due while the gateway is stopped runs once when it starts.
use super::{Gateway, GatewayError, Result};
use crate::{error::check, provider::now, schedule};
use a206_core::*;
use a206_protocol::Request;
use std::time::Duration;

/// Most jobs one gateway keeps.
pub const MAX_JOBS: usize = 256;
const MAX_NAME: usize = 200;
const MAX_TEXT: usize = 128 * 1024;
/// Longest the scheduler sleeps without re-reading the clock, so a changed
/// system clock or time zone is noticed.
const RECHECK: u64 = 3600;

impl Gateway {
    pub(super) fn schedule_job(
        &self,
        name: String,
        thread: Id,
        text: String,
        schedule: Schedule,
        author: JobAuthor,
    ) -> Result<Job> {
        let name = name.trim().to_owned();
        check(
            !name.is_empty() && name.len() <= MAX_NAME,
            GatewayError::Invalid("job names are 1–200 bytes"),
        )?;
        check(
            !text.trim().is_empty() && text.len() <= MAX_TEXT,
            GatewayError::Invalid("job messages are 1–131072 bytes"),
        )?;
        let created = now();
        // Relative schedules count from now by this clock, not the client's.
        let schedule = schedule::resolve(schedule, created)?;
        let next = schedule::first(&schedule, created)?;
        let mut store = self.lock()?;
        store.thread(thread)?;
        check(
            store.state.jobs.len() < MAX_JOBS,
            GatewayError::Invalid("remove a scheduled message before adding another"),
        )?;
        if let JobAuthor::Agent { .. } = author {
            let added = store
                .state
                .jobs
                .iter()
                .filter(|job| job.thread == thread && job.author == author)
                .count();
            check(
                added < MAX_AGENT_JOBS,
                GatewayError::Invalid("remove one of this conversation's scheduled messages first"),
            )?;
        }
        let job = Job {
            id: Id::new_v4(),
            name,
            thread,
            text,
            schedule,
            enabled: true,
            created,
            next: Some(next),
            last: None,
            author,
        };
        store.state.jobs.push(job.clone());
        store.persist(Some(EventKind::JobChanged { id: job.id }))?;
        drop(store);
        self.jobs_changed.notify_one();
        Ok(job)
    }

    pub(super) fn set_job_enabled(&self, id: Id, enabled: bool) -> Result<Reply> {
        let mut store = self.lock()?;
        let job = job_mut(&mut store.state, id)?;
        // Resuming starts from now: times passed while paused are skipped.
        job.next = if enabled {
            Some(schedule::first(&job.schedule, now())?)
        } else {
            None
        };
        job.enabled = enabled;
        let job = job.clone();
        store.persist(Some(EventKind::JobChanged { id }))?;
        drop(store);
        self.jobs_changed.notify_one();
        Ok(Reply::Job(Box::new(job)))
    }

    pub(super) fn remove_job(&self, id: Id) -> Result<Reply> {
        let mut store = self.lock()?;
        let before = store.state.jobs.len();
        store.state.jobs.retain(|job| job.id != id);
        check(
            store.state.jobs.len() < before,
            GatewayError::NotFound("scheduled message not found"),
        )?;
        store.persist(Some(EventKind::JobChanged { id }))?;
        Ok(Reply::Ok)
    }

    /// Run a job now, leaving its schedule as it is.
    pub(super) fn run_job(&self, id: Id) -> Result<Reply> {
        let at = now();
        let mut store = self.lock()?;
        let job = job_mut(&mut store.state, id)?;
        job.last = Some(JobRun::start(at));
        let job = job.clone();
        store.persist(Some(EventKind::JobChanged { id }))?;
        drop(store);
        self.fire(&job);
        Ok(Reply::Job(Box::new(job)))
    }

    /// Start every enabled job due at `now` and schedule its next run.
    /// Returns when the next job is due.
    pub fn run_due_jobs(&self, now: u64) -> Result<Option<u64>> {
        let mut store = self.lock()?;
        let mut due = vec![];
        for job in store
            .state
            .jobs
            .iter_mut()
            .filter(|job| job.enabled && job.next.is_some_and(|next| next <= now))
        {
            // Only one run per wake, however many times passed since the last.
            match schedule::next_after(&job.schedule, now) {
                Ok(next) => {
                    job.next = next;
                    job.enabled = next.is_some();
                    job.last = Some(JobRun::start(now));
                    due.push(job.clone());
                }
                // A schedule that stopped resolving (a time zone removed from
                // the system) is paused with the reason, never retried blindly.
                Err(error) => {
                    job.next = None;
                    job.enabled = false;
                    job.last = Some(JobRun {
                        id: Id::new_v4(),
                        at: now,
                        outcome: JobOutcome::Refused(crate::error::handle(
                            "schedule_job",
                            &GatewayError::from(error),
                        )),
                    });
                    due.push(job.clone());
                }
            }
        }
        for job in &due {
            store.persist(Some(EventKind::JobChanged { id: job.id }))?;
        }
        let next = store
            .state
            .jobs
            .iter()
            .filter(|job| job.enabled)
            .filter_map(|job| job.next)
            .min();
        drop(store);
        for job in &due {
            self.fire(job);
        }
        Ok(next)
    }

    /// Send `job`'s text in the background, if its latest run has started,
    /// and record how that run ended.
    fn fire(&self, job: &Job) {
        let Some(JobRun {
            id: run,
            outcome: JobOutcome::Running,
            ..
        }) = job.last
        else {
            return;
        };
        let gateway = self.clone();
        let (id, thread, text) = (job.id, job.thread, job.text.clone());
        // Detached: the run belongs to the conversation, where the user sees
        // and can cancel it; shutdown cancels every run, which ends this task.
        drop(tokio::spawn(async move {
            let reply = gateway
                .request(Request {
                    id: Id::new_v4(),
                    body: Command::Send {
                        thread,
                        text,
                        attachments: vec![],
                    },
                })
                .await;
            let outcome = match reply.body {
                Reply::Thread(thread) => match thread.state {
                    RunState::Complete => JobOutcome::Complete,
                    RunState::Cancelled => JobOutcome::Cancelled,
                    _ => JobOutcome::Failed,
                },
                Reply::Error(report) => JobOutcome::Refused(report),
                _ => JobOutcome::Failed,
            };
            // Nobody waits on this task, so a failure to record the outcome
            // is logged; the conversation still shows the run itself.
            if let Err(error) = gateway.finish_job(id, run, outcome) {
                crate::error::handle("schedule_job", &error);
            }
        }));
    }
    fn finish_job(&self, id: Id, run: Id, outcome: JobOutcome) -> Result<()> {
        let mut store = self.lock()?;
        // The job may have been removed, or run again, meanwhile.
        let Some(job) = store.state.jobs.iter_mut().find(|job| job.id == id) else {
            return Ok(());
        };
        match &mut job.last {
            Some(last) if last.id == run => last.outcome = outcome,
            _ => return Ok(()),
        }
        store.persist(Some(EventKind::JobChanged { id }))?;
        Ok(())
    }

    /// Run jobs as they come due until the gateway stops.
    pub async fn supervise_schedule(&self) {
        loop {
            let wait = match self.run_due_jobs(now()) {
                Ok(next) => next.map_or(RECHECK, |next| next.saturating_sub(now())),
                // The scheduler must not stop; it tries again after a while.
                Err(error) => {
                    crate::error::handle("schedule_jobs", &error);
                    RECHECK
                }
            };
            tokio::select! {
                () = self.stop.cancelled() => return,
                () = self.jobs_changed.notified() => {}
                () = tokio::time::sleep(Duration::from_secs(wait.clamp(1, RECHECK))) => {}
            }
        }
    }
}

/// Most jobs a model may keep on one conversation.
pub const MAX_AGENT_JOBS: usize = 16;

/// What the schedule tools may do with one conversation's jobs: add one the
/// user approved, list them and remove those a model added. It runs nothing
/// and cannot change a grant, a permission or another conversation's jobs.
#[derive(Clone)]
pub struct Jobs {
    /// The gateway without its tool hosts, so holding it forms no cycle.
    base: Gateway,
}
impl Gateway {
    pub(crate) fn jobs(&self) -> Jobs {
        let mut base = self.clone();
        base.tool_hosts = Default::default();
        base.delegation = Default::default();
        Jobs { base }
    }
}
impl Jobs {
    /// Schedule `text` to `thread` itself, as a job its model authored.
    pub fn add(
        &self,
        thread: Id,
        name: String,
        text: String,
        schedule: Schedule,
    ) -> std::result::Result<Job, ErrorReport> {
        self.base
            .schedule_job(name, thread, text, schedule, JobAuthor::Agent { thread })
            .map_err(|error| crate::error::report(&error))
    }
    /// The jobs that message `thread`.
    pub fn list(&self, thread: Id) -> std::result::Result<Vec<Job>, ErrorReport> {
        let store = self.base.lock().map_err(|e| crate::error::report(&e))?;
        Ok(store
            .state
            .jobs
            .iter()
            .filter(|job| job.thread == thread)
            .cloned()
            .collect())
    }
    /// Remove a job a model of `thread` added; the user's own jobs stay.
    pub fn remove(&self, thread: Id, id: Id) -> std::result::Result<(), ErrorReport> {
        let removed = (|| {
            let owned = self.base.lock()?.state.jobs.iter().any(|job| {
                job.id == id && job.thread == thread && job.author == JobAuthor::Agent { thread }
            });
            check(
                owned,
                GatewayError::NotFound("no scheduled message this conversation added has that ID"),
            )?;
            self.base.remove_job(id)
        })();
        removed.map(drop).map_err(|e| crate::error::report(&e))
    }
}

fn job_mut(state: &mut Snapshot, id: Id) -> Result<&mut Job> {
    state
        .jobs
        .iter_mut()
        .find(|job| job.id == id)
        .ok_or(GatewayError::NotFound("scheduled message not found"))
}

#[cfg(test)]
mod tests;
