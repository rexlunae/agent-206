use super::*;
use crate::{Capability, Mode, ModelKey};
use uuid::Uuid;

fn profile(id: &str, model: Option<&ModelKey>) -> AgentProfile {
    AgentProfile {
        id: id.into(),
        name: id.into(),
        description: None,
        mode: Some(Mode::Research),
        model: model.map(|k| k.model.clone()),
        connection: None,
        capabilities: None,
        instructions: String::new(),
        digest: String::new(),
    }
}
fn team() -> AgentTeam {
    AgentTeam {
        id: format!("t{}", Uuid::new_v4().simple()),
        name: Uuid::new_v4().to_string(),
        description: None,
        coordinator: "lead".into(),
        members: vec!["member".into()],
        instructions: String::new(),
        digest: String::new(),
    }
}
fn model() -> ModelKey {
    ModelKey {
        provider: Uuid::new_v4(),
        model: Uuid::new_v4().to_string(),
    }
}

#[test]
fn a_team_starts_with_its_coordinators_mode_and_a_permitted_model_only() {
    let model = model();
    let mut parent = Thread::new(Uuid::new_v4().to_string(), Mode::Code);
    parent.subagent_models = [model.clone()].into();
    parent.grant.capabilities = [Capability::ReadWorkspace].into();
    let team = team();
    let profiles = AgentProfiles {
        profiles: vec![profile("lead", Some(&model))],
        problems: vec![],
    };
    let Some(Command::StartTeam {
        parent: id,
        team: started,
        mode,
        model: chosen,
        requested,
        ..
    }) = start(&team, &parent, &profiles, &[])
    else {
        panic!("the team could not start");
    };
    assert_eq!((id, started), (parent.id, team.id.clone()));
    assert_eq!((mode, chosen), (Mode::Research, model));
    assert_eq!(requested, parent.grant.capabilities);

    // A coordinator naming a model the conversation does not permit, or a
    // missing coordinator profile, starts nothing.
    let elsewhere = AgentProfiles {
        profiles: vec![profile("lead", Some(&self::model()))],
        problems: vec![],
    };
    assert_eq!(start(&team, &parent, &elsewhere, &[]), None);
    assert_eq!(start(&team, &parent, &AgentProfiles::default(), &[]), None);
}
