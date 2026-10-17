//! Questions an agent asked with `ask_user`, answered in the conversation.
//!
//! An answer is only text for the model: this card approves nothing, and the
//! gateway checks the answer against the question again before the run sees it.
use crate::error::ErrorNotice;
use a206_core::{Command, ErrorKind, ErrorReport, PromptAnswer, UserPrompt as Question};
use fc_dioxus::prelude::*;
use uuid::Uuid;

/// A question waiting for the user, with its suggested answers, a field for
/// the user's own words when the agent allows them, and Decline.
#[component]
pub(crate) fn PendingPrompt(thread: Uuid, question: Question) -> Element {
    let mut sending = use_signal(|| false);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut text = use_signal(String::new);
    let prompt = question.id;
    let checked = question.clone();
    // A `Callback` is `Copy`, so every button can hold one.
    let answer = use_callback(move |answer: PromptAnswer| {
        if *sending.peek() {
            return;
        }
        if let Err(error) = checked.check(&answer) {
            failure.set(Some(ErrorReport::new(
                ErrorKind::InvalidRequest,
                error.to_string(),
            )));
            return;
        }
        sending.set(true);
        failure.set(None);
        spawn(async move {
            let command = Command::AnswerPrompt {
                thread,
                prompt,
                answer,
            };
            // A question is answered once, so after success the card stays
            // disabled until the next snapshot removes it; on failure the
            // user can try again.
            if let Err(report) = crate::call(command).await {
                failure.set(Some(report));
                sending.set(false);
            }
        });
    });
    let send_text = move || answer(PromptAnswer::Text { text: text() });
    let availability = if sending() {
        Availability::Disabled
    } else {
        Availability::Enabled
    };
    rsx! {
        UserPrompt { label: name!("Question from the agent"), region: true,
            record: Record { state: Some(Lifecycle::Pending), ..Record::default() },
            title: rsx! { "{question.question}" },
            meta: rsx! { "Your answer goes to the agent as text. It does not approve anything." },
            actions: rsx! {
                if question.other {
                    Button { variant: ButtonVariant::Primary, availability: availability.clone(), busy: sending(),
                        onclick: move |_| send_text(), "Send answer" }
                }
                Button { variant: ButtonVariant::Ghost, availability: availability.clone(),
                    onclick: move |_| answer(PromptAnswer::Decline), "Decline" }
            },
            if !question.choices.is_empty() {
                div { class: "a206-prompt-choices", role: "group", aria_label: "Suggested answers",
                    for (index, choice) in question.choices.iter().enumerate() {
                        Button { key: "{index}", variant: ButtonVariant::Secondary, availability: availability.clone(),
                            onclick: move |_| answer(PromptAnswer::Choice { index: index as u32 }), "{choice}" }
                    }
                }
            }
            if question.other {
                Field { label: rsx! { if question.choices.is_empty() { "Your answer" } else { "Or answer in your own words" } },
                    div { onkeydown: move |event: KeyboardEvent| {
                        if event.key() == Key::Enter && !event.is_composing() && !event.modifiers().shift() {
                            event.prevent_default();
                            send_text();
                        }
                    },
                    Textarea { autosize: true, rows: "1", value: ReadSignal::from(text), oninput: move |value| text.set(value) }
                    }
                }
            }
            if let Some(report) = failure() {
                ErrorNotice { report, title: "Could not send the answer".to_string() }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(question: Question) -> String {
        #[derive(Props, Clone, PartialEq)]
        struct Shown {
            question: Question,
        }
        fn root(props: Shown) -> Element {
            rsx! { PendingPrompt { thread: Uuid::nil(), question: props.question } }
        }
        let mut dom = VirtualDom::new_with_props(root, Shown { question });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }
    fn question(choices: usize, other: bool) -> Question {
        Question {
            id: Uuid::new_v4(),
            call: Uuid::new_v4().to_string(),
            question: Uuid::new_v4().simple().to_string(),
            choices: (0..choices)
                .map(|_| Uuid::new_v4().simple().to_string())
                .collect(),
            other,
            requested_at: 0,
        }
    }
    #[test]
    fn the_card_offers_exactly_the_answers_the_question_accepts() {
        let closed = question(2, false);
        let html = render(closed.clone());
        assert!(html.contains(&closed.question));
        assert!(closed.choices.iter().all(|choice| html.contains(choice)));
        assert!(html.contains("Decline"));
        assert!(!html.contains("<textarea") && !html.contains("Send answer"));
        // Nothing identifying the question or its call reaches the DOM.
        assert!(!html.contains(&closed.id.to_string()) && !html.contains(&closed.call));

        let open = question(0, true);
        let html = render(open.clone());
        assert!(html.contains("<textarea") && html.contains("Send answer"));
        assert!(html.contains("Your answer") && !html.contains("Suggested answers"));
    }
}
