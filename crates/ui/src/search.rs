//! Searching message text across every conversation, through the gateway.
use crate::{error::ErrorNotice, route::Route};
use a206_core::{Command, ErrorKind, ErrorReport, MessageHit, Reply, Role, search::MAX_HITS};
use fc_dioxus::prelude::*;

fn role(role: &Role) -> &'static str {
    match role {
        Role::User => "You",
        Role::Assistant => "Agent",
        Role::System => "System",
        Role::Tool => "Tool",
    }
}

#[derive(Clone, PartialEq)]
enum Found {
    Idle,
    Searching,
    Hits {
        query: String,
        hits: Vec<MessageHit>,
    },
    Failed(ErrorReport),
}

#[component]
pub(crate) fn MessageSearch() -> Element {
    let mut query = use_signal(String::new);
    let mut found = use_signal(|| Found::Idle);
    let mut run = move || {
        let text = query.peek().trim().to_owned();
        if text.is_empty() || *found.peek() == Found::Searching {
            return;
        }
        found.set(Found::Searching);
        spawn(async move {
            let command = Command::SearchMessages {
                query: text.clone(),
                limit: MAX_HITS,
            };
            found.set(match crate::call(command).await {
                Ok(Reply::Messages(hits)) => Found::Hits { query: text, hits },
                Ok(_) => Found::Failed(ErrorReport::new(
                    ErrorKind::Protocol,
                    "Unexpected search response",
                )),
                Err(report) => Found::Failed(report),
            });
        });
    };
    let searching = found() == Found::Searching;
    rsx! {
        div { class: "a206-row", onkeydown: move |event: KeyboardEvent| {
                if event.key() == Key::Enter && !event.is_composing() {
                    event.prevent_default();
                    run();
                }
            },
            Field { label: rsx! { "Search message text" }, hint: rsx! { "Finds messages containing every word, in any conversation." },
                Input { value: ReadSignal::from(query), oninput: move |value| query.set(value), placeholder: "Words to find" }
            }
            Button { variant: ButtonVariant::Secondary, busy: searching,
                availability: if query().trim().is_empty() { Availability::Disabled } else { Availability::Enabled },
                onclick: move |_| run(), "Search" }
        }
        match found() {
            Found::Idle | Found::Searching => rsx! {},
            Found::Failed(report) => rsx! { ErrorNotice { report, title: "Could not search messages".to_string() } },
            Found::Hits { query, hits } => rsx! { SearchResults { query, hits } },
        }
    }
}

/// The hits of one search, each opening its conversation.
#[component]
fn SearchResults(query: String, hits: Vec<MessageHit>) -> Element {
    if hits.is_empty() {
        return rsx! { p { class: "a206-muted", role: "status", "No messages contain “{query}”." } };
    }
    let count = hits.len();
    rsx! {
        p { class: "a206-muted", role: "status",
            if u32::try_from(count).is_ok_and(|count| count >= MAX_HITS) { "Showing the newest {count} matching messages." } else if count == 1 { "1 matching message." } else { "{count} matching messages." }
        }
        div { class: "a206-nav-group", aria_label: "Matching messages",
            for hit in hits {
                LinkButton { key: "{hit.message}", class: "a206-recent-thread", size: ButtonSize::Sm, variant: ButtonVariant::Ghost,
                    href: Route::Chat { id: hit.thread, split: None, message: Some(hit.message) }.to_string(),
                    Icon { name: IconName::Search, size: IconSize::Sm }
                    span { class: "a206-nav-title",
                        strong { "{hit.title}" }
                        span { class: "a206-muted", "{role(&hit.role)}: {hit.before}" mark { "{hit.matched}" } "{hit.after}" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn render(query: String, hits: Vec<MessageHit>) -> String {
        #[derive(Props, Clone, PartialEq)]
        struct Shown {
            query: String,
            hits: Vec<MessageHit>,
        }
        fn root(props: Shown) -> Element {
            rsx! { SearchResults { query: props.query, hits: props.hits } }
        }
        let mut dom = VirtualDom::new_with_props(root, Shown { query, hits });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }
    fn word() -> String {
        Uuid::new_v4().simple().to_string()
    }

    #[test]
    fn hits_link_to_their_conversation_and_mark_the_match() {
        let hit = MessageHit {
            thread: Uuid::new_v4(),
            title: word(),
            message: Uuid::new_v4(),
            role: Role::Assistant,
            at: 0,
            before: word(),
            matched: word(),
            after: word(),
        };
        let html = render(word(), vec![hit.clone()]);
        assert!(
            html.contains(
                &Route::Chat {
                    id: hit.thread,
                    split: None,
                    message: Some(hit.message),
                }
                .to_string()
            )
        );
        assert!(
            html.contains(&hit.title) && html.contains(&hit.before) && html.contains(&hit.after)
        );
        assert!(html.contains(&format!("<mark>{}</mark>", hit.matched)));
        assert!(html.contains("1 matching message."));

        let query = word();
        assert!(render(query.clone(), vec![]).contains(&format!("No messages contain “{query}”")));
    }
}
