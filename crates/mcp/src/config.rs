//! Static validation of user-authored MCP server entries.
use crate::{McpError, SERVER_CAPABILITIES};
use a206_core::{Capability, McpServerConfig};
use std::{collections::BTreeSet, path::Path};

/// Longest per-call timeout a server entry may request, in seconds.
pub const MAX_TIMEOUT_SECONDS: u64 = 1800;

/// Check every entry, and that names are unique. Paths are checked again,
/// against the filesystem, when a server starts.
pub fn validate(servers: &[McpServerConfig]) -> Result<(), McpError> {
    let mut names = BTreeSet::new();
    for server in servers {
        validate_one(server)?;
        if !names.insert(server.name.as_str()) {
            return Err(McpError::DuplicateName);
        }
    }
    Ok(())
}

fn validate_one(server: &McpServerConfig) -> Result<(), McpError> {
    if !valid_name(&server.name) {
        return Err(McpError::Name);
    }
    if !Path::new(&server.executable).is_absolute()
        || !Path::new(&server.runtime_root).is_absolute()
    {
        return Err(McpError::RelativePath);
    }
    let texts = [&server.executable, &server.runtime_root]
        .into_iter()
        .chain(&server.args)
        .chain(server.env.keys())
        .chain(server.env.values());
    for text in texts {
        if text.contains('\0') {
            return Err(McpError::Nul);
        }
    }
    if server
        .env
        .keys()
        .any(|key| key.is_empty() || key.contains('='))
    {
        return Err(McpError::Environment);
    }
    if let Some(&capability) = server
        .capabilities
        .iter()
        .find(|c| !SERVER_CAPABILITIES.contains(c))
    {
        return Err(McpError::Capability { capability });
    }
    let reads = server.capabilities.contains(&Capability::ReadWorkspace);
    if server.capabilities.contains(&Capability::WriteWorkspace) && !reads {
        return Err(McpError::WriteWithoutRead);
    }
    match (reads, server.project) {
        (true, None) => return Err(McpError::ProjectRequired),
        (false, Some(_)) => return Err(McpError::ProjectUnused),
        _ => {}
    }
    if server
        .timeout_seconds
        .is_some_and(|t| t == 0 || t > MAX_TIMEOUT_SECONDS)
    {
        return Err(McpError::TimeoutRange {
            max: MAX_TIMEOUT_SECONDS,
        });
    }
    Ok(())
}

/// Lowercase ASCII letters and digits, separated by single hyphens. Names never
/// contain `_`, so `mcp__<server>__<tool>` style identifiers stay unambiguous.
fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{Id, ToolPolicy};
    fn server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            enabled: true,
            executable: "/opt/server/bin/server".into(),
            runtime_root: "/opt/server".into(),
            args: vec![],
            env: Default::default(),
            capabilities: Default::default(),
            project: None,
            default_policy: ToolPolicy::Ask,
            tools: Default::default(),
            timeout_seconds: None,
        }
    }
    #[test]
    fn accepts_a_minimal_entry() {
        validate(&[server("files-2")]).unwrap();
    }
    #[test]
    fn rejects_ambiguous_or_unsafe_names() {
        for name in [
            "",
            "-a",
            "a-",
            "a--b",
            "A",
            "a_b",
            "a/b",
            "..",
            &"a".repeat(33),
        ] {
            assert!(
                matches!(validate(&[server(name)]), Err(McpError::Name)),
                "{name:?}"
            );
        }
        assert!(matches!(
            validate(&[server("a"), server("a")]),
            Err(McpError::DuplicateName)
        ));
    }
    #[test]
    fn rejects_authority_mcp_servers_cannot_hold() {
        let mut entry = server("a");
        entry.capabilities = [Capability::UseSecret].into();
        assert!(matches!(
            validate(&[entry]),
            Err(McpError::Capability {
                capability: Capability::UseSecret
            })
        ));
    }
    #[test]
    fn workspace_access_is_bound_to_exactly_one_project() {
        let mut entry = server("a");
        entry.capabilities = [Capability::WriteWorkspace].into();
        assert!(matches!(
            validate(std::slice::from_ref(&entry)),
            Err(McpError::WriteWithoutRead)
        ));
        entry.capabilities.insert(Capability::ReadWorkspace);
        assert!(matches!(
            validate(std::slice::from_ref(&entry)),
            Err(McpError::ProjectRequired)
        ));
        entry.project = Some(Id::new_v4());
        validate(std::slice::from_ref(&entry)).unwrap();
        entry.capabilities.clear();
        assert!(matches!(validate(&[entry]), Err(McpError::ProjectUnused)));
    }
    #[test]
    fn rejects_relative_paths_nul_and_bad_environment() {
        let mut entry = server("a");
        entry.executable = "server".into();
        assert!(matches!(validate(&[entry]), Err(McpError::RelativePath)));
        let mut entry = server("a");
        entry.args.push("a\0b".into());
        assert!(matches!(validate(&[entry]), Err(McpError::Nul)));
        let mut entry = server("a");
        entry.env.insert("A=B".into(), "c".into());
        assert!(matches!(validate(&[entry]), Err(McpError::Environment)));
        let mut entry = server("a");
        entry.timeout_seconds = Some(0);
        assert!(matches!(
            validate(&[entry]),
            Err(McpError::TimeoutRange { .. })
        ));
    }
}
