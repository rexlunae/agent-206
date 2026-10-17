use super::*;
use crate::{
    delegation_tool::{DelegationHost, WAIT},
    tools::{ToolRequest, ToolScope},
};
use serde_json::{Value, json};
use std::time::Duration;

async fn thread(g: &Gateway) -> Id {
    let reply = g
        .request(Request {
            id: Id::new_v4(),
            body: Command::CreateThread {
                title: Id::new_v4().simple().to_string(),
                mode: Mode::Chat,
            },
        })
        .await
        .body;
    let Reply::Thread(thread) = reply else {
        panic!("thread created: {reply:?}");
    };
    thread.id
}
/// A parent conversation and one subagent of it.
async fn family(g: &Gateway) -> (Id, Id) {
    let (parent, child) = (thread(g).await, thread(g).await);
    g.lock().unwrap().thread_mut(child).unwrap().parent = Some(parent);
    (parent, child)
}
/// Pretend `thread` has a run in progress, as `Send` records one.
fn running(g: &Gateway, thread: Id) -> CancellationToken {
    let token = CancellationToken::new();
    g.running.lock().unwrap().insert(thread, token.clone());
    token
}

#[tokio::test]
async fn a_subagent_run_reserved_before_it_starts_is_cancelled_with_its_parent() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let (parent, child) = family(&g).await;
    let delegator = g.delegator();

    // Only a running conversation can reserve a run for its subagent.
    let refused = delegator.reserve(parent, child).unwrap_err();
    assert_eq!(refused.kind, ErrorKind::Conflict);

    let parent_run = running(&g, parent);
    delegator.reserve(parent, child).unwrap();
    cancel_run(&g, parent).unwrap();
    assert!(parent_run.is_cancelled());
    // The run is admitted only after its parent was cancelled, and starts
    // cancelled.
    assert!(take_reserved(&g, child).unwrap().is_cancelled());
    // A thread nobody reserved gets a fresh token.
    assert!(!take_reserved(&g, child).unwrap().is_cancelled());

    // Reserving after the parent's cancellation is cancelled as well.
    delegator.reserve(parent, child).unwrap();
    assert!(take_reserved(&g, child).unwrap().is_cancelled());

    // A reserved run can be cancelled on its own, and released.
    let parent = thread(&g).await;
    g.lock().unwrap().thread_mut(child).unwrap().parent = Some(parent);
    let parent_run = running(&g, parent);
    delegator.reserve(parent, child).unwrap();
    cancel_run(&g, child).unwrap();
    assert!(!parent_run.is_cancelled());
    assert!(take_reserved(&g, child).unwrap().is_cancelled());
    delegator.reserve(parent, child).unwrap();
    delegator.release(child);
    assert!(!take_reserved(&g, child).unwrap().is_cancelled());
    assert!(matches!(
        cancel_run(&g, child),
        Err(GatewayError::NotFound(_))
    ));
}

/// The subagent tools `g` offers and the host list they run subagents with.
fn bound(g: &Gateway) -> (Arc<dyn ToolHost>, Option<Arc<HostList>>) {
    let host = g.delegation.host.clone().expect("subagent tools are bound");
    let hosts = g.delegation.link.read().unwrap().upgrade();
    (host, hosts)
}
fn offers(g: &Gateway, host: &Arc<dyn ToolHost>) -> bool {
    g.tool_hosts
        .iter()
        .any(|h| std::ptr::addr_eq(Arc::as_ptr(h), Arc::as_ptr(host)))
}

#[test]
fn a_clone_given_another_host_leaves_the_originals_subagent_tools_bound() {
    let state = tempfile::tempdir().unwrap();
    let original = Gateway::open(state.path()).unwrap();
    let (host, hosts) = bound(&original);
    assert!(Arc::ptr_eq(&hosts.unwrap(), &original.tool_hosts));
    assert!(offers(&original, &host));

    let extended = original
        .clone()
        .with_tool_host(Arc::new(crate::exec_tool::ExecHost::new(state.path())));
    let (other, hosts) = bound(&extended);
    assert!(Arc::ptr_eq(&hosts.unwrap(), &extended.tool_hosts));
    assert!(offers(&extended, &other) && !offers(&extended, &host));
    assert_eq!(extended.tool_hosts.len(), original.tool_hosts.len() + 1);

    drop(extended);
    let (same, hosts) = bound(&original);
    assert!(Arc::ptr_eq(&same, &host));
    assert!(
        hosts.is_some_and(|h| Arc::ptr_eq(&h, &original.tool_hosts)),
        "the original's subagent tools lost their hosts"
    );
}

async fn wait(host: &DelegationHost, caller: Id, child: Id, seconds: u64) -> Value {
    let output = host
        .call(
            &ToolScope {
                thread: caller,
                workspace: None,
                capabilities: Default::default(),
            },
            ToolRequest {
                call: Id::new_v4().to_string(),
                name: WAIT.into(),
                arguments: json!({"thread": child, "seconds": seconds})
                    .as_object()
                    .unwrap()
                    .clone(),
                approved: false,
            },
        )
        .await
        .unwrap();
    serde_json::from_str(&output.text).unwrap()
}

#[tokio::test(start_paused = true)]
async fn waiting_follows_a_run_the_subagent_tools_did_not_start() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let (parent, child) = family(&g).await;
    let host = DelegationHost::new(g.delegator());
    // A run the user started in the subagent's thread.
    g.lock().unwrap().thread_mut(child).unwrap().state = RunState::Running;

    let started = tokio::time::Instant::now();
    let status = wait(&host, parent, child, 2).await;
    assert_eq!(status["state"], "running");
    assert_eq!(status["waited_seconds"], 2);
    assert!(started.elapsed() >= Duration::from_secs(2));

    let finisher = g.clone();
    let finish = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        finisher.lock().unwrap().thread_mut(child).unwrap().state = RunState::Complete;
    });
    let started = tokio::time::Instant::now();
    let status = wait(&host, parent, child, 60).await;
    finish.await.unwrap();
    assert_eq!(status["state"], "complete");
    assert!(status.get("waited_seconds").is_none());
    assert!(started.elapsed() < Duration::from_secs(4));
}
