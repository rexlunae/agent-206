//! The actual import reader and OS sandbox, re-running this test executable.
#![forbid(unsafe_code)]
use std::process::ExitCode;

fn main() -> ExitCode {
    if let Some(result) = a206_sandbox::dispatch() {
        return result.unwrap();
    }
    if let Some(result) = a206_runtime::imports::dispatch() {
        return result.unwrap();
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if std::env::args().nth(1).as_deref() == Some("probe") {
            return probe();
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(previews_keep_source_immutable_and_redact_failures());
        runtime.block_on(source_is_the_only_user_directory_and_is_read_only());
        runtime.block_on(reviewed_settings_require_unchanged_source_and_target());
        runtime.block_on(selected_settings_and_partial_receipt_failures_are_explicit());
        runtime.block_on(source_metadata_is_removed_or_refused_before_any_reply());
        println!("import reader: ok");
    }
    ExitCode::SUCCESS
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn source_metadata_is_removed_or_refused_before_any_reply() {
    use a206_core::{Command, ErrorKind, Reply, Settings};
    let profile = tempfile::tempdir().unwrap();
    let gateway = a206_runtime::gateway::Gateway::open(profile.path()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let private = uuid::Uuid::new_v4().to_string();
    let config = source.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"[model]
provider = "openai"
model = "{private}"
[[custom_providers]]
id = "{private}"
display_name = "{private}"
api_key_secret = "{private}"
base_url = "https://example.test/v1"
"#
        ),
    )
    .unwrap();
    let review = review(&gateway, source.path()).await;
    assert!(
        review
            .preview
            .providers
            .iter()
            .all(|provider| provider.model_reference.is_none() && provider.credential.is_none())
    );
    assert_eq!(review.preview.providers[1].label, "Imported provider 2");
    let encoded = a206_protocol::encode(&Reply::ImportPreview(Box::new(review))).unwrap();
    assert!(
        !encoded
            .windows(private.len())
            .any(|window| window == private.as_bytes())
    );

    // Credential-shaped text in fields necessary to import must refuse the
    // whole preview, not enter a reply/log or silently alter the destination.
    let key = format!("sk-proj-{}", uuid::Uuid::new_v4().simple());
    std::fs::write(
        &config,
        format!(
            r#"[[custom_providers]]
id = "ordinary"
base_url = "https://example.test/{key}"
"#
        ),
    )
    .unwrap();
    let reply = request(
        &gateway,
        Command::PreviewImport {
            source: source.path().to_owned(),
        },
    )
    .await;
    let Reply::Error(error) = &reply else {
        panic!("sensitive endpoint accepted")
    };
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(error.correlation.is_some());
    assert!(!format!("{reply:?}").contains(&key));
    let encoded = a206_protocol::encode(&reply).unwrap();
    assert!(
        !encoded
            .windows(key.len())
            .any(|window| window == key.as_bytes())
    );

    // The legacy terminal reader also refuses recognizable credential text
    // in ordinary metadata, even though it keeps legitimate source labels.
    std::fs::write(
        &config,
        format!(
            r#"[[custom_providers]]
id = "ordinary"
display_name = "{key}"
base_url = "https://example.test/v1"
"#
        ),
    )
    .unwrap();
    let failure = a206_runtime::imports::preview(source.path(), &Settings::default(), None)
        .await
        .unwrap_err();
    assert!(!format!("{failure:?} {failure:#}").contains(&key));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn request(
    gateway: &a206_runtime::gateway::Gateway,
    command: a206_core::Command,
) -> a206_core::Reply {
    gateway
        .request(a206_protocol::Request {
            id: uuid::Uuid::new_v4(),
            body: command,
        })
        .await
        .body
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn installation() -> tempfile::TempDir {
    let source = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let config = format!(
        r#"[model]
provider = "openai"
[[custom_providers]]
id = "{id}"
display_name = "Imported account"
base_url = "https://{id}.example.test/v1"
[[messengers]]
name = "inbox"
messenger_type = "telegram"
token = "{id}"
allowed_chats = ["123"]
allowed_users = ["456"]
[mcp.servers.untrusted]
command = "untrusted-program"
"#
    );
    std::fs::write(source.path().join("config.toml"), config).unwrap();
    source
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn review(
    gateway: &a206_runtime::gateway::Gateway,
    source: &std::path::Path,
) -> a206_core::imports::Review {
    let a206_core::Reply::ImportPreview(review) = request(
        gateway,
        a206_core::Command::PreviewImport {
            source: source.to_owned(),
        },
    )
    .await
    else {
        panic!("preview failed")
    };
    *review
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn chosen() -> a206_core::imports::Selection {
    a206_core::imports::Selection {
        providers: [1].into(),
        messengers: [0].into(),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn reviewed_settings_require_unchanged_source_and_target() {
    use a206_core::{Command, ErrorKind, Reply};
    let profile = tempfile::tempdir().unwrap();
    let gateway = a206_runtime::gateway::Gateway::open(profile.path()).unwrap();
    let source = installation();
    let before = gateway.snapshot().unwrap().settings;
    let first = review(&gateway, source.path()).await;
    let config = source.path().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}\n# changed since review\n")).unwrap();
    let Reply::Error(error) = request(
        &gateway,
        Command::ApplyImport {
            review: first.id,
            selection: chosen(),
        },
    )
    .await
    else {
        panic!("changed source accepted")
    };
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert_eq!(gateway.snapshot().unwrap().settings, before);

    let second = review(&gateway, source.path()).await;
    assert!(matches!(
        request(&gateway, Command::SaveSettings(Box::new(before.clone()))).await,
        Reply::Ok
    ));
    let Reply::Error(error) = request(
        &gateway,
        Command::ApplyImport {
            review: second.id,
            selection: chosen(),
        },
    )
    .await
    else {
        panic!("changed target accepted")
    };
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert!(gateway.snapshot().unwrap().settings.providers.is_empty());

    let third = review(&gateway, source.path()).await;
    let selection = a206_core::imports::Selection {
        providers: [999].into(),
        messengers: Default::default(),
    };
    let Reply::Error(error) = request(
        &gateway,
        Command::ApplyImport {
            review: third.id,
            selection,
        },
    )
    .await
    else {
        panic!("unknown selection accepted")
    };
    assert_eq!(error.kind, ErrorKind::InvalidRequest);
    assert!(gateway.snapshot().unwrap().settings.providers.is_empty());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn selected_settings_and_partial_receipt_failures_are_explicit() {
    use a206_core::{Command, ErrorKind, Reply};
    for block_receipt in [false, true] {
        let profile = tempfile::tempdir().unwrap();
        let gateway = a206_runtime::gateway::Gateway::open(profile.path()).unwrap();
        let source = installation();
        let reviewed = review(&gateway, source.path()).await;
        if block_receipt {
            std::fs::write(profile.path().join("imports"), "blocked").unwrap();
        }
        let id = uuid::Uuid::new_v4();
        let command = Command::ApplyImport {
            review: reviewed.id,
            selection: chosen(),
        };
        let response = gateway
            .request(a206_protocol::Request {
                id,
                body: command.clone(),
            })
            .await;
        let Reply::ImportApplied(applied) = &response.body else {
            panic!("{:?}", response.body)
        };
        assert_eq!(applied.receipt.providers_added.len(), 1);
        assert_eq!(applied.receipt.messengers_added, vec!["inbox"]);
        assert!(!applied.receipt.vault_imported);
        assert_eq!(applied.receipt_error.is_some(), block_receipt);
        if !block_receipt {
            let receipt = profile
                .path()
                .join("imports")
                .join(format!("{}.toml", reviewed.id));
            assert!(receipt.is_file());
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(receipt).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        assert_eq!(
            gateway
                .request(a206_protocol::Request { id, body: command })
                .await
                .body,
            response.body
        );
        let settings = gateway.snapshot().unwrap().settings;
        assert_eq!(settings.providers.len(), 1);
        assert_eq!(
            settings.providers[0].base_url,
            reviewed.preview.providers[1].base_url
        );
        let messenger = &settings.messengers[0];
        assert!(!messenger.enabled);
        assert!(messenger.credential.is_none());
        assert_eq!(messenger.allowed_chats, ["123".to_owned()].into());
        assert_eq!(messenger.allowed_users, ["456".to_owned()].into());
        assert!(settings.mcp_servers.is_empty());
        assert!(!a206_broker::vault_path(profile.path()).exists());
        let Reply::Error(error) = request(
            &gateway,
            Command::ApplyImport {
                review: reviewed.id,
                selection: chosen(),
            },
        )
        .await
        else {
            panic!("used review accepted")
        };
        assert_eq!(error.kind, ErrorKind::Conflict);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn previews_keep_source_immutable_and_redact_failures() {
    use a206_core::{ErrorKind, Settings};
    use a206_runtime::imports::{ReaderError, preview};
    let source = tempfile::tempdir().unwrap();
    let config = source.path().join("config.toml");
    let secret = uuid::Uuid::new_v4().to_string();
    let text = format!("clawhub_token = {secret:?}\n[model]\nprovider = \"openai\"\n");
    std::fs::write(&config, &text).unwrap();
    let result = preview(source.path(), &Settings::default(), Some(false))
        .await
        .unwrap();
    assert_eq!(result.providers.len(), 1);
    assert!(!toml::to_string(&result).unwrap().contains(&secret));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    assert_eq!(std::fs::read_dir(source.path()).unwrap().count(), 1);

    // Semantic errors can echo values even without a TOML source excerpt.
    std::fs::write(&config, format!("[model]\nprovider = [{secret:?}]\n")).unwrap();
    let failure = preview(source.path(), &Settings::default(), None)
        .await
        .unwrap_err();
    let ReaderError::Failed(report) = &failure else {
        panic!("{failure:?}")
    };
    assert_eq!(report.kind, ErrorKind::InvalidRequest);
    assert!(report.detail().contains("config.toml"));
    assert!(report.causes().next().is_some());
    assert!(!format!("{failure:?} {failure:#} {}", report.detail()).contains(&secret));

    std::fs::write(
        &config,
        "x".repeat(a206_import::MAX_CONFIG_BYTES as usize + 1),
    )
    .unwrap();
    let failure = preview(source.path(), &Settings::default(), None)
        .await
        .unwrap_err();
    assert_eq!(
        a206_runtime::error::report(&failure).kind,
        ErrorKind::InvalidRequest
    );

    let outside = tempfile::tempdir().unwrap();
    let private = outside.path().join("private.toml");
    std::fs::write(&private, format!("clawhub_token = {secret:?}")).unwrap();
    std::fs::remove_file(&config).unwrap();
    std::os::unix::fs::symlink(&private, &config).unwrap();
    let failure = preview(source.path(), &Settings::default(), None)
        .await
        .unwrap_err();
    assert_eq!(
        a206_runtime::error::report(&failure).kind,
        ErrorKind::InvalidRequest
    );
    let linked = outside.path().join("source");
    std::os::unix::fs::symlink(source.path(), &linked).unwrap();
    assert!(matches!(
        preview(&linked, &Settings::default(), None).await,
        Err(ReaderError::Source)
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn source_is_the_only_user_directory_and_is_read_only() {
    let source = tempfile::tempdir().unwrap();
    let source = std::fs::canonicalize(source.path()).unwrap();
    let readable = source.join("source.txt");
    std::fs::write(&readable, "reviewed data").unwrap();
    let outside = tempfile::tempdir().unwrap();
    let private = outside.path().join("private.txt");
    std::fs::write(&private, "private data").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mut child = a206_runtime::sandbox::Confined::spawn_self_reading(Some(&source), |_| {
        vec![
            "probe".into(),
            readable.clone().into_os_string(),
            private.into_os_string(),
            address.into(),
        ]
    })
    .await
    .unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.child.wait())
        .await
        .unwrap()
        .unwrap();
    child.stop().await;
    assert!(status.success(), "sandbox probe failed: {status}");
    assert_eq!(std::fs::read_to_string(readable).unwrap(), "reviewed data");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn probe() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(2).collect();
    let source = std::path::Path::new(&args[0]);
    assert_eq!(std::fs::read_to_string(source).unwrap(), "reviewed data");
    assert!(std::fs::write(source, "changed").is_err());
    assert!(std::fs::read(&args[1]).is_err());
    assert!(
        std::process::Command::new("/usr/bin/true")
            .status()
            .is_err()
    );
    let address = args[2].to_str().unwrap().parse().unwrap();
    assert!(
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(1)).is_err()
    );
    assert!(std::env::var_os("HOME").is_none());
    ExitCode::SUCCESS
}
