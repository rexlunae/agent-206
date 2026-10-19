use super::*;
use crate::gateway::Gateway;
use a206_core::{Catalog, Mode, ModelInfo, ProviderConfig, Thread, ToolBudget, Usage};

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
    let now = number();
    let (fetched, expires) = (now - 1, now + number());
    state.catalogs.push(Catalog {
        provider,
        source: word(),
        fetched_at: fetched,
        expires_at: expires,
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

    let status = describe(&state, id, &permissions, now).unwrap();
    let conversation = &status["conversation"];
    assert_eq!(conversation["id"], json!(id));
    assert_eq!(conversation["mode"], json!(Mode::Code.label()));
    assert_eq!(conversation["parent"]["id"], json!(parent));
    let mut reported = status["model"].clone();
    let catalog = reported["catalog"].take();
    assert_eq!(
        reported,
        json!({
            "connection": label,
            "model": model.model,
            "name": name,
            "context_limit": context,
            // Not reported by the models API: unknown, never zero.
            "output_limit": null,
            "catalog": null,
        })
    );
    assert_eq!(catalog["fetched_at_unix"], json!(fetched));
    assert_eq!(catalog["expires_at_unix"], json!(expires));
    assert_eq!(catalog["stale"], json!(false));
    // Once the catalog expires its figures are marked stale.
    let expired = describe(&state, id, &permissions, expires).unwrap();
    assert_eq!(expired["model"]["catalog"]["stale"], json!(true));
    assert_eq!(expired["model"]["context_limit"], json!(context));
    assert_eq!(
        status["permissions"],
        json!([Capability::ReadWorkspace.label()])
    );
    assert_eq!(status["gateway_time_unix"], json!(now));
    // Only the newest subagents are listed, with how many there are.
    assert_eq!(status["subagents"]["total"], json!(children.len()));
    assert_eq!(
        status["subagents"]["omitted"],
        json!(children.len() - MAX_SUBAGENTS)
    );
    let listed = status["subagents"]["items"].as_array().unwrap();
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
    assert_eq!(unlisted["model"]["catalog"], Value::Null);
}

#[tokio::test]
async fn limits_are_the_smallest_along_the_conversations_ancestry() {
    let (_dir, _gateway, mut state) = state();
    let grandparent = thread(&mut state, None);
    let parent = thread(&mut state, Some(grandparent));
    let id = thread(&mut state, Some(parent));
    let small = || 1 + (number() % 100) as u32;
    let (rounds, calls, tokens) = (small(), small(), number());
    thread_mut(&mut state, id).tool_budget = ToolBudget {
        rounds: rounds + 1,
        tool_calls: calls,
        tokens: None,
    };
    // A parent lowered after the subagent started still bounds it.
    thread_mut(&mut state, parent).tool_budget = ToolBudget {
        rounds,
        tool_calls: calls + 1,
        tokens: Some(tokens + 1),
    };
    thread_mut(&mut state, grandparent).tool_budget = ToolBudget {
        rounds: rounds + 2,
        tool_calls: calls + 2,
        tokens: Some(tokens),
    };
    let status = describe(&state, id, &BTreeSet::new(), number()).unwrap();
    assert_eq!(
        status["limits_per_response"],
        json!({"model_requests": rounds, "tool_calls": calls, "tokens": tokens})
    );
}

#[tokio::test]
async fn a_status_with_long_names_and_lists_stays_within_the_output_limit() {
    let (_dir, _gateway, mut state) = state();
    let provider = Id::new_v4();
    let long = || word().repeat(MAX_TEXT);
    state.settings.providers.push(ProviderConfig {
        id: provider,
        label: long(),
        base_url: format!("https://{}.example", word()),
        credential: None,
        api: Default::default(),
    });
    let id = thread(&mut state, None);
    let models: BTreeSet<_> = (0..200)
        .map(|_| ModelKey {
            provider,
            model: long(),
        })
        .collect();
    thread_mut(&mut state, id).title = long();
    thread_mut(&mut state, id).subagent_models = models.clone();
    for _ in 0..MAX_SUBAGENTS {
        let child = thread(&mut state, Some(id));
        thread_mut(&mut state, child).title = long();
    }
    for _ in 0..200 {
        thread_mut(&mut state, id).usage_total.record(
            Id::new_v4(),
            &Usage {
                input_tokens: None,
                output_tokens: None,
                cost: Some(number() as f64),
            },
        );
    }

    let status = describe(&state, id, &BTreeSet::new(), number()).unwrap();
    assert!(status.to_string().len() <= tools::MAX_OUTPUT);
    let listed = &status["subagent_models"];
    assert_eq!(listed["total"], json!(models.len()));
    let shown = listed["items"].as_array().unwrap().len();
    assert!(shown > 0 && shown < models.len());
    assert_eq!(listed["omitted"], json!(models.len() - shown));
    let title = status["conversation"]["title"].as_str().unwrap();
    assert!(title.len() <= MAX_TEXT && title.ends_with('…'));
    let costs = &status["usage"]["this_conversation"]["costs"];
    assert_eq!(costs["total"], json!(200));
    assert!(costs["omitted"].as_u64().unwrap() > 0);
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
    assert_eq!(own["costs"]["items"][0]["connection"], Value::Null);
    assert_eq!(own["costs"]["items"][0]["reported"], json!(cost));

    let all = &status["usage"]["with_subagents"];
    assert_eq!(all["responses"], json!(2));
    assert_eq!(all["unfinished_requests"], json!(1));
    assert_eq!(all["input_tokens"]["reported"], json!(input + child_input));
    // An unfinished request may have used tokens nobody reported.
    assert_eq!(all["input_tokens"]["exact"], json!(false));
    assert_eq!(all["costs"]["items"][0]["responses_without_it"], json!(1));
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
