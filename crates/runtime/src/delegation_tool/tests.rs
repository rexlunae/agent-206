use super::*;
use crate::tools::ToolHost;
use std::sync::atomic::{AtomicBool, Ordering};

fn key(provider: Id, model: &str) -> ModelKey {
    ModelKey {
        provider,
        model: model.into(),
    }
}
fn name() -> String {
    Id::new_v4().simple().to_string()
}
fn caller(model: Option<ModelKey>, subagent_models: &[ModelKey]) -> Caller {
    Caller {
        mode: Mode::Chat,
        model,
        subagent_models: subagent_models.iter().cloned().collect(),
        capabilities: Default::default(),
        ancestors: 0,
    }
}

#[test]
fn models_resolve_only_within_the_permitted_set() {
    let (a, b) = (Id::new_v4(), Id::new_v4());
    let (own, other, shared) = (name(), name(), name());
    let permitted = [
        key(a, &own),
        key(a, &other),
        key(a, &shared),
        key(b, &shared),
    ];
    let with_own = caller(Some(key(a, &own)), &permitted);

    assert_eq!(choose_model(&with_own, None).unwrap(), key(a, &own));
    assert_eq!(
        choose_model(&with_own, Some(other.clone())).unwrap(),
        key(a, &other)
    );
    assert!(matches!(
        choose_model(&with_own, Some(shared.clone())),
        Err(DelegationToolError::AmbiguousModel(_))
    ));
    assert_eq!(
        choose_model(&with_own, Some(format!("{b}/{shared}"))).unwrap(),
        key(b, &shared)
    );
    assert!(matches!(
        choose_model(&with_own, Some(name())),
        Err(DelegationToolError::ModelDenied(_))
    ));
    // Without a permitted own model, the choice must be unambiguous.
    let elsewhere = caller(Some(key(b, &name())), &permitted);
    assert!(matches!(
        choose_model(&elsewhere, None),
        Err(DelegationToolError::ChooseModel)
    ));
    let single = caller(None, &[key(b, &other)]);
    assert_eq!(choose_model(&single, None).unwrap(), key(b, &other));
    assert!(matches!(
        choose_model(&caller(Some(key(a, &own)), &[]), None),
        Err(DelegationToolError::NoModels)
    ));

    let names = model_names(&permitted.iter().cloned().collect());
    assert_eq!(names[&key(a, &own)], own);
    assert_eq!(names[&key(b, &shared)], format!("{b}/{shared}"));
}

#[test]
fn titles_come_from_the_tasks_first_line_within_bounds() {
    let line = name();
    assert_eq!(title_of(&format!("\n  {line}  \nmore")), line);
    let long = "é".repeat(TITLE);
    let title = title_of(&long);
    assert!(title.len() <= TITLE + '…'.len_utf8());
    assert!(title.ends_with('…'));
}

struct Flagged(Arc<AtomicBool>);
impl Drop for Flagged {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl ToolHost for Flagged {
    fn list<'a>(&'a self, _: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async { ToolListing::default() })
    }
    fn call<'a>(
        &'a self,
        _: &'a ToolScope,
        _: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async { Err(ErrorReport::new(ErrorKind::NotFound, "unused")) })
    }
}

#[test]
fn the_subagent_tools_do_not_keep_a_closed_gateway_alive() {
    let state = tempfile::tempdir().unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let gateway = crate::gateway::Gateway::open(state.path())
        .unwrap()
        .with_tool_host(Arc::new(Flagged(dropped.clone())));
    let copy = gateway.clone();
    drop(gateway);
    assert!(!dropped.load(Ordering::SeqCst));
    drop(copy);
    assert!(
        dropped.load(Ordering::SeqCst),
        "tool hosts outlived the gateway"
    );
}
