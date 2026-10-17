//! The native `ask_user` tool: an agent asks the user a question and its run
//! waits for the answer.
//!
//! The question is published in the conversation ([`UserPrompt`]) with a
//! journaled [`EventKind::PromptRequested`]; only
//! [`Command::AnswerPrompt`](a206_core::Command::AnswerPrompt) from a client
//! answers it. The tool uses no capability and can approve nothing: like every
//! tool it is denied until the user permits it in a conversation, and an
//! answer is only text for the model. A question expires with its call, when
//! the run is cancelled, its permission is withdrawn or the gateway restarts.
use crate::{
    provider::now,
    store::{Store, StoreError},
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    AnswerError, ErrorKind, ErrorReport, EventKind, Id, PromptAnswer, PromptOutcome,
    ToolPermission, UserPrompt,
    prompts::{MAX_CHOICE, MAX_CHOICES, MAX_QUESTION},
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};
use tokio::sync::oneshot;

const NAME: &str = "ask_user";

/// Why a question could not be asked or answered.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("`question` must be text of 1 to {MAX_QUESTION} characters")]
    Question,
    #[error(
        "`choices` must be at most {MAX_CHOICES} distinct texts of 1 to {MAX_CHOICE} characters"
    )]
    Choices,
    #[error("`allow_other` must be true or false")]
    Other,
    #[error("a question without `choices` must allow an answer in the user's own words")]
    NothingToAnswer,
    #[error("unexpected argument `{0}`")]
    Unexpected(String),
    #[error("this question is no longer waiting for an answer")]
    NotPending,
    #[error("the answer does not fit the question")]
    Answer(#[source] AnswerError),
    #[error("could not record the question")]
    Store(#[source] StoreError),
    #[error("the conversation asking this question no longer exists")]
    Thread(#[source] StoreError),
}
impl crate::error::Classified for PromptError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Question
            | Self::Choices
            | Self::Other
            | Self::NothingToAnswer
            | Self::Unexpected(_)
            | Self::Answer(_) => ErrorKind::InvalidRequest,
            Self::NotPending => ErrorKind::NotFound,
            Self::Thread(_) => ErrorKind::NotFound,
            Self::Store(error) => return crate::error::classify(error),
        })
    }
}

struct Waiting {
    prompt: UserPrompt,
    thread: Id,
    answer: oneshot::Sender<PromptAnswer>,
}

/// Questions waiting for the user, shared by the tool host and the gateway's
/// `AnswerPrompt` handler.
pub(crate) struct Prompts {
    store: Arc<Mutex<Store>>,
    waiting: Mutex<BTreeMap<Id, Waiting>>,
}
impl Prompts {
    pub(crate) fn new(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            waiting: Mutex::default(),
        }
    }
    /// Deliver the user's answer to the run that asked. The answer is checked
    /// against the question first, and a question is answered at most once.
    pub(crate) fn answer(
        &self,
        thread: Id,
        prompt: Id,
        answer: PromptAnswer,
    ) -> Result<(), PromptError> {
        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        let pending = waiting
            .get(&prompt)
            .filter(|pending| pending.thread == thread)
            .ok_or(PromptError::NotPending)?;
        pending.prompt.check(&answer).map_err(PromptError::Answer)?;
        let pending = waiting.remove(&prompt).ok_or(PromptError::NotPending)?;
        // Fails only if the call ended this instant; it expires on its own.
        pending
            .answer
            .send(answer)
            .map_err(|_| PromptError::NotPending)
    }
    /// Register the waiter for `prompt`, then publish it in `thread`, so a
    /// client that sees the question can always answer it.
    fn ask(&self, thread: Id, prompt: UserPrompt) -> Result<Asked<'_>, PromptError> {
        let (sender, receiver) = oneshot::channel();
        let id = prompt.id;
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                Waiting {
                    prompt: prompt.clone(),
                    thread,
                    answer: sender,
                },
            );
        let published = {
            let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
            match store.thread_mut(thread) {
                Err(error) => Err(PromptError::Thread(error)),
                Ok(state) => {
                    state.prompts.push(prompt);
                    store
                        .persist(Some(EventKind::PromptRequested { thread, prompt: id }))
                        .map_err(|error| {
                            if let Ok(state) = store.thread_mut(thread) {
                                state.prompts.retain(|pending| pending.id != id);
                            }
                            PromptError::Store(error)
                        })
                }
            }
        };
        if let Err(error) = published {
            self.waiting
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            return Err(error);
        }
        Ok(Asked {
            prompts: self,
            thread,
            prompt: id,
            receiver: Some(receiver),
            outcome: PromptOutcome::Expired,
        })
    }
    /// Withdraw `prompt` from its thread, recording how it ended.
    fn withdraw(&self, thread: Id, prompt: Id, outcome: PromptOutcome) {
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&prompt);
        let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok(state) = store.thread_mut(thread) else {
            // The conversation was deleted; nothing is left to withdraw.
            return;
        };
        state.prompts.retain(|pending| pending.id != prompt);
        if let Err(error) = store.persist(Some(EventKind::PromptResolved {
            thread,
            prompt,
            outcome,
        })) {
            // Called from a drop as well, so the failure cannot propagate; a
            // restart clears stale questions.
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                %thread,
                %prompt,
                "could not withdraw a question"
            );
        }
    }
}

/// A published question. Dropping it (the call was cancelled or revoked)
/// withdraws the question as expired.
struct Asked<'a> {
    prompts: &'a Prompts,
    thread: Id,
    prompt: Id,
    receiver: Option<oneshot::Receiver<PromptAnswer>>,
    outcome: PromptOutcome,
}
impl Asked<'_> {
    async fn wait(mut self) -> Option<PromptAnswer> {
        let answer = self.receiver.take()?.await.ok()?;
        self.outcome = match answer {
            PromptAnswer::Decline => PromptOutcome::Declined,
            _ => PromptOutcome::Answered,
        };
        Some(answer)
    }
}
impl Drop for Asked<'_> {
    fn drop(&mut self) {
        self.prompts
            .withdraw(self.thread, self.prompt, self.outcome);
    }
}

/// Offers `ask_user` to native runs.
pub struct PromptHost {
    prompts: Arc<Prompts>,
}
impl PromptHost {
    pub(crate) fn new(prompts: Arc<Prompts>) -> Self {
        Self { prompts }
    }
    fn spec() -> ToolSpec {
        ToolSpec {
            whole_output: false,
            name: NAME.into(),
            description: "Ask the user a question and wait for the answer. Use it when you \
                cannot continue without a decision or a fact only the user has. Offer \
                `choices` when the likely answers are known; the user may also answer in \
                their own words unless `allow_other` is false. The user may decline to \
                answer."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "maxLength": MAX_QUESTION,
                        "description": "The question, as the user should read it."
                    },
                    "choices": {
                        "type": "array",
                        "maxItems": MAX_CHOICES,
                        "items": { "type": "string", "maxLength": MAX_CHOICE },
                        "description": "Suggested answers, most likely first."
                    },
                    "allow_other": {
                        "type": "boolean",
                        "description": "Whether the user may answer in their own words (default true)."
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
            capabilities: Default::default(),
            permission: ToolPermission::Allow,
        }
    }
}

/// The question an `ask_user` call describes.
fn parse(call: String, arguments: &Map<String, Value>) -> Result<UserPrompt, PromptError> {
    if let Some(name) = arguments
        .keys()
        .find(|name| !matches!(name.as_str(), "question" | "choices" | "allow_other"))
    {
        return Err(PromptError::Unexpected(name.clone()));
    }
    let bounded = |text: &str, max: usize| !text.trim().is_empty() && text.chars().count() <= max;
    let question = arguments
        .get("question")
        .and_then(Value::as_str)
        .filter(|question| bounded(question, MAX_QUESTION))
        .ok_or(PromptError::Question)?
        .to_owned();
    let choices = match arguments.get("choices") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(items)) if items.len() <= MAX_CHOICES => {
            let mut choices: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                let choice = item
                    .as_str()
                    .filter(|choice| bounded(choice, MAX_CHOICE))
                    .ok_or(PromptError::Choices)?;
                if choices.iter().any(|seen| seen == choice) {
                    return Err(PromptError::Choices);
                }
                choices.push(choice.to_owned());
            }
            choices
        }
        Some(_) => return Err(PromptError::Choices),
    };
    let other = match arguments.get("allow_other") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(other)) => *other,
        Some(_) => return Err(PromptError::Other),
    };
    if choices.is_empty() && !other {
        return Err(PromptError::NothingToAnswer);
    }
    Ok(UserPrompt {
        id: Id::new_v4(),
        call,
        question,
        choices,
        other,
        requested_at: now(),
    })
}

/// What the model receives for an answer.
fn reply(prompt: &UserPrompt, answer: Option<PromptAnswer>) -> ToolOutput {
    let value = match answer {
        Some(PromptAnswer::Choice { index }) => usize::try_from(index)
            .ok()
            .and_then(|index| prompt.choices.get(index))
            .map_or_else(
                || json!({ "declined": true }),
                |choice| json!({ "answer": choice }),
            ),
        Some(PromptAnswer::Text { text }) => json!({ "answer": text }),
        Some(PromptAnswer::Decline) => json!({ "declined": true }),
        // The waiter vanished without an answer: the run is ending.
        None => json!({ "declined": true, "expired": true }),
    };
    ToolOutput {
        text: value.to_string(),
        is_error: false,
    }
}

impl ToolHost for PromptHost {
    fn list<'a>(&'a self, _scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            ToolListing {
                tools: vec![Self::spec()],
                unavailable: vec![],
            }
        })
    }
    /// Cancel-safe: dropping the future withdraws the question.
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            let result = async {
                if request.name != NAME {
                    return Err(PromptError::Unexpected(request.name));
                }
                let prompt = parse(request.call, &request.arguments)?;
                let asked = self.prompts.ask(scope.thread, prompt.clone())?;
                Ok(reply(&prompt, asked.wait().await))
            };
            result.await.map_err(|error| crate::error::report(&error))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("not an object"),
        }
    }

    #[test]
    fn questions_are_bounded_and_answerable() {
        let call = Id::new_v4().to_string();
        let question = Id::new_v4().to_string();
        let prompt = parse(call.clone(), &args(json!({ "question": question }))).unwrap();
        assert_eq!(prompt.question, question);
        assert!(prompt.choices.is_empty() && prompt.other);
        assert_eq!(prompt.call, call);

        let choices: Vec<String> = (0..MAX_CHOICES).map(|_| Id::new_v4().to_string()).collect();
        let prompt = parse(
            call.clone(),
            &args(json!({ "question": question, "choices": choices, "allow_other": false })),
        )
        .unwrap();
        assert_eq!(prompt.choices, choices);
        assert!(!prompt.other);

        for (bad, expected) in [
            (json!({}), "Question"),
            (json!({ "question": " " }), "Question"),
            (
                json!({ "question": "x".repeat(MAX_QUESTION + 1) }),
                "Question",
            ),
            (json!({ "question": "q", "choices": "a" }), "Choices"),
            (json!({ "question": "q", "choices": ["a", "a"] }), "Choices"),
            (json!({ "question": "q", "choices": [""] }), "Choices"),
            (
                json!({ "question": "q", "choices": (0..=MAX_CHOICES).map(|i| i.to_string()).collect::<Vec<_>>() }),
                "Choices",
            ),
            (json!({ "question": "q", "allow_other": "yes" }), "Other"),
            (
                json!({ "question": "q", "allow_other": false }),
                "NothingToAnswer",
            ),
            (json!({ "question": "q", "extra": 1 }), "Unexpected"),
        ] {
            let error = parse(call.clone(), &args(bad.clone())).unwrap_err();
            assert!(
                format!("{error:?}").starts_with(expected),
                "{bad} gave {error:?}"
            );
            assert_eq!(crate::error::report(&error).kind, ErrorKind::InvalidRequest);
        }
    }

    #[test]
    fn the_model_receives_the_answer_text_or_a_declination() {
        let prompt = parse(
            Id::new_v4().to_string(),
            &args(json!({ "question": "q", "choices": ["first", "second"] })),
        )
        .unwrap();
        let text = |answer| reply(&prompt, answer).text;
        assert_eq!(
            text(Some(PromptAnswer::Choice { index: 1 })),
            json!({ "answer": "second" }).to_string()
        );
        let own = Id::new_v4().to_string();
        assert_eq!(
            text(Some(PromptAnswer::Text { text: own.clone() })),
            json!({ "answer": own }).to_string()
        );
        assert_eq!(
            text(Some(PromptAnswer::Decline)),
            json!({ "declined": true }).to_string()
        );
        assert_eq!(
            text(None),
            json!({ "declined": true, "expired": true }).to_string()
        );
    }
}
