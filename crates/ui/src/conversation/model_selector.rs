//! Choosing a model stays in chat; setup is an explicit choice in the list.
use super::*;
use a206_core::{ModelKey, Snapshot};

const MANAGE: &str = "manage-models";
const REFRESH: &str = "refresh-models:";

fn value(key: &ModelKey) -> String {
    format!("{}:{}", key.provider, key.model)
}

fn choices(snapshot: &Snapshot, selected: Option<&ModelKey>) -> Vec<SelectChoice> {
    let mut choices: Vec<_> = snapshot
        .catalogs
        .iter()
        .flat_map(|catalog| &catalog.models)
        .map(|model| {
            let provider = snapshot
                .settings
                .providers
                .iter()
                .find(|p| p.id == model.key.provider)
                .map(|p| p.label.as_str())
                .unwrap_or("Unavailable connection");
            SelectChoice::new(value(&model.key), &model.name).in_group(provider)
        })
        .collect();
    if let Some(selected) = selected
        && !choices.iter().any(|choice| choice.value == value(selected))
    {
        // Keep the thread's explicit selection visible without inventing API
        // facts or selecting another model while its catalog is unavailable.
        choices.insert(
            0,
            SelectChoice::new(value(selected), &selected.model)
                .describe("Catalog unavailable · refresh models to check availability")
                .disabled(),
        );
    }
    if choices.is_empty() {
        choices.push(SelectChoice::new("no-models", "No models loaded").disabled());
    }
    for provider in &snapshot.settings.providers {
        choices.push(
            SelectChoice::new(
                format!("{REFRESH}{}", provider.id),
                format!("Refresh {} models", provider.label),
            )
            .in_group("Setup"),
        );
    }
    choices.push(SelectChoice::new(MANAGE, "Manage models…").in_group("Setup"));
    choices
}

#[derive(Debug, PartialEq)]
enum Selection {
    Manage,
    Refresh(Uuid),
    Model(ModelKey),
}

fn selection(snapshot: &Snapshot, selected: &str) -> Option<Selection> {
    if selected == MANAGE {
        return Some(Selection::Manage);
    }
    if let Some(provider) = selected.strip_prefix(REFRESH) {
        let id = Uuid::parse_str(provider).ok()?;
        return snapshot
            .settings
            .providers
            .iter()
            .any(|provider| provider.id == id)
            .then_some(Selection::Refresh(id));
    }
    snapshot
        .catalogs
        .iter()
        .flat_map(|catalog| &catalog.models)
        .find(|model| value(&model.key) == selected)
        .map(|model| Selection::Model(model.key.clone()))
}

#[component]
pub(super) fn ModelSelector(thread: Thread, availability: Availability) -> Element {
    let state = use_context::<State>();
    let id = thread.id;
    let selected = use_memo(move || {
        state
            .data
            .read()
            .threads
            .iter()
            .find(|thread| thread.id == id)
            .and_then(|thread| thread.model.as_ref())
            .map(value)
    });
    let options = choices(&state.data.read(), thread.model.as_ref());
    rsx! {
        Select {
            class: "a206-compose-select a206-model-select", label: name!("Provider and model"),
            value: ReadSignal::from(selected), availability,
            options, placeholder: "Choose model",
            onchange: move |chosen: String| {
                // Read current state at activation, preserving concurrent mode
                // and subagent scope changes and refusing stale catalog choices.
                let action = selection(&state.data.peek(), &chosen);
                match action {
                    Some(Selection::Manage) => { state.navigator.push(Route::Providers {}); }
                    Some(Selection::Refresh(provider)) => dispatch(state, Command::RefreshCatalog { provider }),
                    Some(Selection::Model(model)) => {
                        let command = state.data.peek().threads.iter().find(|thread| thread.id == id)
                            .map(|thread| Command::ConfigureThread {
                                id, mode: thread.mode, model: Some(model),
                                subagent_models: thread.subagent_models.clone(),
                            });
                        if let Some(command) = command { dispatch(state, command); }
                    }
                    None => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{Catalog, ModelInfo, ProviderApi, ProviderConfig};

    fn fixture() -> (Snapshot, ModelKey) {
        let mut snapshot = crate::empty_snapshot();
        let provider = ProviderConfig {
            id: Uuid::new_v4(),
            label: Uuid::new_v4().to_string(),
            base_url: "https://example.test".into(),
            api: ProviderApi::default(),
            credential: None,
        };
        let key = ModelKey {
            provider: provider.id,
            model: Uuid::new_v4().to_string(),
        };
        snapshot.settings.providers.push(provider);
        snapshot.catalogs.push(Catalog {
            provider: key.provider,
            source: Uuid::new_v4().to_string(),
            fetched_at: 1,
            expires_at: u64::MAX,
            models: vec![ModelInfo {
                key: key.clone(),
                name: Uuid::new_v4().to_string(),
                context_limit: None,
                output_limit: None,
                input_cost_per_token: None,
                output_cost_per_token: None,
                image_input: None,
                reasoning: None,
                reasoning_efforts: vec![],
            }],
            balance: None,
            balance_currency: None,
            token_balance: None,
        });
        (snapshot, key)
    }

    #[test]
    fn catalog_models_and_setup_actions_are_distinct_choices() {
        let (snapshot, key) = fixture();
        let options = choices(&snapshot, Some(&key));
        assert!(
            options
                .iter()
                .any(|option| option.value == value(&key) && !option.disabled)
        );
        assert_eq!(
            selection(&snapshot, &value(&key)),
            Some(Selection::Model(key.clone()))
        );
        assert_eq!(selection(&snapshot, MANAGE), Some(Selection::Manage));
        assert_eq!(
            selection(&snapshot, &format!("{REFRESH}{}", key.provider)),
            Some(Selection::Refresh(key.provider))
        );
        assert_eq!(selection(&snapshot, "no-models"), None);
        assert_eq!(selection(&snapshot, &Uuid::new_v4().to_string()), None);
    }

    #[test]
    fn missing_catalog_keeps_selection_visible_and_setup_reachable() {
        let (mut snapshot, key) = fixture();
        snapshot.catalogs.clear();
        let options = choices(&snapshot, Some(&key));
        assert!(
            options
                .iter()
                .any(|option| option.value == value(&key) && option.disabled)
        );
        assert!(
            options
                .iter()
                .any(|option| option.value == MANAGE && !option.disabled)
        );
        assert_eq!(selection(&snapshot, &value(&key)), None);
        let empty = choices(&crate::empty_snapshot(), None);
        assert!(
            empty
                .iter()
                .any(|option| option.value == MANAGE && !option.disabled)
        );
    }
}
