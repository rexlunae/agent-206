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
    ) -> Result<Reply> {
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
        let next = schedule::first(&schedule, created)?;
        let mut store = self.lock()?;
        store.thread(thread)?;
        check(
            store.state.jobs.len() < MAX_JOBS,
            GatewayError::Invalid("remove a scheduled message before adding another"),
        )?;
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
        Ok(Reply::Job(Box::new(job)))
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
        job.last = Some(JobRun {
            at,
            outcome: JobOutcome::Running,
        });
        let job = job.clone();
        store.persist(Some(EventKind::JobChanged { id }))?;
        drop(store);
        self.fire(&job, at);
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
                    job.last = Some(JobRun {
                        at: now,
                        outcome: JobOutcome::Running,
                    });
                    due.push(job.clone());
                }
                // A schedule that stopped resolving (a time zone removed from
                // the system) is paused with the reason, never retried blindly.
                Err(error) => {
                    job.next = None;
                    job.enabled = false;
                    job.last = Some(JobRun {
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
        for job in due.iter().filter(|job| {
            matches!(
                job.last,
                Some(JobRun {
                    outcome: JobOutcome::Running,
                    ..
                })
            )
        }) {
            self.fire(job, now);
        }
        Ok(next)
    }

    /// Send `job`'s text in the background and record how the run ended.
    fn fire(&self, job: &Job, at: u64) {
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
            if let Err(error) = gateway.finish_job(id, at, outcome) {
                crate::error::handle("schedule_job", &error);
            }
        }));
    }
    fn finish_job(&self, id: Id, at: u64, outcome: JobOutcome) -> Result<()> {
        let mut store = self.lock()?;
        // The job may have been removed, or run again, meanwhile.
        let Some(job) = store.state.jobs.iter_mut().find(|job| job.id == id) else {
            return Ok(());
        };
        match &mut job.last {
            Some(run) if run.at == at => run.outcome = outcome,
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

fn job_mut(state: &mut Snapshot, id: Id) -> Result<&mut Job> {
    state
        .jobs
        .iter_mut()
        .find(|job| job.id == id)
        .ok_or(GatewayError::NotFound("scheduled message not found"))
}

#[cfg(test)]
mod tests;
