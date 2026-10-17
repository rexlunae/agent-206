use super::*;
use crate::gateway::Gateway;
use a206_core::{Command, Mode, Reply};
use a206_protocol::Request;

fn nonce() -> String {
    Id::new_v4().simple().to_string()
}
async fn send(g: &Gateway, body: Command) -> Reply {
    g.request(Request {
        id: Id::new_v4(),
        body,
    })
    .await
    .body
}
async fn thread(g: &Gateway) -> Id {
    let reply = send(
        g,
        Command::CreateThread {
            title: nonce(),
            mode: Mode::Chat,
        },
    )
    .await;
    let Reply::Thread(thread) = reply else {
        panic!("thread created: {reply:?}");
    };
    thread.id
}
fn call(host: &ScheduleHost, thread: Id, name: &str, args: Value) -> Result<Value, ErrorReport> {
    let Value::Object(arguments) = args else {
        panic!("arguments are an object");
    };
    host.run(
        thread,
        &ToolRequest {
            call: nonce(),
            name: name.into(),
            arguments,
            approved: true,
        },
    )
}
fn every() -> Value {
    json!({"name": nonce(), "message": nonce(), "every_seconds": 60 + Id::new_v4().as_u128() as u64 % 100_000})
}
fn ids(listed: &Value) -> Vec<(String, String)> {
    listed["scheduled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| {
            (
                job["id"].as_str().unwrap().to_owned(),
                job["added_by"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_model_reaches_only_its_own_conversations_jobs_and_removes_only_its_own() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let host = ScheduleHost::new(g.jobs());
    let (mine, other) = (thread(&g).await, thread(&g).await);

    let added = call(&host, mine, ADD, every()).unwrap();
    let id = added["id"].as_str().unwrap().to_owned();
    assert_eq!(added["added_by"], "you");
    let theirs = call(&host, other, ADD, every()).unwrap();
    let Reply::Job(users) = send(
        &g,
        Command::ScheduleJob {
            name: nonce(),
            thread: mine,
            text: nonce(),
            schedule: Schedule::Every {
                seconds: 3600,
                anchor: now(),
            },
        },
    )
    .await
    else {
        panic!("user job scheduled");
    };

    let listed = call(&host, mine, LIST, json!({})).unwrap();
    let mut seen = ids(&listed);
    seen.sort();
    let mut expected = vec![
        (id.clone(), "you".to_owned()),
        (users.id.to_string(), "user".to_owned()),
    ];
    expected.sort();
    assert_eq!(seen, expected);
    assert_eq!(listed["you_may_add"], MAX_AGENT_JOBS - 1);

    // The user's job and another conversation's job stay.
    for foreign in [
        users.id.to_string(),
        theirs["id"].as_str().unwrap().to_owned(),
    ] {
        let refused = call(&host, mine, REMOVE, json!({"id": foreign})).unwrap_err();
        assert_eq!(refused.kind, ErrorKind::NotFound);
    }
    assert_eq!(g.snapshot().unwrap().jobs.len(), 3);
    assert_eq!(
        call(&host, mine, REMOVE, json!({"id": id})).unwrap()["removed"],
        id
    );
    let left = ids(&call(&host, mine, LIST, json!({})).unwrap());
    assert_eq!(left, vec![(users.id.to_string(), "user".to_owned())]);
}

#[tokio::test]
async fn a_conversation_keeps_a_bounded_number_of_jobs_its_models_added() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let host = ScheduleHost::new(g.jobs());
    let (mine, other) = (thread(&g).await, thread(&g).await);
    for _ in 0..MAX_AGENT_JOBS {
        call(&host, mine, ADD, every()).unwrap();
    }
    let refused = call(&host, mine, ADD, every()).unwrap_err();
    assert_eq!(refused.kind, ErrorKind::InvalidRequest);
    assert_eq!(
        call(&host, mine, LIST, json!({})).unwrap()["you_may_add"],
        0
    );
    // Other conversations and the user are not limited by it.
    call(&host, other, ADD, every()).unwrap();
    let user = send(
        &g,
        Command::ScheduleJob {
            name: nonce(),
            thread: mine,
            text: nonce(),
            schedule: Schedule::Every {
                seconds: 3600,
                anchor: now(),
            },
        },
    )
    .await;
    assert!(matches!(user, Reply::Job(_)), "{user:?}");
}

#[tokio::test]
async fn schedules_are_parsed_from_exactly_one_form() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let host = ScheduleHost::new(g.jobs());
    let mine = thread(&g).await;
    let base = || json!({"name": nonce(), "message": nonce()});
    let with = |extra: Value| {
        let mut args = base();
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        args
    };
    let future = now() + 1 + Id::new_v4().as_u128() as u64 % 1_000_000;
    let at = jiff::Timestamp::from_second(future as i64).unwrap();
    let added = call(&host, mine, ADD, with(json!({"at": at.to_string()}))).unwrap();
    assert_eq!(added["schedule"]["at"], at.to_string());
    assert_eq!(added["next"], at.to_string());
    let zone = "UTC";
    let added = call(
        &host,
        mine,
        ADD,
        with(json!({"cron": "0 0 * * *", "zone": zone})),
    )
    .unwrap();
    assert_eq!(added["schedule"]["zone"], zone);

    for (bad, kind) in [
        (base(), ErrorKind::InvalidRequest),
        (
            with(json!({"at": at.to_string(), "every_seconds": 3600})),
            ErrorKind::InvalidRequest,
        ),
        (
            with(json!({"every_seconds": 3600, "zone": zone})),
            ErrorKind::InvalidRequest,
        ),
        (with(json!({"at": nonce()})), ErrorKind::InvalidRequest),
        (
            with(json!({"every_seconds": "often"})),
            ErrorKind::InvalidRequest,
        ),
        (
            with(json!({"every_seconds": 3600, "extra": 1})),
            ErrorKind::InvalidRequest,
        ),
        (
            json!({"name": nonce(), "every_seconds": 3600}),
            ErrorKind::InvalidRequest,
        ),
        // The gateway's own checks: a past time and a bad expression.
        (
            with(json!({"at": "1970-01-01T00:00:01Z"})),
            ErrorKind::InvalidRequest,
        ),
        (with(json!({"cron": nonce()})), ErrorKind::InvalidRequest),
    ] {
        let error = call(&host, mine, ADD, bad.clone()).unwrap_err();
        assert_eq!(error.kind, kind, "{bad}: {error:?}");
    }
    assert_eq!(g.snapshot().unwrap().jobs.len(), 2);
    let unknown = call(&host, mine, &nonce(), json!({})).unwrap_err();
    assert_eq!(unknown.kind, ErrorKind::NotFound);
}
