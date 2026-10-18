//! `agent206 messenger`: configure chat service accounts and connect them.
//! Every change is a `SaveSettings` through the gateway, which validates it;
//! entries are added disabled and allow no chat until the user says so.
use a206_core::{
    Command, MessengerConfig, MessengerKind, Mode, ModelKey, Reply, Settings, WebhookFormat,
};
use a206_runtime::client::Client;
use anyhow::{Result, bail};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum MessengerAction {
    /// List configured messengers with their status and refused chats.
    List,
    /// Add a disabled messenger. Store its bot token in the vault first.
    Add {
        name: String,
        /// The chat service.
        #[arg(long, default_value = "telegram")]
        kind: String,
        #[command(flatten)]
        options: Options,
    },
    /// Change a messenger's credential, endpoint, model or mode.
    Set {
        name: String,
        #[command(flatten)]
        options: Options,
    },
    /// Remove a messenger. Existing chat threads stay.
    Remove { name: String },
    /// Let a messenger connect when the gateway runs.
    Enable { name: String },
    /// Stop a messenger and keep it from connecting.
    Disable { name: String },
    /// Let a chat's messages reach a thread. In a group chat, only senders
    /// allowed with allow-sender are accepted.
    Allow { name: String, chat: String },
    /// Refuse a chat's messages from now on.
    Disallow { name: String, chat: String },
    /// Accept this sender's messages in allowed chats. Once any sender is
    /// allowed, other senders are refused in private chats too.
    AllowSender { name: String, sender: String },
    /// Refuse this sender's messages from now on.
    DisallowSender { name: String, sender: String },
    /// Reconnect an enabled messenger after a failure or a disconnect.
    Connect { name: String },
    /// Disconnect a messenger until the next connect or gateway restart.
    Disconnect { name: String },
}

#[derive(clap::Args)]
pub struct Options {
    /// Vault credential holding the bot or access token (Beeper: the Desktop
    /// API token; a webhook: its whole URL), never the token itself.
    #[arg(long)]
    credential: Option<String>,
    /// Vault credential holding the app-level token a service receives
    /// events with (Slack's xapp- token).
    #[arg(long)]
    app_credential: Option<String>,
    /// Account the bot signs in as (IRC: its registered nickname; Signal:
    /// the account's +number, as signal-cli serves it).
    #[arg(long)]
    login: Option<String>,
    /// API base URL, when not the service's official endpoint. Required for
    /// Matrix (the homeserver's client API origin), IRC (ircs://host:port)
    /// and Signal (the signal-cli daemon: unix:///path or tcp://127.0.0.1:port);
    /// for a webhook, its URL's origin (https://hooks.slack.com).
    #[arg(long)]
    endpoint: Option<String>,
    /// What a webhook posts: text (Slack, Google Chat and other JSON text
    /// webhooks), discord or teams.
    #[arg(long)]
    format: Option<String>,
    /// Provider id for new chat threads; requires --model.
    #[arg(long, requires = "model")]
    provider: Option<uuid::Uuid>,
    /// Model id, as the provider's API lists it; requires --provider.
    #[arg(long, requires = "provider")]
    model: Option<String>,
    /// Mode of new chat threads (chat unless set).
    #[arg(long)]
    mode: Option<String>,
}
impl Options {
    fn apply(self, config: &mut MessengerConfig) -> Result<()> {
        if let Some(credential) = self.credential {
            config.credential = Some(credential);
        }
        if let Some(credential) = self.app_credential {
            config.app_credential = Some(credential);
        }
        if let Some(login) = self.login {
            config.login = Some(login);
        }
        if let Some(endpoint) = self.endpoint {
            config.endpoint = Some(endpoint);
        }
        if let Some(format) = self.format {
            config.format = WebhookFormat::ALL
                .into_iter()
                .find(|f| f.key().eq_ignore_ascii_case(&format))
                .ok_or_else(|| {
                    anyhow::anyhow!("unknown webhook format; use text, discord or teams")
                })?;
        }
        if let Some((provider, model)) = self.provider.zip(self.model) {
            config.model = Some(ModelKey { provider, model });
        }
        if let Some(mode) = self.mode {
            config.mode = Mode::ALL
                .into_iter()
                .find(|m| m.label().eq_ignore_ascii_case(&mode))
                .ok_or_else(|| anyhow::anyhow!("unknown mode"))?;
        }
        Ok(())
    }
}

fn kind(value: &str) -> Result<MessengerKind> {
    MessengerKind::ALL
        .into_iter()
        .find(|k| k.label().eq_ignore_ascii_case(value))
        .ok_or_else(|| {
            let kinds: Vec<_> = MessengerKind::ALL
                .iter()
                .map(|k| k.label().to_lowercase())
                .collect();
            anyhow::anyhow!(
                "unknown messenger kind; Agent 206 supports {}",
                kinds.join(", ")
            )
        })
}

fn entry<'a>(settings: &'a mut Settings, name: &str) -> Result<&'a mut MessengerConfig> {
    match settings.messengers.iter_mut().find(|m| m.name == name) {
        Some(entry) => Ok(entry),
        None => bail!("no messenger named {name}; run messenger list"),
    }
}

/// A chat or sender as the messenger compares it ([`MessengerKind::fold`]),
/// so `#Rust` allows `#rust` on IRC.
fn fold(entry: &MessengerConfig, name: String) -> String {
    entry.kind.fold(&name)
}

/// The settings `action` saves, or `None` for actions that are not edits.
fn edit(mut settings: Settings, action: MessengerAction) -> Result<Option<Settings>> {
    match action {
        MessengerAction::Add {
            name,
            kind: service,
            options,
        } => {
            if settings.messengers.iter().any(|m| m.name == name) {
                bail!("a messenger named {name} already exists");
            }
            let mut config = MessengerConfig::new(name, kind(&service)?);
            options.apply(&mut config)?;
            settings.messengers.push(config);
        }
        MessengerAction::Set { name, options } => options.apply(entry(&mut settings, &name)?)?,
        MessengerAction::Remove { name } => {
            entry(&mut settings, &name)?;
            settings.messengers.retain(|m| m.name != name);
        }
        MessengerAction::Enable { name } => entry(&mut settings, &name)?.enabled = true,
        MessengerAction::Disable { name } => entry(&mut settings, &name)?.enabled = false,
        MessengerAction::Allow { name, chat } => {
            let entry = entry(&mut settings, &name)?;
            let chat = fold(entry, chat);
            entry.allowed_chats.insert(chat);
        }
        MessengerAction::Disallow { name, chat } => {
            let entry = entry(&mut settings, &name)?;
            let chat = fold(entry, chat);
            if !entry.allowed_chats.remove(&chat) {
                bail!("{name} does not allow chat {chat}");
            }
        }
        MessengerAction::AllowSender { name, sender } => {
            let entry = entry(&mut settings, &name)?;
            let sender = fold(entry, sender);
            entry.allowed_users.insert(sender);
        }
        MessengerAction::DisallowSender { name, sender } => {
            let entry = entry(&mut settings, &name)?;
            let sender = fold(entry, sender);
            if !entry.allowed_users.remove(&sender) {
                bail!("{name} does not allow sender {sender}");
            }
        }
        MessengerAction::List
        | MessengerAction::Connect { .. }
        | MessengerAction::Disconnect { .. } => return Ok(None),
    }
    Ok(Some(settings))
}

pub async fn run(client: &Client, action: MessengerAction) -> Result<()> {
    let command = match action {
        MessengerAction::Connect { name } => Command::ConnectMessenger { name },
        MessengerAction::Disconnect { name } => Command::DisconnectMessenger { name },
        action => {
            let snapshot = match client.call(Command::Snapshot).await? {
                Reply::Snapshot(snapshot) => snapshot,
                Reply::Error(report) => return Err(report.into()),
                _ => bail!("gateway did not return settings"),
            };
            match edit(snapshot.settings.clone(), action)? {
                Some(settings) => Command::SaveSettings(Box::new(settings)),
                None => {
                    #[derive(serde::Serialize)]
                    struct Listing<'a> {
                        messengers: &'a [MessengerConfig],
                        status: &'a [a206_core::MessengerStatus],
                    }
                    print!(
                        "{}",
                        toml::to_string_pretty(&Listing {
                            messengers: &snapshot.settings.messengers,
                            status: &snapshot.messengers,
                        })?
                    );
                    return Ok(());
                }
            }
        }
    };
    match client.call(command).await? {
        Reply::Ok => println!("OK"),
        Reply::Error(report) => return Err(report.into()),
        _ => bail!("gateway sent an unexpected reply"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random() -> String {
        format!("m-{}", uuid::Uuid::new_v4().simple())
    }

    fn options() -> Options {
        Options {
            credential: Some(random()),
            app_credential: None,
            login: None,
            endpoint: None,
            provider: None,
            model: None,
            mode: None,
            format: None,
        }
    }

    fn add(name: &str) -> MessengerAction {
        MessengerAction::Add {
            name: name.to_owned(),
            kind: "telegram".into(),
            options: options(),
        }
    }

    #[test]
    fn added_messengers_are_disabled_until_enabled_and_allow_only_named_chats() {
        let name = random();
        let settings = edit(Settings::default(), add(&name)).unwrap().unwrap();
        let config = &settings.messengers[0];
        assert!(!config.enabled && config.allowed_chats.is_empty());
        assert!(edit(settings.clone(), add(&name)).is_err());

        // Set changes only what it names.
        let model = ModelKey {
            provider: uuid::Uuid::new_v4(),
            model: random(),
        };
        let mut change = options();
        change.credential = None;
        change.provider = Some(model.provider);
        change.model = Some(model.model.clone());
        let before = settings.messengers[0].clone();
        let settings = edit(
            settings,
            MessengerAction::Set {
                name: name.clone(),
                options: change,
            },
        )
        .unwrap()
        .unwrap();
        let config = &settings.messengers[0];
        assert_eq!(config.model.as_ref(), Some(&model));
        assert_eq!(config.credential, before.credential);
        assert_eq!(config.mode, before.mode);

        let enabled = edit(settings, MessengerAction::Enable { name: name.clone() })
            .unwrap()
            .unwrap();
        assert!(enabled.messengers[0].enabled);
        let chat = random();
        let allowed = edit(
            enabled,
            MessengerAction::Allow {
                name: name.clone(),
                chat: chat.clone(),
            },
        )
        .unwrap()
        .unwrap();
        assert!(allowed.messengers[0].allowed_chats.contains(&chat));
        let sender = random();
        let allowed = edit(
            allowed,
            MessengerAction::AllowSender {
                name: name.clone(),
                sender: sender.clone(),
            },
        )
        .unwrap()
        .unwrap();
        assert!(allowed.messengers[0].allowed_users.contains(&sender));
        let unlisted = MessengerAction::DisallowSender {
            name: name.clone(),
            sender: random(),
        };
        assert!(edit(allowed.clone(), unlisted).is_err());
        let allowed = edit(
            allowed,
            MessengerAction::DisallowSender {
                name: name.clone(),
                sender,
            },
        )
        .unwrap()
        .unwrap();
        assert!(allowed.messengers[0].allowed_users.is_empty());
        let disallow = |chat: &str| MessengerAction::Disallow {
            name: name.clone(),
            chat: chat.to_owned(),
        };
        assert!(edit(allowed.clone(), disallow(&random())).is_err());
        let refused = edit(allowed, disallow(&chat)).unwrap().unwrap();
        assert!(refused.messengers[0].allowed_chats.is_empty());
        let removed = edit(refused, MessengerAction::Remove { name: name.clone() })
            .unwrap()
            .unwrap();
        assert!(removed.messengers.is_empty());
        assert!(edit(removed, MessengerAction::Enable { name }).is_err());
    }

    #[test]
    fn unknown_kinds_and_modes_are_refused() {
        let mut action = add(&random());
        if let MessengerAction::Add { kind, .. } = &mut action {
            *kind = random();
        }
        assert!(edit(Settings::default(), action).is_err());
        let mut action = add(&random());
        if let MessengerAction::Add { options, .. } = &mut action {
            options.mode = Some(random());
        }
        assert!(edit(Settings::default(), action).is_err());
    }

    #[test]
    fn irc_entries_take_a_login_and_fold_names_like_the_network() {
        let (name, login) = (random(), random());
        let action = MessengerAction::Add {
            name: name.clone(),
            kind: "irc".into(),
            options: Options {
                login: Some(login.clone()),
                ..options()
            },
        };
        let settings = edit(Settings::default(), action).unwrap().unwrap();
        assert_eq!(settings.messengers[0].kind, MessengerKind::Irc);
        assert_eq!(settings.messengers[0].login, Some(login));
        let chat = format!("#{}", random().to_uppercase());
        let allow = MessengerAction::Allow {
            name: name.clone(),
            chat: chat.clone(),
        };
        let settings = edit(settings, allow).unwrap().unwrap();
        assert!(
            settings.messengers[0]
                .allowed_chats
                .contains(&chat.to_lowercase())
        );
        let sender = random().to_uppercase();
        let allow = MessengerAction::AllowSender {
            name: name.clone(),
            sender: sender.clone(),
        };
        let settings = edit(settings, allow).unwrap().unwrap();
        assert!(
            settings.messengers[0]
                .allowed_users
                .contains(&sender.to_lowercase())
        );
        let disallow = MessengerAction::Disallow { name, chat };
        let settings = edit(settings, disallow).unwrap().unwrap();
        assert!(settings.messengers[0].allowed_chats.is_empty());
    }

    #[test]
    fn webhooks_take_a_format_and_unknown_formats_are_refused() {
        let name = random();
        let add = |format: &str| MessengerAction::Add {
            name: name.clone(),
            kind: "webhook".into(),
            options: Options {
                format: Some(format.to_owned()),
                ..options()
            },
        };
        for format in WebhookFormat::ALL {
            let settings = edit(Settings::default(), add(&format.key().to_uppercase()))
                .unwrap()
                .unwrap();
            assert_eq!(settings.messengers[0].kind, MessengerKind::Webhook);
            assert_eq!(settings.messengers[0].format, format);
        }
        assert!(edit(Settings::default(), add(&random())).is_err());
    }
}
