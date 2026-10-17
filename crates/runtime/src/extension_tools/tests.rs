//! Listing, naming and the checks a call passes before any worker starts.
//! Calls that reach the real worker are in `tests/extension_tools.rs`.
use super::*;
use crate::store::Store;
use a206_core::{Capability, Id, extension::ParamSpec};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

fn nonce() -> String {
    Id::new_v4().simple().to_string()
}
struct Rig {
    dir: tempfile::TempDir,
    store: Arc<Mutex<Store>>,
    host: ExtensionToolHost,
}
fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(Store::open(&dir.path().join("data")).unwrap()));
    let host = ExtensionToolHost::new(ExtensionHost::new(store.clone(), dir.path().into()));
    Rig { dir, store, host }
}
fn workspace() -> a206_extensions::Manifest {
    a206_extensions::builtin("agent206.workspace")
        .unwrap()
        .unwrap()
        .manifest
}
impl Rig {
    fn grant(&self, capabilities: &[Capability]) {
        let mut store = self.store.lock().unwrap();
        let plugin = store
            .state
            .plugins
            .iter_mut()
            .find(|p| p.id == workspace().id)
            .unwrap();
        plugin.granted = capabilities.iter().copied().collect();
    }
    fn enable(&self, enabled: bool) {
        let mut store = self.store.lock().unwrap();
        for plugin in &mut store.state.plugins {
            plugin.enabled = enabled;
        }
    }
    fn scope(&self, capabilities: &[Capability]) -> ToolScope {
        ToolScope {
            thread: Id::new_v4(),
            workspace: Some(self.dir.path().into()),
            capabilities: capabilities.iter().copied().collect(),
        }
    }
    async fn list(&self) -> ToolListing {
        self.host.list(&self.scope(&[])).await
    }
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        capabilities: &[Capability],
    ) -> Result<ToolOutput, ErrorReport> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        let request = ToolRequest {
            call: nonce(),
            name: name.into(),
            arguments,
            approved: false,
        };
        self.host.call(&self.scope(capabilities), request).await
    }
}
fn names(listing: &ToolListing) -> Vec<&str> {
    listing.tools.iter().map(|t| t.name.as_str()).collect()
}
/// The sorted names of `manifest`'s tools that `granted` covers.
fn granted_names(
    manifest: &a206_extensions::Manifest,
    granted: &BTreeSet<Capability>,
) -> Vec<String> {
    let mut names: Vec<String> = manifest
        .tools
        .iter()
        .filter(|t| t.capabilities.is_subset(granted))
        .map(|t| format!("ext__agent206_workspace__{}", t.name))
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn granted_tools_are_listed_with_namespaced_names_schemas_and_capabilities() {
    let rig = rig();
    assert!(
        rig.list().await.tools.is_empty(),
        "nothing before the user grants the extension"
    );
    rig.grant(&[Capability::ReadWorkspace]);
    let listing = rig.list().await;
    assert!(listing.unavailable.is_empty(), "{:?}", listing.unavailable);
    let manifest = workspace();
    let read = [Capability::ReadWorkspace].into();
    assert_eq!(names(&listing), granted_names(&manifest, &read));
    // Tools needing more than the user granted stay unlisted until they grant it.
    let all: BTreeSet<Capability> = manifest
        .tools
        .iter()
        .flat_map(|t| t.capabilities.iter().copied())
        .collect();
    if all != read {
        rig.grant(&all.iter().copied().collect::<Vec<_>>());
        let wider = rig.list().await;
        assert_eq!(names(&wider), granted_names(&manifest, &all));
        assert!(wider.tools.len() > listing.tools.len());
    }
    for spec in &listing.tools {
        assert!(tools::valid_name(&spec.name), "{}", spec.name);
        let declared = manifest
            .tools
            .iter()
            .find(|t| spec.name.ends_with(&format!("__{}", t.name)))
            .unwrap();
        assert_eq!(spec.description, declared.description);
        assert_eq!(spec.capabilities, declared.capabilities);
        assert_eq!(spec.permission, ToolPermission::Allow);
        assert_eq!(spec.parameters["type"], "object");
        assert_eq!(spec.parameters["additionalProperties"], false);
        for param in &declared.params {
            let property = &spec.parameters["properties"][&param.name];
            assert_eq!(property["type"], json_type(param.kind));
            assert_eq!(property["description"], param.description.as_str());
            let required = spec.parameters["required"]
                .as_array()
                .is_some_and(|r| r.iter().any(|n| n == param.name.as_str()));
            assert_eq!(required, param.required, "{}", param.name);
        }
    }
}

#[tokio::test]
async fn disabled_or_ungranted_extensions_are_never_offered_or_reached() {
    let rig = rig();
    let name = tool_name(&workspace().id, &workspace().tools[0].name);
    let path = || json!({"path": "."});
    // Enabled but not granted: not listed, and a call by name finds nothing.
    let error = rig
        .call(&name, path(), &[Capability::ReadWorkspace])
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotFound);
    rig.grant(&[Capability::ReadWorkspace]);
    rig.enable(false);
    assert!(rig.list().await.tools.is_empty());
    let error = rig
        .call(&name, path(), &[Capability::ReadWorkspace])
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotFound);
    // Listing and calling never enable anything.
    assert!(
        rig.store
            .lock()
            .unwrap()
            .state
            .plugins
            .iter()
            .all(|p| !p.enabled)
    );
}

#[tokio::test]
async fn the_conversation_ceiling_bounds_the_extension_grant() {
    let rig = rig();
    rig.grant(&[Capability::ReadWorkspace]);
    let name = tool_name(&workspace().id, &workspace().tools[0].name);
    // The user granted the extension `read_workspace`, but this conversation
    // was not: the host refuses before any guest code runs.
    let error = rig
        .call(&name, json!({"path": "."}), &[Capability::ReadHarness])
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::PermissionDenied);
    assert!(
        error.detail().contains("does not allow"),
        "{}",
        error.detail()
    );
}

#[tokio::test]
async fn arguments_that_do_not_match_the_declaration_are_refused_before_the_guest() {
    let rig = rig();
    rig.grant(&[Capability::ReadWorkspace]);
    let manifest = workspace();
    let tool = manifest
        .tools
        .iter()
        .find(|t| t.params.iter().any(|p| p.required))
        .unwrap();
    let name = tool_name(&manifest.id, &tool.name);
    let required = tool.params.iter().find(|p| p.required).unwrap();
    let all = [Capability::ReadWorkspace];
    for (arguments, expected) in [
        (
            json!({}),
            format!("parameter `{}` is required", required.name),
        ),
        (
            json!({required.name.clone(): 1}),
            format!("parameter `{}` must be", required.name),
        ),
        (json!({nonce(): "x"}), "tool has no parameter".into()),
        (
            json!({required.name.clone(): u64::MAX}),
            "outside the range".into(),
        ),
    ] {
        let error = rig.call(&name, arguments.clone(), &all).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidRequest, "{arguments}");
        assert!(error.detail().contains(&expected), "{}", error.detail());
    }
}

#[test]
fn json_arguments_keep_their_kinds() {
    let text = nonce();
    let Value::Object(object) = json!({
        "s": text, "i": -3, "f": 2.0, "b": true, "n": null,
        "l": [1, "x"], "m": {"k": [false]},
    }) else {
        unreachable!()
    };
    let args = arguments(object).unwrap();
    use ext::Value as V;
    assert_eq!(args["s"], V::Str(text));
    assert_eq!(args["i"], V::Int(-3));
    assert_eq!(args["f"], V::Float(2.0));
    assert_eq!(args["b"], V::Bool(true));
    assert_eq!(args["n"], V::Nil);
    assert_eq!(args["l"], V::List(vec![V::Int(1), V::Str("x".into())]));
    assert_eq!(
        args["m"],
        V::Map([("k".into(), V::List(vec![V::Bool(false)]))].into())
    );
    let Value::Object(nested) = json!({"m": {"k": [u64::MAX]}}) else {
        unreachable!()
    };
    assert!(matches!(
        arguments(nested),
        Err(ExtensionToolError::Number(p)) if p == "m"
    ));
}

fn spec(name: &str) -> ext::ToolSpec {
    ext::ToolSpec {
        name: name.into(),
        description: nonce(),
        capabilities: BTreeSet::new(),
        params: vec![ParamSpec {
            name: "p".into(),
            kind: ParamKind::Integer,
            description: nonce(),
            required: false,
        }],
    }
}
fn source(extension: &str, tools: &[&str]) -> ExtensionTools {
    ExtensionTools {
        extension: extension.into(),
        tools: Ok(tools.iter().map(|t| spec(t)).collect()),
    }
}

#[test]
fn shared_or_overlong_names_are_refused_not_guessed() {
    let base = format!("x{}", &nonce()[..8]);
    let dotted = format!("{base}.a");
    let underscored = format!("{base}_a");
    let distinct = format!("{base}.b");
    let long = format!("{base}.{}", "l".repeat(tools::MAX_NAME));
    let broken = format!("{base}.c");
    let offers = offers(Ok(vec![
        source(&dotted, &["t", "own"]),
        source(&underscored, &["t"]),
        source(&distinct, &["t"]),
        source(&long, &["t"]),
        ExtensionTools {
            extension: broken.clone(),
            tools: Err(ExtensionError::NotInstalled(broken.clone())),
        },
    ]));
    let offered: Vec<_> = offers.tools.keys().cloned().collect();
    assert_eq!(
        offered,
        vec![tool_name(&dotted, "own"), tool_name(&distinct, "t"),]
    );
    // Both claimants of the shared name are reported, by their own source.
    let reported = |source: &str| {
        offers
            .unavailable
            .iter()
            .filter(|u| u.source == source)
            .map(|u| u.error.kind)
            .collect::<Vec<_>>()
    };
    assert_eq!(reported(&dotted), vec![ErrorKind::Conflict]);
    assert_eq!(reported(&underscored), vec![ErrorKind::Conflict]);
    assert_eq!(reported(&long), vec![ErrorKind::InvalidRequest]);
    assert_eq!(reported(&broken), vec![ErrorKind::NotFound]);
    assert_eq!(offers.unavailable.len(), 4);
    // The order sources arrive in does not change the outcome.
    let reversed = super::offers(Ok(vec![
        source(&distinct, &["t"]),
        source(&underscored, &["t"]),
        source(&dotted, &["own", "t"]),
    ]));
    assert_eq!(reversed.tools.keys().cloned().collect::<Vec<_>>(), offered);
}

#[tokio::test]
async fn an_enabled_extension_that_cannot_load_is_reported_beside_the_others() {
    let rig = rig();
    rig.grant(&[Capability::ReadWorkspace]);
    let missing = format!("x{}.missing", nonce());
    {
        let mut store = rig.store.lock().unwrap();
        let mut plugin = workspace().summary(true, &BTreeSet::new());
        plugin.id = missing.clone();
        store.state.plugins.push(plugin);
    }
    let listing = rig.list().await;
    assert_eq!(
        listing.tools.len(),
        granted_names(&workspace(), &[Capability::ReadWorkspace].into()).len()
    );
    assert_eq!(listing.unavailable.len(), 1);
    let report = &listing.unavailable[0];
    assert_eq!(report.source, missing);
    assert_eq!(report.error.kind, ErrorKind::NotFound);
    // The cause chain survives: context first, the host's reason beneath.
    assert!(report.error.source.is_some());
}
