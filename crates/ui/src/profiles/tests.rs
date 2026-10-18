use super::*;

fn key() -> ModelKey {
    ModelKey {
        provider: Uuid::new_v4(),
        model: format!("model-{}", Uuid::new_v4()),
    }
}
fn profile(mode: Option<Mode>, model: Option<&ModelKey>) -> AgentProfile {
    AgentProfile {
        id: "p".into(),
        name: "P".into(),
        description: None,
        mode,
        model: model.map(|k| k.model.clone()),
        connection: None,
        capabilities: None,
        instructions: String::new(),
        digest: String::new(),
    }
}

#[test]
fn a_delegated_conversation_takes_the_profiles_mode_and_only_a_permitted_model_it_names() {
    let first = key();
    let named = key();
    let permitted = BTreeSet::from([first.clone(), named.clone()]);
    let expected_first = permitted.iter().next().unwrap().clone();
    // Without a profile: Plan and the first permitted model, as before.
    assert_eq!(
        delegation(None, &permitted, &[]),
        Some((Mode::Plan, expected_first.clone()))
    );
    assert_eq!(
        delegation(Some(&profile(Some(Mode::Review), None)), &permitted, &[]),
        Some((Mode::Review, expected_first))
    );
    assert_eq!(
        delegation(Some(&profile(None, Some(&named))), &permitted, &[]),
        Some((Mode::Plan, named.clone()))
    );
    // A profile's model the conversation may not delegate to is never used.
    let other = key();
    assert_eq!(
        delegation(Some(&profile(None, Some(&other))), &permitted, &[]),
        None
    );
    assert_eq!(delegation(None, &BTreeSet::new(), &[]), None);
}
