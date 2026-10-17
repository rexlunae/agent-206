//! A socket session run in its own task, for services that push events over
//! a WebSocket (Discord, Slack). The task keeps the session alive (answering
//! heartbeats and acknowledgements) while turns run, and queues events for
//! the supervisor's `receive`.
use super::{Batch, Event, MessengerError, Result};
use std::future::Future;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Events waiting for the gateway; past this the session restarts rather
/// than stop answering the service.
const BACKLOG: usize = 1024;
/// Most events handed to the supervisor at once.
const MAX_BATCH: usize = 64;

/// Where a session queues events.
pub(crate) struct Queue {
    service: &'static str,
    sender: mpsc::Sender<Result<(String, Event)>>,
}
impl Queue {
    /// Queue one event under its deduplication key. A full queue ends the
    /// session; a closed one means the messenger stopped.
    pub(crate) fn push(&self, key: String, event: Event) -> Result<()> {
        match self.sender.try_send(Ok((key, event))) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(MessengerError::Backlog {
                service: self.service,
            }),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(MessengerError::SocketEnded {
                service: self.service,
            }),
        }
    }
}

/// A running session task and what it has queued.
pub(crate) struct Listener {
    service: &'static str,
    /// The account the session was opened for.
    account: String,
    events: mpsc::Receiver<Result<(String, Event)>>,
    /// A failure that arrived behind queued events, reported next.
    failed: Option<MessengerError>,
    /// Stops the task when the listener is dropped.
    _stop: tokio_util::sync::DropGuard,
}

impl Listener {
    /// Run `session` for `account` until it fails or the listener is dropped.
    /// A session only ends by failing; its failure is queued last.
    pub(crate) fn start<F>(
        service: &'static str,
        account: String,
        session: impl FnOnce(Queue) -> F,
    ) -> Self
    where
        F: Future<Output = MessengerError> + Send + 'static,
    {
        let (sender, events) = mpsc::channel(BACKLOG);
        let stop = CancellationToken::new();
        let session = session(Queue {
            service,
            sender: sender.clone(),
        });
        // Detached: the drop guard cancels it with the listener, and it holds
        // nothing that needs awaiting. Its failure reaches `receive`.
        tokio::spawn(run(session, sender, stop.clone()));
        Self {
            service,
            account,
            events,
            failed: None,
            _stop: stop.drop_guard(),
        }
    }

    pub(crate) fn account(&self) -> &str {
        &self.account
    }

    /// The queued events, waiting for at least one, or the session's failure.
    pub(crate) async fn receive(&mut self) -> Result<Batch> {
        let first = match self.failed.take() {
            Some(error) => Err(error),
            None => match self.events.recv().await {
                Some(first) => first,
                None => Err(MessengerError::SocketEnded {
                    service: self.service,
                }),
            },
        }?;
        let mut events = vec![first];
        while events.len() < MAX_BATCH {
            match self.events.try_recv() {
                Ok(Ok(event)) => events.push(event),
                Ok(Err(error)) => {
                    self.failed = Some(error);
                    break;
                }
                Err(_) => break,
            }
        }
        Ok(Batch {
            events,
            complete: true,
            next: None,
        })
    }
}

/// The receive half of an adapter with a listener: the queued events, and a
/// failed session dropped so the next connect starts another.
pub(crate) async fn receive(
    listener: &mut Option<Listener>,
    service: &'static str,
) -> Result<Batch> {
    let Some(current) = listener.as_mut() else {
        return Err(MessengerError::SocketEnded { service });
    };
    let batch = current.receive().await;
    if batch.is_err() {
        *listener = None;
    }
    batch
}

async fn run(
    session: impl Future<Output = MessengerError>,
    sender: mpsc::Sender<Result<(String, Event)>>,
    stop: CancellationToken,
) {
    let error = tokio::select! {
        error = session => error,
        () = stop.cancelled() => return,
    };
    if let Err(unsent) = sender.send(Err(error)).await
        && let Err(error) = unsent.0
    {
        // The listener is gone, so nobody is waiting for this failure.
        tracing::debug!(
            error = &error as &dyn std::error::Error,
            "messenger session ended after its messenger stopped"
        );
    }
}
