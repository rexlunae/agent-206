//! A model pushes and fetches over an ssh remote with `git_push` and
//! `git_fetch`, authenticating with an SSH key that only the vault broker
//! holds. The harness is custom because this binary is also the `ssh` the
//! gateway runs: it re-runs itself with a directory on PATH whose `ssh`
//! links back to it. As `ssh`, it checks the options the gateway passed,
//! has the one-use agent sign a user authentication request and verifies
//! the signature, checks the host is in the profile's known hosts, and
//! relays the git service's conversation to the stand-in HTTP forge at the
//! same address. Every identifier, file and key is generated at runtime.
#[cfg(unix)]
mod git_forge;
#[cfg(unix)]
mod support;

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    {
        let args: Vec<String> = std::env::args().collect();
        if std::path::Path::new(&args[0]).file_name() == Some("ssh".as_ref()) {
            return fake::ssh(&args[1..]);
        }
        if std::env::var_os(RELAUNCHED).is_none() {
            return relaunch();
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(tests::a_model_pushes_and_fetches_over_ssh_with_a_vault_key());
        println!("git over ssh: ok");
    }
    std::process::ExitCode::SUCCESS
}

/// Set on the re-run that holds the tests.
#[cfg(unix)]
const RELAUNCHED: &str = "A206_GIT_SSH_TEST";

/// Run this binary again with an `ssh` on PATH that is this binary.
#[cfg(unix)]
fn relaunch() -> std::process::ExitCode {
    let exe = std::env::current_exe().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&exe, dir.path().join("ssh")).unwrap();
    let path = std::env::join_paths(std::iter::once(dir.path().to_owned()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let status = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("PATH", path)
        .env(RELAUNCHED, "1")
        .status()
        .unwrap();
    if status.success() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// The stand-in `ssh`.
#[cfg(unix)]
mod fake {
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        os::unix::net::UnixStream,
        process::ExitCode,
    };

    /// Fail as ssh does, with a message the gateway reports.
    fn fail(message: &str) -> ExitCode {
        eprintln!("{message}");
        ExitCode::from(255)
    }

    fn put(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend(u32::try_from(bytes.len()).unwrap().to_be_bytes());
        out.extend(bytes);
    }

    fn take<'a>(rest: &mut &'a [u8]) -> &'a [u8] {
        let length = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
        let bytes = &rest[4..4 + length];
        *rest = &rest[4 + length..];
        bytes
    }

    /// One agent request and its answer.
    fn exchange(agent: &mut UnixStream, message: &[u8]) -> Vec<u8> {
        let mut framed = vec![];
        put(&mut framed, message);
        agent.write_all(&framed).unwrap();
        let mut length = [0; 4];
        agent.read_exact(&mut length).unwrap();
        let mut reply = vec![0; u32::from_be_bytes(length) as usize];
        agent.read_exact(&mut reply).unwrap();
        reply
    }

    fn unquote(value: &str) -> &str {
        value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap()
    }

    pub fn ssh(args: &[String]) -> ExitCode {
        // No configuration, terminal, X11 or agent forwarding.
        assert_eq!(args[..5], ["-F", "none", "-T", "-x", "-a"]);
        let mut options = BTreeMap::new();
        let mut rest = &args[5..];
        while rest[0] == "-o" {
            let (key, value) = rest[1].split_once('=').unwrap();
            assert!(
                options.insert(key.to_owned(), value.to_owned()).is_none(),
                "{key} given twice"
            );
            rest = &rest[2..];
        }
        for (key, value) in [
            ("BatchMode", "yes"),
            ("StrictHostKeyChecking", "yes"),
            ("UpdateHostKeys", "no"),
            ("IdentitiesOnly", "yes"),
            ("PreferredAuthentications", "publickey"),
            ("PasswordAuthentication", "no"),
            ("ForwardAgent", "no"),
            ("ClearAllForwardings", "yes"),
            ("PermitLocalCommand", "no"),
            ("ControlMaster", "no"),
            ("ControlPath", "none"),
        ] {
            assert_eq!(options[key], value, "{key}");
        }
        assert!(options.contains_key("ServerAliveInterval"));
        assert!(options.contains_key("ConnectTimeout"));
        assert!(std::env::var_os("SSH_AUTH_SOCK").is_none());
        let [port_flag, port, user_flag, user, separator, host, command] = rest else {
            panic!("unexpected arguments {rest:?}");
        };
        assert_eq!(
            [port_flag, user_flag, separator].map(String::as_str),
            ["-p", "-l", "--"]
        );
        // The URL's user, not the credential's.
        assert_eq!(user, "git");

        // Only hosts in the profile's known hosts are reached.
        let known = options["UserKnownHostsFile"]
            .strip_prefix("~/.ssh/known_hosts ")
            .map(unquote)
            .unwrap();
        let entry = format!("[{host}]:{port} ");
        let trusted = std::fs::read_to_string(known)
            .is_ok_and(|text| text.lines().any(|line| line.starts_with(&entry)));
        if !trusted {
            return fail(&format!(
                "No ED25519 host key is known for [{host}]:{port}.\r\nHost key verification failed."
            ));
        }

        // The agent offers the identity's key and signs authentication with
        // it, as the server checks.
        let identity = std::fs::read_to_string(unquote(&options["IdentityFile"])).unwrap();
        let public = ssh_key::PublicKey::from_openssh(identity.trim()).unwrap();
        let blob = public.to_bytes().unwrap();
        let mut agent = UnixStream::connect(unquote(&options["IdentityAgent"])).unwrap();
        let listed = exchange(&mut agent, &[11]);
        let mut body = &listed[1..];
        assert_eq!(listed[0], 12);
        assert_eq!(u32::from_be_bytes(body[..4].try_into().unwrap()), 1);
        body = &body[4..];
        assert_eq!(take(&mut body), blob.as_slice());
        let mut data = vec![];
        put(&mut data, uuid::Uuid::new_v4().as_bytes());
        data.push(50);
        put(&mut data, user.as_bytes());
        put(&mut data, b"ssh-connection");
        put(&mut data, b"publickey");
        data.push(1);
        put(&mut data, public.algorithm().as_str().as_bytes());
        put(&mut data, &blob);
        let mut request = vec![13];
        put(&mut request, &blob);
        put(&mut request, &data);
        request.extend(0u32.to_be_bytes());
        let signed = exchange(&mut agent, &request);
        if signed[0] != 14 {
            return fail(&format!("{user}@{host}: Permission denied (publickey)."));
        }
        let mut body = &signed[1..];
        let signature = {
            use ssh_encoding::Decode;
            ssh_key::Signature::decode(&mut take(&mut body)).unwrap()
        };
        signature::Verifier::verify(&public, &data, &signature).unwrap();
        // Anything but authentication is refused.
        let mut other = vec![13];
        put(&mut other, &blob);
        put(&mut other, uuid::Uuid::new_v4().as_bytes());
        other.extend(0u32.to_be_bytes());
        assert_eq!(exchange(&mut agent, &other), [5]);
        drop(agent);

        // `git-upload-pack '/team/repo.git'`, relayed to the forge.
        let (service, path) = command.split_once(' ').unwrap();
        let path = path.strip_prefix('\'').unwrap().strip_suffix('\'').unwrap();
        let base = format!("http://{host}:{port}{path}");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let advertised = runtime.block_on(async {
            client
                .get(format!("{base}/info/refs?service={service}"))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        });
        // Over ssh the advertisement has no `# service` line and flush.
        let first =
            usize::from_str_radix(std::str::from_utf8(&advertised[..4]).unwrap(), 16).unwrap();
        assert_eq!(&advertised[first..first + 4], b"0000");
        let mut stdout = std::io::stdout();
        stdout.write_all(&advertised[first + 4..]).unwrap();
        stdout.flush().unwrap();
        let mut request = vec![];
        std::io::stdin().read_to_end(&mut request).unwrap();
        if request == b"0000" {
            return ExitCode::SUCCESS;
        }
        let answer = runtime.block_on(async {
            client
                .post(format!("{base}/{service}"))
                .header("content-type", format!("application/x-{service}-request"))
                .body(request)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        });
        stdout.write_all(&answer).unwrap();
        stdout.flush().unwrap();
        ExitCode::SUCCESS
    }
}

#[cfg(unix)]
mod tests {
    use crate::{git_forge::*, support::*};
    use a206_core::*;
    use serde_json::json;

    pub async fn a_model_pushes_and_fetches_over_ssh_with_a_vault_key() {
        let mut rig = rig_over(Transport::Ssh).await;
        let first = gix::open(rig.workspace.path())
            .unwrap()
            .head_id()
            .unwrap()
            .detach();

        // An unknown host is never reached; the user learns why.
        let (record, _) = rig.push(json!({}), ApprovalDecision::Approve).await;
        assert_eq!(record.state, ToolCallState::Failed);
        let error = record.error.unwrap();
        assert_eq!(error.kind, ErrorKind::PermissionDenied, "{error:?}");
        let cause = error.source.as_deref().unwrap();
        assert_eq!(cause.kind, ErrorKind::PermissionDenied, "{error:?}");
        assert!(cause.message.contains("host key"), "{error:?}");
        assert!(rig.forge.seen.lock().unwrap().is_empty());

        // Once the user trusts its host key, the push is signed by the vault.
        let address = rig.origin.strip_prefix("ssh://").unwrap();
        let (host, port) = address.rsplit_once(':').unwrap();
        std::fs::write(
            rig._state.path().join("known_hosts"),
            format!("[{host}]:{port} ssh-ed25519 {}\n", nonce()),
        )
        .unwrap();
        let (record, result) = rig.push(json!({}), ApprovalDecision::Approve).await;
        assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
        let pushed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(pushed["commit"], first.to_string());
        assert_eq!(rig.forge.branch(&rig.branch), Some(first));
        assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(first));
        // Nothing new: the conversation ends without a pack.
        let posts = rig.forge.posts();
        let (record, result) = rig.push(json!({}), ApprovalDecision::Approve).await;
        assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
        assert!(result.unwrap().contains("\"up_to_date\":true"));
        assert_eq!(rig.forge.posts(), posts);

        // A commit made on the forge arrives in the remote-tracking branch.
        let theirs = rig.forge.commit(&rig.branch, &[first]);
        let (record, result) = rig.fetch(json!({}), ApprovalDecision::Approve).await;
        assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
        let fetched: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(fetched["commit"], theirs.to_string());
        assert_eq!(tracking(rig.workspace.path(), &rig.branch), Some(theirs));
        // Nothing new to fetch: no pack is asked for.
        let asked = rig.forge.count("git-upload-pack");
        let (record, result) = rig.fetch(json!({}), ApprovalDecision::Approve).await;
        assert_eq!(record.state, ToolCallState::Succeeded, "{:?}", record.error);
        assert!(result.unwrap().contains("\"up_to_date\":true"));
        assert_eq!(rig.forge.count("git-upload-pack"), asked);
        // No request carried a token: ssh authenticated each one.
        assert!(
            rig.forge
                .seen
                .lock()
                .unwrap()
                .iter()
                .all(|(auth, _)| auth.is_empty())
        );
    }
}
