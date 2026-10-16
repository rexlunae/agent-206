//! First-time gateway setup: which gateway a client uses, and the SSH host keys
//! the user confirmed for it. Clients never connect, or launch a local gateway,
//! before a target is saved here or named explicitly with `--remote`.
//!
//! Failures are typed [`ConnectionError`]s, recorded with `tracing` where they
//! occur, so clients can both branch on them (an untrusted host key opens the
//! fingerprint review) and display them without parsing strings.
use a206_core::{GatewayConnection, GatewayTarget, HostKey};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const FILE: &str = "connection.toml";
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error(
        "no gateway is configured; run `agent206 gateway setup --local` or \
         `agent206 gateway setup --ssh ssh://user@host`, or choose one in the desktop app"
    )]
    NotConfigured,
    #[error("invalid gateway endpoint {endpoint:?}")]
    InvalidEndpoint {
        endpoint: String,
        #[source]
        source: crate::transport::SshEndpointError,
    },
    #[error("invalid gateway setup in {}", path.display())]
    InvalidSetup {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("could not encode gateway setup for {}", path.display())]
    EncodeSetup {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },
    #[error("could not access {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not run {program}; install OpenSSH")]
    MissingTool {
        program: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{operation} for {host} timed out")]
    TimedOut {
        operation: &'static str,
        host: String,
    },
    #[error("the SSH host key for {host} is not trusted yet; review its fingerprint")]
    UnknownHost { host: String },
    #[error(
        "the SSH host key for {host} changed; confirm the new key with the server's \
         administrator before trusting it"
    )]
    HostKeyChanged { host: String },
    #[error("SSH to {host} works, but `agent206` is not installed on its PATH")]
    NotInstalled { host: String },
    #[error("SSH to {host} failed: {detail}")]
    Ssh {
        host: String,
        exit_code: Option<i32>,
        detail: String,
    },
    #[error("{host} offered no SSH host keys")]
    NoHostKeys { host: String },
    #[error(
        "{host} is reached through a proxy, so its host key cannot be scanned here; \
         connect once with `ssh {host}` to verify and record it, then retry"
    )]
    ProxiedHost { host: String },
    #[error("the host key entry does not match its fingerprint")]
    HostKeyMismatch,
    /// A client could not run setup work off its UI thread.
    #[error("gateway setup work stopped before finishing")]
    SetupWorker {
        #[source]
        source: Option<std::io::Error>,
    },
}
impl crate::error::Classified for ConnectionError {
    fn kind(&self) -> Option<a206_core::ErrorKind> {
        use a206_core::ErrorKind as K;
        Some(match self {
            Self::NotConfigured | Self::NotInstalled { .. } => K::Unavailable,
            Self::InvalidEndpoint { .. } | Self::InvalidSetup { .. } => K::InvalidRequest,
            Self::UnknownHost { .. }
            | Self::HostKeyChanged { .. }
            | Self::ProxiedHost { .. }
            | Self::HostKeyMismatch => K::PermissionDenied,
            Self::TimedOut { .. } => K::Timeout,
            Self::Ssh { .. } | Self::NoHostKeys { .. } => K::Unavailable,
            Self::EncodeSetup { .. } => K::Internal,
            Self::Io { .. } | Self::MissingTool { .. } | Self::SetupWorker { .. } => return None,
        })
    }
}
type Result<T, E = ConnectionError> = std::result::Result<T, E>;

/// Record a failure where it happens, keeping the structured error for the caller.
fn traced(error: ConnectionError) -> ConnectionError {
    match &error {
        ConnectionError::NotConfigured | ConnectionError::UnknownHost { .. } => {
            tracing::info!(
                error = &error as &dyn std::error::Error,
                "gateway setup needed"
            );
        }
        _ => tracing::warn!(
            error = &error as &dyn std::error::Error,
            "gateway connection failed"
        ),
    }
    error
}
fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ConnectionError + '_ {
    move |source| {
        traced(ConnectionError::Io {
            path: path.to_owned(),
            source,
        })
    }
}

/// The profile holds the gateway target and trusted host keys: owner-only, like
/// the gateway's own state.
fn private_root(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root).map_err(io(root))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).map_err(io(root))?;
    }
    Ok(())
}
pub fn known_hosts(root: &Path) -> PathBuf {
    root.join("known_hosts")
}
pub fn load(root: &Path) -> Result<Option<GatewayConnection>> {
    let path = root.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io(&path)(error)),
    };
    let connection: GatewayConnection = toml::from_str(&text).map_err(|source| {
        traced(ConnectionError::InvalidSetup {
            path: path.clone(),
            source,
        })
    })?;
    validate(&connection.target)?;
    Ok(Some(connection))
}
pub fn save(root: &Path, connection: &GatewayConnection) -> Result<()> {
    validate(&connection.target)?;
    let path = root.join(FILE);
    let text = toml::to_string_pretty(connection).map_err(|source| {
        traced(ConnectionError::EncodeSetup {
            path: path.clone(),
            source,
        })
    })?;
    private_root(root)?;
    // NamedTempFile is created owner-only (0600) before it is renamed into place.
    let mut file = tempfile::NamedTempFile::new_in(root).map_err(io(root))?;
    file.write_all(text.as_bytes()).map_err(io(&path))?;
    file.as_file().sync_all().map_err(io(&path))?;
    file.persist(&path)
        .map_err(|error| io(&path)(error.error))?;
    Ok(())
}
pub fn validate(target: &GatewayTarget) -> Result<()> {
    if let GatewayTarget::Ssh { endpoint } = target {
        endpoint_of(endpoint)?;
    }
    Ok(())
}
fn endpoint_of(endpoint: &str) -> Result<crate::transport::SshEndpoint> {
    crate::transport::parse_ssh_endpoint(endpoint).map_err(|source| {
        traced(ConnectionError::InvalidEndpoint {
            endpoint: endpoint.to_owned(),
            source,
        })
    })
}
/// An explicit `--remote` wins; otherwise the saved setup. `None` means setup is required.
pub fn resolve(root: &Path, remote: Option<String>) -> Result<Option<GatewayTarget>> {
    if let Some(endpoint) = remote {
        let target = GatewayTarget::Ssh { endpoint };
        validate(&target)?;
        return Ok(Some(target));
    }
    Ok(load(root)?.map(|connection| connection.target))
}
/// Like [`resolve`], but a missing setup is [`ConnectionError::NotConfigured`].
pub fn require(root: &Path, remote: Option<String>) -> Result<GatewayTarget> {
    resolve(root, remote)?.ok_or_else(|| traced(ConnectionError::NotConfigured))
}

/// Connect with strict host checking and return the remote `agent206 --version`.
/// An untrusted host is [`ConnectionError::UnknownHost`]; review it with
/// [`scan_host_keys`] and [`trust_host_keys`].
pub async fn check_ssh(root: &Path, endpoint: &str) -> Result<String> {
    let host = endpoint_of(endpoint)?.host;
    let mut command = crate::transport::gateway_ssh_command(root, endpoint).map_err(|source| {
        traced(ConnectionError::InvalidEndpoint {
            endpoint: endpoint.to_owned(),
            source,
        })
    })?;
    // Fixed command text: OpenSSH executes remote arguments through a shell.
    command
        .arg("agent206 --version")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(PROBE_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            traced(ConnectionError::TimedOut {
                operation: "SSH check",
                host: host.clone(),
            })
        })?
        .map_err(|source| {
            traced(ConnectionError::MissingTool {
                program: "ssh",
                source,
            })
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(traced(
        if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
            ConnectionError::HostKeyChanged { host }
        } else if stderr.contains("Host key verification failed") {
            ConnectionError::UnknownHost { host }
        } else if output.status.code() == Some(127) {
            ConnectionError::NotInstalled { host }
        } else {
            ConnectionError::Ssh {
                host,
                exit_code: output.status.code(),
                detail: tail(&stderr),
            }
        },
    ))
}
fn tail(text: &str) -> String {
    let text = text.trim();
    let start = text.len().saturating_sub(600);
    let start = (start..=text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    text[start..].to_owned()
}
/// Where OpenSSH will really connect for an endpoint, and the name it looks up
/// in known_hosts, after applying the user's ssh_config (`Host` aliases,
/// `HostName`, `Port`, `HostKeyAlias`).
#[derive(Debug, PartialEq, Eq)]
struct Resolved {
    hostname: String,
    port: u16,
    known_hosts_name: String,
}
fn resolve_ssh_config(host: &str, config: &str) -> Result<Resolved> {
    let mut hostname = None;
    let mut port = 22;
    let mut alias = None;
    for line in config.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" => hostname = Some(value.to_owned()),
            "port" => port = value.parse().unwrap_or(22),
            "hostkeyalias" => alias = Some(value.to_owned()),
            "proxycommand" | "proxyjump" if value != "none" => {
                return Err(traced(ConnectionError::ProxiedHost {
                    host: host.to_owned(),
                }));
            }
            _ => {}
        }
    }
    let hostname = hostname.unwrap_or_else(|| host.to_owned());
    let known_hosts_name = alias.unwrap_or_else(|| {
        if port == 22 {
            hostname.clone()
        } else {
            format!("[{hostname}]:{port}")
        }
    });
    Ok(Resolved {
        hostname,
        port,
        known_hosts_name,
    })
}
/// Fetch the keys the endpoint's real host offers so the user can compare
/// fingerprints out of band. Entries are named for that host only: the name in
/// ssh-keyscan's output is ignored, so a scan cannot vouch for another host.
pub async fn scan_host_keys(endpoint: &str) -> Result<Vec<HostKey>> {
    let parsed = endpoint_of(endpoint)?;
    let mut config = crate::transport::ssh_config_command(endpoint).map_err(|source| {
        traced(ConnectionError::InvalidEndpoint {
            endpoint: endpoint.to_owned(),
            source,
        })
    })?;
    config.stdin(Stdio::null()).kill_on_drop(true);
    let output = run(config, "ssh", "SSH configuration", &parsed.host).await?;
    if !output.status.success() {
        return Err(traced(ConnectionError::Ssh {
            host: parsed.host,
            exit_code: output.status.code(),
            detail: tail(&String::from_utf8_lossy(&output.stderr)),
        }));
    }
    let resolved = resolve_ssh_config(&parsed.host, &String::from_utf8_lossy(&output.stdout))?;
    let mut command = tokio::process::Command::new("ssh-keyscan");
    command
        .args(["-T", "10", "-p"])
        .arg(resolved.port.to_string())
        .arg("--")
        .arg(&resolved.hostname)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = run(command, "ssh-keyscan", "host key scan", &parsed.host).await?;
    let keys = parse_host_keys(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .filter_map(|key| {
            let blob = key.entry.split_whitespace().nth(2)?;
            Some(HostKey {
                entry: format!("{} {} {blob}", resolved.known_hosts_name, key.algorithm),
                ..key
            })
        })
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return Err(traced(ConnectionError::NoHostKeys { host: parsed.host }));
    }
    Ok(keys)
}
async fn run(
    mut command: tokio::process::Command,
    program: &'static str,
    operation: &'static str,
    host: &str,
) -> Result<std::process::Output> {
    tokio::time::timeout(PROBE_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            traced(ConnectionError::TimedOut {
                operation,
                host: host.to_owned(),
            })
        })?
        .map_err(|source| traced(ConnectionError::MissingTool { program, source }))
}
fn parse_host_keys(text: &str) -> Vec<HostKey> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (host, algorithm, blob) = (fields.next()?, fields.next()?, fields.next()?);
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(blob)
                .ok()?;
            let digest = Sha256::digest(decoded);
            Some(HostKey {
                algorithm: algorithm.to_owned(),
                fingerprint: format!(
                    "SHA256:{}",
                    base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
                ),
                entry: format!("{host} {algorithm} {blob}"),
            })
        })
        .collect()
}
/// Record host keys the user confirmed. Entries are rebuilt from parsed fields,
/// so a crafted scan cannot add options or extra lines to known_hosts.
pub fn trust_host_keys(root: &Path, keys: &[HostKey]) -> Result<()> {
    let mut lines = String::new();
    for key in keys {
        let parsed = parse_host_keys(&key.entry);
        if parsed.len() != 1 || parsed[0] != *key {
            return Err(traced(ConnectionError::HostKeyMismatch));
        }
        lines.push_str(&key.entry);
        lines.push('\n');
    }
    if lines.is_empty() {
        return Err(traced(ConnectionError::HostKeyMismatch));
    }
    let path = known_hosts(root);
    private_root(root)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(io(&path))?;
    file.write_all(lines.as_bytes()).map_err(io(&path))?;
    file.sync_all().map_err(io(&path))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn blob(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
    #[test]
    fn setup_round_trips_and_rejects_unsafe_endpoints() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load(root.path()).unwrap(), None);
        assert_eq!(resolve(root.path(), None).unwrap(), None);
        let endpoint = format!("ssh://u{}@gateway.test:2222", uuid::Uuid::new_v4().simple());
        let saved = GatewayConnection {
            target: GatewayTarget::Ssh {
                endpoint: endpoint.clone(),
            },
        };
        save(root.path(), &saved).unwrap();
        assert_eq!(load(root.path()).unwrap(), Some(saved.clone()));
        assert_eq!(
            resolve(root.path(), Some(endpoint.clone())).unwrap(),
            Some(saved.target)
        );
        for bad in ["ssh://-oProxyCommand=x", "http://host", "ssh://u:p@host"] {
            let target = GatewayTarget::Ssh {
                endpoint: bad.into(),
            };
            assert!(
                save(
                    root.path(),
                    &GatewayConnection {
                        target: target.clone()
                    }
                )
                .is_err()
            );
            assert!(resolve(root.path(), Some(bad.into())).is_err(), "{bad}");
        }
        // A hand-edited file is validated on load, not trusted.
        std::fs::write(
            root.path().join(FILE),
            "[target]\nkind = \"ssh\"\nendpoint = \"ssh://-evil\"\n",
        )
        .unwrap();
        assert!(load(root.path()).is_err());
    }
    #[test]
    fn host_key_names_follow_the_effective_ssh_configuration() {
        let alias = format!("lab{}", uuid::Uuid::new_v4().simple());
        let real = format!("{alias}.internal.test");
        let config = format!("user u\nhostname {real}\nport 2222\nproxycommand none\n");
        assert_eq!(
            resolve_ssh_config(&alias, &config).unwrap(),
            Resolved {
                hostname: real.clone(),
                port: 2222,
                known_hosts_name: format!("[{real}]:2222"),
            }
        );
        let config = format!("hostname {real}\nport 22\nhostkeyalias {alias}\n");
        assert_eq!(
            resolve_ssh_config(&alias, &config)
                .unwrap()
                .known_hosts_name,
            alias
        );
        for proxy in ["proxyjump bastion", "proxycommand nc %h %p"] {
            assert!(matches!(
                resolve_ssh_config(&alias, &format!("hostname {real}\n{proxy}\n")),
                Err(ConnectionError::ProxiedHost { .. })
            ));
        }
    }
    #[cfg(unix)]
    #[test]
    fn setup_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("profile");
        save(
            &root,
            &GatewayConnection {
                target: GatewayTarget::Local,
            },
        )
        .unwrap();
        let key = uuid::Uuid::new_v4().into_bytes();
        let keys = parse_host_keys(&format!("gateway.test ssh-ed25519 {}", blob(&key)));
        trust_host_keys(&root, &keys).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join(FILE)), 0o600);
        assert_eq!(mode(&known_hosts(&root)), 0o600);
    }
    #[test]
    fn host_keys_are_fingerprinted_and_only_exact_scans_are_trusted() {
        let root = tempfile::tempdir().unwrap();
        let key = uuid::Uuid::new_v4().into_bytes();
        let scan = format!(
            "# gateway.test:22 SSH-2.0\n[gateway.test]:2222 ssh-ed25519 {}\nnot a key\n",
            blob(&key)
        );
        let keys = parse_host_keys(&scan);
        assert_eq!(keys.len(), 1);
        let expected = format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(key))
        );
        assert_eq!(keys[0].fingerprint, expected);
        let mut forged = keys[0].clone();
        forged
            .entry
            .push_str("\n@cert-authority * ssh-ed25519 AAAA");
        assert!(trust_host_keys(root.path(), &[forged]).is_err());
        let mut mismatched = keys[0].clone();
        mismatched.fingerprint = "SHA256:other".into();
        assert!(trust_host_keys(root.path(), &[mismatched]).is_err());
        assert!(!known_hosts(root.path()).exists());
        trust_host_keys(root.path(), &keys).unwrap();
        assert_eq!(
            std::fs::read_to_string(known_hosts(root.path())).unwrap(),
            format!("{}\n", keys[0].entry)
        );
    }
}
