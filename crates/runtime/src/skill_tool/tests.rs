use super::*;
use a206_core::Id;
use serde_json::json;

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
        programs: vec![],
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
    let root = host.root.clone();
    let pins = host.pins().map_err(|error| crate::error::report(&error))?;
    let request = ToolRequest {
        call: Id::new_v4().to_string(),
        name: name.into(),
        arguments,
        approved: false,
    };
    io(move || read(&root, &pins, &request))
        .await
        .map(|text| ToolOutput {
            text,
            is_error: false,
        })
        .map_err(|error| crate::error::report(&error))
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

#[tokio::test]
async fn oversized_index_is_refused_instead_of_returning_partial_entries() {
    let profile = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(Store::open(profile.path()).unwrap()));
    let host = SkillToolHost::new(store.clone(), profile.path());
    let dir = profile.path().join("skills/notes");
    std::fs::create_dir_all(&dir).unwrap();
    let source = format!(
        "---\nname: notes\ndescription: {}\n---\nInstructions",
        "x".repeat(tools::MAX_OUTPUT)
    );
    std::fs::write(dir.join("SKILL.md"), &source).unwrap();
    let digest = a206_skills::parse(&source, "notes").unwrap().digest;
    store
        .lock()
        .unwrap()
        .state
        .settings
        .enabled_skills
        .insert("notes".into(), digest);
    assert_eq!(
        call(&host, LIST, json!({})).await.unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
}

#[tokio::test]
async fn empty_approvals_still_surface_a_broken_skill_directory() {
    let profile = tempfile::tempdir().unwrap();
    std::fs::write(profile.path().join("skills"), "not a directory").unwrap();
    let store = Arc::new(Mutex::new(Store::open(profile.path()).unwrap()));
    let host = SkillToolHost::new(store, profile.path());
    let listing = host.list(&scope()).await;
    assert!(listing.tools.is_empty());
    assert_eq!(listing.unavailable.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_reads_do_not_block_the_async_executor() {
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let task = tokio::spawn(io(move || {
        started.send(()).unwrap();
        wait.recv().unwrap();
        Ok(())
    }));
    ready.await.unwrap();
    // This runs on the same single async worker while the filesystem stand-in
    // is blocked. A direct synchronous read would prevent it from running.
    tokio::task::yield_now().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
}

#[tokio::test]
async fn a_list_too_large_for_one_result_is_refused_rather_than_cut() {
    let profile = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(Store::open(profile.path()).unwrap()));
    let host = SkillToolHost::new(store.clone(), profile.path());
    let dir = profile.path().join("skills/long");
    std::fs::create_dir_all(&dir).unwrap();
    let description = Id::new_v4()
        .simple()
        .to_string()
        .repeat(tools::MAX_OUTPUT / 32 + 1);
    let text = format!("---\nname: long\ndescription: {description}\n---\nBody");
    std::fs::write(dir.join("SKILL.md"), &text).unwrap();
    let digest = a206_skills::parse(&text, "long").unwrap().digest;
    store
        .lock()
        .unwrap()
        .state
        .settings
        .enabled_skills
        .insert("long".into(), digest);
    let refused = call(&host, LIST, json!({})).await.unwrap_err();
    assert_eq!(refused.kind, ErrorKind::InvalidRequest);
    assert_eq!(
        call(&host, INFO, json!({"name": "long"}))
            .await
            .unwrap()
            .text,
        "Body"
    );
}
