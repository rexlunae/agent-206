//! The web search services the native `web_search` tool may query, on the
//! Connections page: added and removed through the same settings command as
//! `agent206 search-services`.
use crate::{State, dispatch};
use a206_core::{
    Command,
    web_search::{SearchApi, SearchConfig},
};
use fc_dioxus::prelude::*;

#[component]
pub fn SearchServices() -> Element {
    let state = use_context::<State>();
    let mut api = use_signal(|| None::<String>);
    let mut label = use_signal(String::new);
    let mut endpoint = use_signal(String::new);
    let mut credential = use_signal(String::new);
    let chosen = SearchApi::ALL
        .into_iter()
        .find(|a| Some(a.label()) == api.read().as_deref());
    let snapshot = state.data.read();
    let ready = chosen.is_some()
        && !label().trim().is_empty()
        && !endpoint().trim().is_empty()
        && !credential().trim().is_empty();
    rsx! {
        h2 { "Web search" }
        p { class: "a206-muted", "Services the web_search tool sends queries to. A conversation can use one once it has the network grant and permits the tool." }
        Card {
            CardHeader { CardTitle { "Add a search service" } CardSubtitle { "The key stays in the vault; the broker sends it only to the endpoint it is bound to." } }
            CardBody { div { class: "a206-grid",
                Field { label: rsx! { "Service" }, Select { label: name!("Search service"), value: ReadSignal::from(api), placeholder: "Choose a service",
                    options: SearchApi::ALL.into_iter().map(|a| SelectChoice::new(a.label(), a.label())).collect(),
                    onchange: move |value: String| {
                        if let Some(chosen) = SearchApi::ALL.into_iter().find(|a| a.label() == value) {
                            endpoint.set(chosen.default_base_url().to_owned());
                            if label().trim().is_empty() { label.set(chosen.label().to_owned()); }
                        }
                        api.set(Some(value));
                    }
                } }
                Field { label: rsx! { "Name" }, hint: rsx! { "The model picks a service by this name." }, Input { value: ReadSignal::from(label), oninput: move |v| label.set(v), placeholder: "A name you recognize" } }
                Field { label: rsx! { "API endpoint" }, hint: rsx! { "HTTPS or loopback HTTP." }, Input { r#type: InputType::Url, value: ReadSignal::from(endpoint), oninput: move |v| endpoint.set(v), placeholder: "The service's API endpoint" } }
                Field { label: rsx! { "Vault credential name" }, hint: rsx! { "A reference only. Never paste an API key here." }, Input { value: ReadSignal::from(credential), oninput: move |v| credential.set(v), placeholder: "Saved credential name" } }
            } }
            if let Some(chosen) = chosen { CardBody { p { class: "a206-muted", "{chosen.setup()}" } } }
            CardFooter { Button { variant: ButtonVariant::Primary, availability: if ready { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| {
                let Some(chosen) = SearchApi::ALL.into_iter().find(|a| Some(a.label()) == api.read().as_deref()) else { return };
                let mut settings = state.data.read().settings.clone();
                let mut service = SearchConfig::new(label().trim().to_owned(), chosen, credential().trim().to_owned());
                service.base_url = endpoint().trim().to_owned();
                settings.search_services.push(service);
                dispatch(state, Command::SaveSettings(Box::new(settings)));
            }, "Save search service" } }
        }
        for service in &snapshot.settings.search_services {
            { let id = service.id; rsx! { Card { key: "{id}",
                CardHeader { CardTitle { "{service.label}" } CardSubtitle { "{service.base_url}" } }
                CardBody { div { class: "a206-row", Badge { "{service.api.label()}" } Badge { "Vault credential: {service.credential}" } } }
                CardFooter { Button { variant: ButtonVariant::Ghost, onclick: move |_| {
                    let mut settings = state.data.read().settings.clone();
                    settings.search_services.retain(|s| s.id != id);
                    dispatch(state, Command::SaveSettings(Box::new(settings)));
                }, "Remove search service" } }
            } } }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::Snapshot;
    use dioxus::history::{History, MemoryHistory};
    use std::rc::Rc;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Routable)]
    enum Fixture {
        #[route("/")]
        Page {},
    }

    #[component]
    fn Page() -> Element {
        let snapshot = use_context::<Snapshot>();
        let state = crate::route::tests::loaded_state(snapshot);
        use_context_provider(|| state);
        rsx! { SearchServices {} }
    }

    #[test]
    fn configured_services_are_listed_with_their_endpoint_and_key_name() {
        fn app() -> Element {
            rsx! { Router::<Fixture> {} }
        }
        let word = || Uuid::new_v4().simple().to_string();
        let mut snapshot = crate::empty_snapshot();
        let service = SearchConfig::new(word(), SearchApi::Exa, word());
        snapshot.settings.search_services = vec![service.clone()];
        let mut dom = VirtualDom::new(app);
        dom.provide_root_context(snapshot);
        dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        for shown in [
            service.label.as_str(),
            service.base_url.as_str(),
            service.credential.as_str(),
            SearchApi::Exa.label(),
            "Remove search service",
        ] {
            assert!(html.contains(shown), "{shown} missing");
        }
    }
}
