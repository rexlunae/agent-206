//! Slash commands typed in a conversation's composer. Each one is an action
//! the conversation's own controls or the command palette already offer.
use crate::{
    State, dispatch,
    palette::{self, Action},
};
use a206_core::{Command, ErrorKind, ErrorReport, RunState, Thread};

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

pub(crate) const COMMANDS: [Spec; 4] = [
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
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Slash {
    New,
    Rename(String),
    Compact,
    Stop,
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
    let slash = match (spec.name, argument.is_empty()) {
        ("rename", false) => Slash::Rename(argument.to_owned()),
        ("rename", true) => return Err(refused("Give the new title after /rename")),
        (_, false) => {
            return Err(refused(&format!(
                "/{} takes no text; send it on its own",
                spec.name
            )));
        }
        ("new", true) => Slash::New,
        ("compact", true) => Slash::Compact,
        ("stop", true) => Slash::Stop,
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

/// What running `slash` in `thread` does, or why it cannot run now.
/// `can_create` is whether the gateway can take a new conversation.
pub(crate) fn resolve(
    slash: Slash,
    thread: &Thread,
    can_create: bool,
) -> Result<Command, ErrorReport> {
    match slash {
        Slash::New if can_create => Ok(Command::CreateThread {
            title: "New conversation".into(),
            mode: a206_core::Mode::Chat,
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
        Slash::Stop if thread.state == RunState::Running => {
            Ok(Command::CancelRun { thread: thread.id })
        }
        Slash::Stop => Err(refused("Nothing is running in this conversation")),
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
    use a206_core::{CodingBackend, Mode, ModelKey};
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
    }

    #[test]
    fn suggestions_follow_the_name_being_typed() {
        let names =
            |draft: &str| -> Vec<&str> { suggestions(draft).into_iter().map(|s| s.name).collect() };
        assert_eq!(names("/"), ["new", "rename", "compact", "stop"]);
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
        let mut thread = Thread::new(text(), Mode::Chat);
        assert!(resolve(Slash::Stop, &thread, true).is_err());
        assert!(resolve(Slash::Compact, &thread, true).is_err());
        assert!(resolve(Slash::New, &thread, false).is_err());
        assert!(matches!(
            resolve(Slash::New, &thread, true),
            Ok(Command::CreateThread { .. })
        ));
        let title = text();
        assert_eq!(
            resolve(Slash::Rename(title.clone()), &thread, true),
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
            resolve(Slash::Compact, &thread, true),
            Ok(Command::CompactThread { thread: thread.id })
        );
        thread.state = RunState::Running;
        assert_eq!(
            resolve(Slash::Stop, &thread, true),
            Ok(Command::CancelRun { thread: thread.id })
        );
        assert!(resolve(Slash::Compact, &thread, true).is_err());
        thread.state = RunState::Idle;
        thread.backend = CodingBackend::Codex;
        assert!(resolve(Slash::Compact, &thread, true).is_err());
    }
}
