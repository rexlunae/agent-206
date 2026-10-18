use super::*;
use a206_core::Id;

fn key() -> ModelKey {
    ModelKey {
        provider: Id::new_v4(),
        model: format!("model-{}", Id::new_v4()),
    }
}
fn provider(id: Id, label: &str) -> ProviderConfig {
    ProviderConfig {
        id,
        label: label.into(),
        base_url: String::new(),
        credential: None,
        api: Default::default(),
    }
}

#[test]
fn a_profile_reads_its_fields_and_refuses_what_it_cannot_use() {
    let profile = parse(
        "reviewer",
        r#"
name = " Reviewer "
description = "Reads diffs"
mode = "review"
capabilities = ["read_workspace", "git_read"]
instructions = """
  Look for bugs.
"""
"#,
        "d".into(),
    )
    .unwrap();
    assert_eq!(profile.name, "Reviewer");
    assert_eq!(profile.mode, Some(Mode::Review));
    assert_eq!(profile.instructions, "Look for bugs.");
    assert_eq!(
        profile.capabilities,
        Some([Capability::ReadWorkspace, Capability::GitRead].into())
    );

    let refused = |id: &str, text: &str| parse(id, text, String::new()).unwrap_err();
    assert!(matches!(refused("Upper", "name = \"x\""), ProfileError::Id));
    assert!(matches!(refused("a", "name = \" \""), ProfileError::Name));
    assert!(matches!(
        refused("a", "name = \"x\"\ngrant = \"everything\""),
        ProfileError::Parse(_)
    ));
    assert!(matches!(
        refused("a", "name = \"x\"\ncapabilities = [\"root\"]"),
        ProfileError::Parse(_)
    ));
    assert!(matches!(
        refused("a", "name = \"x\"\nconnection = \"c\""),
        ProfileError::ConnectionWithoutModel
    ));
    let long = format!(
        "name = \"x\"\ninstructions = \"{}\"",
        "a".repeat(MAX_INSTRUCTIONS + 1)
    );
    assert!(matches!(refused("a", &long), ProfileError::Instructions));
}

#[test]
fn a_profile_only_narrows_and_names_a_model_only_among_permitted_ones() {
    let chosen = key();
    let other = key();
    let label = format!("connection-{}", Id::new_v4());
    let providers = [
        provider(chosen.provider, &label),
        provider(other.provider, "other"),
    ];
    let text = format!(
        "name = \"x\"\nmode = \"plan\"\nmodel = \"{}\"\ncapabilities = [\"read_workspace\"]",
        chosen.model
    );
    let profile = parse("x", &text, String::new()).unwrap();

    let permitted = BTreeSet::from([chosen.clone(), other.clone()]);
    assert_eq!(
        model(&profile, &permitted, &providers).unwrap(),
        Some(chosen.clone())
    );
    // A profile's model is never added: one the user did not permit is refused.
    assert!(matches!(
        model(&profile, &[other.clone()].into(), &providers),
        Err(ProfileError::ModelDenied { .. })
    ));
    // The same name on two connections needs `connection` to choose.
    let twin = ModelKey {
        provider: other.provider,
        model: chosen.model.clone(),
    };
    let both = BTreeSet::from([chosen.clone(), twin.clone()]);
    assert!(matches!(
        model(&profile, &both, &providers),
        Err(ProfileError::AmbiguousModel { .. })
    ));
    let pinned = parse(
        "x",
        &format!("{text}\nconnection = \"{label}\""),
        String::new(),
    )
    .unwrap();
    assert_eq!(
        model(&pinned, &both, &providers).unwrap(),
        Some(chosen.clone())
    );

    let asked = BTreeSet::from([Capability::ReadWorkspace, Capability::Execute]);
    for key in [&chosen, &twin] {
        assert!(matches!(
            admit(&profile, Mode::Plan, key, &both, &providers, asked.clone()),
            Err(ProfileError::AmbiguousModel { .. })
        ));
    }
    assert_eq!(
        admit(
            &profile,
            Mode::Plan,
            &chosen,
            &permitted,
            &providers,
            asked.clone()
        )
        .unwrap(),
        [Capability::ReadWorkspace].into()
    );
    assert!(matches!(
        admit(
            &profile,
            Mode::Code,
            &chosen,
            &permitted,
            &providers,
            asked.clone()
        ),
        Err(ProfileError::Mode { .. })
    ));
    assert!(matches!(
        admit(&profile, Mode::Plan, &other, &permitted, &providers, asked),
        Err(ProfileError::ModelMismatch { .. })
    ));
}

#[test]
fn excess_files_produce_one_bounded_problem_and_a_sendable_listing() {
    let dir = tempfile::tempdir().unwrap();
    for index in (0..200).rev() {
        std::fs::write(
            dir.path().join(format!("p{index:03}.toml")),
            "name = 'Profile'",
        )
        .unwrap();
    }
    let loaded = load(dir.path()).unwrap();
    assert_eq!(loaded.profiles.len(), 64);
    assert_eq!(loaded.problems.len(), 1);
    assert_eq!(loaded.profiles[0].id, "p000");
    assert_eq!(loaded.profiles.last().unwrap().id, "p063");
    assert!(a206_protocol::encode(&a206_core::Reply::AgentProfiles(loaded)).is_ok());
}

#[test]
fn a_directory_lists_usable_profiles_beside_the_files_it_could_not_use() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        load(&dir.path().join("missing")).unwrap(),
        AgentProfiles::default()
    );
    std::fs::write(dir.path().join("b.toml"), "name = \"B\"").unwrap();
    std::fs::write(dir.path().join("a.toml"), "name = \"A\"").unwrap();
    std::fs::write(dir.path().join("broken.toml"), "name = ").unwrap();
    std::fs::write(dir.path().join("notes.md"), "ignored").unwrap();
    std::fs::write(
        dir.path().join("big.toml"),
        "#".repeat(MAX_FILE as usize + 1),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("dir.toml")).unwrap();
    let loaded = load(dir.path()).unwrap();
    let ids: Vec<_> = loaded.profiles.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["a", "b"]);
    let problems: Vec<_> = loaded.problems.iter().map(|p| p.file.as_str()).collect();
    assert_eq!(problems, ["big.toml", "broken.toml", "dir.toml"]);
    assert!(
        loaded
            .problems
            .iter()
            .all(|p| p.error.kind == ErrorKind::InvalidRequest)
    );
    assert_eq!(loaded.profiles[0].digest.len(), 64);

    assert_eq!(find(dir.path(), "a").unwrap().name, "A");
    assert!(matches!(
        find(dir.path(), "zzz"),
        Err(ProfileError::Unknown(_))
    ));
    assert!(matches!(
        find(dir.path(), "../a"),
        Err(ProfileError::Unknown(_))
    ));
    assert!(matches!(
        find(dir.path(), "broken"),
        Err(ProfileError::Unusable(_, _))
    ));
}
