//! OS isolation for external agent runtimes and commands on Linux and macOS.
//!
//! On Linux two independent layers confine a runtime, and both are required:
//!
//! 1. **Bubblewrap** gives it new user, PID, IPC, UTS, cgroup and network
//!    namespaces and a mount namespace that holds only the system libraries,
//!    its installation, a private scratch directory and the granted project.
//! 2. **Landlock**, applied by this crate's helper inside that namespace,
//!    limits reads, writes and execution to the same allowlist, so a mistake in
//!    one layer does not open the filesystem.
//!
//! By default the new network namespace has only loopback. When the runtime
//! needs its credential proxy, the helper listens on the proxy port there and
//! relays each connection over a Unix socket to the gateway, so the runtime
//! reaches that proxy and nothing else. Without a proxy it has no network at
//! all. Only a runtime the user granted network access shares the host's.
//!
//! On macOS a [`Profile`] becomes one Seatbelt policy that `sandbox-exec`
//! applies before the runtime starts (see `seatbelt.rs`), with the same
//! filesystem grants and hidden paths. A runtime with a credential proxy may
//! reach that loopback port and nothing else.
//!
//! Nothing here downgrades: a missing bubblewrap, a kernel without Landlock or a
//! policy the layers cannot express is an error, never a weaker sandbox.
//!
//! The approach follows RustyClaw's `sandbox` module (MIT, see NOTICE), but
//! fails closed where RustyClaw fell back to path validation, runs Landlock
//! inside the namespace rather than in the gateway, and replaces the shared
//! network with the loopback relay.
use a206_core::{ErrorKind, ErrorReport};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[cfg(unix)]
mod elf;
mod gateway;
#[cfg(target_os = "linux")]
mod helper;
#[cfg(unix)]
mod profile;
#[cfg(unix)]
mod seatbelt;
#[cfg(target_os = "macos")]
mod terminal;

#[cfg(unix)]
pub use elf::interpreter;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use gateway::Control;
pub use gateway::StartError;
#[cfg(unix)]
pub use profile::{HIDDEN_HOME_PATHS, NetworkAccess, Profile, hidden_home_paths};

/// First argument that turns a process into the in-sandbox helper.
pub const HELPER_COMMAND: &str = "__a206-sandbox-helper";
/// First argument that turns a process into the macOS launcher that makes
/// its stdin its controlling terminal and then runs the rest of its
/// arguments; see [`Profile::command`].
pub const TERMINAL_COMMAND: &str = "__a206-sandbox-terminal";
/// Version of [`Spec`] and [`Status`]; the helper and the gateway are the same
/// executable, so a mismatch means a corrupted or foreign invocation.
pub const SPEC_VERSION: u32 = 5;
/// Bubblewrap is resolved from this fixed system path, never from `PATH`.
pub const BUBBLEWRAP: &str = "/usr/bin/bwrap";
/// macOS's Seatbelt launcher, likewise from its fixed system path.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// Largest status message the gateway accepts from a helper.
pub const MAX_STATUS: u64 = 64 * 1024;

/// What a Landlock rule permits beneath its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Access {
    /// Read files and list directories.
    Read,
    /// Read, and execute files.
    Execute,
    /// Read, create, change and remove files, but not execute them.
    Write,
    /// Read and write an existing device such as `/dev/null`.
    Device,
    /// Read, write and control a terminal device: `Device` plus its ioctls,
    /// which programs use to configure a terminal they open.
    Terminal,
}

/// One Landlock allowlist entry, as a path inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub path: PathBuf,
    pub access: Access,
}

/// The one network destination a runtime may reach: its credential proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proxy {
    /// Loopback port the runtime expects the proxy on.
    pub port: u16,
    /// Unix socket that reaches the proxy from inside the sandbox.
    pub socket: PathBuf,
}

/// The network a runtime has inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Network {
    /// An empty loopback; no TCP connection is allowed.
    None,
    /// Only its credential proxy, relayed from the private loopback.
    Proxy(Proxy),
    /// The host's network, for a runtime the user granted it.
    Host,
}

/// What the gateway tells its helper, passed as one MessagePack argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub version: u32,
    /// The runtime to start once both layers are in place.
    pub executable: PathBuf,
    pub network: Network,
    /// Unix socket on which the helper reports [`Status`].
    pub status: PathBuf,
    /// Whether the runtime keeps the helper's stderr; otherwise it is null.
    pub stderr: bool,
    /// The helper's stdin is a terminal that becomes the runtime's
    /// controlling terminal, and its stdout and stderr.
    pub terminal: bool,
    pub rules: Vec<Rule>,
}

impl Spec {
    pub fn encode(&self) -> Result<String, SandboxError> {
        use base64::Engine;
        let bytes = rmp_serde::to_vec_named(self).map_err(SandboxError::Encode)?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }
    pub fn decode(text: &str) -> Result<Self, SandboxError> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text)
            .map_err(SandboxError::Base64)?;
        let spec: Self = rmp_serde::from_slice(&bytes).map_err(SandboxError::Decode)?;
        if spec.version != SPEC_VERSION {
            return Err(SandboxError::Version {
                found: spec.version,
            });
        }
        Ok(spec)
    }
}

/// The helper's one message to the gateway: both layers are in place and the
/// runtime started, or why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Ready { version: u32 },
    Failed(ErrorReport),
}

/// Why the sandbox could not be described, entered or started.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("Linux isolation requires bubblewrap at {BUBBLEWRAP}; install the bubblewrap package")]
    Bubblewrap(#[source] std::io::Error),
    #[error("macOS isolation requires {SANDBOX_EXEC}")]
    SandboxExec(#[source] std::io::Error),
    #[error("{} cannot be named in a sandbox policy; use a UTF-8 path without control characters", path.display())]
    Path { path: PathBuf },
    #[error("could not inspect {} for the sandbox", path.display())]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{} must stay hidden but lies inside a writable grant where it does not exist as a directory; narrow the grant",
        path.display()
    )]
    Exposed { path: PathBuf },
    #[error("could not encode the sandbox specification")]
    Encode(#[source] rmp_serde::encode::Error),
    #[error("sandbox specification is not valid base64")]
    Base64(#[source] base64::DecodeError),
    #[error("sandbox specification is malformed")]
    Decode(#[source] rmp_serde::decode::Error),
    #[error("sandbox specification version {found} is not {SPEC_VERSION}")]
    Version { found: u32 },
    #[error("sandbox helper needs its specification argument")]
    Usage,
    #[error("sandbox helper argument is not UTF-8")]
    NonUtf8Argument,
    #[cfg(target_os = "linux")]
    #[error("could not build the Landlock policy")]
    Landlock(#[source] landlock::RulesetError),
    #[cfg(target_os = "linux")]
    #[error("could not open {} for a Landlock rule", path.display())]
    LandlockPath {
        path: PathBuf,
        #[source]
        source: landlock::PathFdError,
    },
    #[error(
        "this kernel does not enforce Landlock filesystem rules; Linux 5.13 or newer with Landlock enabled is required"
    )]
    NotEnforced,
    #[error("could not listen for the runtime's proxy connections inside the sandbox")]
    Listen(#[source] std::io::Error),
    #[error("could not report to the gateway")]
    Report(#[source] std::io::Error),
    #[error("could not start the runtime inside the sandbox")]
    Spawn(#[source] std::io::Error),
    #[error("could not wait for the runtime inside the sandbox")]
    Wait(#[source] std::io::Error),
    #[error("could not make the terminal the runtime's controlling terminal")]
    Terminal(#[source] std::io::Error),
    #[error("could not start {}", program.to_string_lossy())]
    Launch {
        program: std::ffi::OsString,
        #[source]
        source: std::io::Error,
    },
    #[error("OS isolation for external runtimes is only implemented for Linux and macOS")]
    Unsupported,
}

impl SandboxError {
    /// The client-facing kind; `None` defers to the I/O cause.
    pub fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Bubblewrap(_) | Self::SandboxExec(_) | Self::NotEnforced | Self::Unsupported => {
                ErrorKind::Unavailable
            }
            Self::Exposed { .. } => ErrorKind::PermissionDenied,
            Self::Path { .. } => ErrorKind::InvalidRequest,
            Self::Inspect { .. } => return None,
            Self::Encode(_)
            | Self::Base64(_)
            | Self::Decode(_)
            | Self::Version { .. }
            | Self::Usage
            | Self::NonUtf8Argument => ErrorKind::Protocol,
            #[cfg(target_os = "linux")]
            Self::Landlock(_) | Self::LandlockPath { .. } => ErrorKind::Unavailable,
            Self::Listen(_)
            | Self::Report(_)
            | Self::Spawn(_)
            | Self::Wait(_)
            | Self::Terminal(_)
            | Self::Launch { .. } => ErrorKind::Internal,
        })
    }
    /// Capture this error and its causes for the gateway.
    pub fn report(&self) -> ErrorReport {
        ErrorReport::capture(self, |link| {
            link.downcast_ref::<SandboxError>().and_then(Self::kind)
        })
    }
}

/// Run the in-sandbox helper, or the macOS terminal launcher, when this
/// process was started as one.
///
/// Every binary that hosts a gateway calls this first, before it starts
/// threads, and exits with the returned code. `None` means this is not a
/// helper invocation. An error is returned only when it could not also be
/// reported to the gateway, for the binary to print; the launcher's errors
/// are all returned, and its stderr is the command's terminal.
pub fn dispatch() -> Option<Result<std::process::ExitCode, SandboxError>> {
    let mut args = std::env::args_os().skip(1);
    let command = args.next()?;
    if command == TERMINAL_COMMAND {
        #[cfg(target_os = "macos")]
        return Some(terminal::run(args));
        #[cfg(not(target_os = "macos"))]
        return Some(Err(SandboxError::Unsupported));
    }
    if command != HELPER_COMMAND {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        Some(helper::run(args))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(Err(SandboxError::Unsupported))
    }
}
