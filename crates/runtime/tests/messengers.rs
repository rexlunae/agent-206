//! Gateway handling of messenger settings and connect/disconnect.
use a206_core::*;
use a206_protocol::Request;
use a206_runtime::gateway::Gateway;

async fn run(gateway: &Gateway, command: Command) -> Reply {
    gateway
        .request(Request {
            id: Id::new_v4(),
            body: command,
        })
        .await
        .body
}

fn name() -> String {
    format!("m-{}", Id::new_v4().simple())
}

fn kind(reply: &Reply) -> Option<ErrorKind> {
    match reply {
        Reply::Error(report) => Some(report.kind),
        _ => None,
    }
}

#[tokio::test]
async fn unsafe_entries_are_refused_and_saved_ones_start_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(directory.path()).unwrap();
    let invalid = |change: fn(&mut MessengerConfig)| {
        let mut config = MessengerConfig::new(name(), MessengerKind::Telegram);
        change(&mut config);
        Settings {
            messengers: vec![config],
            ..Settings::default()
        }
    };
    let cases: [fn(&mut MessengerConfig); 4] = [
        |c| c.name = c.name.to_uppercase(),
        |c| c.endpoint = Some(format!("ftp://{}.example", Id::new_v4())),
        |c| c.credential = Some(format!("{}\n", Id::new_v4())),
        |c| {
            c.allowed_chats.insert(String::new());
        },
    ];
    for change in cases {
        let reply = run(&gateway, Command::SaveSettings(invalid(change))).await;
        assert!(kind(&reply).is_some(), "accepted {reply:?}");
    }
    let duplicate = MessengerConfig::new(name(), MessengerKind::Telegram);
    let reply = run(
        &gateway,
        Command::SaveSettings(Settings {
            messengers: vec![duplicate.clone(), duplicate],
            ..Settings::default()
        }),
    )
    .await;
    assert!(kind(&reply).is_some(), "accepted duplicate names");
    assert!(gateway.snapshot().unwrap().settings.messengers.is_empty());

    let config = MessengerConfig::new(name(), MessengerKind::Telegram);
    let settings = Settings {
        messengers: vec![config.clone()],
        ..Settings::default()
    };
    assert_eq!(
        run(&gateway, Command::SaveSettings(settings)).await,
        Reply::Ok
    );
    let snapshot = gateway.snapshot().unwrap();
    assert_eq!(snapshot.settings.messengers, std::slice::from_ref(&config));
    assert_eq!(snapshot.messengers.len(), 1);
    assert_eq!(snapshot.messengers[0].name, config.name);
    assert_eq!(snapshot.messengers[0].state, MessengerState::Stopped);

    // A disabled entry cannot be connected; unknown ones are not found.
    let reply = run(
        &gateway,
        Command::ConnectMessenger {
            name: config.name.clone(),
        },
    )
    .await;
    assert_eq!(kind(&reply), Some(ErrorKind::InvalidRequest));
    let reply = run(&gateway, Command::ConnectMessenger { name: name() }).await;
    assert_eq!(kind(&reply), Some(ErrorKind::NotFound));
    // Disconnecting is always allowed.
    let reply = run(
        &gateway,
        Command::DisconnectMessenger {
            name: config.name.clone(),
        },
    )
    .await;
    assert_eq!(reply, Reply::Ok);

    // Removing the entry removes its status.
    let mut settings = gateway.snapshot().unwrap().settings;
    settings.messengers.clear();
    assert_eq!(
        run(&gateway, Command::SaveSettings(settings)).await,
        Reply::Ok
    );
    assert!(gateway.snapshot().unwrap().messengers.is_empty());
}
