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
        let endpoint = messenger.endpoint().ok_or(GatewayError::Invalid(
            "this messenger needs the URL of its server (for Matrix, the homeserver; for IRC, ircs://host:port; for Signal, the signal-cli daemon's socket; for a webhook, its URL's origin)",
        ))?;
        if messenger.kind.is_irc() {
            validate_irc(messenger, endpoint)?;
        } else if messenger.kind.is_signal() {
            validate_signal(messenger, endpoint)?;
        } else {
            let endpoint = a206_core::providers::base_url(endpoint)?;
            // The broker puts a Telegram token in the first path segment, so a
            // base path would move behind it; every kind keeps to an origin.
            check(
                endpoint.path() == "/",
                GatewayError::Invalid("a messenger endpoint is an origin, without a path"),
            )?;
            check(
                messenger.login.is_none(),
                GatewayError::Invalid("only IRC and Signal messengers sign in with a login"),
            )?;
        }
        check(
            !messenger.kind.is_send_only()
                || (messenger.allowed_chats.is_empty()
                    && messenger.allowed_users.is_empty()
                    && messenger.model.is_none()),
            GatewayError::Invalid(
                "a webhook only sends: it has no chats, senders or model to allow or choose",
            ),
        )?;
        check(
            messenger.credential.as_deref().is_none_or(field),
            GatewayError::Invalid("invalid messenger credential name"),
        )?;
        check(
            messenger.app_credential.as_deref().is_none_or(field),
            GatewayError::Invalid("invalid messenger app credential name"),
        )?;
        check(
            messenger.allowed_chats.len() <= MAX_ALLOWED_CHATS
                && messenger.allowed_chats.iter().all(|chat| field(chat)),
            GatewayError::Invalid("a messenger allows at most 256 chats with valid ids"),
        )?;
        check(
            messenger.allowed_users.len() <= MAX_ALLOWED_CHATS
                && messenger.allowed_users.iter().all(|user| field(user)),
            GatewayError::Invalid("a messenger allows at most 256 senders with valid ids"),
        )?;
    }
    Ok(())
}

/// An IRC entry names a TLS server and a nickname, and allows channels and
/// services accounts written as the network compares them, so a listed name
/// always matches and always stays one command parameter.
fn validate_irc(messenger: &MessengerConfig, endpoint: &str) -> Result<()> {
    use a206_core::messengers::{IrcServer, irc_lowercase, valid_irc_channel, valid_irc_nick};
    IrcServer::parse(endpoint).map_err(GatewayError::IrcServer)?;
    check(
        messenger.login.as_deref().is_some_and(valid_irc_nick),
        GatewayError::Invalid(
            "an IRC messenger needs a login: the bot's registered nickname (letters, digits and []\\^_`{|}-, at most 32)",
        ),
    )?;
    check(
        messenger
            .allowed_chats
            .iter()
            .all(|chat| valid_irc_channel(chat) || valid_irc_nick(chat)),
        GatewayError::Invalid(
            "IRC chats are channels (#name) or, for direct messages, services account names",
        ),
    )?;
    check(
        messenger
            .allowed_users
            .iter()
            .all(|user| valid_irc_nick(user)),
        GatewayError::Invalid("IRC senders are services account names"),
    )?;
    check(
        messenger
            .allowed_chats
            .iter()
            .chain(&messenger.allowed_users)
            .all(|name| irc_lowercase(name) == *name),
        GatewayError::Invalid("write IRC channels and account names in lowercase"),
    )?;
    Ok(())
}

/// A Signal entry names a local signal-cli daemon and the account it
/// serves, spends no vault credential, and allows direct chats and senders
/// by ACI and groups by their id, so a listed id is exactly what signal-cli
/// reports.
fn validate_signal(messenger: &MessengerConfig, endpoint: &str) -> Result<()> {
    use a206_core::messengers::{
        SignalDaemon, valid_signal_account, valid_signal_aci, valid_signal_group,
    };
    SignalDaemon::parse(endpoint).map_err(GatewayError::SignalDaemon)?;
    check(
        messenger.login.as_deref().is_some_and(valid_signal_account),
        GatewayError::Invalid(
            "a Signal messenger needs a login: the account signal-cli serves, as +number or its ACI",
        ),
    )?;
    check(
        messenger.credential.is_none() && messenger.app_credential.is_none(),
        GatewayError::Invalid(
            "a Signal messenger uses no vault credential: signal-cli keeps the account's keys",
        ),
    )?;
    check(
        messenger
            .allowed_chats
            .iter()
            .all(|chat| valid_signal_aci(chat) || valid_signal_group(chat)),
        GatewayError::Invalid(
            "Signal chats are group ids or, for direct messages, the sender's ACI (lowercase)",
        ),
    )?;
    check(
        messenger
            .allowed_users
            .iter()
            .all(|user| valid_signal_aci(user)),
        GatewayError::Invalid("Signal senders are ACIs (lowercase)"),
    )?;
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
            .persist(Some(EventKind::ThreadCreated { id }))
            .map_err(|e| conversation(e.into()))?;
        Ok(id)
    }
    async fn converse(&self, thread: Id, text: String) -> Result<Option<String>, MessengerError> {
        let gateway = self.clone();
        // The turn runs as its own task: `Send` is not cancel-safe (its run
        // registration and thread state are cleaned up only when it
        // completes), so a messenger that is stopped mid-turn drops this
        // handle, detaching the turn, which then ends on its own or through
        // shutdown's cancellation of registered runs.
        let response = tokio::spawn(async move {
            gateway
                .request(a206_protocol::Request {
                    id: Id::new_v4(),
                    body: Command::Send {
                        thread,
                        text,
                        attachments: vec![],
                    },
                })
                .await
        })
        .await
        .map_err(MessengerError::TurnAborted)?;
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

/// Sends through send-only messengers (webhooks) for the `send_message`
/// tool. It holds the gateway without its tool hosts, so holding it forms no
/// cycle, and offers only what the tool needs: which webhooks may be used and
/// a send to one of them.
#[derive(Clone)]
pub struct Outbox {
    base: Gateway,
}
impl Gateway {
    pub(crate) fn outbox(&self) -> Outbox {
        let mut base = self.clone();
        base.tool_hosts = Default::default();
        base.delegation = Default::default();
        Outbox { base }
    }
}
impl Outbox {
    /// The webhooks a model may post to: enabled and not disconnected by the
    /// user, by name, with their format.
    pub fn destinations(&self) -> Vec<(String, WebhookFormat)> {
        self.base
            .messengers
            .borrow()
            .iter()
            .filter(|entry| entry.run && entry.config.kind.is_send_only())
            .map(|entry| (entry.config.name.clone(), entry.config.format))
            .collect()
    }

    /// Send `text` to the webhook `name`; the number of parts sent. The
    /// messenger's status shows the outcome, so a failure is also visible on
    /// the Messengers page.
    pub async fn send(&self, name: &str, text: &str) -> Result<usize, MessengerError> {
        let config = self
            .base
            .messengers
            .borrow()
            .iter()
            .find(|entry| {
                entry.run && entry.config.kind.is_send_only() && entry.config.name == name
            })
            .map(|entry| entry.config.clone())
            .ok_or_else(|| MessengerError::NoWebhook {
                name: name.to_owned(),
            })?;
        let pairing = Host::pairing(&self.base).ok_or(MessengerError::NoPairing)?;
        // The user may withdraw the webhook while a long message is going
        // out; the parts still to send then stay unsent.
        let current = self.base.messengers.subscribe();
        let withdrawn = || {
            !current
                .borrow()
                .iter()
                .any(|entry| entry.run && entry.config == config)
        };
        let result = crate::messengers::webhook::send(
            Host::root(&self.base),
            pairing,
            &config,
            text,
            withdrawn,
        )
        .await;
        match &result {
            Ok(_) => self.base.update(name, |status| {
                status.state = MessengerState::Ready;
                status.failure = None;
            }),
            Err(error) => {
                let report = crate::error::handle("messenger", error);
                self.base
                    .update(name, |status| status.failure = Some(report));
            }
        }
        result
    }
}
