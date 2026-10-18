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
        println!("import reader: ok");
    }
    ExitCode::SUCCESS
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
