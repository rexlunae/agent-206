//! The macOS form of a [`Profile`]: one Seatbelt (SBPL) policy that
//! `sandbox-exec` applies before it executes the runtime, inherited by every
//! process the runtime starts.
//!
//! The policy denies by default and then allows what the Linux layers allow:
//! the system libraries, the runtime's installation, its private scratch
//! directory, the granted project, `/usr/bin` and `/bin` with `execute`, and
//! the network the profile names. Hidden paths are denied last, so they stay
//! invisible even inside a granted directory; Seatbelt applies the last
//! matching rule. Unlike bubblewrap, Seatbelt checks paths rather than
//! mounts, so a hidden path that is a file or does not exist yet is covered
//! too.
//!
//! Generating the text needs nothing from macOS, so it is tested everywhere.
use crate::{SandboxError, profile::NetworkAccess, profile::Profile};
use std::{collections::BTreeSet, path::Path};

/// System locations a process needs to load and link; readable and mappable,
/// never executable as programs.
const SYSTEM: &[&str] = &[
    "/System/Library",
    "/System/iOSSupport/System/Library",
    "/usr/lib",
    "/usr/share",
    "/Library/Apple",
    "/private/var/db/dyld",
];
/// Directories whose programs the Execute capability allows, as on Linux.
const EXECUTE_DIRECTORIES: &[&str] = &["/usr/bin", "/bin"];
/// `/bin/sh` picks the shell it runs from this link.
const SHELL_SELECTION: &str = "/private/var/select/sh";

/// Quote `path` as an SBPL string literal. Control characters are refused
/// rather than escaped, so no path can end a rule early.
fn quote(path: &Path) -> Result<String, SandboxError> {
    let text = path.to_str().ok_or_else(|| SandboxError::Path {
        path: path.to_path_buf(),
    })?;
    if text.chars().any(char::is_control) {
        return Err(SandboxError::Path {
            path: path.to_path_buf(),
        });
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        if matches!(c, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    Ok(quoted)
}

impl Profile<'_> {
    /// The Seatbelt policy for this launch. All paths must be canonical, as
    /// Seatbelt compares resolved paths.
    pub fn seatbelt(&self) -> Result<String, SandboxError> {
        let root = quote(self.root)?;
        let scratch = quote(self.scratch)?;
        let mut policy = String::from(
            r#"(version 1)
(deny default)
(allow process-fork)
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow system-mac-syscall (mac-policy-name "vnguard"))
(allow system-mac-syscall (require-all (mac-policy-name "Sandbox") (mac-syscall-number 67)))
(allow file-read* (literal "/"))
(allow file-read-metadata (literal "/var") (literal "/tmp") (literal "/etc"))
(allow mach-lookup (global-name "com.apple.system.logger") (global-name "com.apple.system.opendirectoryd"))
(allow file-read* file-write* (literal "/dev/null"))
(allow file-read* (literal "/dev/urandom") (literal "/dev/random") (literal "/dev/zero"))
"#,
        );
        let mut rule = |text: String| policy.push_str(&text);
        let system = SYSTEM
            .iter()
            .map(|path| quote(Path::new(path)).map(|path| format!(" (subpath {path})")))
            .collect::<Result<String, _>>()?;
        rule(format!(
            "(allow file-read* file-map-executable{system} (subpath {root}) (subpath {scratch}))\n"
        ));
        rule(format!("(allow process-exec (subpath {root}))\n"));
        rule(format!("(allow file-write* (subpath {scratch}))\n"));
        let mut traversed: Vec<&Path> = vec![self.root, self.scratch, self.cwd];
        traversed.extend(SYSTEM.iter().map(Path::new));
        if let Some((workspace, writable)) = self.workspace {
            let quoted = quote(workspace)?;
            rule(format!("(allow file-read* (subpath {quoted}))\n"));
            if writable {
                rule(format!("(allow file-write* (subpath {quoted}))\n"));
            }
            traversed.push(workspace);
        }
        if self.execute {
            let directories = EXECUTE_DIRECTORIES
                .iter()
                .map(|path| quote(Path::new(path)).map(|path| format!(" (subpath {path})")))
                .collect::<Result<String, _>>()?;
            rule(format!(
                "(allow process-exec file-read* file-map-executable{directories})\n"
            ));
            rule(format!(
                "(allow file-read* (literal {}))\n",
                quote(Path::new(SHELL_SELECTION))?
            ));
            traversed.push(Path::new(SHELL_SELECTION));
        }
        match self.network {
            NetworkAccess::None => {}
            NetworkAccess::Proxy(port) => rule(format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))\n"
            )),
            NetworkAccess::Host => rule(
                r#"(allow network-outbound)
(allow system-socket)
(allow mach-lookup (global-name "com.apple.dnssd.service") (global-name "com.apple.SystemConfiguration.configd") (global-name "com.apple.trustd.agent"))
(allow file-read* (subpath "/private/etc/ssl") (literal "/private/etc/resolv.conf") (literal "/private/etc/hosts") (literal "/private/etc/services") (literal "/private/etc/protocols"))
"#
                .into(),
            ),
        }
        // Resolving a path needs the metadata of each ancestor, and only that:
        // nothing else elsewhere in the filesystem becomes visible.
        let ancestors: BTreeSet<&Path> = traversed
            .iter()
            .flat_map(|path| path.ancestors().skip(1))
            .collect();
        for ancestor in ancestors {
            rule(format!(
                "(allow file-read-metadata (literal {}))\n",
                quote(ancestor)?
            ));
        }
        // The control directory belongs to the gateway, as on Linux.
        rule(format!(
            "(deny file-write* (subpath {}))\n",
            quote(self.control)?
        ));
        // Last, so they win over every grant above.
        for hidden in self.hidden {
            rule(format!(
                "(deny file-read* file-write* (subpath {}))\n",
                quote(hidden)?
            ));
        }
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Paths {
        root: PathBuf,
        scratch: PathBuf,
        control: PathBuf,
        project: PathBuf,
        hidden: Vec<PathBuf>,
    }

    fn paths() -> Paths {
        let base = PathBuf::from("/private/var/folders").join(uuid::Uuid::new_v4().to_string());
        let scratch = base.join("scratch");
        let project = base.join("project");
        Paths {
            root: base.join("root"),
            control: scratch.join("control"),
            hidden: vec![project.join("state")],
            scratch,
            project,
        }
    }

    impl Paths {
        fn profile<'a>(
            &'a self,
            workspace: Option<(&'a Path, bool)>,
            execute: bool,
        ) -> Profile<'a> {
            Profile {
                helper: &self.root,
                executable: &self.root,
                root: &self.root,
                scratch: &self.scratch,
                control: &self.control,
                workspace,
                execute,
                hidden: &self.hidden,
                cwd: &self.scratch,
                network: NetworkAccess::None,
                stderr: false,
            }
        }
    }

    fn quoted(path: &Path) -> String {
        quote(path).unwrap()
    }

    #[test]
    fn grants_follow_capabilities() {
        let paths = paths();
        let project = quoted(&paths.project);
        let none = paths.profile(None, false).seatbelt().unwrap();
        assert!(none.starts_with("(version 1)\n(deny default)\n"));
        assert!(!none.contains(&format!("(subpath {project})")));
        assert!(!none.contains("\"/usr/bin\""));
        assert!(!none.contains("network-outbound"));

        let read = paths
            .profile(Some((&paths.project, false)), false)
            .seatbelt()
            .unwrap();
        assert!(read.contains(&format!("(allow file-read* (subpath {project}))")));
        assert!(!read.contains(&format!("(allow file-write* (subpath {project}))")));

        let write = paths
            .profile(Some((&paths.project, true)), true)
            .seatbelt()
            .unwrap();
        assert!(write.contains(&format!("(allow file-write* (subpath {project}))")));
        assert!(write.contains(
            "(allow process-exec file-read* file-map-executable (subpath \"/usr/bin\") (subpath \"/bin\"))"
        ));
        // Only the runtime's own installation runs without Execute.
        let exec = |policy: &str| {
            policy
                .lines()
                .filter(|line| line.contains("process-exec"))
                .count()
        };
        assert_eq!(exec(&none), 1);
        assert_eq!(exec(&write), 2);
    }

    #[test]
    fn hidden_paths_and_control_are_denied_after_every_grant() {
        let paths = paths();
        let policy = paths
            .profile(Some((&paths.project, true)), true)
            .seatbelt()
            .unwrap();
        let line = |needle: &str| {
            policy
                .lines()
                .position(|line| line == needle)
                .unwrap_or_else(|| panic!("{needle} missing from\n{policy}"))
        };
        let hidden = line(&format!(
            "(deny file-read* file-write* (subpath {}))",
            quoted(&paths.hidden[0])
        ));
        let control = line(&format!(
            "(deny file-write* (subpath {}))",
            quoted(&paths.control)
        ));
        let last_allow = policy
            .lines()
            .collect::<Vec<_>>()
            .iter()
            .rposition(|line| line.starts_with("(allow"))
            .unwrap();
        assert!(hidden > last_allow && control > last_allow);
    }

    #[test]
    fn network_follows_the_profile() {
        let paths = paths();
        let with = |network| {
            Profile {
                network,
                ..paths.profile(None, false)
            }
            .seatbelt()
            .unwrap()
        };
        let proxy = with(NetworkAccess::Proxy(4242));
        assert!(proxy.contains("(allow network-outbound (remote ip \"localhost:4242\"))"));
        assert!(!proxy.contains("(allow network-outbound)\n"));
        let host = with(NetworkAccess::Host);
        assert!(host.contains("(allow network-outbound)\n"));
        assert!(host.contains("com.apple.dnssd.service"));
    }

    #[test]
    fn ancestors_get_metadata_only() {
        let paths = paths();
        let policy = paths
            .profile(Some((&paths.project, false)), false)
            .seatbelt()
            .unwrap();
        let parent = quoted(paths.project.parent().unwrap());
        assert!(policy.contains(&format!("(allow file-read-metadata (literal {parent}))")));
        assert!(!policy.contains(&format!("(subpath {parent})")));
    }

    #[test]
    fn quoting_cannot_end_a_rule() {
        assert_eq!(
            quoted(Path::new(r#"/a "b" \c"#)),
            r#""/a \"b\" \\c""#.to_string()
        );
        let paths = paths();
        let hostile = paths.project.join("x\")) (allow default) (\"\n");
        let profile = Profile {
            root: &hostile,
            ..paths.profile(None, false)
        };
        assert!(matches!(profile.seatbelt(), Err(SandboxError::Path { .. })));
        let hostile = paths.project.join("x\")) (allow default");
        let profile = Profile {
            root: &hostile,
            ..paths.profile(None, false)
        };
        let policy = profile.seatbelt().unwrap();
        assert!(!policy.lines().any(|line| line == "(allow default)"));
        assert!(policy.contains(r#"x\")) (allow default"#));
    }
}
