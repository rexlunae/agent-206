//! Scheduled messages. A due job sends its text to its conversation through
//! `Send`, so it is admitted, journaled and run like a message the user typed,
//! under the conversation's grants and permissions as they are at that time.
//! A job that comes due while the gateway is stopped runs once when it starts.
//! A heartbeat check reads the project's `HEARTBEAT.md` when it runs and sends
//! it the same way; a reply of exactly `HEARTBEAT_OK` ends it as quiet.
//! A job the user gave a messenger destination also posts each run's reply
//! there through the [`super::Outbox`], which checks the destination is
//! still enabled and connected when it posts.
use super::{Gateway, GatewayError, Result};
use crate::{error::check, provider::now, schedule, tools::ToolScope};
use a206_core::{
    schedule::{heartbeat_message, is_heartbeat_ok},
    *,
};
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
        kind: JobKind,
        schedule: Schedule,
        author: JobAuthor,
    ) -> Result<Job> {
        let name = name.trim().to_owned();
        check(
            !name.is_empty() && name.len() <= MAX_NAME,
            GatewayError::Invalid("job names are 1–200 bytes"),
        )?;
        match kind {
            JobKind::Message => check(
                !text.trim().is_empty() && text.len() <= MAX_TEXT,
                GatewayError::Invalid("job messages are 1–131072 bytes"),
            )?,
            JobKind::Heartbeat => check(
                text.is_empty(),
                GatewayError::Invalid("a heartbeat check sends HEARTBEAT.md and takes no text"),
            )?,
        }
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
            kind,
            schedule,
            enabled: true,
            created,
            next: Some(next),
            last: None,
            author,
            history: 0,
            deliver_to: None,
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

    /// See [`Command::SetJobDelivery`].
    pub(super) fn set_job_delivery(&self, id: Id, to: Option<String>) -> Result<Reply> {
        let mut store = self.lock()?;
        if let Some(to) = &to {
            check(
                messengers::post_destinations(&store.state.settings.messengers).contains(to),
                GatewayError::Invalid(
                    "post to a webhook, or to an allowed chat of a messenger that can post, by its name or name/chat",
                ),
            )?;
        }
        let job = job_mut(&mut store.state, id)?;
        job.deliver_to = to;
        let job = job.clone();
        store.persist(Some(EventKind::JobChanged { id }))?;
        Ok(Reply::Job(Box::new(job)))
    }

    /// See [`Command::JobRuns`].
    pub(super) fn job_runs(&self, id: Id) -> Result<Reply> {
        let store = self.lock()?;
        check(
            store.state.jobs.iter().any(|job| job.id == id),
            GatewayError::NotFound("scheduled message not found"),
        )?;
        Ok(Reply::JobRuns(store.job_runs(id)?))
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
                        outcome: JobOutcome::Refused(crate::error::handle(
                            "schedule_job",
                            &GatewayError::from(error),
                        )),
                        ended: Some(now),
                        ..JobRun::start(now)
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
        let (id, thread, kind, text) = (job.id, job.thread, job.kind, job.text.clone());
        // Detached: the run belongs to the conversation, where the user sees
        // and can cancel it; shutdown cancels every run, which ends this task.
        // A post still going out when the gateway stops is recorded as
        // posting, which restart recovery reports as unrecorded.
        drop(tokio::spawn(async move {
            let text = match kind {
                JobKind::Message => Ok(Some(text)),
                JobKind::Heartbeat => gateway.heartbeat(thread, run).await,
            };
            let (outcome, reply) = match text {
                Err(error) => (
                    JobOutcome::Refused(crate::error::handle("schedule_job", &error)),
                    None,
                ),
                // An empty checklist has nothing to check.
                Ok(None) => (JobOutcome::Quiet, None),
                Ok(Some(text)) => gateway.send_job(thread, kind, text).await,
            };
            // Nobody waits on this task, so a failure to record the outcome
            // is logged; the conversation still shows the run itself.
            let to = match gateway.finish_job(id, run, outcome, reply.is_some()) {
                Ok(to) => to,
                Err(error) => {
                    crate::error::handle("schedule_job", &error);
                    return;
                }
            };
            let (Some(to), Some(reply)) = (to, reply) else {
                return;
            };
            let delivery = gateway.deliver(&to, &reply).await;
            if let Err(error) =
                gateway.update_run(id, run, |_, last| last.delivery = Some(delivery))
            {
                crate::error::handle("schedule_delivery", &error);
            }
        }));
    }
    /// Send a job's message to `thread` and report how its run ended, with
    /// its reply.
    async fn send_job(
        &self,
        thread: Id,
        kind: JobKind,
        text: String,
    ) -> (JobOutcome, Option<String>) {
        // A conversation's messages are only ever added to, so the job's
        // message is the first with its text from here on.
        let from = match self
            .lock()
            .and_then(|store| Ok(store.thread(thread)?.messages.len()))
        {
            Ok(from) => from,
            Err(error) => {
                return (
                    JobOutcome::Refused(crate::error::handle("schedule_job", &error)),
                    None,
                );
            }
        };
        let reply = self
            .request(Request {
                id: Id::new_v4(),
                body: Command::Send {
                    thread,
                    text: text.clone(),
                    attachments: vec![],
                    images: vec![],
                },
            })
            .await;
        match reply.body {
            Reply::Thread(thread) => (ended(kind, &thread), run_reply(&thread, &text, from)),
            Reply::Error(report) => (JobOutcome::Refused(report), None),
            _ => (JobOutcome::Failed, None),
        }
    }
    /// Post a run's `reply` to the messenger destination `to`.
    async fn deliver(&self, to: &str, reply: &str) -> Delivery {
        match self.outbox().send(to, reply).await {
            Ok(parts) => Delivery::Sent {
                parts: u64::try_from(parts).unwrap_or(u64::MAX),
            },
            Err(error) => Delivery::Failed(crate::error::handle("schedule_delivery", &error)),
        }
    }
    /// The heartbeat check message for `thread`, from `HEARTBEAT.md` at its
    /// project root as the conversation's grants allow reading it now, or
    /// `None` when the checklist is empty.
    async fn heartbeat(&self, thread: Id, run: Id) -> Result<Option<String>> {
        let readable = |scope: &ToolScope| {
            check(
                crate::instructions::permitted(&scope.capabilities),
                GatewayError::Denied(
                    "this conversation may not read its project, so not HEARTBEAT.md",
                ),
            )
        };
        let scope = self.scope(thread)?;
        let root = scope.workspace.clone().ok_or(GatewayError::NotFound(
            "a heartbeat check needs a conversation with a project folder",
        ))?;
        readable(&scope)?;
        let checklist =
            tokio::task::spawn_blocking(move || crate::instructions::heartbeat(root)).await?;
        // The grant may have been withdrawn while the file was read.
        readable(&self.scope(thread)?)?;
        match checklist.map_err(GatewayError::Heartbeat)? {
            None => Err(GatewayError::NotFound("the project has no HEARTBEAT.md")),
            Some(checklist) if checklist.trim().is_empty() => Ok(None),
            Some(checklist) => Ok(Some(heartbeat_message(&checklist, run))),
        }
    }
    /// Apply `change` to run `run` of job `id`, given the job's current
    /// messenger destination: to `Job::last` while it is that run, else to
    /// its row in the history. `None` when the job, or the run, is gone.
    fn update_run<T>(
        &self,
        id: Id,
        run: Id,
        change: impl FnOnce(Option<String>, &mut JobRun) -> T,
    ) -> Result<Option<T>> {
        let mut store = self.lock()?;
        // The job may have been removed, or run again, meanwhile.
        let Some(job) = store.state.jobs.iter_mut().find(|job| job.id == id) else {
            return Ok(None);
        };
        let to = job.deliver_to.clone();
        let (result, settled) = match &mut job.last {
            Some(last) if last.id == run => (change(to, last), None),
            // A newer run is the latest; this one changes in the history only.
            _ => {
                let Some(mut earlier) = store
                    .job_runs(id)?
                    .into_iter()
                    .find(|earlier| earlier.id == run)
                else {
                    // Already pruned from the history.
                    return Ok(None);
                };
                let result = change(to, &mut earlier);
                job_mut(&mut store.state, id)?.history += 1;
                (result, Some(earlier))
            }
        };
        store.persist_settling(Some(EventKind::JobChanged { id }), settled.as_ref())?;
        Ok(Some(result))
    }

    /// Record how a run ended. When it has a reply to post and the job a
    /// destination, the run is recorded as posting and the destination
    /// returned, so a gateway stopped while posting leaves a record of it.
    fn finish_job(
        &self,
        id: Id,
        run: Id,
        outcome: JobOutcome,
        posts: bool,
    ) -> Result<Option<String>> {
        let ended = Some(now());
        let posts = posts && outcome == JobOutcome::Complete;
        let recorded = self.update_run(id, run, |to, last| {
            let to = to.filter(|_| posts);
            last.outcome = outcome;
            last.ended = ended;
            last.delivery = to.as_ref().map(|_| Delivery::Posting);
            to
        })?;
        match recorded {
            Some(to) => Ok(to),
            // Newer runs pushed this one out of the history; its reply is
            // still posted while the job keeps a destination.
            None if posts => Ok(self
                .lock()?
                .state
                .jobs
                .iter()
                .find(|job| job.id == id)
                .and_then(|job| job.deliver_to.clone())),
            None => Ok(None),
        }
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
            .schedule_job(
                name,
                thread,
                text,
                JobKind::Message,
                schedule,
                JobAuthor::Agent { thread },
            )
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

/// How a job's run ended, from its conversation once the run returned.
fn ended(kind: JobKind, thread: &Thread) -> JobOutcome {
    match thread.state {
        // Messages steered too late for the run follow its reply.
        RunState::Complete
            if kind == JobKind::Heartbeat
                && thread
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::Assistant)
                    .is_some_and(|m| is_heartbeat_ok(&m.text)) =>
        {
            JobOutcome::Quiet
        }
        RunState::Complete => JobOutcome::Complete,
        RunState::Cancelled => JobOutcome::Cancelled,
        _ => JobOutcome::Failed,
    }
}

/// A run's reply: the last assistant text after the message `sent` that
/// started it, the first such message from index `from`, where the
/// conversation ended before it was sent. Messages steered into the run, even
/// with the same text, follow it; the reply to those it took in comes after
/// them, and those steered too late come after the reply and are skipped.
fn run_reply(thread: &Thread, sent: &str, from: usize) -> Option<String> {
    let start = from
        + thread
            .messages
            .get(from..)?
            .iter()
            .position(|m| m.role == Role::User && m.text == sent)?;
    thread.messages[start + 1..]
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !m.text.trim().is_empty())
        .map(|m| m.text.clone())
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
