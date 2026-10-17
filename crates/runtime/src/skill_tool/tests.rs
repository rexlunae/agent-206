use super::*;
use a206_core::Id;

fn write(root: &Path, name: &str, body: &str) -> String {
    let dir = root.join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let text = format!("---\nname: {name}\ndescription: Helps <here>.\n---\n{body}");
    std::fs::write(dir.join("SKILL.md"), &text).unwrap();
    a206_skills::parse(&text, name).unwrap().digest
}
fn scope() -> ToolScope {
    ToolScope {
        thread: Id::new_v4(),
        workspace: None,
        capabilities: Default::default(),
    }
}
async fn call(
    host: &SkillToolHost,
    name: &str,
    arguments: Value,
) -> Result<ToolOutput, ErrorReport> {
    let Value::Object(arguments) = arguments else {
        panic!("arguments are an object")
    };
    host.call(
        &scope(),
        ToolRequest {
            call: Id::new_v4().to_string(),
            name: name.into(),
            arguments,
            approved: false,
        },
    )
    .await
}

#[tokio::test]
async fn only_approved_unchanged_skills_reach_the_model() {
    let profile = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(Store::open(profile.path()).unwrap()));
    let host = SkillToolHost::new(store.clone(), profile.path());
    let body = Id::new_v4().to_string();
    let digest = write(profile.path(), "notes", &body);
    write(profile.path(), "other", "unapproved");

    // Installed but unapproved skills offer no tools at all.
    assert!(host.list(&scope()).await.tools.is_empty());

    store
        .lock()
        .unwrap()
        .state
        .settings
        .enabled_skills
        .insert("notes".into(), digest);
    let listing = host.list(&scope()).await;
    let names: Vec<_> = listing.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, [LIST, INFO]);
    assert!(listing.tools.iter().all(|t| t.capabilities.is_empty()));

    let index = call(&host, LIST, json!({})).await.unwrap().text;
    assert!(index.contains("<skill name=\"notes\">Helps &lt;here&gt;.</skill>"));
    assert!(
        !index.contains("name=\"other\""),
        "unapproved skills are not listed"
    );
    assert_eq!(
        call(&host, INFO, json!({"name": "notes"}))
            .await
            .unwrap()
            .text,
        body
    );

    let refused = call(&host, INFO, json!({"name": "other"}))
        .await
        .unwrap_err();
    assert_eq!(refused.kind, ErrorKind::PermissionDenied);

    // An edit after approval withdraws it.
    write(profile.path(), "notes", "edited");
    let changed = call(&host, INFO, json!({"name": "notes"}))
        .await
        .unwrap_err();
    assert_eq!(changed.kind, ErrorKind::PermissionDenied);
    assert!(host.list(&scope()).await.tools.is_empty());
}

#[tokio::test]
async fn supporting_files_and_arguments_are_checked() {
    let profile = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(Store::open(profile.path()).unwrap()));
    let host = SkillToolHost::new(store.clone(), profile.path());
    let digest = write(profile.path(), "notes", "See ref.md");
    store
        .lock()
        .unwrap()
        .state
        .settings
        .enabled_skills
        .insert("notes".into(), digest);
    let dir = profile.path().join("skills/notes");
    let text = Id::new_v4().to_string();
    std::fs::write(dir.join("ref.md"), &text).unwrap();
    std::fs::write(dir.join("big.md"), "x".repeat(tools::MAX_OUTPUT + 1)).unwrap();
    std::fs::write(dir.join("blob.bin"), [0xff, 0xfe]).unwrap();

    let read = |path: &str| call(&host, INFO, json!({"name": "notes", "path": path}));
    assert_eq!(read("ref.md").await.unwrap().text, text);
    for (path, kind) in [
        ("../notes/ref.md", ErrorKind::InvalidRequest),
        ("big.md", ErrorKind::InvalidRequest),
        ("blob.bin", ErrorKind::InvalidRequest),
        ("missing.md", ErrorKind::NotFound),
    ] {
        assert_eq!(read(path).await.unwrap_err().kind, kind, "{path}");
    }
    for arguments in [
        json!({}),
        json!({"name": 1}),
        json!({"name": "notes", "extra": true}),
    ] {
        assert_eq!(
            call(&host, INFO, arguments.clone()).await.unwrap_err().kind,
            ErrorKind::InvalidRequest,
            "{arguments}"
        );
    }
    assert_eq!(
        call(&host, LIST, json!({"name": "notes"}))
            .await
            .unwrap_err()
            .kind,
        ErrorKind::InvalidRequest
    );
}
