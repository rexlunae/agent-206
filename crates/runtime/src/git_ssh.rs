//! git's ssh transport for `git_push` and `git_fetch`: the remote's
//! `git-receive-pack` or `git-upload-pack` runs over OpenSSH, which the
//! gateway starts with no configuration file, strict host key checking
//! against the user's and the profile's known hosts, and one key: the vault
//! key the user chose for the server, offered by a one-use agent
//! ([`crate::ssh_agent`]) whose signatures the vault broker makes. No
//! ambient key, agent, proxy or forwarding is used.
//!
//! The agent needs Unix sockets; elsewhere ssh remotes are recognized and
//! refused.
#![cfg_attr(not(unix), allow(dead_code))]
use crate::git_remote::RemoteError;
#[cfg(unix)]
use crate::ssh_agent::{self, Agent, Signer};
use a206_core::ssh_host::{SSH_PORT, SshHost};
use a206_git_push::Service;
use std::{path::Path, process::Stdio};
use tokio::process::Command;
#[cfg(unix)]
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
    sync::{mpsc, watch},
    task::JoinHandle,
};

/// Longest repository path accepted.
const MAX_PATH: usize = 4096;
/// Most of ssh's own error output kept to explain a failure.
#[cfg(unix)]
const MAX_STDERR: usize = 16 * 1024;
/// How long ssh may take to exit once the conversation is over.
#[cfg(unix)]
const EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// A repository on an ssh server, as a remote's URL names it:
/// `ssh://[user@]host[:port]/path` or `[user@]host:path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshRemote {
    pub user: Option<String>,
    pub host: SshHost,
    /// The path the server's git command is given, as the URL has it.
    pub path: String,
}
impl SshRemote {
    /// `None` when `configured` is not an ssh repository URL.
    pub fn parse(configured: &str) -> Option<Result<Self, RemoteError>> {
        let lower = configured.to_ascii_lowercase();
        for scheme in ["ssh://", "git+ssh://", "ssh+git://"] {
            if lower.starts_with(scheme) {
                return Some(Self::url(&configured[scheme.len()..]));
            }
        }
        if configured.contains("://") {
            return None;
        }
        // git's scp-like syntax: a colon before any slash.
        let colon = configured.find(':')?;
        if configured[..colon].contains('/') {
            return None;
        }
        Some(Self::scp(configured))
    }

    fn url(rest: &str) -> Result<Self, RemoteError> {
        let slash = rest.find('/').ok_or(RemoteError::SshUrl)?;
        let (authority, path) = rest.split_at(slash);
        if path == "/" {
            return Err(RemoteError::SshUrl);
        }
        let (user, address) = match authority.split_once('@') {
            Some((user, address)) => (Some(user), address),
            None => (None, authority),
        };
        let (host, port) = match address.strip_prefix('[') {
            Some(inner) => {
                let (host, after) = inner.split_once(']').ok_or(RemoteError::SshUrl)?;
                match after {
                    "" => (host, None),
                    _ => (
                        host,
                        Some(after.strip_prefix(':').ok_or(RemoteError::SshUrl)?),
                    ),
                }
            }
            None => match address.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (address, None),
            },
        };
        let port = match port {
            None | Some("") => SSH_PORT,
            Some(port) => port.parse().map_err(|_| RemoteError::SshUrl)?,
        };
        // As git does, `/~user/repo` names a path under a home directory.
        let path = match path.strip_prefix('/') {
            Some(home) if home.starts_with('~') => home,
            _ => path,
        };
        Self::new(user, host, port, path)
    }

    fn scp(configured: &str) -> Result<Self, RemoteError> {
        let (user, rest) = match configured.split_once('@') {
            Some((user, rest)) if !user.contains([':', '/', '[']) => (Some(user), rest),
            _ => (None, configured),
        };
        let (host, path) = match rest.strip_prefix('[') {
            Some(inner) => {
                let (host, after) = inner.split_once(']').ok_or(RemoteError::SshUrl)?;
                (host, after.strip_prefix(':').ok_or(RemoteError::SshUrl)?)
            }
            None => rest.split_once(':').ok_or(RemoteError::SshUrl)?,
        };
        Self::new(user, host, SSH_PORT, path)
    }

    fn new(user: Option<&str>, host: &str, port: u16, path: &str) -> Result<Self, RemoteError> {
        if let Some(user) = user {
            check_user(user)?;
        }
        let host = SshHost::new(host, port).map_err(|_| RemoteError::SshUrl)?;
        // A percent sign is refused rather than guessed at: git does not
        // decode it, but a URL's reader might.
        if path.is_empty()
            || path.len() > MAX_PATH
            || path.chars().any(|c| c.is_control() || c == '%')
        {
            return Err(RemoteError::SshUrl);
        }
        Ok(Self {
            user: user.map(str::to_owned),
            host,
            path: path.to_owned(),
        })
    }

    /// The server a key for this repository is bound to.
    pub fn origin(&self) -> String {
        self.host.origin()
    }

    /// The same repository's address on the forge's HTTPS side, by the
    /// convention GitHub, GitLab, Gitea and Forgejo share: the host's
    /// default HTTPS port and the same path. `None` for a path under a home
    /// directory, which a forge does not serve.
    pub fn https(&self) -> Option<url::Url> {
        if self.path.starts_with('~') {
            return None;
        }
        let path = self.path.strip_prefix('/').unwrap_or(&self.path);
        url::Url::parse(&format!("https://{}/{path}", self.host.host)).ok()
    }

    /// The command the server runs, with the path quoted for its shell as
    /// git quotes it.
    pub fn command(&self, service: Service) -> String {
        let mut quoted = String::from("'");
        for c in self.path.chars() {
            match c {
                '\'' | '!' => {
                    quoted.push_str("'\\");
                    quoted.push(c);
                    quoted.push('\'');
                }
                c => quoted.push(c),
            }
        }
        quoted.push('\'');
        format!("{} {quoted}", service.name())
    }
}

/// A login name OpenSSH is given: letters, digits, `.`, `_` and `-`, not
/// starting with `-`.
pub fn check_user(user: &str) -> Result<(), RemoteError> {
    let valid = !user.is_empty()
        && user.len() <= 64
        && !user.starts_with('-')
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if valid {
        Ok(())
    } else {
        Err(RemoteError::SshUser)
    }
}

/// A path as an OpenSSH option value: quoted, and refused when it holds a
/// quote, a newline or a `%` that OpenSSH would expand.
fn option_path(path: &Path) -> Result<String, RemoteError> {
    path.to_str()
        .filter(|p| !p.contains(['"', '\n', '%']))
        .map(|p| format!("\"{p}\""))
        .ok_or_else(|| RemoteError::SshPath(path.to_owned()))
}

/// The OpenSSH command for one conversation with `service` on `remote`, as
/// `user`, trusting host keys in the user's and `known_hosts`, and using
/// only the key in `identity` through the agent at `agent`.
pub fn ssh_command(
    remote: &SshRemote,
    user: &str,
    service: Service,
    known_hosts: &Path,
    identity: &Path,
    agent: &Path,
) -> Result<Command, RemoteError> {
    check_user(user)?;
    let mut command = Command::new("ssh");
    command.args(["-F", "none", "-T", "-x", "-a"]);
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "UpdateHostKeys=no",
        "IdentitiesOnly=yes",
        "PreferredAuthentications=publickey",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "ForwardAgent=no",
        "ClearAllForwardings=yes",
        "PermitLocalCommand=no",
        "ControlMaster=no",
        "ControlPath=none",
        "ConnectTimeout=20",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
        "LogLevel=ERROR",
    ] {
        command.arg("-o").arg(option);
    }
    command.arg("-o").arg(format!(
        "UserKnownHostsFile=~/.ssh/known_hosts {}",
        option_path(known_hosts)?
    ));
    command
        .arg("-o")
        .arg(format!("IdentityFile={}", option_path(identity)?));
    command
        .arg("-o")
        .arg(format!("IdentityAgent={}", option_path(agent)?));
    command
        .arg("-p")
        .arg(remote.host.port.to_string())
        .arg("-l")
        .arg(user)
        .arg("--")
        .arg(
            remote
                .host
                .host
                .trim_start_matches('[')
                .trim_end_matches(']'),
        )
        .arg(remote.command(service));
    for variable in [
        "SSH_AUTH_SOCK",
        "SSH_ASKPASS",
        "SSH_ASKPASS_REQUIRE",
        "DISPLAY",
    ] {
        command.env_remove(variable);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

#[cfg(unix)]
/// The public key `key` (SSH wire format) as an `authorized_keys` line.
fn public_line(key: &[u8]) -> Result<String, RemoteError> {
    use base64::Engine as _;
    let mut rest = key;
    let algorithm = ssh_agent::string(&mut rest)
        .and_then(|a| std::str::from_utf8(a).ok())
        .filter(|a| a.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or(RemoteError::UnexpectedReply)?;
    Ok(format!(
        "{algorithm} {} agent206\n",
        base64::engine::general_purpose::STANDARD.encode(key)
    ))
}

#[cfg(unix)]
/// One conversation with a git service over ssh. Dropping it kills ssh and
/// stops its agent, so a cancelled call leaves nothing running.
pub struct SshSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: JoinHandle<Vec<u8>>,
    agent: JoinHandle<()>,
    failures: mpsc::Receiver<RemoteError>,
    remote: SshRemote,
    _dir: tempfile::TempDir,
}
#[cfg(unix)]
impl Drop for SshSession {
    fn drop(&mut self) {
        // Both tasks only serve this session's ssh, which `kill_on_drop`
        // ends with it.
        self.agent.abort();
        self.stderr.abort();
    }
}
#[cfg(unix)]
impl SshSession {
    /// Start ssh for `service` on `remote` as `user`, offering `key`
    /// (SSH wire format), whose signatures `signer` makes.
    pub async fn open<S: Signer<Error = RemoteError>>(
        remote: &SshRemote,
        user: &str,
        service: Service,
        known_hosts: &Path,
        key: Vec<u8>,
        signer: S,
    ) -> Result<Self, RemoteError> {
        // Owner-only, so no other user can reach the agent's socket.
        let dir = tempfile::Builder::new()
            .prefix("a206-ssh-")
            .tempdir()
            .map_err(RemoteError::SshStart)?;
        let identity = dir.path().join("key.pub");
        std::fs::write(&identity, public_line(&key)?).map_err(RemoteError::SshStart)?;
        let socket = dir.path().join("agent");
        let listener = tokio::net::UnixListener::bind(&socket).map_err(RemoteError::SshStart)?;
        let mut command = ssh_command(remote, user, service, known_hosts, &identity, &socket)?;
        let (client, waiting) = watch::channel(None);
        let (failures, failed) = mpsc::channel(4);
        let agent = tokio::spawn(ssh_agent::serve(
            listener,
            Agent {
                key,
                comment: "agent206".into(),
                signer,
                client: waiting,
                failures,
            },
        ));
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                agent.abort();
                return Err(RemoteError::SshStart(error));
            }
        };
        // Sent once ssh exists and before it can have reached the agent:
        // it connects only after the server's key exchange.
        client.send_replace(child.id());
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            agent.abort();
            return Err(RemoteError::SshStart(std::io::Error::other(
                "ssh's standard streams were not opened",
            )));
        };
        let stderr = tokio::spawn(tail(stderr));
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr,
            agent,
            failures: failed,
            remote: remote.clone(),
            _dir: dir,
        })
    }

    /// The references the service advertises, up to its flush, as packets.
    /// Not cancel-safe: a dropped read loses the session's place.
    pub async fn advertisement(&mut self, limit: usize) -> Result<Vec<u8>, RemoteError> {
        let mut body = vec![];
        loop {
            let mut length = [0; 4];
            if self.stdout.read_exact(&mut length).await.is_err() {
                return Err(self.failure().await);
            }
            body.extend(length);
            let length = std::str::from_utf8(&length)
                .ok()
                .and_then(|l| usize::from_str_radix(l, 16).ok())
                .filter(|&l| l == 0 || l > 4)
                .ok_or(RemoteError::Wire(a206_git_push::WireError::Malformed(
                    "a packet length is not four hex digits",
                )))?;
            if length == 0 {
                return Ok(body);
            }
            if body.len() + length > limit {
                return Err(RemoteError::SshTooLarge(limit));
            }
            let start = body.len();
            body.resize(start + length - 4, 0);
            if self.stdout.read_exact(&mut body[start..]).await.is_err() {
                return Err(self.failure().await);
            }
        }
    }

    /// Send `request` (or a flush, ending the conversation, when there is
    /// none), then read the service's whole answer, at most `limit` bytes,
    /// and wait for ssh to exit.
    pub async fn finish(
        mut self,
        request: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<u8>, RemoteError> {
        let mut stdin = self.stdin.take().ok_or(RemoteError::SshClosed)?;
        let sent = stdin.write_all(request.unwrap_or(b"0000")).await;
        // Closing it tells the server the request is complete.
        drop(stdin);
        if sent.is_err() {
            return Err(self.failure().await);
        }
        let mut body = vec![];
        let read = (&mut self.stdout)
            .take(limit as u64 + 1)
            .read_to_end(&mut body)
            .await;
        let status = self.exit().await;
        match status {
            Some(status) if status.success() && read.is_ok() => {}
            _ => return Err(self.failure_after(status).await),
        }
        // ssh authenticated after all, so a refused signature cost nothing.
        while let Ok(error) = self.failures.try_recv() {
            tracing::warn!(
                error = &error as &dyn std::error::Error,
                "the vault refused a signature ssh asked for"
            );
        }
        if body.len() > limit {
            return Err(RemoteError::SshTooLarge(limit));
        }
        Ok(body)
    }

    /// Why ssh stopped early: a signing failure the agent reported, else
    /// ssh's own account.
    async fn failure(&mut self) -> RemoteError {
        let status = self.exit().await;
        self.failure_after(status).await
    }

    /// Why ssh exited with `status`: a signing failure the agent reported,
    /// else ssh's own account.
    async fn failure_after(&mut self, status: Option<std::process::ExitStatus>) -> RemoteError {
        match self.failures.try_recv() {
            Ok(error) => error,
            Err(_) => self.explain(status).await,
        }
    }

    async fn exit(&mut self) -> Option<std::process::ExitStatus> {
        match tokio::time::timeout(EXIT_WAIT, self.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(error)) => {
                tracing::warn!(
                    error = &error as &dyn std::error::Error,
                    "could not wait for ssh"
                );
                None
            }
            Err(_) => {
                if let Err(error) = self.child.kill().await {
                    tracing::warn!(
                        error = &error as &dyn std::error::Error,
                        "could not stop ssh"
                    );
                }
                None
            }
        }
    }

    async fn explain(&mut self, status: Option<std::process::ExitStatus>) -> RemoteError {
        // The reader is aborted only when the session drops, so a failed
        // join is a panic there; ssh's exit status still explains the stop.
        let stderr: Vec<u8> = (&mut self.stderr).await.unwrap_or_default();
        explain(&self.remote, status.and_then(|s| s.code()), &stderr)
    }
}

#[cfg(unix)]
/// The end of a stream, at most [`MAX_STDERR`] bytes of it.
async fn tail(mut stream: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut kept = vec![];
    let mut buffer = [0; 4096];
    while let Ok(read) = stream.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        kept.extend_from_slice(&buffer[..read]);
        if kept.len() > MAX_STDERR {
            kept.drain(..kept.len() - MAX_STDERR);
        }
    }
    kept
}

/// The failure ssh's exit `code` and error output describe.
fn explain(remote: &SshRemote, code: Option<i32>, stderr: &[u8]) -> RemoteError {
    let text = String::from_utf8_lossy(stderr);
    let host = remote.host.origin();
    if text.contains("Host key verification failed")
        || text.contains("HOST IDENTIFICATION HAS CHANGED")
    {
        return RemoteError::SshHostKey { host };
    }
    if text.contains("Permission denied") {
        return RemoteError::SshDenied { host };
    }
    if text.contains("does not appear to be a git repository")
        || text.contains("Repository not found")
        || text.contains("not found")
    {
        return RemoteError::NoRepository;
    }
    let message: String = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(300)
        .collect();
    RemoteError::SshFailed {
        code,
        message: message.trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    #[test]
    fn ssh_urls_and_scp_paths_name_a_server_and_a_path() {
        let host = format!("{}.test", name());
        let parse = |url: &str| {
            SshRemote::parse(url).map(|r| {
                r.map(|r| (r.user.clone(), r.origin(), r.path))
                    .map_err(drop)
            })
        };
        let same = |user: Option<&str>, origin: String, path: &str| {
            Some(Ok((user.map(str::to_owned), origin, path.to_owned())))
        };
        let origin = format!("ssh://{host}:22");
        assert_eq!(
            parse(&format!("ssh://git@{host}/team/repo.git")),
            same(Some("git"), origin.clone(), "/team/repo.git")
        );
        assert_eq!(
            parse(&format!("SSH://{host}:2222/~me/repo")),
            same(None, format!("ssh://{host}:2222"), "~me/repo")
        );
        assert_eq!(
            parse("git+ssh://git@[::1]:2200/r"),
            same(Some("git"), "ssh://[::1]:2200".into(), "/r")
        );
        assert_eq!(
            parse(&format!("git@{host}:team/repo.git")),
            same(Some("git"), origin.clone(), "team/repo.git")
        );
        assert_eq!(
            parse(&format!("{host}:/srv/r@x.git")),
            same(None, origin.clone(), "/srv/r@x.git")
        );
        assert_eq!(
            parse("git@[::1]:r"),
            same(Some("git"), "ssh://[::1]:22".into(), "r")
        );
        // Not ssh at all.
        for other in [
            format!("https://{host}/r.git"),
            "./a:b".into(),
            "/srv/repo.git".into(),
            "repo".into(),
        ] {
            assert_eq!(parse(&other), None, "{other}");
        }
        // Ssh, but not usable.
        for bad in [
            format!("ssh://{host}"),
            format!("ssh://{host}/"),
            "ssh://-oProxyCommand=x/r".into(),
            format!("ssh://-l@{host}/r"),
            format!("ssh://{host}:0/r"),
            format!("ssh://{host}:x/r"),
            format!("ssh://{host}/a%20b"),
            format!("ssh://{host}/a\nb"),
            format!("{host}:"),
            "-oProxyCommand=x:r".into(),
            format!("user;rm@{host}:r"),
        ] {
            assert!(matches!(parse(&bad), Some(Err(_))), "{bad}");
        }
    }

    #[test]
    fn a_forge_repository_has_the_same_path_over_https() {
        let host = format!("{}.test", name());
        let https = |url: &str| SshRemote::parse(url).unwrap().unwrap().https();
        let expected = Some(format!("https://{host}/team/repo.git").parse().unwrap());
        assert_eq!(https(&format!("git@{host}:team/repo.git")), expected);
        assert_eq!(
            https(&format!("ssh://git@{host}:2222/team/repo.git")),
            expected
        );
        assert_eq!(
            https("git@[::1]:r.git"),
            Some("https://[::1]/r.git".parse().unwrap())
        );
        assert_eq!(https(&format!("ssh://{host}/~me/repo")), None);
    }

    #[test]
    fn the_server_command_quotes_the_path_as_git_does() {
        let remote = |path: &str| SshRemote {
            user: None,
            host: SshHost::new("h.test", 22).unwrap(),
            path: path.into(),
        };
        assert_eq!(
            remote("team/repo.git").command(Service::UploadPack),
            "git-upload-pack 'team/repo.git'"
        );
        assert_eq!(
            remote("a'b!c $(x)").command(Service::ReceivePack),
            "git-receive-pack 'a'\\''b'\\!'c $(x)'"
        );
    }

    #[test]
    fn ssh_uses_no_configuration_and_only_the_vault_key() {
        let dir = tempfile::tempdir().unwrap();
        let remote = SshRemote::parse(&format!("ssh://{}.test:2222/r.git", name()))
            .unwrap()
            .unwrap();
        let (known, identity, agent) = (
            dir.path().join("known_hosts"),
            dir.path().join("key.pub"),
            dir.path().join("agent"),
        );
        let user = name();
        let command = ssh_command(
            &remote,
            &user,
            Service::UploadPack,
            &known,
            &identity,
            &agent,
        )
        .unwrap();
        let command = command.as_std();
        assert_eq!(command.get_program(), "ssh");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(&args[..2], ["-F", "none"]);
        let options: Vec<&str> = args
            .windows(2)
            .filter(|pair| pair[0] == "-o")
            .map(|pair| pair[1].as_str())
            .collect();
        for expected in [
            "BatchMode=yes".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
            "IdentitiesOnly=yes".to_owned(),
            "ForwardAgent=no".to_owned(),
            format!("IdentityFile=\"{}\"", identity.display()),
            format!("IdentityAgent=\"{}\"", agent.display()),
            format!(
                "UserKnownHostsFile=~/.ssh/known_hosts \"{}\"",
                known.display()
            ),
        ] {
            assert!(
                options.contains(&expected.as_str()),
                "{expected}: {options:?}"
            );
        }
        let end = args.len();
        assert_eq!(
            &args[end - 6..],
            [
                "2222".to_owned(),
                "-l".to_owned(),
                user,
                "--".to_owned(),
                remote.host.host.clone(),
                "git-upload-pack '/r.git'".to_owned()
            ]
        );
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "SSH_AUTH_SOCK" && value.is_none())
        );
        // Names OpenSSH would read as options or expand are refused.
        assert!(matches!(
            ssh_command(
                &remote,
                "-oX",
                Service::UploadPack,
                &known,
                &identity,
                &agent
            ),
            Err(RemoteError::SshUser)
        ));
        assert!(matches!(
            ssh_command(
                &remote,
                "git",
                Service::UploadPack,
                &dir.path().join("%h"),
                &identity,
                &agent
            ),
            Err(RemoteError::SshPath(_))
        ));
    }

    #[test]
    fn ssh_failures_are_told_apart() {
        let remote = SshRemote::parse("git@h.test:r").unwrap().unwrap();
        let explained = |text: &str| explain(&remote, Some(255), text.as_bytes());
        assert!(matches!(
            explained("Host key verification failed.\r\n"),
            RemoteError::SshHostKey { .. }
        ));
        assert!(matches!(
            explained("git@h.test: Permission denied (publickey).\n"),
            RemoteError::SshDenied { .. }
        ));
        assert!(matches!(
            explained("fatal: 'r' does not appear to be a git repository\n"),
            RemoteError::NoRepository
        ));
        let reason = name();
        match explained(&format!("\n{reason}\u{1b}[0m\n\n")) {
            RemoteError::SshFailed { code, message } => {
                assert_eq!(code, Some(255));
                assert_eq!(message, format!("{reason} [0m"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
