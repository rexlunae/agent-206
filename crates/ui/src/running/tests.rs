use super::*;
use a206_core::{ApprovalRequest, JobAuthor, JobRun, Mode, Schedule, UserPrompt};
use dioxus::history::{History, MemoryHistory};
use std::rc::Rc;

fn text() -> String {
    format!("t-{}", Uuid::new_v4().simple())
}
fn thread(state: RunState, parent: Option<Uuid>) -> Thread {
    let mut thread = Thread::new(text(), Mode::Chat);
    thread.state = state;
    thread.parent = parent;
    thread
}
fn ids(nodes: &[Node]) -> Vec<Uuid> {
    nodes.iter().map(|node| node.thread.id).collect()
}

#[derive(Clone, Debug, PartialEq, Routable)]
enum Fixture {
    #[route("/")]
    Page {},
}

/// Whether the fixture's gateway is connected.
#[derive(Clone, Copy)]
struct Online(bool);

#[component]
fn Page() -> Element {
    let snapshot = use_context::<Snapshot>();
    let Online(online) = use_context::<Online>();
    let mut state = crate::route::tests::loaded_state(snapshot);
    state.connected = use_signal(|| online);
    use_context_provider(|| state);
    rsx! { Running {} }
}

fn render(threads: Vec<Thread>, jobs: Vec<Job>) -> String {
    render_as(Online(true), threads, jobs)
}

fn render_as(online: Online, threads: Vec<Thread>, jobs: Vec<Job>) -> String {
    fn app() -> Element {
        rsx! { Router::<Fixture> {} }
    }
    let mut snapshot = crate::empty_snapshot();
    snapshot.threads = threads;
    snapshot.jobs = jobs;
    let mut dom = VirtualDom::new(app);
    dom.provide_root_context(snapshot);
    dom.provide_root_context(online);
    dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

#[test]
fn running_conversations_group_under_the_running_conversation_that_delegated_them() {
    let root = thread(RunState::Running, None);
    let child = thread(RunState::Running, Some(root.id));
    let grandchild = thread(RunState::Running, Some(child.id));
    let finished = thread(RunState::Complete, Some(root.id));
    let finished_parent = thread(RunState::Complete, None);
    // Its parent is done, so it is shown on its own.
    let orphan = thread(RunState::Running, Some(finished_parent.id));
    let gone = thread(RunState::Running, Some(Uuid::new_v4()));
    // Malformed links that form a cycle are still shown, once each.
    let mut first = thread(RunState::Running, None);
    let second = thread(RunState::Running, Some(first.id));
    first.parent = Some(second.id);
    let mut snapshot = crate::empty_snapshot();
    snapshot.threads = vec![
        grandchild.clone(),
        finished.clone(),
        child.clone(),
        root.clone(),
        finished_parent,
        orphan.clone(),
        gone.clone(),
        first.clone(),
        second.clone(),
    ];
    let trees = running(&snapshot);
    assert_eq!(ids(&trees[..3]), vec![root.id, orphan.id, gone.id]);
    assert_eq!(ids(&trees[0].below), vec![child.id]);
    assert_eq!(ids(&trees[0].below[0].below), vec![grandchild.id]);
    assert_eq!(trees.iter().map(count).sum::<usize>(), 7);
    assert_eq!(trees.len(), 4, "the cycle is one more tree");
}

#[test]
fn the_page_offers_pause_resume_and_stop_only_where_they_apply() {
    let working = thread(RunState::Running, None);
    let mut paused = thread(RunState::Running, None);
    paused.paused = true;
    let mut compacting = thread(RunState::Running, None);
    compacting.compacting = Some(RunState::Complete);
    let mut external = thread(RunState::Running, None);
    external.backend = CodingBackend::Codex;
    for (thread, pause, resume) in [
        (&working, true, false),
        (&paused, false, true),
        (&compacting, false, false),
        (&external, false, false),
    ] {
        assert_eq!(pausable(thread), pause, "{}", thread.title);
        assert_eq!(resumable(thread), resume, "{}", thread.title);
    }
    let html = render(
        vec![
            working.clone(),
            paused.clone(),
            compacting.clone(),
            external.clone(),
        ],
        vec![],
    );
    assert_eq!(html.matches(">Pause<").count(), 1, "{html}");
    assert_eq!(html.matches(">Resume<").count(), 1, "{html}");
    assert_eq!(html.matches(">Stop<").count(), 4, "{html}");
    assert!(html.contains("Pausing after this step"), "{html}");
    assert!(!html.contains("Nothing is running"), "{html}");
}

#[test]
fn subagents_have_their_own_controls_and_an_offline_gateway_offers_none() {
    let root = thread(RunState::Running, None);
    let mut child = thread(RunState::Running, Some(root.id));
    child.paused = true;
    let grandchild = thread(RunState::Running, Some(child.id));
    let threads = vec![root.clone(), child.clone(), grandchild.clone()];
    let html = render(threads.clone(), vec![]);
    assert_eq!(html.matches(">Stop<").count(), 3, "{html}");
    assert_eq!(html.matches(">Pause<").count(), 2, "{html}");
    assert_eq!(html.matches(">Resume<").count(), 1, "{html}");
    assert!(
        html.contains(&format!("Delegated by {}", root.title))
            && html.contains(&format!("Delegated by {}", child.title)),
        "{html}"
    );
    assert!(!html.contains("not connected"), "{html}");

    let offline = render_as(Online(false), threads, vec![]);
    assert!(
        offline.contains("The gateway is not connected"),
        "{offline}"
    );
    assert!(offline.contains(&grandchild.title), "{offline}");
    for control in [">Stop<", ">Pause<", ">Resume<"] {
        assert!(!offline.contains(control), "{offline}");
    }
}

#[test]
fn what_waits_on_the_user_and_scheduled_runs_are_listed() {
    let mut asking = thread(RunState::Running, None);
    asking.approvals = vec![ApprovalRequest {
        id: Uuid::new_v4(),
        message: Uuid::new_v4(),
        call: text(),
        tool: text(),
        arguments: "{}".into(),
        digest: text(),
        capabilities: Default::default(),
        requested_at: 1,
    }];
    asking.prompts = (0..2)
        .map(|_| UserPrompt {
            id: Uuid::new_v4(),
            call: text(),
            question: text(),
            choices: vec![],
            other: true,
            requested_at: 1,
        })
        .collect();
    assert_eq!(asks(&asking), "1 approval and 2 questions");
    let idle = thread(RunState::Complete, None);
    let job = Job {
        kind: a206_core::JobKind::Message,
        id: Uuid::new_v4(),
        name: text(),
        thread: idle.id,
        text: text(),
        schedule: Schedule::At { at: 2 },
        enabled: false,
        created: 1,
        next: None,
        last: Some(JobRun::start(2)),
        author: JobAuthor::User,
        history: 0,
        deliver_to: None,
    };
    let html = render(vec![asking.clone(), idle.clone()], vec![job.clone()]);
    assert!(
        html.contains("Waiting on you") && html.contains("1 approval and 2 questions"),
        "{html}"
    );
    assert!(
        html.contains(&job.name) && html.contains(&format!("In {}", idle.title)),
        "{html}"
    );

    let quiet = render(vec![idle], vec![]);
    assert!(quiet.contains("Nothing is running"), "{quiet}");
    assert!(!quiet.contains("approval and"), "{quiet}");
}
