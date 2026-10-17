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
    let snapshot = state.data.read();
    rsx! { PageFrame { kicker: "Messengers", title: "Talk to a thread from chat.", description: "Each allowed chat gets its own conversation, which runs with that conversation's permissions and model. Bot tokens stay in the vault.",
        for messenger in &snapshot.settings.messengers {
            MessengerCard { key: "{messenger.name}", config: messenger.clone(), status: snapshot.messengers.iter().find(|s| s.name == messenger.name).cloned().unwrap_or_default() }
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

/// `provider:model` choices from the current API catalogs, grouped by connection.
fn model_choices(snapshot: &Snapshot) -> Vec<SelectChoice> {
    snapshot
        .catalogs
        .iter()
        .flat_map(|c| &c.models)
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
fn MessengerCard(config: MessengerConfig, status: MessengerStatus) -> Element {
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
    let models = model_choices(&snapshot);
    let model_known = config.model.as_ref().is_none_or(|key| {
        snapshot
            .catalogs
            .iter()
            .flat_map(|c| &c.models)
            .any(|m| &m.key == key)
    });
    let running = matches!(
        status.state,
        MessengerState::Connected | MessengerState::Connecting | MessengerState::WaitingForVault
    );
    let endpoint = config
        .endpoint
        .clone()
        .unwrap_or_else(|| config.kind.default_endpoint().to_owned());
    rsx! { Card {
        CardHeader { CardTitle { "{config.name}" } CardSubtitle { "{config.kind.label()} · {endpoint}" } }
        CardBody {
            div { class: "a206-row",
                Badge { tone: tone(status.state), "{status.state.label()}" }
                if let Some(account) = &status.account { span { class: "a206-muted", "Bot account {account}" } }
            }
            p { class: "a206-muted", match &config.credential { Some(credential) => format!("Bot token: vault credential {credential}"), None => "No vault credential chosen; it cannot connect.".to_owned() } }
            Checkbox { label: rsx! { "Enabled" }, checked: ReadSignal::from(enabled), onchange: { let name = name.clone(); move |on: bool| save_messenger(state, &name, |m| m.enabled = on) } }
            if let Some(report) = status.failure.clone() { ErrorNotice { report, title: "Messenger failed".to_string() } }
            div { class: "a206-grid",
                Field { label: rsx! { "Model for new chats" }, hint: rsx! { "The only model a chat's conversation may use. Without one, chats are told no model is chosen." },
                    if models.is_empty() {
                        p { class: "a206-muted", "Connect a provider to discover models." }
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
            if !model_known { Alert { tone: AlertTone::Warning, title: rsx! { "Model not in the current catalog" }, "The chosen model is not listed by its provider right now, so new chats cannot start until it is listed again or you choose another." } }
            AllowList { messenger: config.name.clone(), title: "Allowed chats", hint: "Chat ids whose messages reach a conversation.", entries: config.allowed_chats.iter().cloned().collect::<Vec<_>>(), senders: false }
            AllowList { messenger: config.name.clone(), title: "Allowed senders", hint: "User ids accepted in allowed chats. Group chats accept only these senders; once any is listed, private chats do too.", entries: config.allowed_users.iter().cloned().collect::<Vec<_>>(), senders: true }
            if !status.refused.is_empty() {
                h3 { "Refused recently" }
                p { class: "a206-muted", "Titles come from the chat service and may be misleading. Allow only chats and people you recognize." }
                for refused in status.refused.iter().rev() {
                    RefusedRow { key: "{refused.chat}-{refused.sender:?}", messenger: config.name.clone(), refused: refused.clone(), chat_allowed: config.allowed_chats.contains(&refused.chat) }
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

#[component]
fn RefusedRow(messenger: String, refused: RefusedChat, chat_allowed: bool) -> Element {
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
        if let Some(sender) = refused.sender.clone() {
            if chat_allowed {
                Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, onclick: move |_| edit_list(state, &messenger, true, |list| { list.insert(sender.clone()); }), "Allow sender" }
            }
        } else if !chat_allowed {
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, onclick: { let chat = refused.chat.clone(); move |_| edit_list(state, &messenger, false, |list| { list.insert(chat.clone()); }) }, "Allow chat" }
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
    let ready = selected.is_some() && !name().trim().is_empty() && !credential().trim().is_empty();
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
                Field { label: rsx! { "Vault credential" }, hint: rsx! { "The name of the vault entry holding the bot token, never the token itself." }, Input { value: ReadSignal::from(credential), oninput: move |v| credential.set(v), placeholder: "Credential name" } }
                Field { label: rsx! { "API endpoint" }, hint: rsx! { "Leave empty for the service's official API. An origin only, with no path." }, Input { value: ReadSignal::from(endpoint), oninput: move |v| endpoint.set(v), placeholder: selected.map(|k| k.default_endpoint()).unwrap_or_default() } }
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
