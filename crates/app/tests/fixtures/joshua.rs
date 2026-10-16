//! Standalone Rust mock, compiled into the isolated test profile rather than shipped.
#![forbid(unsafe_code)]
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    time::{Duration, SystemTime},
};
fn main() -> std::io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.get(1).map(String::as_str), Some("serve"));
    let argument = |flag| {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
            .expect("required Joshua argument")
    };
    assert!(Path::new(argument("--model")).is_file());
    let listener = TcpListener::bind(argument("--addr"))?;
    listener.set_nonblocking(true)?;
    let lifetime = std::env::var_os("A206_SMOKE_LIFETIME").expect("test lifetime marker");
    // Generate all model facts inside the independent API process.
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let model = format!("{}-{seed}", std::process::id());
    let context = seed % 100_000 + 1;
    while Path::new(&lifetime).exists() {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => return Err(error),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut request = String::new();
        BufReader::new(&stream).take(8193).read_line(&mut request)?;
        let (status, body) = if request.starts_with("GET /v1/models HTTP/") {
            (
                "200 OK",
                format!("{{\"data\":[{{\"id\":\"{model}\",\"context_length\":{context}}}]}}"),
            )
        } else {
            ("404 Not Found", "{}".into())
        };
        let _result = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
    Ok(())
}
