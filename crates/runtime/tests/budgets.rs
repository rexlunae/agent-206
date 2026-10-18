//! User budget editing through the same command API used by GUI and CLI.
use a206_core::{Command, ErrorKind, Id, Mode, Reply, ToolBudget};
use a206_protocol::Request;
use a206_runtime::gateway::Gateway;

async fn send(gateway: &Gateway, body: Command) -> Reply {
    gateway
        .request(Request {
            id: Id::new_v4(),
            body,
        })
        .await
        .body
}

#[tokio::test]
async fn stale_budget_edits_are_rejected_without_overwriting_the_saved_limit() {
    let profile = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(profile.path()).unwrap();
    let Reply::Thread(thread) = send(
        &gateway,
        Command::CreateThread {
            title: "Budget review".into(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!("thread created");
    };
    let saved = ToolBudget {
        rounds: 2,
        tool_calls: 0,
        tokens: Some(91),
    };
    assert_eq!(
        send(
            &gateway,
            Command::UpdateToolBudget {
                thread: thread.id,
                budget: saved,
                expected: thread.tool_budget
            }
        )
        .await,
        Reply::Ok
    );
    let Reply::Error(error) = send(
        &gateway,
        Command::UpdateToolBudget {
            thread: thread.id,
            budget: thread.tool_budget,
            expected: thread.tool_budget,
        },
    )
    .await
    else {
        panic!("stale edit must fail");
    };
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert_eq!(gateway.snapshot().unwrap().threads[0].tool_budget, saved);
    drop(gateway);
    let gateway = Gateway::open(profile.path()).unwrap();
    assert_eq!(gateway.snapshot().unwrap().threads[0].tool_budget, saved);
    // Reloading allows an intentional update with the actual saved version.
    assert_eq!(
        send(
            &gateway,
            Command::UpdateToolBudget {
                thread: thread.id,
                budget: thread.tool_budget,
                expected: saved
            }
        )
        .await,
        Reply::Ok
    );
}

#[test]
fn legacy_messagepack_budget_commands_remain_decodable() {
    #[derive(serde::Serialize, serde::Deserialize)]
    enum LegacyCommand {
        SetToolBudget { thread: Id, budget: ToolBudget },
    }
    let thread = Id::new_v4();
    let budget = ToolBudget {
        rounds: 3,
        tool_calls: 0,
        tokens: None,
    };
    let bytes = rmp_serde::to_vec_named(&LegacyCommand::SetToolBudget { thread, budget }).unwrap();
    assert_eq!(
        rmp_serde::from_slice::<Command>(&bytes).unwrap(),
        Command::SetToolBudget { thread, budget }
    );
    // Older gateways refuse the new atomic operation rather than silently
    // ignoring a compare field and accepting a stale update.
    let bytes = rmp_serde::to_vec_named(&Command::UpdateToolBudget {
        thread,
        budget,
        expected: budget,
    })
    .unwrap();
    assert!(rmp_serde::from_slice::<LegacyCommand>(&bytes).is_err());
}
