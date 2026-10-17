//! Steering: messages added to a conversation while its native run is going,
//! as RustyClaw's session steering does. A steered message waits in
//! `Thread::steering` and joins the conversation between model rounds, never
//! between a reply and its tool results, so compaction and providers see an
//! ordinary user turn. A run that would end with one waiting continues.
//!
//! The set of runs taking messages is held beside the store and changed only
//! under the store lock, so a message is either accepted before the run's
//! last look or refused, never left behind by a run that ended.
use super::{
    Gateway, GatewayError, MESSAGE_OVERHEAD, Result, check, now, poisoned, run::NEXT_ROUND,
};
use a206_core::{EventKind, Id, Message, Reply, Role, Thread};
use std::sync::{MutexGuard, PoisonError};

/// Longest steered message, as for `Send`.
const MAX_TEXT: usize = 128 * 1024;

impl Gateway {
    pub(super) fn accepting(&self) -> Result<MutexGuard<'_, std::collections::BTreeSet<Id>>> {
        self.accepting.lock().map_err(poisoned("steering"))
    }
    /// Start taking messages for `thread`'s native run. The caller holds the
    /// store lock and is admitting that run.
    pub(super) fn open_steering(&self, thread: Id) -> Result<()> {
        self.accepting()?.insert(thread);
        Ok(())
    }
    /// Undo a run's admission that failed before it was saved, so neither
    /// `CancelRun` nor `Steer` sees a run that never started. A poisoned
    /// lock is taken anyway: removing the entry is still right.
    pub(super) fn withdraw(&self, thread: Id) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&thread);
        self.accepting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&thread);
    }
    /// Queue `text` for `thread`'s running native run.
    pub(super) fn steer(&self, thread: Id, text: String) -> Result<Reply> {
        check(
            !text.trim().is_empty() && text.len() <= MAX_TEXT,
            GatewayError::Invalid("message must be 1–131072 bytes"),
        )?;
        let mut store = self.lock()?;
        store.thread(thread)?;
        check(
            self.accepting()?.contains(&thread),
            GatewayError::Conflict(
                "the conversation has no native run taking messages; send it as a new message",
            ),
        )?;
        // As for `Send`: room for the message and the reply it leads to.
        check(
            store.remaining()? >= text.len() + MESSAGE_OVERHEAD + NEXT_ROUND,
            GatewayError::HistoryFull,
        )?;
        let current = store.thread_mut(thread)?;
        let message = Message::new(Role::User, text, now(), current.backend);
        current.steering.push(message);
        store.persist(Some(EventKind::ThreadChanged { id: thread }))?;
        Ok(Reply::Ok)
    }
    /// Move messages steered to `thread` into the conversation, before its
    /// run's next round.
    pub(super) fn drain_steering(&self, thread: Id) -> Result<()> {
        let mut store = self.lock()?;
        let current = store.thread_mut(thread)?;
        if current.steering.is_empty() {
            return Ok(());
        }
        let mut steered = std::mem::take(&mut current.steering);
        current.messages.append(&mut steered);
        store.persist(Some(EventKind::ThreadChanged { id: thread }))?;
        Ok(())
    }
    /// Whether `thread`'s run, about to end, has a steered message to answer
    /// first. When it has none the run stops taking messages, in the same
    /// critical section, so none can arrive after this answer.
    pub(super) fn steering_pending(&self, thread: Id) -> Result<bool> {
        let store = self.lock()?;
        let pending = !store.thread(thread)?.steering.is_empty();
        if !pending {
            self.accepting()?.remove(&thread);
        }
        Ok(pending)
    }
    /// Stop taking messages for `thread`'s ended run, keeping any that came
    /// too late for it in the conversation after its outcome. The caller
    /// holds the store lock and is settling that run.
    pub(super) fn settle_steering(&self, thread: &mut Thread) -> Result<()> {
        self.accepting()?.remove(&thread.id);
        thread.messages.append(&mut thread.steering);
        Ok(())
    }
}
