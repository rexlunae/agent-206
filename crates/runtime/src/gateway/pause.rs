//! Pausing a conversation's work, as RustyClaw's task pause/resume intends.
//! A paused conversation's native run, and the runs of its subagents, wait
//! before their next model round, the same boundary steered messages join
//! at. A round in progress finishes first: its reply, tool calls and
//! approvals are not interrupted. Cancelling still ends a paused run.
use super::{Gateway, GatewayError, Result, check, run};
use a206_core::{CodingBackend, EventKind, Id, Reply, RunState};

impl Gateway {
    /// Pause or resume `thread`'s work.
    pub(super) fn set_paused(&self, thread: Id, paused: bool) -> Result<Reply> {
        let mut store = self.lock()?;
        let current = store.thread(thread)?;
        if paused {
            // Only an admitted native run waits for a pause; a conversation
            // that is Running while it is compacted has none, and a pause
            // left on it would hold its next run.
            check(
                current.state == RunState::Running
                    && current.backend == CodingBackend::Native
                    && current.compacting.is_none()
                    && self.accepting()?.contains(&thread),
                GatewayError::Conflict("the conversation has no native run to pause"),
            )?;
            check(
                !current.paused,
                GatewayError::Conflict("the conversation is already paused"),
            )?;
        } else {
            check(
                current.paused,
                GatewayError::Conflict("the conversation is not paused"),
            )?;
        }
        store.thread_mut(thread)?.paused = paused;
        store.persist(Some(EventKind::ThreadChanged { id: thread }))?;
        // Waiting runs look again; a run whose ancestor is still paused
        // keeps waiting.
        self.pauses.send_modify(|generation| *generation += 1);
        Ok(Reply::Ok)
    }
    /// Whether `thread` or any ancestor is paused now.
    pub(super) fn pause_pending(&self, thread: Id) -> Result<bool> {
        Ok(run::lineage(&*self.lock()?, thread)?
            .iter()
            .any(|t| t.paused))
    }
    /// Wait while `thread` or any ancestor is paused, marking the
    /// conversation held meanwhile. Returns `false` when the run was
    /// cancelled.
    pub(super) async fn wait_while_paused(
        &self,
        thread: Id,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<bool> {
        let mut held = false;
        let proceed = loop {
            // Subscribed before looking, so a resume in between is seen.
            let mut changed = self.pauses.subscribe();
            if !self.pause_pending(thread)? {
                break true;
            }
            if !held {
                self.set_held(thread, true)?;
                held = true;
            }
            tokio::select! {
                () = cancel.cancelled() => break false,
                // The sender lives as long as the gateway, which outlives
                // its runs.
                _ = changed.changed() => {}
            }
        };
        if held {
            self.set_held(thread, false)?;
        }
        Ok(proceed)
    }
    fn set_held(&self, thread: Id, held: bool) -> Result<()> {
        let mut store = self.lock()?;
        store.thread_mut(thread)?.held = held;
        store.persist(Some(EventKind::ThreadChanged { id: thread }))?;
        Ok(())
    }
}
