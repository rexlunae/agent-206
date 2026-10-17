//! The mount namespace and Landlock allowlist for one runtime launch.
use crate::{Access, Network, Rule, SPEC_VERSION, SandboxError, Spec};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Credential stores and agent configuration under `$HOME` that stay hidden
/// even when a granted project contains them.
pub const HIDDEN_HOME_PATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".config/gcloud",
    ".codex",
    ".claude",
    ".agent206",
    "Library/Keychains",
];

/// [`HIDDEN_HOME_PATHS`] under the user's home. A symlinked home resolves to
/// the canonical paths the grants are compared with; a home that cannot be
/// resolved is still hidden at its literal path.
pub fn hidden_home_paths() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| std::fs::canonicalize(&home).unwrap_or_else(|_| PathBuf::from(home)))
        .map(|home| {
            HIDDEN_HOME_PATHS
                .iter()
                .map(|suffix| home.join(suffix))
                .collect()
        })
        .unwrap_or_default()
}

/// System directories mounted read-only; symbolic links (merged `/usr`) are
/// recreated as links instead.
const SYSTEM_MOUNTS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32",
];
/// Shared libraries and data a runtime may read; programs there stay unexecutable.
const SYSTEM_READ: &[&str] = &[
    "/usr/lib",
    "/usr/lib32",
    "/usr/lib64",
    "/usr/libx32",
    "/usr/share",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
];
/// Dynamic linker configuration and the time zone, nothing else from `/etc`.
const SYSTEM_ETC: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/localtime",
];
/// Name resolution and certificates for a runtime granted the host network.
/// These are bound and allowed at their own paths, even when they are links.
const NETWORK_ETC: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/services",
    "/etc/protocols",
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/pki",
];
/// Extra configuration that system tools expect once Execute is granted.
const EXECUTE_ETC: &[&str] = &["/etc/alternatives", "/etc/passwd", "/etc/group"];
/// Directories whose programs the Execute capability allows.
const EXECUTE_DIRECTORIES: &[&str] = &["/usr/bin", "/bin"];
const READ_DEVICES: &[&str] = &["/dev/zero", "/dev/random", "/dev/urandom"];

/// One runtime launch. All paths are canonical host paths; the sandbox mounts
/// each one at the same path, so the Landlock rules name the same locations.
#[derive(Debug, Clone)]
pub struct Profile<'a> {
    /// This executable, which runs as the helper inside the sandbox.
    pub helper: &'a Path,
    /// The runtime, inside `root`.
    pub executable: &'a Path,
    /// The runtime's installation, readable and executable.
    pub root: &'a Path,
    /// Private, writable home, temporary and working directories.
    pub scratch: &'a Path,
    /// The helper's sockets, inside `scratch` but read-only to the runtime.
    pub control: &'a Path,
    /// The granted project and whether it is writable.
    pub workspace: Option<(&'a Path, bool)>,
    /// Whether programs in `/usr/bin` and `/bin` may run.
    pub execute: bool,
    /// Paths that stay invisible even inside a granted directory.
    pub hidden: &'a [PathBuf],
    pub cwd: &'a Path,
    pub network: NetworkAccess,
    /// Pass the runtime's stderr to the gateway once it starts, after
    /// bubblewrap's own diagnostics; otherwise it is discarded.
    pub stderr: bool,
    /// The pseudo-terminal device the runtime's stdin and stdout are, which
    /// becomes its controlling terminal; its stderr goes there too. Only
    /// this terminal and `/dev/tty` are added to what it may open.
    pub terminal: Option<&'a Path>,
}

/// The network a runtime may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAccess {
    /// None at all.
    None,
    /// Only its credential proxy on this loopback port.
    Proxy(u16),
    /// The host's network, which only the user can grant.
    Host,
}

impl Profile<'_> {
    pub fn proxy_socket(&self) -> PathBuf {
        self.control.join("proxy.sock")
    }
    pub fn status_socket(&self) -> PathBuf {
        self.control.join("status.sock")
    }

    /// The Landlock allowlist. Optional system paths that do not exist are
    /// left out; everything named by the profile must exist.
    pub fn rules(&self) -> Result<Vec<Rule>, SandboxError> {
        let mut rules = Vec::new();
        let mut add = |path: PathBuf, access| {
            if !rules
                .iter()
                .any(|rule: &Rule| rule.path == path && rule.access == access)
            {
                rules.push(Rule { path, access });
            }
        };
        for path in SYSTEM_READ.iter().chain(SYSTEM_ETC) {
            if let Some(path) = existing(Path::new(path))? {
                add(path, Access::Read);
            }
        }
        add(PathBuf::from("/proc"), Access::Read);
        add(PathBuf::from("/dev/null"), Access::Device);
        if self.terminal.is_some() {
            // Its own controlling terminal; bubblewrap's `/dev` has no other.
            add(PathBuf::from("/dev/tty"), Access::Device);
        }
        for device in READ_DEVICES {
            add(PathBuf::from(device), Access::Read);
        }
        add(self.root.to_path_buf(), Access::Execute);
        let loader =
            crate::interpreter(self.executable).map_err(|source| SandboxError::Inspect {
                path: self.executable.to_path_buf(),
                source,
            })?;
        if let Some(loader) = loader {
            let loader = existing(&loader)?.ok_or_else(|| SandboxError::Inspect {
                path: loader.clone(),
                source: std::io::ErrorKind::NotFound.into(),
            })?;
            add(loader, Access::Execute);
        }
        add(self.scratch.to_path_buf(), Access::Write);
        if let Some((workspace, writable)) = self.workspace {
            add(
                workspace.to_path_buf(),
                if writable {
                    Access::Write
                } else {
                    Access::Read
                },
            );
        }
        if self.network == NetworkAccess::Host {
            for path in NETWORK_ETC {
                let path = Path::new(path);
                match std::fs::metadata(path) {
                    Ok(_) => add(path.to_path_buf(), Access::Read),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => return Err(inspect(path, source)),
                }
            }
        }
        if self.execute {
            for path in EXECUTE_DIRECTORIES.iter() {
                if let Some(path) = existing(Path::new(path))? {
                    add(path, Access::Execute);
                }
            }
            for path in EXECUTE_ETC {
                if let Some(path) = existing(Path::new(path))? {
                    add(path, Access::Read);
                }
            }
        }
        Ok(rules)
    }

    /// The helper's specification.
    pub fn spec(&self) -> Result<Spec, SandboxError> {
        Ok(Spec {
            version: SPEC_VERSION,
            executable: self.executable.to_path_buf(),
            network: match self.network {
                NetworkAccess::None => Network::None,
                NetworkAccess::Proxy(port) => Network::Proxy(crate::Proxy {
                    port,
                    socket: self.proxy_socket(),
                }),
                NetworkAccess::Host => Network::Host,
            },
            status: self.status_socket(),
            stderr: self.stderr,
            terminal: self.terminal.is_some(),
            rules: self.rules()?,
        })
    }

    /// Bubblewrap's arguments up to and including the helper invocation.
    /// Arguments appended to the returned command go to the runtime.
    pub fn arguments(&self) -> Result<Vec<OsString>, SandboxError> {
        let mut args: Vec<OsString> = [
            "--unshare-all",
            "--die-with-parent",
            // A new session keeps the runtime off any controlling terminal.
            "--new-session",
            "--cap-drop",
            "ALL",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        if self.network == NetworkAccess::Host {
            args.push("--share-net".into());
        }
        let bind = |args: &mut Vec<OsString>, option: &str, path: &Path| {
            args.extend([option.into(), path.into(), path.into()]);
        };
        for path in SYSTEM_MOUNTS {
            let path = Path::new(path);
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let target =
                        std::fs::read_link(path).map_err(|source| inspect(path, source))?;
                    args.extend(["--symlink".into(), target.into(), path.into()]);
                }
                Ok(metadata) if metadata.is_dir() => bind(&mut args, "--ro-bind", path),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(inspect(path, source)),
            }
        }
        let etc = SYSTEM_ETC
            .iter()
            .chain(if self.execute { EXECUTE_ETC } else { &[] });
        let network_etc: &[&str] = if self.network == NetworkAccess::Host {
            NETWORK_ETC
        } else {
            &[]
        };
        for path in etc.chain(network_etc) {
            bind(&mut args, "--ro-bind-try", Path::new(path));
        }
        bind(&mut args, "--ro-bind", self.root);
        bind(&mut args, "--ro-bind", self.helper);
        bind(&mut args, "--bind", self.scratch);
        bind(&mut args, "--ro-bind", self.control);
        let mut writable = vec![self.scratch];
        if let Some((workspace, write)) = self.workspace {
            bind(
                &mut args,
                if write { "--bind" } else { "--ro-bind" },
                workspace,
            );
            if write {
                writable.push(workspace);
            }
        }
        let mounted = [
            Some(self.root),
            Some(self.scratch),
            self.workspace.map(|(w, _)| w),
        ];
        for hidden in self.hidden {
            if !mounted
                .iter()
                .flatten()
                .any(|base| hidden.starts_with(base))
            {
                continue;
            }
            match std::fs::symlink_metadata(hidden) {
                // An empty directory nobody may enter replaces it.
                Ok(metadata) if metadata.is_dir() => args.extend([
                    "--perms".into(),
                    "0000".into(),
                    "--tmpfs".into(),
                    hidden.into(),
                ]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && !writable.iter().any(|base| hidden.starts_with(base)) => {}
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(inspect(hidden, error));
                }
                // A file cannot be covered, and a missing path inside a writable
                // grant could be created by the runtime.
                Ok(_) | Err(_) => {
                    return Err(SandboxError::Exposed {
                        path: hidden.clone(),
                    });
                }
            }
        }
        args.extend(["--chdir".into(), self.cwd.into(), "--".into()]);
        args.extend([
            self.helper.into(),
            crate::HELPER_COMMAND.into(),
            self.spec()?.encode()?.into(),
        ]);
        Ok(args)
    }

    /// The bubblewrap command that runs the helper, which starts the runtime.
    #[cfg(not(target_os = "macos"))]
    pub fn command(&self) -> Result<std::process::Command, SandboxError> {
        std::fs::metadata(crate::BUBBLEWRAP).map_err(SandboxError::Bubblewrap)?;
        let mut command = std::process::Command::new(crate::BUBBLEWRAP);
        command.args(self.arguments()?);
        Ok(command)
    }

    /// The `sandbox-exec` command that starts the runtime under this
    /// profile's Seatbelt policy, in `cwd`. Without `stderr` the runtime's
    /// stderr is discarded, as the Linux helper does. With a `terminal` the
    /// command starts its own session, so it must not be put in a process
    /// group first.
    #[cfg(target_os = "macos")]
    pub fn command(&self) -> Result<std::process::Command, SandboxError> {
        std::fs::metadata(crate::SANDBOX_EXEC).map_err(SandboxError::SandboxExec)?;
        // `sandbox-exec` replaces itself with the runtime, so a terminal must
        // already be the controlling one: this executable's launcher makes
        // it so, then becomes `sandbox-exec`.
        let mut command = match self.terminal {
            Some(_) => {
                let mut command = std::process::Command::new(self.helper);
                command.args([crate::TERMINAL_COMMAND, crate::SANDBOX_EXEC]);
                command
            }
            None => std::process::Command::new(crate::SANDBOX_EXEC),
        };
        command
            .arg("-p")
            .arg(self.seatbelt()?)
            .arg(self.executable)
            .current_dir(self.cwd);
        if !self.stderr {
            command.stderr(std::process::Stdio::null());
        }
        Ok(command)
    }
}

/// The canonical form of an optional system path, or `None` when absent.
fn existing(path: &Path) -> Result<Option<PathBuf>, SandboxError> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(inspect(path, source)),
    }
}

fn inspect(path: &Path, source: std::io::Error) -> SandboxError {
    SandboxError::Inspect {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Layout {
        _directory: tempfile::TempDir,
        root: PathBuf,
        executable: PathBuf,
        scratch: PathBuf,
        control: PathBuf,
        project: PathBuf,
    }

    fn layout() -> Layout {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let root = base.join(uuid::Uuid::new_v4().to_string());
        let scratch = base.join(uuid::Uuid::new_v4().to_string());
        let control = scratch.join("control");
        let project = base.join(uuid::Uuid::new_v4().to_string());
        for path in [&root, &control, &project] {
            std::fs::create_dir_all(path).unwrap();
        }
        let executable = root.join("runtime");
        std::fs::write(&executable, "#!/bin/sh\n").unwrap();
        Layout {
            _directory: directory,
            root,
            executable,
            scratch,
            control,
            project,
        }
    }

    impl Layout {
        fn profile<'a>(
            &'a self,
            workspace: Option<(&'a Path, bool)>,
            hidden: &'a [PathBuf],
            execute: bool,
        ) -> Profile<'a> {
            Profile {
                helper: &self.executable,
                executable: &self.executable,
                root: &self.root,
                scratch: &self.scratch,
                control: &self.control,
                workspace,
                execute,
                hidden,
                cwd: &self.scratch,
                network: NetworkAccess::Proxy(1),
                stderr: false,
                terminal: None,
            }
        }
    }

    fn access(rules: &[Rule], path: &Path) -> Vec<Access> {
        rules
            .iter()
            .filter(|rule| rule.path == path)
            .map(|rule| rule.access)
            .collect()
    }

    #[test]
    fn grants_follow_capabilities() {
        let layout = layout();
        let read = layout
            .profile(Some((&layout.project, false)), &[], false)
            .rules()
            .unwrap();
        assert_eq!(access(&read, &layout.project), [Access::Read]);
        assert_eq!(access(&read, &layout.root), [Access::Execute]);
        assert_eq!(access(&read, &layout.scratch), [Access::Write]);
        assert!(
            read.iter()
                .all(|rule| rule.access != Access::Execute || rule.path == layout.root)
        );
        let write = layout
            .profile(Some((&layout.project, true)), &[], true)
            .rules()
            .unwrap();
        assert_eq!(access(&write, &layout.project), [Access::Write]);
        let bin = std::fs::canonicalize("/usr/bin").unwrap();
        assert_eq!(access(&write, &bin), [Access::Execute]);
        let none = layout.profile(None, &[], false).rules().unwrap();
        assert!(access(&none, &layout.project).is_empty());
        assert!(access(&none, &bin).is_empty());
    }

    #[test]
    fn only_a_terminal_session_may_open_its_controlling_terminal() {
        let layout = layout();
        let tty = Path::new("/dev/tty");
        let piped = layout.profile(None, &[], true);
        assert!(access(&piped.rules().unwrap(), tty).is_empty());
        assert!(!piped.spec().unwrap().terminal);
        let device = layout.scratch.join("pts");
        let terminal = Profile {
            terminal: Some(&device),
            ..piped
        };
        assert_eq!(access(&terminal.rules().unwrap(), tty), [Access::Device]);
        // The device itself is not mounted on Linux; the inherited copy is used.
        assert!(access(&terminal.rules().unwrap(), &device).is_empty());
        assert!(terminal.spec().unwrap().terminal);
    }

    #[test]
    fn arguments_mount_only_grants_and_cover_hidden_paths() {
        let layout = layout();
        let hidden = vec![layout.project.join("state")];
        std::fs::create_dir(&hidden[0]).unwrap();
        let args = layout
            .profile(Some((&layout.project, false)), &hidden, false)
            .arguments()
            .unwrap();
        let position = |needle: &Path| args.iter().position(|arg| arg == needle.as_os_str());
        let project = position(&layout.project).unwrap();
        assert_eq!(args[project - 1], "--ro-bind");
        let state = position(&hidden[0]).unwrap();
        assert!(
            state > project,
            "the cover must follow the grant it hides within"
        );
        assert_eq!(args[state - 1], "--tmpfs");
        assert_eq!(args[state - 3], "--perms");
        assert!(args.iter().any(|arg| arg == "--unshare-all"));
        assert!(!args.iter().any(|arg| arg == "--share-net"));
        let spec = crate::Spec::decode(args.last().unwrap().to_str().unwrap()).unwrap();
        assert_eq!(
            spec.network,
            Network::Proxy(crate::Proxy {
                port: 1,
                socket: layout.control.join("proxy.sock")
            })
        );
        let none = layout.profile(None, &hidden, false).arguments().unwrap();
        assert!(!none.iter().any(|arg| arg == layout.project.as_os_str()));
        assert!(!none.iter().any(|arg| arg == hidden[0].as_os_str()));
    }

    #[test]
    fn refuses_hidden_paths_it_cannot_cover() {
        let layout = layout();
        let missing = vec![layout.project.join(uuid::Uuid::new_v4().to_string())];
        assert!(matches!(
            layout
                .profile(Some((&layout.project, true)), &missing, false)
                .arguments(),
            Err(SandboxError::Exposed { .. })
        ));
        // A path that does not exist in a read-only grant cannot be created.
        layout
            .profile(Some((&layout.project, false)), &missing, false)
            .arguments()
            .unwrap();
        let file = vec![layout.project.join("file")];
        std::fs::write(&file[0], "").unwrap();
        assert!(matches!(
            layout
                .profile(Some((&layout.project, false)), &file, false)
                .arguments(),
            Err(SandboxError::Exposed { .. })
        ));
    }

    #[test]
    fn spec_round_trips_and_rejects_other_versions() {
        let layout = layout();
        let spec = layout.profile(None, &[], false).spec().unwrap();
        assert_eq!(Spec::decode(&spec.encode().unwrap()).unwrap(), spec);
        let other = Spec {
            version: SPEC_VERSION + 1,
            ..spec
        };
        assert!(matches!(
            Spec::decode(&other.encode().unwrap()),
            Err(SandboxError::Version { .. })
        ));
        assert!(matches!(Spec::decode("!"), Err(SandboxError::Base64(_))));
        assert!(matches!(Spec::decode("AAAA"), Err(SandboxError::Decode(_))));
    }

    #[test]
    fn a_single_executable_without_a_proxy_gets_no_network_and_no_directory() {
        let layout = layout();
        let profile = Profile {
            root: &layout.executable,
            network: NetworkAccess::None,
            ..layout.profile(None, &[], false)
        };
        let rules = profile.rules().unwrap();
        assert_eq!(access(&rules, &layout.executable), [Access::Execute]);
        assert!(access(&rules, &layout.root).is_empty());
        let args = profile.arguments().unwrap();
        assert!(!args.iter().any(|arg| arg == layout.root.as_os_str()));
        let spec = crate::Spec::decode(args.last().unwrap().to_str().unwrap()).unwrap();
        assert_eq!(spec.network, Network::None);
        assert!(!args.iter().any(|arg| arg == "--share-net"));
    }

    #[test]
    fn only_a_host_network_grant_shares_the_network() {
        let layout = layout();
        let profile = Profile {
            network: NetworkAccess::Host,
            ..layout.profile(None, &[], false)
        };
        let args = profile.arguments().unwrap();
        assert!(args.iter().any(|arg| arg == "--share-net"));
        let spec = crate::Spec::decode(args.last().unwrap().to_str().unwrap()).unwrap();
        assert_eq!(spec.network, Network::Host);
        assert!(
            !args
                .iter()
                .any(|arg| arg == layout.control.join("proxy.sock").as_os_str())
        );
    }
}
