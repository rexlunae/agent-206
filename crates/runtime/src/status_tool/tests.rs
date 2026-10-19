use super::*;
use crate::gateway::Gateway;
use a206_core::{Catalog, Mode, ModelInfo, ProviderConfig, Thread, Usage};

fn word() -> String {
    Id::new_v4().simple().to_string()
}
/// A number generated at runtime.
fn number() -> u64 {
    1 + (Id::new_v4().as_u128() % 1_000_000) as u64
}
fn state() -> (tempfile::TempDir, Gateway, Snapshot) {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(dir.path()).unwrap();
    let snapshot = gateway.snapshot().unwrap();
    (dir, gateway, snapshot)
}
fn thread(state: &mut Snapshot, parent: Option<Id>) -> Id {
    let mut thread = Thread::new(word(), Mode::Code);
    thread.parent = parent;
    let id = thread.id;
    state.threads.push(thread);
    id
}
fn thread_mut(state: &mut Snapshot, id: Id) -> &mut Thread {
    state.threads.iter_mut().find(|t| t.id == id).unwrap()
}

#[tokio::test]
async fn a_conversation_reads_its_model_limits_permissions_and_subagents() {
    let (_dir, _gateway, mut state) = state();
    let provider = Id::new_v4();
    let label = word();
    state.settings.providers.push(ProviderConfig {
        id: provider,
        label: label.clone(),
        base_url: format!("https://{}.example", word()),
        credential: None,
        api: Default::default(),
    });
    let model = ModelKey {
        provider,
        model: word(),
    };
    let (name, context) = (word(), number());
    state.catalogs.push(Catalog {
        provider,
        source: word(),
        fetched_at: number(),
        expires_at: number(),
        models: vec![ModelInfo {
            key: model.clone(),
            name: name.clone(),
            context_limit: Some(context),
            output_limit: None,
            input_cost_per_token: None,
            output_cost_per_token: None,
            reasoning: None,
            reasoning_efforts: vec![],
            image_input: None,
        }],
        balance: None,
        balance_currency: None,
        token_balance: None,
    });
    let parent = thread(&mut state, None);
    let id = thread(&mut state, Some(parent));
    thread_mut(&mut state, id).model = Some(model.clone());
    let other = thread(&mut state, None);
    let children: Vec<_> = (0..MAX_SUBAGENTS + 2)
        .map(|_| thread(&mut state, Some(id)))
        .collect();
    thread_mut(&mut state, children[children.len() - 1]).paused = true;
    let permissions = BTreeSet::from([Capability::ReadWorkspace]);
    let now = number();

    let status = describe(&state, id, &permissions, now).unwrap();
    let conversation = &status["conversation"];
    assert_eq!(conversation["id"], json!(id));
    assert_eq!(conversation["mode"], json!(Mode::Code.label()));
    assert_eq!(conversation["parent"]["id"], json!(parent));
    assert_eq!(
        status["model"],
        json!({
            "connection": label,
            "model": model.model,
            "name": name,
            "context_limit": context,
            // Not reported by the models API: unknown, never zero.
            "output_limit": null,
        })
    );
    assert_eq!(
        status["permissions"],
        json!([Capability::ReadWorkspace.label()])
    );
    assert_eq!(status["gateway_time_unix"], json!(now));
    // Only the newest subagents are listed, with how many there are.
    assert_eq!(status["subagents"]["total"], json!(children.len()));
    let listed = status["subagents"]["newest"].as_array().unwrap();
    assert_eq!(listed.len(), MAX_SUBAGENTS);
    let newest = listed.last().unwrap();
    assert_eq!(newest["id"], json!(children[children.len() - 1]));
    assert_eq!(newest["paused"], json!(true));
    assert!(listed.iter().all(|child| child["id"] != json!(children[0])));
    // Another conversation is never described.
    let other_title = &state.threads.iter().find(|t| t.id == other).unwrap().title;
    assert!(!status.to_string().contains(other_title.as_str()));

    // A model the catalog does not list has no limits to report.
    thread_mut(&mut state, id).model = Some(ModelKey {
        provider,
        model: word(),
    });
    let unlisted = describe(&state, id, &permissions, now).unwrap();
    assert_eq!(unlisted["model"]["context_limit"], Value::Null);
    assert_eq!(unlisted["model"]["name"], Value::Null);
}

#[tokio::test]
async fn usage_counts_what_providers_reported_and_marks_what_is_missing() {
    let (_dir, _gateway, mut state) = state();
    let provider = Id::new_v4();
    let id = thread(&mut state, None);
    let child = thread(&mut state, Some(id));
    let (input, output, cost) = (number(), number(), number() as f64);
    thread_mut(&mut state, id).usage_total.record(
        provider,
        &Usage {
            input_tokens: Some(input),
            output_tokens: None,
            cost: Some(cost),
        },
    );
    let child_input = number();
    thread_mut(&mut state, child).usage_total.record(
        provider,
        &Usage {
            input_tokens: Some(child_input),
            output_tokens: Some(output),
            cost: None,
        },
    );
    thread_mut(&mut state, child)
        .usage_total
        .record_incomplete();

    let status = describe(&state, id, &BTreeSet::new(), number()).unwrap();
    let own = &status["usage"]["this_conversation"];
    assert_eq!(own["responses"], json!(1));
    assert_eq!(own["input_tokens"]["reported"], json!(input));
    assert_eq!(own["input_tokens"]["exact"], json!(true));
    assert_eq!(own["output_tokens"]["responses_without_it"], json!(1));
    assert_eq!(own["output_tokens"]["exact"], json!(false));
    // A connection that is no longer configured is named as unknown.
    assert_eq!(own["costs"][0]["connection"], Value::Null);
    assert_eq!(own["costs"][0]["reported"], json!(cost));

    let all = &status["usage"]["with_subagents"];
    assert_eq!(all["responses"], json!(2));
    assert_eq!(all["unfinished_requests"], json!(1));
    assert_eq!(all["input_tokens"]["reported"], json!(input + child_input));
    // An unfinished request may have used tokens nobody reported.
    assert_eq!(all["input_tokens"]["exact"], json!(false));
    assert_eq!(all["costs"][0]["responses_without_it"], json!(1));
}

#[tokio::test]
async fn the_tool_takes_no_arguments_and_needs_its_conversation() {
    let (_dir, gateway, _) = state();
    let host = StatusHost::new(gateway.statuses());
    let scope = |thread: Id| ToolScope {
        thread,
        workspace: None,
        capabilities: BTreeSet::new(),
        programs: vec![],
    };
    let request = |arguments: serde_json::Map<String, Value>| ToolRequest {
        call: word(),
        name: NAME.into(),
        arguments,
        approved: false,
    };
    let missing = host
        .call(&scope(Id::new_v4()), request(Default::default()))
        .await
        .unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound);
    let mut extra = serde_json::Map::new();
    extra.insert(word(), json!(1));
    let refused = host
        .call(&scope(Id::new_v4()), request(extra))
        .await
        .unwrap_err();
    assert_eq!(refused.kind, ErrorKind::InvalidRequest);
}
