//! This executable re-run through `sandbox::Confined`, as the extension worker
//! is: the probe runs inside and reports what it could reach.
use a206_runtime::sandbox::Confined;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// First argument that turns this test binary into the probe.
pub const PROBE: &str = "__a206-confined-probe";

/// Inside the sandbox: try what a compromised worker would, then echo stdin.
pub fn probe() {
    let outcome = |result: Result<(), std::io::Error>| match result {
        Ok(()) => "reached".to_owned(),
        Err(error) => format!("denied:{:?}", error.kind()),
    };
    let file = outcome(std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).map(drop));
    let listen = outcome(std::net::TcpListener::bind("127.0.0.1:0").map(drop));
    let exec = outcome(
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("true")
            .status()
            .map(drop),
    );
    let private = outcome(std::fs::write("probe", b"scratch"));
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    println!(
        "file={file} listen={listen} exec={exec} private={private} echo={}",
        line.trim()
    );
}

pub async fn confines_this_executable_without_network_files_or_programs() {
    let mut confined = Confined::spawn_self(|_| vec![PROBE.into()]).await.unwrap();
    let marker = uuid::Uuid::new_v4().to_string();
    let mut stdin = confined.child.stdin.take().unwrap();
    stdin
        .write_all(format!("{marker}\n").as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let mut report = String::new();
    confined
        .child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut report)
        .await
        .unwrap();
    let fields: std::collections::BTreeMap<_, _> = report
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .collect();
    assert!(fields["file"].starts_with("denied"), "{report}");
    assert!(fields["listen"].starts_with("denied"), "{report}");
    assert!(fields["exec"].starts_with("denied"), "{report}");
    assert_eq!(fields["private"], "reached", "{report}");
    assert_eq!(fields["echo"], marker, "{report}");
    assert!(confined.child.wait().await.unwrap().success());
    confined.stop().await;
}
