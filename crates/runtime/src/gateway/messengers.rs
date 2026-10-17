//! Gateway side of messengers: settings validation, user connect/disconnect,
//! published status, and the [`Host`] the supervisor drives. A chat turn is an
//! ordinary `Send` request, journaled and admitted like one from the app.
use super::{Gateway, GatewayError, Result, poisoned};
use crate::error::check;
use crate::messengers::{Desired, Host, MessengerError};
use a206_core::messengers::{MAX_ALLOWED_CHATS, valid_field as field, valid_name};
use a206_core::*;
use std::collections::BTreeSet;

/// Refuse messenger entries the gateway could not run safely.
pub(super) fn validate(messengers: &[MessengerConfig]) -> Result<()> {
    let mut names = BTreeSet::new();
    for messenger in messengers {
        check(
            valid_name(&messenger.name),
            GatewayError::Invalid(
                "messenger names use lowercase letters, digits and single hyphens (at most 64)",
            ),
        )?;
        check(
            names.insert(messenger.name.as_str()),
            GatewayError::Invalid("messenger names must be unique"),
        )?;
        a206_core::providers::base_url(messenger.endpoint())?;
        check(
            messenger.credential.as_deref().is_none_or(field),
            GatewayError::Invalid("invalid messenger credential name"),
        )?;
        check(
            messenger.allowed_chats.len() <= MAX_ALLOWED_CHATS
                && messenger.allowed_chats.iter().all(|chat| field(chat)),
            GatewayError::Invalid("a messenger allows at most 256 chats with valid ids"),
        )?;
    }
    Ok(())
}

impl Gateway {
    /// Publish what should run: enabled entries the user has not disconnected.
    pub(super) fn publish_messengers(&self) -> Result<()> {
        let configs = self.lock()?.state.settings.messengers.clone();
        let stopped = self
            .messengers_stopped
            .lock()
            .map_err(poisoned("messengers"))?
            .clone();
        let desired = configs
            .into_iter()
            .map(|config| Desired {
                run: config.enabled && !stopped.contains(&config.name),
                config,
            })
            .collect();
        self.messengers.send_replace(desired);
        Ok(())
    }

    /// Statuses for the settings about to be saved: one per entry, keeping
    /// what is known about entries that remain.
    pub(super) fn messenger_statuses(
        current: &[MessengerStatus],
        messengers: &[MessengerConfig],
    ) -> Vec<MessengerStatus> {
        messengers
            .iter()
            .map(|config| {
                current
                    .iter()
                    .find(|s| s.name == config.name)
                    .cloned()
                    .unwrap_or_else(|| MessengerStatus {
                        name: config.name.clone(),
                        ..MessengerStatus::default()
                    })
            })
            .collect()
    }

    pub(super) fn connect_messenger(&self, name: &str, connect: bool) -> Result<Reply> {
        let enabled = self
            .lock()?
            .state
            .settings
            .messengers
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.enabled)
            .ok_or(GatewayError::NotFound("messenger not configured"))?;
        check(
            enabled || !connect,
            GatewayError::Invalid("enable the messenger in settings before connecting it"),
        )?;
        {
            let mut stopped = self
                .messengers_stopped
                .lock()
                .map_err(poisoned("messengers"))?;
            if connect {
                stopped.remove(name);
            } else {
                stopped.insert(name.to_owned());
            }
        }
        self.publish_messengers()?;
        Ok(Reply::Ok)
    }

    /// Run messengers until the gateway stops; see [`crate::messengers::supervise`].
    pub async fn supervise_messengers(&self) {
        let desired = self.messengers.subscribe();
        crate::messengers::supervise(
            self.clone(),
            desired,
            crate::messengers::Backoff::default(),
            self.stop.clone(),
        )
        .await;
    }
}

impl Host for Gateway {
    fn root(&self) -> std::path::PathBuf {
        match self.lock() {
            Ok(store) => store.root.clone(),
            Err(error) => {
                // Unreachable in practice; without a root no request can be
                // made, and the broker call reports the failure.
                crate::error::handle("messenger", &error);
                std::path::PathBuf::new()
            }
        }
    }
    fn pairing(&self) -> Option<BrokerPairing> {
        match Gateway::pairing(self) {
            Ok(pairing) => pairing,
            Err(error) => {
                crate::error::handle("messenger", &error);
                None
            }
        }
    }
    fn pairings(&self) -> tokio::sync::watch::Receiver<u64> {
        self.pairings.subscribe()
    }
    fn thread_for(
        &self,
        config: &MessengerConfig,
        chat: &ChatBinding,
        title: &str,
    ) -> Result<Id, MessengerError> {
        let conversation = |e: GatewayError| MessengerError::Conversation(Box::new(e));
        let mut store = self.lock().map_err(conversation)?;
        if let Some(thread) = store
            .state
            .threads
            .iter()
            .find(|t| t.chat.as_deref() == Some(chat))
        {
            return Ok(thread.id);
        }
        let mut thread = Thread::new(title.chars().take(512).collect(), config.mode);
        if let Some(model) = &config.model {
            super::validate_model(&store.state, model).map_err(conversation)?;
            thread.grant.models.insert(model.clone());
            thread.model = Some(model.clone());
        }
        thread.chat = Some(Box::new(chat.clone()));
        let id = thread.id;
        store.state.threads.push(thread);
        store
            .persist(Some(EventKind::ThreadChanged { id }))
            .map_err(|e| conversation(e.into()))?;
        Ok(id)
    }
    async fn converse(&self, thread: Id, text: String) -> Result<Option<String>, MessengerError> {
        let response = self
            .request(a206_protocol::Request {
                id: Id::new_v4(),
                body: Command::Send {
                    thread,
                    text,
                    attachments: vec![],
                },
            })
            .await;
        let thread = match response.body {
            Reply::Thread(thread) => thread,
            Reply::Error(report) => return Err(MessengerError::Turn(report)),
            _ => return Err(MessengerError::UnexpectedReply),
        };
        if let Some(report) = thread.error.filter(|_| thread.state == RunState::Failed) {
            return Err(MessengerError::Turn(report));
        }
        // The reply is the assistant text after the last user message.
        let start = thread
            .messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .map_or(0, |i| i + 1);
        Ok(thread.messages[start..]
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && !m.text.trim().is_empty())
            .map(|m| m.text.clone()))
    }
    fn update(&self, name: &str, change: impl FnOnce(&mut MessengerStatus) + Send) {
        let result = self.lock().and_then(|mut store| {
            let Some(status) = store.state.messengers.iter_mut().find(|s| s.name == name) else {
                // Removed from settings meanwhile; nothing to report on.
                return Ok(());
            };
            let before = status.clone();
            change(status);
            if *status != before {
                store.persist(Some(EventKind::MessengerChanged {
                    name: name.to_owned(),
                }))?;
            }
            Ok(())
        });
        if let Err(error) = result {
            crate::error::handle("messenger_status", &error);
        }
    }
}
