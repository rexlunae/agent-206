//! IRC sessions relayed for the gateway (`SocketScheme::IrcSasl`).
//!
//! The broker connects to the server (over TLS unless on loopback), signs in
//! to the services account with SASL PLAIN and only then relays lines, so
//! the password and everything said before sign-in stay in the broker. The
//! capabilities it requests are fixed here: `sasl`, and `account-tag`, which
//! names the services account of each message's sender so the gateway's
//! sender lists match accounts rather than nicknames anyone can take.
use super::{Inbox, SocketOut, revoked, write};
use crate::proto::*;
use a206_core::messengers::IrcServer;
use base64::Engine;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::unix::OwnedWriteHalf;
use zeroize::Zeroizing;

/// How long the TCP connection and the TLS handshake may each take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long registration and sign-in may take once connected.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the broker pings the server.
const PING_EVERY: Duration = Duration::from_secs(30);
/// A session that hears nothing for this long (not even a pong) is dead.
const SILENCE_LIMIT: Duration = Duration::from_secs(90);
/// How long one upstream write may take.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest line read from the server: 8191 bytes of message tags and a
/// 512-byte message.
const MAX_READ_LINE: usize = 8191 + 512;
/// Most lines the server may send before sign-in completes.
const MAX_SIGN_IN_LINES: usize = 1024;
/// Most base64 bytes one `AUTHENTICATE` line carries.
const SASL_CHUNK: usize = 400;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Lines read from the server.
struct Lines {
    reader: ReadHalf<Box<dyn Io>>,
    pending: Vec<u8>,
}
impl Lines {
    /// The next line without its line ending, or `None` at the end of the
    /// stream. Cancel-safe: bytes already read stay in `pending`.
    async fn next(&mut self) -> Result<Option<String>, BrokerError> {
        loop {
            if let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
                if end > MAX_READ_LINE {
                    return Err(BrokerError::Upstream(UpstreamFailure::TooLarge));
                }
                let mut line: Vec<u8> = self.pending.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            if self.pending.len() > MAX_READ_LINE {
                return Err(BrokerError::Upstream(UpstreamFailure::TooLarge));
            }
            let mut chunk = [0u8; 4096];
            let read = self.reader.read(&mut chunk).await.map_err(transfer)?;
            if read == 0 {
                return Ok(None);
            }
            self.pending.extend_from_slice(&chunk[..read]);
        }
    }
}

/// Lines written to the server.
struct Writer(WriteHalf<Box<dyn Io>>);
impl Writer {
    /// Send one line, which must not hold a line ending, ended with CR LF.
    async fn line(&mut self, line: &str) -> Result<(), BrokerError> {
        let mut bytes = Zeroizing::new(Vec::with_capacity(line.len() + 2));
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        let sent = tokio::time::timeout(SEND_TIMEOUT, async {
            self.0.write_all(&bytes).await?;
            self.0.flush().await
        })
        .await;
        match sent {
            Ok(sent) => sent.map_err(transfer),
            Err(_) => Err(BrokerError::Upstream(UpstreamFailure::Timeout)),
        }
    }
}

fn transfer(error: std::io::Error) -> BrokerError {
    tracing::warn!(
        error = &error as &dyn std::error::Error,
        "IRC transfer failed"
    );
    BrokerError::Upstream(UpstreamFailure::Transfer)
}

/// A server line's command and parameters; tags and source are skipped.
struct Message<'a> {
    command: &'a str,
    params: Vec<&'a str>,
}
fn parse(line: &str) -> Message<'_> {
    let mut rest = line;
    for marker in ['@', ':'] {
        if rest.starts_with(marker) {
            rest = rest.split_once(' ').map_or("", |(_, rest)| rest);
        }
        rest = rest.trim_start_matches(' ');
    }
    let (command, mut rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let mut params = vec![];
    loop {
        rest = rest.trim_start_matches(' ');
        if rest.is_empty() {
            break;
        }
        if let Some(trailing) = rest.strip_prefix(':') {
            params.push(trailing);
            break;
        }
        let (param, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        params.push(param);
        rest = tail;
    }
    Message { command, params }
}

enum SignIn {
    /// Signed in and registered; the welcome line is relayed first.
    Ready {
        lines: Lines,
        writer: Writer,
        welcome: String,
    },
    Refused(&'static str),
}

/// Serve one IRC session as the rest of the connection, ending with
/// `SocketRefused`, `SocketClosed` or an `Error`. Returns early, without a
/// final frame, when the gateway went away or the broker is stopping.
pub(super) async fn tunnel(
    shared: &super::Shared,
    out: SocketOut,
    login: String,
    id: uuid::Uuid,
    writer: &mut OwnedWriteHalf,
    inbox: &mut Inbox,
) -> std::io::Result<()> {
    let cancel = out.cancel.clone();
    let opened = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(revoked(shared)),
        _ = shared.stop.cancelled() => return Ok(()),
        closed = inbox.closed() => return closed,
        opened = open(&out, &login) => opened,
    };
    drop(out);
    let (mut lines, mut upstream, welcome) = match opened {
        Ok(SignIn::Ready {
            lines,
            writer,
            welcome,
        }) => (lines, writer, welcome),
        Ok(SignIn::Refused(code)) => {
            let refused = Reply::SocketRefused {
                status: 0,
                code: Some(code.to_owned()),
            };
            return write(writer, id, refused).await;
        }
        Err(error) => {
            tracing::info!(
                error = &error as &dyn std::error::Error,
                "vault broker could not open an IRC session"
            );
            return write(writer, id, Reply::Error(error)).await;
        }
    };
    write(writer, id, Reply::SocketOpened).await?;
    write(
        writer,
        id,
        Reply::SocketFrame {
            seq: 0,
            text: welcome,
        },
    )
    .await?;
    let mut seq = 1u64;
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    let mut silent_until = tokio::time::Instant::now() + SILENCE_LIMIT;
    let error = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break revoked(shared),
            _ = shared.stop.cancelled() => return Ok(()),
            request = inbox.next() => {
                // The gateway closed the connection: dropping the stream closes it.
                let Some(request) = request? else { return Ok(()) };
                match request.body {
                    Request::SocketSend { text } if valid_line(&text) => {
                        if let Err(error) = upstream.line(&text).await {
                            break error;
                        }
                    }
                    Request::SocketSend { .. } => {
                        break BrokerError::Invalid(InvalidRequest::IrcLine);
                    }
                    _ => {
                        let busy = BrokerError::Invalid(InvalidRequest::SocketBusy);
                        write(writer, request.id, Reply::Error(busy)).await?;
                    }
                }
            }
            _ = ping.tick() => {
                if let Err(error) = upstream.line("PING :agent206").await {
                    break error;
                }
            }
            () = tokio::time::sleep_until(silent_until) => {
                break BrokerError::Upstream(UpstreamFailure::Timeout);
            }
            line = lines.next() => {
                silent_until = tokio::time::Instant::now() + SILENCE_LIMIT;
                let text = match line {
                    Ok(Some(text)) if text.is_empty() => continue,
                    Ok(Some(text)) => text,
                    Ok(None) => return write(writer, id, Reply::SocketClosed { code: None }).await,
                    Err(error) => break error,
                };
                let frame = Reply::SocketFrame { seq, text };
                // A gateway that stops reading must not keep a revoked session alive.
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Ok(()),
                    written = write(writer, id, frame) => written?,
                }
                seq += 1;
            }
        }
    };
    tracing::info!(
        error = &error as &dyn std::error::Error,
        "vault broker ended an IRC session"
    );
    write(writer, id, Reply::Error(error)).await
}

/// One line the gateway may send: no line ending or NUL, at most
/// [`MAX_IRC_LINE`] bytes.
fn valid_line(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_IRC_LINE
        && !text.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
}

async fn open(out: &SocketOut, login: &str) -> Result<SignIn, BrokerError> {
    let server = IrcServer::parse(out.url.as_str())
        .map_err(|_| BrokerError::Invalid(InvalidRequest::IrcServer))?;
    if out.token.is_empty() || out.token.contains('\0') {
        return Err(BrokerError::Upstream(UpstreamFailure::InvalidPassword));
    }
    let io = connect(&server).await?;
    match tokio::time::timeout(SIGN_IN_TIMEOUT, sign_in(io, login, &out.token)).await {
        Ok(signed) => signed,
        Err(_) => Err(BrokerError::Upstream(UpstreamFailure::Timeout)),
    }
}

/// Connect directly, never through a proxy: over TLS with the bundled web
/// roots, or in plain text on loopback only.
async fn connect(server: &IrcServer) -> Result<Box<dyn Io>, BrokerError> {
    let host = server.host.trim_start_matches('[').trim_end_matches(']');
    let connected = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((host, server.port)),
    )
    .await
    .map_err(|_| BrokerError::Upstream(UpstreamFailure::Timeout))?;
    let tcp = connected.map_err(|error| {
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            "IRC connection failed"
        );
        BrokerError::Upstream(UpstreamFailure::Connect)
    })?;
    if !server.tls {
        return Ok(Box::new(tcp));
    }
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| {
        tracing::warn!(
            error = &error as &dyn std::error::Error,
            "no TLS versions for IRC"
        );
        BrokerError::Upstream(UpstreamFailure::Connect)
    })?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| BrokerError::Invalid(InvalidRequest::IrcServer))?;
    let handshake = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp);
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, handshake)
        .await
        .map_err(|_| BrokerError::Upstream(UpstreamFailure::Timeout))?
        .map_err(|error| {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "IRC TLS handshake failed"
            );
            BrokerError::Upstream(UpstreamFailure::Connect)
        })?;
    Ok(Box::new(tls))
}

/// Negotiate capabilities, register as `login` and sign in with SASL PLAIN.
/// Registration is held open by capability negotiation, so the server
/// cannot welcome the session before sign-in succeeds.
async fn sign_in(io: Box<dyn Io>, login: &str, password: &str) -> Result<SignIn, BrokerError> {
    let (reader, writer) = tokio::io::split(io);
    let mut lines = Lines {
        reader,
        pending: vec![],
    };
    let mut writer = Writer(writer);
    writer.line("CAP LS 302").await?;
    writer.line(&format!("NICK {login}")).await?;
    writer.line(&format!("USER {login} 0 * :Agent 206")).await?;
    let mut offered: Vec<String> = vec![];
    let mut acknowledged: Vec<String> = vec![];
    let mut authenticating = false;
    let mut signed_in = false;
    for _ in 0..MAX_SIGN_IN_LINES {
        let Some(line) = lines.next().await? else {
            return Err(BrokerError::Upstream(UpstreamFailure::Transfer));
        };
        let message = parse(&line);
        match (message.command, message.params.as_slice()) {
            ("PING", [.., token]) => {
                if !valid_line(token) || token.len() > MAX_IRC_LINE - 6 {
                    return Err(BrokerError::Upstream(UpstreamFailure::SocketProtocol));
                }
                writer.line(&format!("PONG :{token}")).await?;
            }
            ("CAP", [_, "LS", rest @ ..]) => {
                let (more, caps) = match rest {
                    ["*", caps] => (true, *caps),
                    [caps] => (false, *caps),
                    _ => return Err(BrokerError::Upstream(UpstreamFailure::SocketProtocol)),
                };
                offered.extend(caps.split(' ').filter(|c| !c.is_empty()).map(str::to_owned));
                if !more {
                    match request(&offered) {
                        Ok(request) => writer.line(&format!("CAP REQ :{request}")).await?,
                        Err(code) => return Ok(SignIn::Refused(code)),
                    }
                }
            }
            // An acknowledgement may span several lines (`*` before the
            // list); sign-in starts once both required capabilities are on.
            ("CAP", [_, "ACK", rest @ ..]) => {
                let caps = match rest {
                    ["*", caps] | [caps] => *caps,
                    _ => return Err(BrokerError::Upstream(UpstreamFailure::SocketProtocol)),
                };
                for cap in caps.split(' ').filter(|c| !c.is_empty()) {
                    match cap.strip_prefix('-') {
                        Some(removed) => acknowledged.retain(|c| c != removed),
                        None => acknowledged.push(cap.to_owned()),
                    }
                }
                let on = |name: &str| acknowledged.iter().any(|c| c == name);
                if !authenticating && on("sasl") && on("account-tag") {
                    authenticating = true;
                    writer.line("AUTHENTICATE PLAIN").await?;
                }
            }
            ("CAP", [_, "NAK", ..]) => return Ok(SignIn::Refused("sasl_unavailable")),
            ("AUTHENTICATE", ["+"]) => send_password(&mut writer, login, password).await?,
            ("903", _) => {
                signed_in = true;
                writer.line("CAP END").await?;
            }
            ("902" | "904" | "905" | "906" | "908", _) => {
                return Ok(SignIn::Refused("sasl_failed"));
            }
            ("433" | "436" | "437", _) => return Ok(SignIn::Refused("nick_in_use")),
            ("432" | "464" | "465" | "ERROR", _) => {
                return Ok(SignIn::Refused("registration_failed"));
            }
            ("001", _) if signed_in => {
                return Ok(SignIn::Ready {
                    lines,
                    writer,
                    welcome: line,
                });
            }
            // Welcomed without signing in: the server ignored the
            // capability negotiation that should have held registration.
            ("001", _) => return Ok(SignIn::Refused("sasl_unavailable")),
            _ => {}
        }
    }
    Err(BrokerError::Upstream(UpstreamFailure::SocketProtocol))
}

/// The capabilities to request from those the server offered, or why it
/// cannot be used.
fn request(offered: &[String]) -> Result<String, &'static str> {
    let has = |name: &str| {
        offered
            .iter()
            .find(|cap| cap.split('=').next() == Some(name))
    };
    // `sasl=` lists mechanisms (version 302); without a list, any is offered.
    let sasl = has("sasl").is_some_and(|cap| {
        cap.split_once('=')
            .is_none_or(|(_, m)| m.split(',').any(|m| m == "PLAIN"))
    });
    if !sasl {
        return Err("sasl_unavailable");
    }
    if has("account-tag").is_none() {
        return Err("account_tag_unavailable");
    }
    let mut request = "sasl account-tag".to_owned();
    if has("message-tags").is_some() {
        request.push_str(" message-tags");
    }
    Ok(request)
}

/// SASL PLAIN with an empty authorization identity, in chunks of at most
/// [`SASL_CHUNK`] bytes; a final `+` marks a payload that filled its last chunk.
async fn send_password(
    writer: &mut Writer,
    login: &str,
    password: &str,
) -> Result<(), BrokerError> {
    let mut plain = Zeroizing::new(String::with_capacity(login.len() + password.len() + 2));
    plain.push('\0');
    plain.push_str(login);
    plain.push('\0');
    plain.push_str(password);
    let encoded =
        Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(plain.as_bytes()));
    for chunk in encoded.as_bytes().chunks(SASL_CHUNK) {
        let mut line = Zeroizing::new(String::from("AUTHENTICATE "));
        // Base64 is ASCII, so every chunk is valid UTF-8.
        line.push_str(&String::from_utf8_lossy(chunk));
        writer.line(&line).await?;
    }
    if encoded.len().is_multiple_of(SASL_CHUNK) {
        writer.line("AUTHENTICATE +").await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[test]
    fn lines_parse_past_tags_and_source() {
        let account = uuid::Uuid::new_v4().simple().to_string();
        let line =
            format!("@account={account};time=x :nick!u@h CAP * LS * :sasl=PLAIN account-tag");
        let message = parse(&line);
        assert_eq!(message.command, "CAP");
        assert_eq!(message.params, ["*", "LS", "*", "sasl=PLAIN account-tag"]);
        let message = parse("PING :token with spaces");
        assert_eq!(
            (message.command, message.params),
            ("PING", vec!["token with spaces"])
        );
        assert_eq!(parse("903").params, Vec::<&str>::new());
    }

    #[test]
    fn sign_in_needs_sasl_plain_and_account_tags() {
        let caps = |caps: &[&str]| caps.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            request(&caps(&["sasl", "account-tag"])),
            Ok("sasl account-tag".to_owned())
        );
        assert_eq!(
            request(&caps(&[
                "sasl=EXTERNAL,PLAIN",
                "account-tag",
                "message-tags"
            ])),
            Ok("sasl account-tag message-tags".to_owned())
        );
        assert_eq!(
            request(&caps(&["sasl=EXTERNAL", "account-tag"])),
            Err("sasl_unavailable")
        );
        assert_eq!(request(&caps(&["account-tag"])), Err("sasl_unavailable"));
        assert_eq!(request(&caps(&["sasl"])), Err("account_tag_unavailable"));
    }

    fn lines(io: tokio::io::DuplexStream) -> Lines {
        let (reader, _) = tokio::io::split(Box::new(io) as Box<dyn Io>);
        Lines {
            reader,
            pending: vec![],
        }
    }

    #[tokio::test]
    async fn oversized_server_lines_are_refused_with_or_without_an_ending() {
        for ending in ["\r\n", ""] {
            let (mut server, client) = tokio::io::duplex(MAX_READ_LINE * 2);
            let mut read = lines(client);
            let long = format!("{}{ending}", "a".repeat(MAX_READ_LINE + 1));
            server.write_all(long.as_bytes()).await.unwrap();
            drop(server);
            assert_eq!(
                read.next().await,
                Err(BrokerError::Upstream(UpstreamFailure::TooLarge)),
                "{ending:?}"
            );
        }
        let (mut server, client) = tokio::io::duplex(1024);
        let mut read = lines(client);
        server.write_all(b"one\r\ntwo\n").await.unwrap();
        drop(server);
        assert_eq!(read.next().await, Ok(Some("one".to_owned())));
        assert_eq!(read.next().await, Ok(Some("two".to_owned())));
        assert_eq!(read.next().await, Ok(None));
    }

    /// Sign-in waits for every required capability, even when the server
    /// acknowledges them on separate lines.
    #[tokio::test]
    async fn sasl_starts_only_once_both_capabilities_are_acknowledged() {
        let (server, client) = tokio::io::duplex(64 * 1024);
        let login = format!("bot{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let password = uuid::Uuid::new_v4().to_string();
        let signing = tokio::spawn({
            let login = login.clone();
            async move { sign_in(Box::new(client), &login, &password).await }
        });
        let (read, mut write) = tokio::io::split(server);
        let mut read = tokio::io::BufReader::new(read).lines();
        let mut next = async || read.next_line().await.unwrap().unwrap();
        assert_eq!(next().await, "CAP LS 302");
        next().await;
        next().await;
        write
            .write_all(b":s CAP * LS :sasl account-tag\r\n")
            .await
            .unwrap();
        assert_eq!(next().await, "CAP REQ :sasl account-tag");
        write.write_all(b":s CAP * ACK * :sasl\r\n").await.unwrap();
        write
            .write_all(b":s CAP * ACK :account-tag\r\n")
            .await
            .unwrap();
        // Nothing was sent between the two acknowledgements.
        assert_eq!(next().await, "AUTHENTICATE PLAIN");
        write.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
        let payload = next().await;
        assert!(payload.starts_with("AUTHENTICATE "), "{payload}");
        assert_ne!(payload, "AUTHENTICATE PLAIN");
        write
            .write_all(format!(":s 903 {login} :ok\r\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(next().await, "CAP END");
        write
            .write_all(format!(":s 001 {login} :Welcome\r\n").as_bytes())
            .await
            .unwrap();
        assert!(matches!(signing.await.unwrap(), Ok(SignIn::Ready { .. })));
    }

    #[test]
    fn the_gateway_sends_single_bounded_lines() {
        assert!(valid_line("PRIVMSG #a :hello"));
        for line in [
            "",
            "a\r\nQUIT",
            "a\nb",
            "a\0b",
            &"a".repeat(MAX_IRC_LINE + 1),
        ] {
            assert!(!valid_line(line), "{line:?}");
        }
    }
}
