//! Policy and host calls, exercised without a worker process. The end-to-end
//! path through the real worker is covered by the app's smoke test.
use super::*;

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
        extension: id.into(),
        effective: effective.iter().copied().collect(),
        workspace,
        store: Arc::clone(store),
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

    grant(&store, &id, &[Capability::ReadWorkspace]);
    assert_eq!(
        host.tools()
            .unwrap()
            .iter()
            .map(|t| &t.spec.name)
            .collect::<Vec<_>>(),
        workspace_extension()
            .tools
            .iter()
            .map(|t| &t.name)
            .collect::<Vec<_>>()
    );
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
