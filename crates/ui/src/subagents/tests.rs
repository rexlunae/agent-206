use super::*;
use a206_core::Mode;
use dioxus::history::{History, MemoryHistory};
use std::rc::Rc;

fn thread(parent: Option<Uuid>, state: RunState) -> Thread {
    let mut thread = Thread::new(format!("t-{}", Uuid::new_v4().simple()), Mode::Plan);
    thread.parent = parent;
    thread.state = state;
    thread
}

#[derive(Clone, Debug, PartialEq, Routable)]
enum Fixture {
    #[route("/")]
    Page {},
}

#[component]
fn Page() -> Element {
    let (snapshot, id) = use_context::<(Snapshot, Uuid)>();
    let state = crate::route::tests::loaded_state(snapshot);
    use_context_provider(|| state);
    rsx! { Subagents { thread: id } }
}

fn render(threads: Vec<Thread>, id: Uuid) -> String {
    fn app() -> Element {
        rsx! { Router::<Fixture> {} }
    }
    let mut snapshot = crate::empty_snapshot();
    snapshot.threads = threads;
    let mut dom = VirtualDom::new(app);
    dom.provide_root_context((snapshot, id));
    dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

#[test]
fn the_tree_shows_every_descendant_nested_with_its_state() {
    let root = thread(None, RunState::Idle);
    let child = thread(Some(root.id), RunState::Running);
    let grandchild = thread(Some(child.id), RunState::Failed);
    let sibling = thread(Some(root.id), RunState::Complete);
    let unrelated = thread(None, RunState::Idle);
    let threads = vec![
        root.clone(),
        child.clone(),
        grandchild.clone(),
        sibling.clone(),
        unrelated.clone(),
    ];
    let html = render(threads.clone(), root.id);
    for t in [&child, &grandchild, &sibling] {
        assert!(html.contains(&format!("/conversations/{}", t.id)), "{html}");
    }
    assert!(!html.contains(&unrelated.title) && !html.contains(&root.title));
    // The grandchild sits in a nested list after its parent.
    let nested = html.find("fc-nav__list--nested").expect("nested list");
    assert!(html.find(&child.title).unwrap() < nested);
    assert!(nested < html.find(&grandchild.title).unwrap());
    for label in ["Running", "Failed", "Done"] {
        assert!(html.contains(label), "{label}: {html}");
    }

    // A subagent links back to where it was delegated from.
    let html = render(threads, grandchild.id);
    assert!(html.contains("Delegated from") && html.contains(&child.title));
    assert!(html.contains("No subagents yet."));
    let html = render(vec![grandchild.clone()], grandchild.id);
    assert!(html.contains("a conversation that no longer exists"));
}

#[test]
fn a_cycle_in_parent_links_ends_instead_of_recursing() {
    let mut first = thread(None, RunState::Idle);
    let second = thread(Some(first.id), RunState::Idle);
    first.parent = Some(second.id);
    let html = render(vec![first.clone(), second.clone()], first.id);
    assert_eq!(html.matches(&second.title).count(), 2, "{html}");
}
