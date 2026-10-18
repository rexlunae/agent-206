use super::*;

fn source(id: &str, text: &str) -> Source {
    Source {
        id: id.into(),
        text: text.into(),
        digest: String::new(),
    }
}
fn profile(id: &str) -> AgentProfile {
    agent_profiles::parse(id, &format!("name = \"{id} name\""), String::new()).unwrap()
}

#[test]
fn a_team_reads_its_fields_and_refuses_what_it_cannot_use() {
    let team = parse(source(
        "research",
        r#"
name = " Research "
coordinator = "lead"
members = ["reader", "writer"]
instructions = "  Cite sources. "
"#,
    ))
    .unwrap();
    assert_eq!(team.name, "Research");
    assert_eq!(team.coordinator, "lead");
    assert_eq!(team.members, ["reader", "writer"]);
    assert_eq!(team.instructions, "Cite sources.");

    let refused = |text: &str| parse(source("t", text)).unwrap_err();
    assert!(matches!(
        refused("name = \"x\"\ncoordinator = \"a\"\nmembers = []"),
        TeamError::Members
    ));
    let many: Vec<String> = (0..=MAX_MEMBERS).map(|i| format!("\"m{i}\"")).collect();
    assert!(matches!(
        refused(&format!(
            "name = \"x\"\ncoordinator = \"a\"\nmembers = [{}]",
            many.join(",")
        )),
        TeamError::Members
    ));
    assert!(matches!(
        refused("name = \"x\"\ncoordinator = \"a\"\nmembers = [\"b\", \"b\"]"),
        TeamError::Duplicate(id) if id == "b"
    ));
    assert!(matches!(
        refused("name = \"x\"\ncoordinator = \"a\"\nmembers = [\"a\"]"),
        TeamError::Duplicate(id) if id == "a"
    ));
    assert!(matches!(
        refused("name = \"x\"\ncoordinator = \"../a\"\nmembers = [\"b\"]"),
        TeamError::ProfileId(_)
    ));
    assert!(matches!(
        refused("name = \"x\"\ncoordinator = \"a\"\nmembers = [\"b\"]\nflows = []"),
        TeamError::Parse(_)
    ));
    assert!(matches!(
        refused("name = \"\"\ncoordinator = \"a\"\nmembers = [\"b\"]"),
        TeamError::File(ProfileError::Name)
    ));
}

#[test]
fn a_team_starts_only_with_usable_members_and_admits_only_them() {
    let team = parse(source(
        "t",
        "name = \"T\"\ncoordinator = \"lead\"\nmembers = [\"reader\", \"writer\"]",
    ))
    .unwrap();
    let record = coordinate(
        &team,
        &profile("lead"),
        [
            ("reader".to_string(), Ok(profile("reader"))),
            ("writer".to_string(), Ok(profile("writer"))),
        ],
    )
    .unwrap();
    assert_eq!(record.role, TeamRole::Coordinator);
    assert_eq!(record.coordinator.name, "lead name");
    assert!(record.has_member("writer") && !record.has_member("lead"));
    assert!(record.prompt().contains("- reader: reader name"));

    let broken = coordinate(
        &team,
        &profile("lead"),
        [
            ("reader".to_string(), Ok(profile("reader"))),
            (
                "writer".to_string(),
                Err(ProfileError::Unknown("writer".into())),
            ),
        ],
    )
    .unwrap_err();
    assert!(matches!(&broken, TeamError::Member { member, .. } if member == "writer"));
    assert_eq!(
        crate::error::report(&broken).kind,
        ErrorKind::NotFound,
        "a missing member profile keeps its own kind"
    );

    // A coordinator's subagents join as members, from member profiles only.
    let joined = join(Some(&record), Some("reader")).unwrap().unwrap();
    assert_eq!(joined.role, TeamRole::Member);
    assert!(joined.prompt().contains("lead name"));
    assert!(matches!(
        join(Some(&record), Some("lead")),
        Err(TeamError::NotAMember(_))
    ));
    assert!(matches!(
        join(Some(&record), None),
        Err(TeamError::MemberRequired)
    ));
    // A member's own subagents, and those outside a team, join none.
    assert_eq!(join(Some(&joined), None).unwrap(), None);
    assert_eq!(join(None, Some("lead")).unwrap(), None);
}

#[test]
fn a_directory_lists_usable_teams_beside_the_files_it_could_not_use() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        load(&dir.path().join("missing")).unwrap(),
        AgentTeams::default()
    );
    std::fs::write(
        dir.path().join("b.toml"),
        "name = \"B\"\ncoordinator = \"x\"\nmembers = [\"y\"]",
    )
    .unwrap();
    std::fs::write(dir.path().join("broken.toml"), "name = \"C\"").unwrap();
    let loaded = load(dir.path()).unwrap();
    assert_eq!(loaded.teams.len(), 1);
    assert_eq!(loaded.teams[0].digest.len(), 64);
    assert_eq!(loaded.problems.len(), 1);
    assert_eq!(loaded.problems[0].file, "broken.toml");
    assert_eq!(loaded.problems[0].error.kind, ErrorKind::InvalidRequest);

    assert_eq!(find(dir.path(), "b").unwrap().name, "B");
    assert!(matches!(
        find(dir.path(), "zzz"),
        Err(TeamError::Unknown(_))
    ));
    assert!(matches!(
        find(dir.path(), "broken"),
        Err(TeamError::Unusable(..))
    ));
    assert!(a206_protocol::encode(&a206_core::Reply::AgentTeams(loaded)).is_ok());
}
