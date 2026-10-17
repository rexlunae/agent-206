//! Policy and host calls, exercised without a worker process. The end-to-end
//! path through the real worker is covered by the app's smoke test.
use super::*;

mod fetch;
mod git;
use a206_extensions::abi::{EntryKind, SearchMatch};

fn store() -> (tempfile::TempDir, Arc<Mutex<Store>>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data")).unwrap();
    (dir, Arc::new(Mutex::new(store)))
}
/// Host calls for a freshly installed, enabled extension with no grants.
fn host_calls(
    store: &Arc<Mutex<Store>>,
    workspace: Option<PathBuf>,
    effective: &[Capability],
) -> HostCalls {
    let id = uuid::Uuid::new_v4().to_string();
    let mut plugin = workspace_extension().summary(true, &BTreeSet::new());
    plugin.id = id.clone();
    store.lock().unwrap().state.plugins.push(plugin);
    calls_for(&id, store, workspace, effective)
}
/// Host calls for the bundled workspace extension, with the same grant
/// stored for it as the invocation's effective set.
fn workspace_calls(
    store: &Arc<Mutex<Store>>,
    workspace: Option<PathBuf>,
    effective: &[Capability],
) -> HostCalls {
    let id = workspace_extension().id;
    grant(store, &id, effective);
    calls_for(&id, store, workspace, effective)
}
fn calls_for(
    id: &str,
    store: &Arc<Mutex<Store>>,
    workspace: Option<PathBuf>,
    effective: &[Capability],
) -> HostCalls {
    HostCalls {
        skills_root: store.lock().unwrap().root.join("skills"),
        extension: id.into(),
        effective: effective.iter().copied().collect(),
        workspace,
        store: Arc::clone(store),
        hosts: Vec::new(),
        net: Arc::new(crate::network::Internet),
        source: PluginSource::Bundled,
        logs: 0,
        last_failure: None,
    }
}
fn code(reply: HostReply) -> Option<HostErrorCode> {
    match reply {
        HostReply::Failed(failure) => Some(failure.code),
        _ => None,
    }
}
fn read(path: &str) -> HostRequest {
    HostRequest::ReadFile { path: path.into() }
}

#[tokio::test]
async fn host_calls_stop_once_the_package_is_replaced() {
    let (_dir, store) = store();
    let digest = || PluginSource::Installed {
        digest: uuid::Uuid::new_v4().simple().to_string(),
    };
    let running = digest();
    let mut calls = host_calls(&store, None, &[]);
    calls.source = running.clone();
    let get = || HostRequest::StateGet { key: "k".into() };
    let set = |source: PluginSource| {
        let mut store = store.lock().unwrap();
        let plugin = store.state.plugins.last_mut().unwrap();
        plugin.source = source;
    };
    set(running.clone());
    assert_eq!(calls.handle(get()).await, HostReply::State(None));
    // An update recorded while the old module runs ends its authority, even
    // once the extension is enabled again.
    set(digest());
    assert_eq!(code(calls.handle(get()).await), Some(HostErrorCode::Denied));
}

#[tokio::test]
async fn workspace_calls_need_the_effective_grant() {
    let (dir, store) = store();
    let body = uuid::Uuid::new_v4().to_string();
    std::fs::write(dir.path().join("note.txt"), &body).unwrap();
    let mut denied = host_calls(&store, Some(dir.path().into()), &[]);
    assert_eq!(
        code(denied.handle(read("note.txt")).await),
        Some(HostErrorCode::Denied)
    );
    // The gateway, not the guest, records why the call failed.
    assert_eq!(
        denied.last_failure.map(|f| f.code),
        Some(HostErrorCode::Denied)
    );
    let mut granted = workspace_calls(
        &store,
        Some(dir.path().into()),
        &[Capability::ReadWorkspace],
    );
    assert_eq!(
        granted.handle(read("note.txt")).await,
        HostReply::File(body.into_bytes().into())
    );
    let mut no_workspace = workspace_calls(&store, None, &[Capability::ReadWorkspace]);
    assert_eq!(
        code(no_workspace.handle(read("note.txt")).await),
        Some(HostErrorCode::NoWorkspace)
    );
}

#[tokio::test]
async fn workspace_calls_stay_inside_the_root() {
    let (dir, store) = store();
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(dir.path().join("outside.txt"), "secret").unwrap();
    std::fs::write(root.join("big.bin"), vec![0; MAX_FILE as usize + 1]).unwrap();
    let mut host = workspace_calls(&store, Some(root.clone()), &[Capability::ReadWorkspace]);
    let outside = dir.path().join("outside.txt");
    for path in ["../outside.txt", outside.to_str().unwrap()] {
        assert_eq!(
            code(host.handle(read(path)).await),
            Some(HostErrorCode::Invalid),
            "{path}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert_eq!(
            code(host.handle(read("link")).await),
            Some(HostErrorCode::Invalid)
        );
    }
    assert_eq!(
        code(host.handle(read("big.bin")).await),
        Some(HostErrorCode::TooLarge)
    );
    assert_eq!(
        code(host.handle(read("sub")).await),
        Some(HostErrorCode::Invalid)
    );
    assert_eq!(
        code(host.handle(read("missing")).await),
        Some(HostErrorCode::NotFound)
    );
    let HostReply::Directory(mut entries) = host
        .handle(HostRequest::ListDirectory { path: ".".into() })
        .await
    else {
        panic!("listing failed");
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let kinds: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert!(kinds.contains(&("sub", EntryKind::Directory)));
    assert!(kinds.contains(&("big.bin", EntryKind::File)));
    assert_eq!(
        code(
            host.handle(HostRequest::ListDirectory { path: "..".into() })
                .await
        ),
        Some(HostErrorCode::Invalid)
    );
}

fn write(path: &str, contents: &str, overwrite: bool) -> HostRequest {
    HostRequest::WriteFile {
        path: path.into(),
        contents: contents.as_bytes().to_vec().into(),
        overwrite,
    }
}

#[tokio::test]
async fn writes_need_the_write_grant_and_stay_inside_the_root() {
    let (dir, store) = store();
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let body = uuid::Uuid::new_v4().to_string();
    let mut read_only = workspace_calls(&store, Some(root.clone()), &[Capability::ReadWorkspace]);
    assert_eq!(
        code(read_only.handle(write("new.txt", &body, false)).await),
        Some(HostErrorCode::Denied)
    );
    assert!(!root.join("new.txt").exists());

    let mut host = workspace_calls(
        &store,
        Some(root.clone()),
        &[Capability::ReadWorkspace, Capability::WriteWorkspace],
    );
    let outside = dir.path().join("outside.txt");
    for path in [
        "../outside.txt",
        outside.to_str().unwrap(),
        ".git/config",
        "sub/../../outside.txt",
        "",
    ] {
        assert_eq!(
            code(host.handle(write(path, &body, true)).await),
            Some(HostErrorCode::Invalid),
            "{path:?}"
        );
    }
    assert!(!outside.exists());
    assert!(!root.join(".git/config").exists());
    #[cfg(unix)]
    {
        std::fs::write(&outside, "kept").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert_eq!(
            code(host.handle(write("link", &body, true)).await),
            Some(HostErrorCode::Invalid)
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "kept");

        // A relative link stays inside the root, so cap-std allows it; it
        // still must not lead a write into `.git`.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::os::unix::fs::symlink(".git", root.join("repo")).unwrap();
        assert_eq!(
            code(host.handle(write("repo/config", &body, false)).await),
            Some(HostErrorCode::Invalid)
        );
        assert_eq!(
            code(
                host.handle(HostRequest::CreateDirectory {
                    path: "repo/hooks".into()
                })
                .await
            ),
            Some(HostErrorCode::Invalid)
        );
        assert!(!root.join(".git/config").exists());
        assert!(!root.join(".git/hooks").exists());
    }
    let huge = "x".repeat(MAX_FILE as usize + 1);
    assert_eq!(
        code(host.handle(write("huge.txt", &huge, false)).await),
        Some(HostErrorCode::TooLarge)
    );
}

#[tokio::test]
async fn writes_create_replace_and_edit_files() {
    let (dir, store) = store();
    let root = dir.path().to_path_buf();
    let mut host = workspace_calls(
        &store,
        Some(root.clone()),
        &[Capability::ReadWorkspace, Capability::WriteWorkspace],
    );
    let (first, second) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    assert_eq!(
        host.handle(HostRequest::CreateDirectory { path: "a/b".into() })
            .await,
        HostReply::Done
    );
    assert_eq!(
        host.handle(write("a/b/f.txt", &first, false)).await,
        HostReply::Done
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a/b/f.txt")).unwrap(),
        first
    );
    assert_eq!(
        code(host.handle(write("a/b/f.txt", &second, false)).await),
        Some(HostErrorCode::Exists)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(root.join("a/b/f.txt"), mode).unwrap();
    }
    assert_eq!(
        host.handle(write("a/b/f.txt", &second, true)).await,
        HostReply::Done
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a/b/f.txt")).unwrap(),
        second
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join("a/b/f.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "replacing keeps permissions");
    }
    // No temporary file is left behind.
    assert_eq!(std::fs::read_dir(root.join("a/b")).unwrap().count(), 1);
    assert_eq!(
        code(
            host.handle(HostRequest::CreateDirectory {
                path: "a/b/f.txt".into()
            })
            .await
        ),
        Some(HostErrorCode::Exists)
    );

    let edit = |old: &str, new: &str| HostRequest::EditFile {
        path: "a/b/f.txt".into(),
        old: old.into(),
        new: new.into(),
    };
    std::fs::write(
        root.join("a/b/f.txt"),
        format!("{first}\n{second}\n{second}\n"),
    )
    .unwrap();
    assert_eq!(host.handle(edit(&first, "one")).await, HostReply::Done);
    assert_eq!(
        code(host.handle(edit(&first, "again")).await),
        Some(HostErrorCode::Conflict),
        "the text is gone"
    );
    assert_eq!(
        code(host.handle(edit(&second, "two")).await),
        Some(HostErrorCode::Conflict),
        "the text is ambiguous"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a/b/f.txt")).unwrap(),
        format!("one\n{second}\n{second}\n")
    );
}

#[tokio::test]
async fn search_is_bounded_and_does_not_follow_links() {
    let (dir, store) = store();
    let root = dir.path().join("project");
    let needle = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("src/a.txt"), format!("x\n{needle} here\n")).unwrap();
    std::fs::write(root.join("src/deep/b.txt"), format!("{needle}\n")).unwrap();
    std::fs::write(root.join(".git/c.txt"), &needle).unwrap();
    std::fs::write(root.join("binary.bin"), [0xff, 0xfe, 0x00]).unwrap();
    // After `src/` in name order, though it is nearer the search root.
    std::fs::write(root.join("top.txt"), &needle).unwrap();
    std::fs::write(dir.path().join("outside.txt"), &needle).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path(), root.join("escape")).unwrap();
    let search = |path: &str, max_matches| HostRequest::Search {
        path: path.into(),
        query: needle.clone(),
        max_matches,
    };
    let mut denied = workspace_calls(&store, Some(root.clone()), &[]);
    assert_eq!(
        code(denied.handle(search(".", 10)).await),
        Some(HostErrorCode::Denied)
    );

    let mut host = workspace_calls(&store, Some(root.clone()), &[Capability::ReadWorkspace]);
    let HostReply::Matches(found) = host.handle(search(".", 10)).await else {
        panic!("search failed");
    };
    assert!(!found.truncated);
    assert_eq!(
        found.matches,
        vec![
            SearchMatch {
                path: "src/a.txt".into(),
                line: 2,
                text: format!("{needle} here")
            },
            SearchMatch {
                path: "src/deep/b.txt".into(),
                line: 1,
                text: needle.clone()
            },
            SearchMatch {
                path: "top.txt".into(),
                line: 1,
                text: needle.clone()
            },
        ]
    );
    let HostReply::Matches(first) = host.handle(search(".", 1)).await else {
        panic!("search failed");
    };
    assert!(first.truncated);
    assert_eq!(
        first
            .matches
            .iter()
            .map(|m| &m.path[..])
            .collect::<Vec<_>>(),
        ["src/a.txt"]
    );
    assert_eq!(
        code(host.handle(search("..", 10)).await),
        Some(HostErrorCode::Invalid)
    );
    let empty = HostRequest::Search {
        path: ".".into(),
        query: String::new(),
        max_matches: 1,
    };
    assert_eq!(code(host.handle(empty).await), Some(HostErrorCode::Invalid));
}

#[tokio::test]
async fn revoking_stops_calls_already_running() {
    let (dir, store) = store();
    std::fs::write(dir.path().join("note.txt"), "text").unwrap();
    let mut host = workspace_calls(
        &store,
        Some(dir.path().into()),
        &[Capability::ReadWorkspace],
    );
    assert!(matches!(
        host.handle(read("note.txt")).await,
        HostReply::File(_)
    ));
    let id = workspace_extension().id;
    grant(&store, &id, &[]);
    assert_eq!(
        code(host.handle(read("note.txt")).await),
        Some(HostErrorCode::Denied)
    );
    grant(&store, &id, &[Capability::ReadWorkspace]);
    store
        .lock()
        .unwrap()
        .state
        .plugins
        .iter_mut()
        .for_each(|p| p.enabled = false);
    assert_eq!(
        code(host.handle(read("note.txt")).await),
        Some(HostErrorCode::Denied)
    );
    // Disabling also stops calls that need no capability.
    let state = HostRequest::StateGet { key: "k".into() };
    assert_eq!(code(host.handle(state).await), Some(HostErrorCode::Denied));
    // A grant stored for an id is not authority for an invocation that was
    // never given it.
    store
        .lock()
        .unwrap()
        .state
        .plugins
        .iter_mut()
        .for_each(|p| p.enabled = true);
    let mut narrow = calls_for(&id, &store, Some(dir.path().into()), &[]);
    assert_eq!(
        code(narrow.handle(read("note.txt")).await),
        Some(HostErrorCode::Denied)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn special_files_are_refused_without_blocking() {
    let (dir, store) = store();
    nix::unistd::mkfifo(
        &dir.path().join("pipe"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    let mut host = workspace_calls(
        &store,
        Some(dir.path().into()),
        &[Capability::ReadWorkspace],
    );
    // A blocking open would wait for a writer that never comes.
    assert_eq!(
        code(host.handle(read("pipe")).await),
        Some(HostErrorCode::Invalid)
    );
}

#[tokio::test]
async fn state_is_private_to_each_extension() {
    let (_dir, store) = store();
    let mut first = host_calls(&store, None, &[]);
    let mut second = host_calls(&store, None, &[]);
    let value = uuid::Uuid::new_v4().as_bytes().to_vec();
    let put = |value: Option<Vec<u8>>| HostRequest::StatePut {
        key: "k".into(),
        value: value.map(Into::into),
    };
    let get = || HostRequest::StateGet { key: "k".into() };
    assert_eq!(
        first.handle(put(Some(value.clone()))).await,
        HostReply::Done
    );
    assert_eq!(
        first.handle(get()).await,
        HostReply::State(Some(value.into()))
    );
    assert_eq!(second.handle(get()).await, HostReply::State(None));
    let invalid = HostRequest::StateGet { key: "a/b".into() };
    assert_eq!(
        code(first.handle(invalid).await),
        Some(HostErrorCode::Invalid)
    );
    let huge = HostRequest::StatePut {
        key: "big".into(),
        value: Some(vec![0; STATE_QUOTA.bytes as usize].into()),
    };
    assert_eq!(code(first.handle(huge).await), Some(HostErrorCode::Budget));
}

#[tokio::test]
async fn logs_are_budgeted() {
    let (_dir, store) = store();
    let mut host = host_calls(&store, None, &[]);
    let log = || HostRequest::Log {
        level: LogLevel::Debug,
        message: "m".into(),
    };
    for _ in 0..MAX_LOGS {
        assert_eq!(host.handle(log()).await, HostReply::Done);
    }
    assert_eq!(code(host.handle(log()).await), Some(HostErrorCode::Budget));
}

fn workspace_extension() -> Manifest {
    a206_extensions::builtin("agent206.workspace")
        .unwrap()
        .unwrap()
        .manifest
}
fn grant(store: &Arc<Mutex<Store>>, id: &str, capabilities: &[Capability]) {
    let mut store = store.lock().unwrap();
    let plugin = store.state.plugins.iter_mut().find(|p| p.id == id).unwrap();
    plugin.granted = capabilities.iter().copied().collect();
}

/// These checks run before a worker is spawned, so they need no binary.
#[tokio::test]
async fn policy_is_checked_before_the_guest_runs() {
    let (dir, store) = store();
    let host = ExtensionHost::new(Arc::clone(&store), dir.path().into());
    let id = workspace_extension().id;
    let tool = workspace_extension().tools[0].clone();
    let path = |p: &str| -> Args { [("path".to_string(), Value::Str(p.into()))].into() };
    let workspace = || Scope {
        workspace: Some(dir.path().into()),
        ceiling: None,
    };
    let kind = |error: ExtensionError| crate::error::report(&error).kind;
    use a206_core::extension::Value;

    assert!(host.tools().unwrap().is_empty(), "no tools before a grant");
    let error = host
        .call_tool(&id, &tool.name, path("."), workspace())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ExtensionError::NotGranted {
            capability: Capability::ReadWorkspace,
            ..
        }
    ));
    assert_eq!(kind(error), ErrorKind::PermissionDenied);

    // Only tools whose every declared capability is granted are offered.
    let listed = |granted: &[Capability]| {
        grant(&store, &id, granted);
        let offered: Vec<_> = host
            .tools()
            .unwrap()
            .into_iter()
            .map(|t| t.spec.name)
            .collect();
        let expected: Vec<_> = workspace_extension()
            .tools
            .into_iter()
            .filter(|t| t.capabilities.iter().all(|c| granted.contains(c)))
            .map(|t| t.name)
            .collect();
        assert_eq!(offered, expected);
        offered.len()
    };
    let all = [Capability::ReadWorkspace, Capability::WriteWorkspace];
    assert!(listed(&all) > listed(&[Capability::ReadWorkspace]));
    let error = host
        .call_tool(&id, &tool.name, path("."), Scope::default())
        .await
        .unwrap_err();
    assert!(matches!(error, ExtensionError::NoProject));
    let narrow = Scope {
        ceiling: Some(BTreeSet::new()),
        ..workspace()
    };
    let error = host
        .call_tool(&id, &tool.name, path("."), narrow)
        .await
        .unwrap_err();
    assert!(matches!(error, ExtensionError::OutOfScope { .. }));
    let wrong: Args = [("path".to_string(), Value::Int(1))].into();
    let error = host
        .call_tool(&id, &tool.name, wrong, workspace())
        .await
        .unwrap_err();
    assert_eq!(kind(error), ErrorKind::InvalidRequest);
    let error = host
        .call_tool(&id, "missing", Args::new(), workspace())
        .await
        .unwrap_err();
    assert_eq!(kind(error), ErrorKind::NotFound);
    let error = host
        .open_panel("missing.extension", "overview", None, workspace())
        .await
        .unwrap_err();
    assert_eq!(kind(error), ErrorKind::NotFound);

    store
        .lock()
        .unwrap()
        .state
        .plugins
        .iter_mut()
        .for_each(|p| p.enabled = false);
    assert!(host.tools().unwrap().is_empty());
    let error = host
        .open_panel(&id, "overview", None, workspace())
        .await
        .unwrap_err();
    assert!(matches!(error, ExtensionError::Disabled(_)));
}

#[tokio::test]
async fn skill_imports_are_builtin_scoped_and_recheck_current_approval() {
    let (_dir, store) = store();
    let root = store.lock().unwrap().root.join("skills/notes");
    std::fs::create_dir_all(&root).unwrap();
    let body = uuid::Uuid::new_v4().to_string();
    let source = format!("---\nname: notes\ndescription: Approved instructions\n---\n{body}");
    std::fs::write(root.join("SKILL.md"), &source).unwrap();
    let digest = a206_skills::parse(&source, "notes").unwrap().digest;
    store
        .lock()
        .unwrap()
        .state
        .settings
        .enabled_skills
        .insert("notes".into(), digest);
    let request = HostRequest::SkillRead {
        name: "notes".into(),
        path: None,
    };
    let mut unrelated = host_calls(&store, None, &[]);
    assert_eq!(
        code(unrelated.handle(request.clone()).await),
        Some(HostErrorCode::Denied)
    );
    let mut builtin = calls_for(crate::skill_tool::EXTENSION, &store, None, &[]);
    let HostReply::File(bytes) = builtin.handle(request.clone()).await else {
        panic!("approved read failed");
    };
    assert_eq!(String::from_utf8(bytes.into_vec()).unwrap(), body);
    store.lock().unwrap().state.settings.enabled_skills.clear();
    assert_eq!(
        code(builtin.handle(request).await),
        Some(HostErrorCode::Denied)
    );
}

#[tokio::test]
async fn oversized_skill_index_keeps_its_size_error_at_the_wasm_boundary() {
    let (_dir, store) = store();
    let root = store.lock().unwrap().root.join("skills/notes");
    std::fs::create_dir_all(&root).unwrap();
    let source = format!(
        "---\nname: notes\ndescription: {}\n---\nBody",
        "x".repeat(crate::tools::MAX_OUTPUT)
    );
    std::fs::write(root.join("SKILL.md"), &source).unwrap();
    let digest = a206_skills::parse(&source, "notes").unwrap().digest;
    store
        .lock()
        .unwrap()
        .state
        .settings
        .enabled_skills
        .insert("notes".into(), digest);
    let mut builtin = calls_for(crate::skill_tool::EXTENSION, &store, None, &[]);
    assert_eq!(
        code(builtin.handle(HostRequest::SkillList).await),
        Some(HostErrorCode::TooLarge)
    );
}

#[tokio::test]
async fn starting_keeps_proposed_packages_but_prunes_unrecorded_ones() {
    let (dir, store) = store();
    let host = ExtensionHost::new(Arc::clone(&store), dir.path().into());
    let builtin = a206_extensions::builtin("agent206.workspace")
        .unwrap()
        .unwrap();
    let id = format!("test.{}", uuid::Uuid::new_v4().simple());
    let manifest = include_str!("../../../../plugins/workspace/plugin.toml").replacen(
        "id = \"agent206.workspace\"",
        &format!("id = \"{id}\""),
        1,
    );
    let proposal = host
        .propose(uuid::Uuid::new_v4(), manifest, builtin.wasm.to_vec(), 0)
        .await
        .unwrap();
    let packages = packages::directory(dir.path());
    let stray = packages.join(uuid::Uuid::new_v4().simple().to_string().repeat(2));
    std::fs::create_dir(&stray).unwrap();
    // A gateway start prunes what nothing records, and only that.
    ExtensionHost::new(Arc::clone(&store), dir.path().into());
    assert!(packages.join(&proposal.digest).exists());
    assert!(!stray.exists());
    // A proposal is not an installed extension: it cannot run.
    assert!(matches!(
        host.load(&id),
        Err(ExtensionError::NotInstalled(_))
    ));
}
