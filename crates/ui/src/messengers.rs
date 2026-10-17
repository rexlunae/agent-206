//! Messengers: chat service accounts whose allowed chats talk to threads.
//! Every change is a `SaveSettings` the gateway validates; nothing here
//! connects an entry or allows a chat unless the user does it.
use crate::{PageFrame, State, dispatch, error::ErrorNotice};
use a206_core::{
    Command, MessengerConfig, MessengerKind, MessengerState, MessengerStatus, Mode, ModelKey,
    RefusedChat, Snapshot,
};
use fc_dioxus::prelude::*;
use uuid::Uuid;

#[component]
pub fn Messengers() -> Element {
    let state = use_context::<State>();
    // Catalogs expire without the snapshot changing, so the page keeps its
    // own clock: it wakes at the next expiry (at most a minute away) and
    // renders the cards with the new time.
    let mut clock = use_signal(now);
    use_future(move || async move {
        loop {
            let at = *clock.peek();
            let next = state
                .data
                .peek()
                .catalogs
                .iter()
                .map(|c| c.expires_at)
                .filter(|&expires| expires > at)
                .min();
            let wait = next.map_or(60, |expires| (expires - at).min(60));
            futures_timer::Delay::new(std::time::Duration::from_secs(wait)).await;
            clock.set(now());
        }
    });
    let now = clock();
    let snapshot = state.data.read();
    rsx! { PageFrame { kicker: "Messengers", title: "Talk to a thread from chat.", description: "Each allowed chat gets its own conversation, which runs with that conversation's permissions and model. Bot tokens stay in the vault.",
        for messenger in &snapshot.settings.messengers {
            MessengerCard { key: "{messenger.name}", now, config: messenger.clone(), status: snapshot.messengers.iter().find(|s| s.name == messenger.name).cloned().unwrap_or_default() }
        }
        AddMessenger {}
    } }
}

/// Save settings with the entry named `name` changed by `edit`.
fn save_messenger(state: State, name: &str, edit: impl FnOnce(&mut MessengerConfig)) {
    let mut settings = state.data.read().settings.clone();
    if let Some(entry) = settings.messengers.iter_mut().find(|m| m.name == name) {
        edit(entry);
        dispatch(state, Command::SaveSettings(settings));
    }
}

fn tone(state: MessengerState) -> BadgeTone {
    match state {
        MessengerState::Connected => BadgeTone::Success,
        MessengerState::Connecting | MessengerState::WaitingForVault => BadgeTone::Info,
        MessengerState::Failed => BadgeTone::Danger,
        MessengerState::Stopped => BadgeTone::Neutral,
    }
}

/// Seconds since the Unix epoch, for judging catalog freshness. A clock set
/// before the epoch makes every catalog stale rather than every one fresh.
fn now() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(u64::MAX, |elapsed| elapsed.as_secs())
}

/// Models of catalogs that have not expired at `now`: the gateway creates a
/// chat thread only for one of these.
fn current_models(snapshot: &Snapshot, now: u64) -> impl Iterator<Item = &a206_core::ModelInfo> {
    snapshot
        .catalogs
        .iter()
        .filter(move |c| !c.stale(now))
        .flat_map(|c| &c.models)
}

/// `provider:model` choices from the current API catalogs, grouped by connection.
fn model_choices(snapshot: &Snapshot, now: u64) -> Vec<SelectChoice> {
    current_models(snapshot, now)
        .map(|model| {
            let provider = snapshot
                .settings
                .providers
                .iter()
                .find(|p| p.id == model.key.provider)
                .map(|p| p.label.as_str())
                .unwrap_or("Unavailable connection");
            SelectChoice::new(model_value(&model.key), &model.name).in_group(provider)
        })
        .collect()
}

fn model_value(key: &ModelKey) -> String {
    format!("{}:{}", key.provider, key.model)
}

fn parse_model(value: &str) -> Option<ModelKey> {
    let (provider, model) = value.split_once(':')?;
    Some(ModelKey {
        provider: Uuid::parse_str(provider).ok()?,
        model: model.to_owned(),
    })
}

#[component]
/// `now` is the page clock, so choices and the warning agree on freshness.
fn MessengerCard(now: u64, config: MessengerConfig, status: MessengerStatus) -> Element {
    let state = use_context::<State>();
    let name = config.name.clone();
    let enabled = config.enabled;
    let enabled = use_memo(use_reactive!(|enabled| if enabled {
        CheckState::On
    } else {
        CheckState::Off
    }));
    let model = config.model.clone();
    let model_selected = use_memo(use_reactive!(|model| model.as_ref().map(model_value)));
    let mode = config.mode;
    let mode_selected = use_memo(use_reactive!(|mode| Some(mode.label().to_owned())));
    let snapshot = state.data.read();
    let models = model_choices(&snapshot, now);
    let model_known = config
        .model
        .as_ref()
        .is_none_or(|key| current_models(&snapshot, now).any(|m| &m.key == key));
    let running = matches!(
        status.state,
        MessengerState::Connected | MessengerState::Connecting | MessengerState::WaitingForVault
    );
    let endpoint = config.endpoint().unwrap_or("no server chosen").to_owned();
    rsx! { Card {
        CardHeader { CardTitle { "{config.name}" } CardSubtitle { "{config.kind.label()} · {endpoint}" } }
        CardBody {
            div { class: "a206-row",
                Badge { tone: tone(status.state), "{status.state.label()}" }
                if let Some(account) = &status.account { span { class: "a206-muted", "Bot account {account}" } }
            }
            p { class: "a206-muted", match &config.credential { Some(credential) => format!("Token: vault credential {credential}"), None => "No vault credential chosen; it cannot connect.".to_owned() } }
            Checkbox { label: rsx! { "Enabled" }, checked: ReadSignal::from(enabled), onchange: { let name = name.clone(); move |on: bool| save_messenger(state, &name, |m| m.enabled = on) } }
            if let Some(report) = status.failure.clone() { ErrorNotice { report, title: "Messenger failed".to_string() } }
            div { class: "a206-grid",
                Field { label: rsx! { "Model for new chats" }, hint: rsx! { "The only model a chat's conversation may use. Without one, chats are told no model is chosen." },
                    if models.is_empty() {
                        p { class: "a206-muted", "Connect or refresh a provider to discover models." }
                    } else {
                        Select { label: name!("Model for new chats"), value: ReadSignal::from(model_selected), options: models, placeholder: "Choose model",
                            onchange: { let name = name.clone(); move |value: String| { let model = parse_model(&value); save_messenger(state, &name, |m| m.model = model) } }
                        }
                    }
                }
                Field { label: rsx! { "Mode for new chats" },
                    Select { label: name!("Mode for new chats"), value: ReadSignal::from(mode_selected),
                        options: Mode::ALL.into_iter().map(|m| SelectChoice::new(m.label(), m.label())).collect(),
                        onchange: { let name = name.clone(); move |value: String| { if let Some(mode) = Mode::ALL.into_iter().find(|m| m.label() == value) { save_messenger(state, &name, |m| m.mode = mode) } } }
                    }
                }
            }
            if !model_known { Alert { tone: AlertTone::Warning, title: rsx! { "Model not in a current catalog" }, "The chosen model is not listed by its provider, or that list has expired. New chats cannot start until you refresh the provider on the Connections page or choose another model." } }
            AllowList { messenger: config.name.clone(), title: "Allowed chats", hint: "Chat ids whose messages reach a conversation.", entries: config.allowed_chats.iter().cloned().collect::<Vec<_>>(), senders: false }
            AllowList { messenger: config.name.clone(), title: "Allowed senders", hint: "User ids accepted in allowed chats. Group chats accept only these senders; once any is listed, private chats do too.", entries: config.allowed_users.iter().cloned().collect::<Vec<_>>(), senders: true }
            if !status.refused.is_empty() {
                h3 { "Refused recently" }
                p { class: "a206-muted", "The latest {a206_core::messengers::MAX_REFUSED_CHATS} refused chats or senders are shown. Older entries are replaced; add other room IDs under Allowed chats. Titles come from the chat service and may be misleading. Allow only chats and people you recognize." }
                for refused in status.refused.iter().rev() {
                    RefusedRow { key: "{refused.chat}-{refused.sender:?}", messenger: config.name.clone(), refused: refused.clone(), offer: Offer::for_refused(&config, refused) }
                }
            }
        }
        CardFooter {
            if running {
                Button { variant: ButtonVariant::Secondary, onclick: { let name = name.clone(); move |_| dispatch(state, Command::DisconnectMessenger { name: name.clone() }) }, "Disconnect" }
            } else {
                Button { variant: ButtonVariant::Secondary, availability: if config.enabled { Availability::Enabled } else { Availability::Disabled }, onclick: { let name = name.clone(); move |_| dispatch(state, Command::ConnectMessenger { name: name.clone() }) }, "Connect" }
            }
            Button { variant: ButtonVariant::Ghost, onclick: move |_| { let mut settings = state.data.read().settings.clone(); settings.messengers.retain(|m| m.name != name); dispatch(state, Command::SaveSettings(settings)); }, "Remove messenger" }
        }
    } }
}

/// Edit one of an entry's allow-lists: chats, or senders when `senders`.
fn edit_list(
    state: State,
    messenger: &str,
    senders: bool,
    edit: impl FnOnce(&mut std::collections::BTreeSet<String>),
) {
    save_messenger(state, messenger, |m| {
        edit(if senders {
            &mut m.allowed_users
        } else {
            &mut m.allowed_chats
        })
    });
}

#[component]
fn AllowList(
    messenger: String,
    title: String,
    hint: String,
    entries: Vec<String>,
    senders: bool,
) -> Element {
    let state = use_context::<State>();
    let mut draft = use_signal(String::new);
    let ready = !draft().trim().is_empty();
    rsx! {
        h3 { "{title}" }
        p { class: "a206-muted", "{hint}" }
        if entries.is_empty() { p { class: "a206-muted", "None." } }
        for entry in entries {
            div { key: "{entry}", class: "a206-row a206-between",
                code { "{entry}" }
                Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, onclick: { let messenger = messenger.clone(); let entry = entry.clone(); move |_| edit_list(state, &messenger, senders, |list| { list.remove(&entry); }) }, "Remove" }
            }
        }
        div { class: "a206-row",
            Input { aria_label: if senders { "Sender id to allow" } else { "Chat id to allow" }, value: ReadSignal::from(draft), oninput: move |v| draft.set(v), placeholder: if senders { "Sender id" } else { "Chat id" } }
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, availability: if ready { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| {
                let value = draft().trim().to_owned();
                edit_list(state, &messenger, senders, |list| { list.insert(value); });
                draft.set(String::new());
            }, if senders { "Allow sender" } else { "Allow chat" } }
        }
    }
}

/// What allowing a refused message would add: its chat, or its sender in an
/// already allowed chat. `None` when allowing it needs nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Offer {
    Chat(String),
    Sender(String),
}

impl Offer {
    fn for_refused(config: &MessengerConfig, refused: &RefusedChat) -> Option<Self> {
        let chat_allowed = config.allowed_chats.contains(&refused.chat);
        match &refused.sender {
            Some(sender) => (chat_allowed && !config.allowed_users.contains(sender))
                .then(|| Self::Sender(sender.clone())),
            None => (!chat_allowed).then(|| Self::Chat(refused.chat.clone())),
        }
    }
    fn apply(&self, config: &mut MessengerConfig) {
        match self {
            Self::Chat(chat) => config.allowed_chats.insert(chat.clone()),
            Self::Sender(sender) => config.allowed_users.insert(sender.clone()),
        };
    }
    fn label(&self) -> &'static str {
        match self {
            Self::Chat(_) => "Allow chat",
            Self::Sender(_) => "Allow sender",
        }
    }
}

#[component]
fn RefusedRow(messenger: String, refused: RefusedChat, offer: Option<Offer>) -> Element {
    let state = use_context::<State>();
    let who = match (&refused.sender, &refused.title) {
        (Some(sender), Some(title)) => {
            format!("Sender {sender} in chat {} ({title})", refused.chat)
        }
        (Some(sender), None) => format!("Sender {sender} in chat {}", refused.chat),
        (None, Some(title)) => format!("Chat {} ({title})", refused.chat),
        (None, None) => format!("Chat {}", refused.chat),
    };
    let times = if refused.count == 1 {
        "once".to_owned()
    } else {
        format!("{} times", refused.count)
    };
    rsx! { div { class: "a206-row a206-between",
        span { "{who} · refused {times}" }
        if let Some(offer) = offer {
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, onclick: { let offer = offer.clone(); move |_| save_messenger(state, &messenger, |m| offer.apply(m)) }, "{offer.label()}" }
        }
    } }
}

#[component]
fn AddMessenger() -> Element {
    let state = use_context::<State>();
    let mut name = use_signal(String::new);
    let mut kind = use_signal(|| Some(MessengerKind::Telegram.label().to_owned()));
    let mut credential = use_signal(String::new);
    let mut endpoint = use_signal(String::new);
    let selected = MessengerKind::ALL
        .into_iter()
        .find(|k| Some(k.label()) == kind().as_deref());
    // A federated service has no official server to fall back on.
    let needs_endpoint = selected.is_some_and(|k| k.default_endpoint().is_none());
    let ready = selected.is_some()
        && !name().trim().is_empty()
        && !credential().trim().is_empty()
        && (!needs_endpoint || !endpoint().trim().is_empty());
    let endpoint_hint = if needs_endpoint {
        "Required: your homeserver's client API origin, with no path."
    } else {
        "Leave empty for the service's official API. An origin only, with no path."
    };
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Add a messenger" } CardSubtitle { "It stays off and allows no chat until you say so." } }
        CardBody {
            if let Some(kind) = selected { p { class: "a206-muted", "{kind.credential_help()}." } }
            div { class: "a206-grid",
                Field { label: rsx! { "Name" }, hint: rsx! { "Lowercase letters, digits and hyphens." }, Input { value: ReadSignal::from(name), oninput: move |v| name.set(v), placeholder: "team" } }
                Field { label: rsx! { "Service" }, Select { label: name!("Chat service"), value: ReadSignal::from(kind),
                    options: MessengerKind::ALL.into_iter().map(|k| SelectChoice::new(k.label(), k.label())).collect(),
                    onchange: move |value: String| kind.set(Some(value))
                } }
                Field { label: rsx! { "Vault credential" }, hint: rsx! { "The name of the vault entry holding the token, never the token itself." }, Input { value: ReadSignal::from(credential), oninput: move |v| credential.set(v), placeholder: "Credential name" } }
                Field { label: rsx! { "API endpoint" }, hint: rsx! { "{endpoint_hint}" }, Input { value: ReadSignal::from(endpoint), oninput: move |v| endpoint.set(v), placeholder: selected.and_then(|k| k.default_endpoint()).unwrap_or("https://homeserver.example") } }
            }
        }
        CardFooter { Button { variant: ButtonVariant::Primary, availability: if ready { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| {
            let Some(service) = selected else { return };
            let mut entry = MessengerConfig::new(name().trim().to_owned(), service);
            entry.credential = Some(credential().trim().to_owned());
            entry.endpoint = Some(endpoint().trim().to_owned()).filter(|e| !e.is_empty());
            let mut settings = state.data.read().settings.clone();
            settings.messengers.push(entry);
            dispatch(state, Command::SaveSettings(settings));
        }, "Add messenger" } }
    } }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(chat: &str, sender: Option<&str>) -> RefusedChat {
        RefusedChat {
            chat: chat.to_owned(),
            sender: sender.map(str::to_owned),
            title: None,
            count: 1,
            last_at: 0,
        }
    }

    #[test]
    fn an_offer_admits_exactly_the_refused_message_and_then_disappears() {
        let id = || Uuid::new_v4().simple().to_string();
        let mut config = MessengerConfig::new(format!("m-{}", id()), MessengerKind::Telegram);
        let (chat, sender) = (id(), id());
        // A refused private chat: allowing the chat admits it.
        let private = refused(&chat, None);
        assert!(!config.accepts(&chat, Some(&chat), false));
        let offer = Offer::for_refused(&config, &private).unwrap();
        offer.apply(&mut config);
        assert!(config.accepts(&chat, Some(&chat), false));
        assert_eq!(Offer::for_refused(&config, &private), None);
        // A refused group sender in that chat: allowing the sender admits it.
        let member = refused(&chat, Some(&sender));
        assert!(!config.accepts(&chat, Some(&sender), true));
        let offer = Offer::for_refused(&config, &member).unwrap();
        offer.apply(&mut config);
        assert!(config.accepts(&chat, Some(&sender), true));
        assert_eq!(Offer::for_refused(&config, &member), None);
        // A sender in a chat that is not allowed is offered nothing: allowing
        // the sender alone would not admit them.
        assert_eq!(
            Offer::for_refused(&config, &refused(&id(), Some(&id()))),
            None
        );
    }

    #[test]
    fn expired_catalogs_offer_no_models() {
        let key = ModelKey {
            provider: Uuid::new_v4(),
            model: Uuid::new_v4().to_string(),
        };
        let expires_at = u64::from(Uuid::new_v4().as_fields().1) + 1;
        let mut snapshot = crate::empty_snapshot();
        snapshot.catalogs.push(a206_core::Catalog {
            provider: key.provider,
            source: Uuid::new_v4().to_string(),
            fetched_at: 0,
            expires_at,
            models: vec![a206_core::ModelInfo {
                key: key.clone(),
                name: Uuid::new_v4().to_string(),
                context_limit: None,
                output_limit: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                reasoning: None,
                reasoning_efforts: vec![],
            }],
            balance: None,
            balance_currency: None,
            token_balance: None,
        });
        assert_eq!(model_choices(&snapshot, expires_at - 1).len(), 1);
        assert!(current_models(&snapshot, expires_at - 1).any(|m| m.key == key));
        assert!(model_choices(&snapshot, expires_at).is_empty());
        assert_eq!(current_models(&snapshot, expires_at).count(), 0);
    }

    #[test]
    fn model_values_round_trip_even_when_ids_contain_colons() {
        let key = ModelKey {
            provider: Uuid::new_v4(),
            model: format!("{}:{}", Uuid::new_v4(), Uuid::new_v4()),
        };
        assert_eq!(parse_model(&model_value(&key)), Some(key));
        assert_eq!(parse_model(&Uuid::new_v4().to_string()), None);
    }
}
