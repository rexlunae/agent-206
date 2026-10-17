//! What the listing reports for each extension and tool. A call through a
//! native run is in `tests/extension_tools.rs`.
use super::*;
use crate::store::Store;
use a206_core::{ExtensionProposal, Id};
use std::sync::Mutex;

/// Stands in for the skill tool host: offers exactly what it is given.
#[derive(Default)]
struct Skills(Mutex<ToolListing>);
impl ToolHost for Skills {
    fn list<'a>(&'a self, _: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move { self.0.lock().unwrap().clone() })
    }
    fn call<'a>(
        &'a self,
        _: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move { Err(crate::error::report(&ListError::UnknownTool(request.name))) })
    }
}

const WORKSPACE: &str = "agent206.workspace";

struct Rig {
    dir: tempfile::TempDir,
    store: Arc<Mutex<Store>>,
    host: ListHost,
    skills: Arc<Skills>,
    thread: Id,
    /// The conversation has a project.
    project: bool,
}
fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(Store::open(&dir.path().join("data")).unwrap()));
    let skills = Arc::new(Skills::default());
    let host = ListHost::new(
        ExtensionHost::new(store.clone(), dir.path().into()),
        skills.clone(),
    );
    Rig {
        dir,
        store,
        host,
        skills,
        thread: Id::new_v4(),
        project: true,
    }
}
impl Rig {
    fn edit(&self, id: &str, change: impl FnOnce(&mut PluginSummary)) {
        let mut store = self.store.lock().unwrap();
        change(store.state.plugins.iter_mut().find(|p| p.id == id).unwrap());
    }
    fn plugin(&self, id: &str) -> PluginSummary {
        let store = self.store.lock().unwrap();
        store
            .state
            .plugins
            .iter()
            .find(|p| p.id == id)
            .unwrap()
            .clone()
    }
    async fn call(
        &self,
        arguments: Value,
        capabilities: &[Capability],
    ) -> Result<Value, ErrorReport> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        let scope = ToolScope {
            thread: self.thread,
            workspace: self.project.then(|| self.dir.path().into()),
            capabilities: capabilities.iter().copied().collect(),
        };
        let request = ToolRequest {
            call: Id::new_v4().to_string(),
            name: NAME.into(),
            arguments,
            approved: false,
        };
        let output = self.host.call(&scope, request).await?;
        assert!(!output.is_error);
        Ok(serde_json::from_str(&output.text).unwrap())
    }
    async fn listed(&self, capabilities: &[Capability]) -> Value {
        self.call(json!({}), capabilities).await.unwrap()
    }
}
fn extension<'a>(listing: &'a Value, id: &str) -> &'a Value {
    listing["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .unwrap()
}
fn tool<'a>(listing: &'a Value, id: &str, name: &str) -> &'a Value {
    extension(listing, id)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tool"] == name)
        .unwrap()
}
fn blocked(tool: &Value) -> Vec<&str> {
    tool["blocked_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect()
}
/// A workspace tool needing exactly `need`.
fn needing(rig: &Rig, need: Capability) -> String {
    rig.plugin(WORKSPACE)
        .tools
        .into_iter()
        .find(|t| t.capabilities == [need].into())
        .unwrap()
        .name
}

#[tokio::test]
async fn the_listing_uses_no_capability_and_checks_its_arguments() {
    let rig = rig();
    // Readable from any conversation the user permits it in.
    assert!(rig.call(json!({}), &[]).await.is_ok());
    let extra = rig.call(json!({"include_state": true}), &[]).await;
    assert_eq!(extra.unwrap_err().kind, ErrorKind::InvalidRequest);
    let number = rig.call(json!({"extension": 1}), &[]).await;
    assert_eq!(number.unwrap_err().kind, ErrorKind::InvalidRequest);
    let missing = format!("test.{}", Id::new_v4().simple());
    let unknown = rig.call(json!({"extension": missing}), &[]).await;
    assert_eq!(unknown.unwrap_err().kind, ErrorKind::NotFound);
    let spec = ListHost::spec();
    assert!(spec.capabilities.is_empty());
    assert!(spec.whole_output);
}

#[tokio::test]
async fn each_tool_gives_its_offered_name_and_what_blocks_it() {
    let rig = rig();
    let (read, write) = (
        needing(&rig, Capability::ReadWorkspace),
        needing(&rig, Capability::WriteWorkspace),
    );
    rig.edit(WORKSPACE, |p| {
        p.enabled = true;
        p.granted = [Capability::ReadWorkspace].into();
    });
    let read_label = Capability::ReadWorkspace.label();
    let write_label = Capability::WriteWorkspace.label();

    // Granted to the extension, but this conversation does not allow it.
    let listing = rig.listed(&[]).await;
    let entry = tool(&listing, WORKSPACE, &read);
    assert_eq!(
        entry["name"],
        crate::extension_tools::tool_name(WORKSPACE, &read)
    );
    assert_eq!(
        blocked(entry),
        [format!("This conversation does not allow: {read_label}.")]
    );
    let entry = tool(&listing, WORKSPACE, &write);
    assert!(entry["name"].is_null());
    assert_eq!(
        blocked(entry),
        [
            format!("The user has not granted this extension: {write_label}."),
            format!("This conversation does not allow: {write_label}."),
        ]
    );
    assert_eq!(
        extension(&listing, WORKSPACE)["permissions_granted"],
        json!([read_label])
    );

    // Allowed here: nothing stands in the way.
    let listing = rig.listed(&[Capability::ReadWorkspace]).await;
    assert!(blocked(tool(&listing, WORKSPACE, &read)).is_empty());

    // Disabled by the user: not callable, and the listing says why.
    rig.edit(WORKSPACE, |p| p.enabled = false);
    let listing = rig.listed(&[Capability::ReadWorkspace]).await;
    let entry = tool(&listing, WORKSPACE, &read);
    assert!(entry["name"].is_null());
    assert_eq!(blocked(entry), ["The user has not enabled this extension."]);
    assert_eq!(extension(&listing, WORKSPACE)["enabled"], false);
}

#[tokio::test]
async fn an_extension_that_cannot_load_reports_its_failure() {
    let rig = rig();
    // Recorded as installed, but its package is missing from the store.
    let id = format!("test.{}", Id::new_v4().simple());
    let mut broken = rig.plugin(WORKSPACE);
    broken.id = id.clone();
    broken.enabled = true;
    broken.granted = broken.capabilities.clone();
    broken.source = PluginSource::Installed {
        digest: "0".repeat(64),
    };
    rig.store.lock().unwrap().state.plugins.push(broken.clone());
    let listing = rig.listed(&[Capability::ReadWorkspace]).await;
    let entry = extension(&listing, &id);
    assert_eq!(entry["source"], "installed by the user");
    assert!(!entry["errors"].as_array().unwrap().is_empty(), "{entry}");
    for tool in entry["tools"].as_array().unwrap() {
        assert!(tool["name"].is_null(), "{tool}");
    }
    // The other extensions are still listed as usual.
    assert!(extension(&listing, WORKSPACE).get("errors").is_none());
}

#[tokio::test]
async fn proposals_say_which_conversation_prepared_them() {
    let rig = rig();
    let proposal = |thread| ExtensionProposal {
        digest: Id::new_v4().simple().to_string(),
        summary: PluginSummary {
            id: format!("test.{}", Id::new_v4().simple()),
            ..rig.plugin(WORKSPACE)
        },
        thread,
        proposed_at: 0,
    };
    let (here, elsewhere) = (proposal(rig.thread), proposal(Id::new_v4()));
    rig.store
        .lock()
        .unwrap()
        .state
        .extension_proposals
        .extend([here.clone(), elsewhere.clone()]);
    let listing = rig.listed(&[]).await;
    let waiting = listing["waiting_for_review"].as_array().unwrap();
    let flag = |p: &ExtensionProposal| {
        waiting
            .iter()
            .find(|w| w["digest"] == p.digest)
            .map(|w| (w["id"].clone(), w["prepared_in_this_conversation"].clone()))
            .unwrap()
    };
    assert_eq!(flag(&here), (json!(here.summary.id), json!(true)));
    assert_eq!(
        flag(&elsewhere),
        (json!(elsewhere.summary.id), json!(false))
    );
    // A proposal is never listed as an installed extension.
    assert!(
        listing["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["id"] != here.summary.id)
    );
    // Listing one extension shows only proposals for that extension.
    let one = rig
        .call(json!({"extension": WORKSPACE}), &[])
        .await
        .unwrap();
    assert!(one["waiting_for_review"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn project_and_skill_requirements_are_reported() {
    let mut rig = rig();
    let read = needing(&rig, Capability::ReadWorkspace);
    rig.edit(WORKSPACE, |p| {
        p.enabled = true;
        p.granted = [Capability::ReadWorkspace].into();
    });
    rig.project = false;
    let listing = rig.listed(&[Capability::ReadWorkspace]).await;
    assert_eq!(
        blocked(tool(&listing, WORKSPACE, &read)),
        ["This conversation has no project."]
    );
    rig.project = true;

    // The skill tools carry the skill host's own verdict, whatever is pinned.
    let skills = rig.plugin(crate::skill_tool::EXTENSION);
    rig.edit(crate::skill_tool::EXTENSION, |p| {
        p.enabled = true;
        p.granted = p.capabilities.clone();
    });
    let all: Vec<Capability> = skills.capabilities.iter().copied().collect();
    let listing = rig.listed(&all).await;
    for spec in &skills.tools {
        let entry = tool(&listing, crate::skill_tool::EXTENSION, &spec.name);
        assert!(entry["name"].is_null(), "{entry}");
        assert_eq!(
            blocked(entry),
            ["The user has no approved skill that is unchanged since approval."]
        );
    }
    // Disabled as well: both obstacles are named, the second as a requirement.
    rig.edit(crate::skill_tool::EXTENSION, |p| p.enabled = false);
    let listing = rig.listed(&all).await;
    let entry = tool(
        &listing,
        crate::skill_tool::EXTENSION,
        &skills.tools[0].name,
    );
    assert_eq!(
        blocked(entry),
        [
            "The user has not enabled this extension.",
            "It also needs a skill the user approved and has not changed since."
        ]
    );
    rig.edit(crate::skill_tool::EXTENSION, |p| p.enabled = true);
    let failure = format!("skills unreadable {}", Id::new_v4().simple());
    rig.skills.0.lock().unwrap().unavailable = vec![UnavailableTools {
        source: crate::skill_tool::SOURCE.into(),
        error: ErrorReport::new(ErrorKind::Storage, failure.clone()),
    }];
    let listing = rig.listed(&all).await;
    let first = &skills.tools[0].name;
    assert_eq!(
        blocked(tool(&listing, crate::skill_tool::EXTENSION, first)),
        [failure]
    );
    *rig.skills.0.lock().unwrap() = ToolListing {
        tools: skills
            .tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: json!({"type": "object"}),
                capabilities: t.capabilities.clone(),
                permission: ToolPermission::Allow,
                whole_output: true,
            })
            .collect(),
        unavailable: vec![],
    };
    let listing = rig.listed(&all).await;
    for spec in &skills.tools {
        let entry = tool(&listing, crate::skill_tool::EXTENSION, &spec.name);
        assert_eq!(entry["name"], spec.name.as_str());
        assert!(blocked(entry).is_empty(), "{entry}");
    }
}

#[tokio::test]
async fn a_large_catalog_fits_one_result_and_names_what_it_left_out() {
    let rig = rig();
    let base = rig.plugin(WORKSPACE);
    // Even shortened, each entry keeps a full short description, so this
    // many cannot fit together.
    let text = "x".repeat(tools::MAX_OUTPUT / 4);
    let ids: Vec<String> = (0..=BUDGET / SHORT_DESCRIPTION)
        .map(|_| format!("test.{}", Id::new_v4().simple()))
        .collect();
    {
        let mut store = rig.store.lock().unwrap();
        for id in &ids {
            let mut plugin = base.clone();
            plugin.id = id.clone();
            plugin.description = text.clone();
            store.state.plugins.push(plugin);
        }
    }
    let scope = ToolScope {
        thread: rig.thread,
        workspace: Some(rig.dir.path().into()),
        capabilities: BTreeSet::new(),
    };
    let request = ToolRequest {
        call: Id::new_v4().to_string(),
        name: NAME.into(),
        arguments: Map::new(),
        approved: false,
    };
    let output = rig.host.call(&scope, request).await.unwrap();
    assert!(
        output.text.len() <= tools::MAX_OUTPUT,
        "{}",
        output.text.len()
    );
    let listing: Value = serde_json::from_str(&output.text).unwrap();
    let listed: Vec<&str> = listing["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    let omitted: Vec<&str> = listing["omitted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();
    assert!(!omitted.is_empty());
    for id in &ids {
        assert!(
            listed.contains(&id.as_str()) != omitted.contains(&id.as_str()),
            "{id} is listed or omitted, exactly once"
        );
    }
    // The full listing leaves tool descriptions out; one extension has them.
    assert!(
        tool(&listing, WORKSPACE, &base.tools[0].name)
            .get("description")
            .is_none()
    );
    let one = rig
        .call(json!({"extension": omitted[0]}), &[])
        .await
        .unwrap();
    assert_eq!(one["extensions"].as_array().unwrap().len(), 1);
    assert_eq!(
        tool(&one, omitted[0], &base.tools[0].name)["description"],
        base.tools[0].description.as_str()
    );

    // An extension whose own description alone is too large is still listed
    // on its own, shortened, with its tools and blockers.
    let huge = format!("test.{}", Id::new_v4().simple());
    {
        let mut plugin = base.clone();
        plugin.id = huge.clone();
        plugin.description = "y".repeat(tools::MAX_OUTPUT);
        rig.store.lock().unwrap().state.plugins.push(plugin);
    }
    let output = rig.call(json!({"extension": huge}), &[]).await.unwrap();
    let entry = extension(&output, &huge);
    assert_eq!(entry["descriptions_shortened"], true);
    assert!(entry["description"].as_str().unwrap().len() < tools::MAX_OUTPUT);
    assert_eq!(entry["tools"].as_array().unwrap().len(), base.tools.len());
}
