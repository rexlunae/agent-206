//! The helper that runs inside bubblewrap's namespaces: it binds the proxy
//! port on the private loopback when there is one, applies Landlock to itself,
//! starts the runtime (which inherits that Landlock domain) and relays proxy
//! traffic.
use crate::{Access, Network, SPEC_VERSION, SandboxError, Spec, Status};
use a206_core::Ignore;
use landlock::{
    ABI, Access as _, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathBeneath, PathFd,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
};
use std::{
    ffi::OsString,
    io::Write,
    net::{Shutdown, TcpListener, TcpStream},
    os::{
        fd::AsFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::ExitCode,
};

pub(crate) fn run(mut args: impl Iterator<Item = OsString>) -> Result<ExitCode, SandboxError> {
    let spec = args.next().ok_or(SandboxError::Usage)?;
    let spec = Spec::decode(spec.to_str().ok_or(SandboxError::NonUtf8Argument)?)?;
    let mut status = UnixStream::connect(&spec.status).map_err(SandboxError::Report)?;
    match start(&spec, args) {
        Ok(mut child) => {
            report(
                &mut status,
                &Status::Ready {
                    version: SPEC_VERSION,
                },
            )?;
            drop(status);
            let exit = child.wait().map_err(SandboxError::Wait)?;
            use std::os::unix::process::ExitStatusExt;
            let code = exit
                .code()
                .or_else(|| exit.signal().map(|signal| 128 + signal))
                .unwrap_or(1);
            Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
        }
        Err(error) => {
            // The report is how the gateway learns why; if it cannot be sent,
            // the caller prints the error to the stderr the gateway captures.
            report(&mut status, &Status::Failed(error.report()))?;
            Ok(ExitCode::FAILURE)
        }
    }
}

fn start(
    spec: &Spec,
    args: impl Iterator<Item = OsString>,
) -> Result<std::process::Child, SandboxError> {
    // Bound before Landlock, which then forbids the runtime from binding.
    let listener = match &spec.network {
        Network::Proxy(proxy) => Some((
            TcpListener::bind(("127.0.0.1", proxy.port)).map_err(SandboxError::Listen)?,
            proxy.socket.clone(),
        )),
        Network::None | Network::Host => None,
    };
    if spec.terminal {
        // Bubblewrap's new session has no controlling terminal. This helper
        // leads a session of its own, unless it already leads one, so its
        // stdin can become that session's terminal.
        match rustix::process::setsid() {
            Ok(_) | Err(rustix::io::Errno::PERM) => {}
            Err(errno) => return Err(SandboxError::Terminal(errno.into())),
        }
        rustix::process::ioctl_tiocsctty(std::io::stdin())
            .map_err(|errno| SandboxError::Terminal(errno.into()))?;
    }
    restrict(spec)?;
    if let Some((listener, socket)) = listener {
        // Detached: the relay lives exactly as long as this process, which exits
        // with the runtime. Each connection's threads end when either side closes.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let socket = socket.clone();
                        std::thread::spawn(move || relay(stream, &socket));
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = &error as &dyn std::error::Error,
                            "proxy accept failed"
                        );
                    }
                }
            }
        });
    }
    let mut command = std::process::Command::new(&spec.executable);
    command.args(args);
    if spec.terminal {
        // The terminal is the runtime's stderr too, as in any terminal.
        let terminal = std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(SandboxError::Terminal)?;
        command.stderr(terminal).process_group(0);
    } else {
        // The gateway reads this process's stderr for diagnostics until it
        // is ready; only a runtime whose stderr the gateway wants writes there.
        command.stderr(if spec.stderr {
            std::process::Stdio::inherit()
        } else {
            std::process::Stdio::null()
        });
    }
    let child = command.spawn().map_err(SandboxError::Spawn)?;
    if spec.terminal {
        foreground(&child)?;
    }
    Ok(child)
}

/// Make the runtime's process group the terminal's foreground group, so its
/// interrupt and suspend keys reach the runtime and not this helper. A
/// runtime that read the terminal before this was stopped for it; it is
/// continued.
fn foreground(child: &std::process::Child) -> Result<(), SandboxError> {
    let terminal = |errno: rustix::io::Errno| SandboxError::Terminal(errno.into());
    let group = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| SandboxError::Terminal(std::io::ErrorKind::InvalidData.into()))?;
    rustix::termios::tcsetpgrp(std::io::stdin(), group).map_err(terminal)?;
    rustix::process::kill_process_group(group, rustix::process::Signal::CONT).map_err(terminal)
}

/// Apply the allowlist to this process. Filesystem rules from the first
/// Landlock ABI are required; later rights (rename across directories,
/// truncation, device ioctls, TCP ports, IPC scopes) are added where the
/// kernel has them, on top of the namespaces that already isolate them.
fn restrict(spec: &Spec) -> Result<(), SandboxError> {
    let required = AccessFs::from_all(ABI::V1);
    let all = AccessFs::from_all(ABI::V5);
    let ruleset = landlock::Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(required)
        .map_err(SandboxError::Landlock)?
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(all)
        .map_err(SandboxError::Landlock)?;
    // Host networking is a user grant; every other runtime's TCP is limited.
    let ruleset = if spec.network == Network::Host {
        ruleset
    } else {
        ruleset
            .handle_access(AccessNet::from_all(ABI::V4))
            .map_err(SandboxError::Landlock)?
    };
    let mut ruleset = ruleset
        .scope(Scope::from_all(ABI::V6))
        .map_err(SandboxError::Landlock)?
        .create()
        .map_err(SandboxError::Landlock)?;
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    for rule in &spec.rules {
        let access = match rule.access {
            Access::Read => read,
            Access::Execute => read | AccessFs::Execute,
            Access::Write => all & !AccessFs::Execute,
            Access::Device => AccessFs::ReadFile | AccessFs::WriteFile,
            Access::Terminal => AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::IoctlDev,
        };
        let fd = PathFd::new(&rule.path).map_err(|source| SandboxError::LandlockPath {
            path: rule.path.clone(),
            source,
        })?;
        // Directory-only rights are invalid on a file.
        let access = if is_dir(&rule.path) {
            access
        } else {
            access & AccessFs::from_file(ABI::V5)
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(SandboxError::Landlock)?;
    }
    // Without a proxy or the host network, no TCP connection is allowed.
    if let Network::Proxy(proxy) = &spec.network {
        ruleset = ruleset
            .add_rule(NetPort::new(proxy.port, AccessNet::ConnectTcp))
            .map_err(SandboxError::Landlock)?;
    }
    let status = ruleset.restrict_self().map_err(SandboxError::Landlock)?;
    if status.ruleset == RulesetStatus::NotEnforced || !status.no_new_privs {
        return Err(SandboxError::NotEnforced);
    }
    Ok(())
}

fn is_dir(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn report(status: &mut UnixStream, message: &Status) -> Result<(), SandboxError> {
    let bytes = rmp_serde::to_vec_named(message).map_err(SandboxError::Encode)?;
    status.write_all(&bytes).map_err(SandboxError::Report)?;
    status
        .shutdown(Shutdown::Write)
        .map_err(SandboxError::Report)
}

/// Copy one proxy connection both ways until either side closes.
fn relay(tcp: TcpStream, proxy: &PathBuf) {
    let failed = |error: &std::io::Error| {
        tracing::warn!(
            error = error as &dyn std::error::Error,
            "proxy relay failed"
        );
    };
    let unix = match UnixStream::connect(proxy) {
        Ok(unix) => unix,
        Err(error) => return failed(&error),
    };
    let (mut tcp_read, mut unix_write) = match (tcp.try_clone(), unix.try_clone()) {
        (Ok(tcp), Ok(unix)) => (tcp, unix),
        (Err(error), _) | (_, Err(error)) => return failed(&error),
    };
    let upstream = std::thread::spawn(move || {
        let result = std::io::copy(&mut tcp_read, &mut unix_write);
        // The peer may already be gone; there is nothing left to tell it.
        unix_write.shutdown(Shutdown::Write).ignore();
        result
    });
    let (mut unix_read, mut tcp_write) = (unix, tcp);
    if let Err(error) = std::io::copy(&mut unix_read, &mut tcp_write) {
        failed(&error);
    }
    // Same: the runtime may have closed its end first.
    tcp_write.shutdown(Shutdown::Write).ignore();
    match upstream.join() {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => failed(&error),
        Err(_) => tracing::warn!("proxy relay thread panicked"),
    }
}
