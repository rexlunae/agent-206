//! Slash commands typed in a conversation's composer. Each one is an action
//! the conversation's own controls or the command palette already offer.
use crate::{
    State, dispatch,
    palette::{self, Action},
};
use a206_core::{Command, ErrorKind, ErrorReport, Mode, ModelInfo, RunState, Snapshot, Thread};

/// One command the composer understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spec {
    pub(crate) name: &'static str,
    /// What follows the name, for a command that takes text.
    pub(crate) argument: Option<&'static str>,
    pub(crate) about: &'static str,
}

impl Spec {
    /// What completing to this command puts in the composer.
    pub(crate) fn completion(&self) -> String {
        match self.argument {
            Some(_) => format!("/{} ", self.name),
            None => format!("/{}", self.name),
        }
    }
    pub(crate) fn usage(&self) -> String {
        match self.argument {
            Some(argument) => format!("/{} {argument}", self.name),
            None => format!("/{}", self.name),
        }
    }
}

pub(crate) const COMMANDS: [Spec; 7] = [
    Spec {
        name: "new",
        argument: None,
        about: "Start a new conversation",
    },
    Spec {
        name: "rename",
        argument: Some("title"),
        about: "Rename this conversation",
    },
    Spec {
        name: "compact",
        argument: None,
        about: "Summarize all but the latest turn",
    },
    Spec {
        name: "stop",
        argument: None,
        about: "Stop the running response",
    },
    Spec {
        name: "mode",
        argument: Some("mode"),
        about: "Switch this conversation's agent mode",
    },
    Spec {
        name: "model",
        argument: Some("model"),
        about: "Choose a connected model by name",
    },
    Spec {
        name: "effort",
        argument: Some("level"),
        about: "Set reasoning effort, or auto",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Slash {
    New,
    Rename(String),
    Compact,
    Stop,
    Mode(String),
    Model(String),
    Effort(String),
}

/// What Enter does with a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Typed {
    /// A command to run instead of sending.
    Run(Slash),
    /// Send this text as a message. Text that only looks like a command, such
    /// as a path, is sent as typed; a leading `//` sends one `/`.
    Send(String),
}

/// Reads a trimmed draft. A known command with a missing or unexpected
/// argument is refused rather than sent as a message.
pub(crate) fn parse(draft: &str) -> Result<Typed, ErrorReport> {
    if let Some(rest) = draft.strip_prefix("//") {
        return Ok(Typed::Send(format!("/{rest}")));
    }
    let Some(rest) = draft.strip_prefix('/') else {
        return Ok(Typed::Send(draft.to_owned()));
    };
    let (name, argument) = match rest.split_once(char::is_whitespace) {
        Some((name, argument)) => (name, argument.trim()),
        None => (rest, ""),
    };
    let Some(spec) = COMMANDS.iter().find(|spec| spec.name == name) else {
        return Ok(Typed::Send(draft.to_owned()));
    };
    match (spec.argument, argument.is_empty()) {
        (Some(wanted), true) => {
            return Err(refused(&format!("Give the {wanted} after /{}", spec.name)));
        }
        (None, false) => {
            return Err(refused(&format!(
                "/{} takes no text; send it on its own",
                spec.name
            )));
        }
        _ => {}
    }
    let argument = argument.to_owned();
    let slash = match spec.name {
        "new" => Slash::New,
        "rename" => Slash::Rename(argument),
        "compact" => Slash::Compact,
        "stop" => Slash::Stop,
        "mode" => Slash::Mode(argument),
        "model" => Slash::Model(argument),
        "effort" => Slash::Effort(argument),
        _ => return Ok(Typed::Send(draft.to_owned())),
    };
    Ok(Typed::Run(slash))
}

/// The commands a draft could be completing: while it is a `/` and the start
/// of a command name, with nothing after it yet.
pub(crate) fn suggestions(draft: &str) -> Vec<Spec> {
    let Some(prefix) = draft.strip_prefix('/') else {
        return vec![];
    };
    if prefix.starts_with('/') || prefix.chars().any(char::is_whitespace) {
        return vec![];
    }
    COMMANDS
        .into_iter()
        .filter(|spec| spec.name.starts_with(prefix))
        .collect()
}

fn refused(message: &str) -> ErrorReport {
    ErrorReport::new(ErrorKind::InvalidRequest, message)
}

/// The models a conversation can choose: those the composer's model
/// selector lists.
fn models(snapshot: &Snapshot) -> impl Iterator<Item = &ModelInfo> {
    snapshot.catalogs.iter().flat_map(|c| &c.models)
}

/// The one model `query` names: an exact name or identifier, else the only
/// one containing it, ignoring case.
fn find_model<'a>(snapshot: &'a Snapshot, query: &str) -> Result<&'a ModelInfo, ErrorReport> {
    let query = query.to_lowercase();
    let names = |m: &ModelInfo| [m.name.to_lowercase(), m.key.model.to_lowercase()];
    if let Some(exact) = models(snapshot).find(|m| names(m).contains(&query)) {
        return Ok(exact);
    }
    let found: Vec<_> = models(snapshot)
        .filter(|m| names(m).iter().any(|name| name.contains(&query)))
        .collect();
    match found.as_slice() {
        [one] => Ok(one),
        [] => Err(refused("No connected model matches that name")),
        many => Err(refused(&format!(
            "{} models match; name one of {}",
            many.len(),
            many.iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// What running `slash` in `thread` does, or why it cannot run now.
/// `can_create` is whether the gateway can take a new conversation.
pub(crate) fn resolve(
    slash: Slash,
    thread: &Thread,
    snapshot: &Snapshot,
    can_create: bool,
) -> Result<Command, ErrorReport> {
    let running = thread.state == RunState::Running;
    // The mode, model and effort selectors are disabled while a response runs.
    let settled = || {
        if running {
            Err(refused("Wait for the running response, or /stop it, first"))
        } else {
            Ok(())
        }
    };
    match slash {
        Slash::New if can_create => Ok(Command::CreateThread {
            title: "New conversation".into(),
            mode: Mode::Chat,
        }),
        Slash::New => Err(refused("The gateway cannot start a conversation right now")),
        Slash::Rename(title) => Ok(Command::RenameThread {
            id: thread.id,
            title,
        }),
        Slash::Compact if palette::can_compact(thread) => {
            Ok(Command::CompactThread { thread: thread.id })
        }
        Slash::Compact => Err(refused(
            "Only a native conversation with a model and no running response can be summarized",
        )),
        Slash::Stop if running => Ok(Command::CancelRun { thread: thread.id }),
        Slash::Stop => Err(refused("Nothing is running in this conversation")),
        Slash::Mode(name) => {
            settled()?;
            let mode = Mode::ALL
                .into_iter()
                .find(|m| m.label().eq_ignore_ascii_case(&name))
                .ok_or_else(|| {
                    refused(&format!(
                        "Choose one of {}",
                        Mode::ALL.map(|m| m.label()).join(", ")
                    ))
                })?;
            Ok(Command::ConfigureThread {
                id: thread.id,
                mode,
                model: thread.model.clone(),
                subagent_models: thread.subagent_models.clone(),
            })
        }
        Slash::Model(query) => {
            settled()?;
            let model = find_model(snapshot, &query)?;
            Ok(Command::ConfigureThread {
                id: thread.id,
                mode: thread.mode,
                model: Some(model.key.clone()),
                subagent_models: thread.subagent_models.clone(),
            })
        }
        Slash::Effort(level) => {
            settled()?;
            // Effort choices come only from the selected model's API listing.
            let efforts = thread
                .model
                .as_ref()
                .and_then(|key| models(snapshot).find(|m| &m.key == key))
                .map(|m| m.reasoning_efforts.as_slice())
                .unwrap_or_default();
            if efforts.is_empty() {
                return Err(refused(
                    "The selected model's API reports no reasoning effort choices",
                ));
            }
            let effort = if level.eq_ignore_ascii_case("auto") {
                None
            } else {
                Some(
                    efforts
                        .iter()
                        .find(|e| e.eq_ignore_ascii_case(&level))
                        .cloned()
                        .ok_or_else(|| {
                            refused(&format!("Choose auto or one of {}", efforts.join(", ")))
                        })?,
                )
            };
            Ok(Command::SetEffort {
                thread: thread.id,
                effort,
            })
        }
    }
}

/// Runs a resolved command the way the palette and the page's buttons do.
pub(crate) fn run(state: State, command: Command) {
    match command {
        Command::CreateThread { .. } => palette::run(state, Action::NewConversation),
        command => dispatch(state, command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{Catalog, CodingBackend, Mode, ModelKey};
    use uuid::Uuid;

    fn text() -> String {
        format!("t-{}", Uuid::new_v4().simple())
    }

    #[test]
    fn known_commands_run_and_anything_else_is_sent_as_typed() {
        let title = text();
        assert_eq!(parse("/new"), Ok(Typed::Run(Slash::New)));
        assert_eq!(parse("/stop"), Ok(Typed::Run(Slash::Stop)));
        assert_eq!(parse("/compact"), Ok(Typed::Run(Slash::Compact)));
        assert_eq!(
            parse(&format!("/rename   {title} ")),
            Ok(Typed::Run(Slash::Rename(title.clone())))
        );
        let path = format!("/{}/{}", text(), text());
        assert_eq!(parse(&path), Ok(Typed::Send(path.clone())));
        assert_eq!(parse(&title), Ok(Typed::Send(title.clone())));
        // Commands match whole names only.
        assert_eq!(parse("/news"), Ok(Typed::Send("/news".into())));
        // A doubled slash sends the command's text itself.
        assert_eq!(parse("//new"), Ok(Typed::Send("/new".into())));
    }

    #[test]
    fn a_known_command_with_the_wrong_text_is_refused_not_sent() {
        let rename = parse("/rename").expect_err("no title");
        assert_eq!(rename.kind, ErrorKind::InvalidRequest);
        assert!(parse(&format!("/stop {}", text())).is_err());
        assert!(parse(&format!("/new {}", text())).is_err());
        for name in ["mode", "model", "effort"] {
            assert!(parse(&format!("/{name}")).is_err());
        }
        let level = text();
        assert_eq!(
            parse(&format!("/effort {level}")),
            Ok(Typed::Run(Slash::Effort(level)))
        );
    }

    #[test]
    fn suggestions_follow_the_name_being_typed() {
        let names =
            |draft: &str| -> Vec<&str> { suggestions(draft).into_iter().map(|s| s.name).collect() };
        assert_eq!(
            names("/"),
            [
                "new", "rename", "compact", "stop", "mode", "model", "effort"
            ]
        );
        assert_eq!(names("/mod"), ["mode", "model"]);
        assert_eq!(names("/s"), ["stop"]);
        assert!(names("/x").is_empty());
        // Once the name is finished, or for ordinary text, there is nothing to complete.
        assert!(names("/rename ").is_empty());
        assert!(names("//").is_empty());
        assert!(names(&text()).is_empty());
        assert_eq!(COMMANDS[1].completion(), "/rename ");
        assert_eq!(COMMANDS[0].completion(), "/new");
        assert_eq!(COMMANDS[1].usage(), "/rename title");
    }

    #[test]
    fn a_command_runs_only_when_the_conversation_allows_it() {
        let snapshot = crate::empty_snapshot();
        let mut thread = Thread::new(text(), Mode::Chat);
        assert!(resolve(Slash::Stop, &thread, &snapshot, true).is_err());
        assert!(resolve(Slash::Compact, &thread, &snapshot, true).is_err());
        assert!(resolve(Slash::New, &thread, &snapshot, false).is_err());
        assert!(matches!(
            resolve(Slash::New, &thread, &snapshot, true),
            Ok(Command::CreateThread { .. })
        ));
        let title = text();
        assert_eq!(
            resolve(Slash::Rename(title.clone()), &thread, &snapshot, true),
            Ok(Command::RenameThread {
                id: thread.id,
                title
            })
        );
        thread.model = Some(ModelKey {
            provider: Uuid::new_v4(),
            model: text(),
        });
        assert_eq!(
            resolve(Slash::Compact, &thread, &snapshot, true),
            Ok(Command::CompactThread { thread: thread.id })
        );
        thread.state = RunState::Running;
        assert_eq!(
            resolve(Slash::Stop, &thread, &snapshot, true),
            Ok(Command::CancelRun { thread: thread.id })
        );
        assert!(resolve(Slash::Compact, &thread, &snapshot, true).is_err());
        thread.state = RunState::Idle;
        thread.backend = CodingBackend::Codex;
        assert!(resolve(Slash::Compact, &thread, &snapshot, true).is_err());
    }

    fn model(efforts: Vec<String>) -> ModelInfo {
        ModelInfo {
            key: ModelKey {
                provider: Uuid::new_v4(),
                model: text(),
            },
            name: text(),
            context_limit: None,
            output_limit: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            reasoning: None,
            reasoning_efforts: efforts,
        }
    }

    fn listing(models: Vec<ModelInfo>) -> Snapshot {
        let mut snapshot = crate::empty_snapshot();
        snapshot.catalogs.push(Catalog {
            provider: Uuid::new_v4(),
            source: text(),
            fetched_at: 0,
            expires_at: 0,
            models,
            balance: None,
            balance_currency: None,
            token_balance: None,
        });
        snapshot
    }

    fn configured(command: Result<Command, ErrorReport>) -> (Mode, Option<ModelKey>) {
        match command {
            Ok(Command::ConfigureThread { mode, model, .. }) => (mode, model),
            other => panic!("expected a configuration, got {other:?}"),
        }
    }

    #[test]
    fn mode_is_chosen_by_label_and_keeps_the_model() {
        let snapshot = crate::empty_snapshot();
        let mut thread = Thread::new(text(), Mode::Chat);
        thread.model = Some(model(vec![]).key);
        for mode in Mode::ALL {
            let typed = mode.label().to_uppercase();
            assert_eq!(
                configured(resolve(Slash::Mode(typed), &thread, &snapshot, true)),
                (mode, thread.model.clone())
            );
        }
        assert!(resolve(Slash::Mode(text()), &thread, &snapshot, true).is_err());
        thread.state = RunState::Running;
        let label = Mode::ALL[0].label().to_owned();
        assert!(resolve(Slash::Mode(label), &thread, &snapshot, true).is_err());
    }

    #[test]
    fn model_is_found_by_name_identifier_or_a_unique_part() {
        let first = model(vec![]);
        let second = model(vec![]);
        let snapshot = listing(vec![first.clone(), second.clone()]);
        let thread = Thread::new(text(), Mode::Chat);
        let chosen = |query: &str| {
            configured(resolve(
                Slash::Model(query.to_owned()),
                &thread,
                &snapshot,
                true,
            ))
            .1
        };
        assert_eq!(chosen(&first.name.to_uppercase()), Some(first.key.clone()));
        assert_eq!(chosen(&second.key.model), Some(second.key.clone()));
        // Generated names share the "t-" prefix, so it names both; a tail names one.
        let tail = &first.name[first.name.len() - 12..];
        assert_eq!(chosen(tail), Some(first.key.clone()));
        assert!(resolve(Slash::Model("t-".into()), &thread, &snapshot, true).is_err());
        assert!(resolve(Slash::Model(text()), &thread, &snapshot, true).is_err());
        // The mode is kept.
        assert_eq!(
            configured(resolve(
                Slash::Model(first.name.clone()),
                &thread,
                &snapshot,
                true
            ))
            .0,
            thread.mode
        );
    }

    #[test]
    fn effort_comes_only_from_the_selected_model_listing() {
        let level = text();
        let listed = model(vec![level.clone(), text()]);
        let plain = model(vec![]);
        let snapshot = listing(vec![listed.clone(), plain.clone()]);
        let mut thread = Thread::new(text(), Mode::Chat);
        let effort = |thread: &Thread, typed: &str| {
            resolve(Slash::Effort(typed.to_owned()), thread, &snapshot, true)
        };
        // No model selected, or one whose listing has no effort choices.
        assert!(effort(&thread, &level).is_err());
        thread.model = Some(plain.key);
        assert!(effort(&thread, "auto").is_err());

        thread.model = Some(listed.key);
        assert_eq!(
            effort(&thread, &level.to_uppercase()),
            Ok(Command::SetEffort {
                thread: thread.id,
                effort: Some(level.clone())
            })
        );
        assert_eq!(
            effort(&thread, "Auto"),
            Ok(Command::SetEffort {
                thread: thread.id,
                effort: None
            })
        );
        assert!(effort(&thread, &text()).is_err());
        thread.state = RunState::Running;
        assert!(effort(&thread, &level).is_err());
    }
}
