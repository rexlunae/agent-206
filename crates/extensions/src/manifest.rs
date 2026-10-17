//! Extension manifests (`plugin.toml`), validated before anything runs.
use a206_core::{
    Capability, PluginSummary,
    extension::{Args, PanelSpec, ParamKind, ToolSpec},
};
use a206_extension_abi::{ABI, valid_name};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Most tools, panels or parameters one manifest may declare.
pub const MAX_DECLARATIONS: usize = 64;
/// Most network destinations one manifest may declare.
pub const MAX_HOSTS: usize = 32;
/// The `hosts` entry that allows any public HTTPS host.
pub const ANY_HOST: &str = "*";

/// Why a manifest was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("extension manifest is not valid TOML for ABI {ABI}")]
    Parse(#[source] toml::de::Error),
    #[error("extension ABI {found} is not supported; this host implements ABI {ABI}")]
    Abi { found: u32 },
    #[error("extension id `{0}` is invalid")]
    Id(String),
    #[error("extension manifest field `{0}` is empty")]
    Empty(&'static str),
    #[error("{what} name `{name}` is invalid")]
    Name { what: &'static str, name: String },
    #[error("{what} `{name}` is declared twice")]
    Duplicate { what: &'static str, name: String },
    #[error("more than {MAX_DECLARATIONS} {what} declarations")]
    TooMany { what: &'static str },
    #[error("`{item}` uses {capability:?}, which the manifest does not request")]
    Undeclared {
        item: String,
        capability: Capability,
    },
    #[error(
        "the network permission needs declared `hosts`, and `hosts` needs the network permission"
    )]
    Hosts,
    #[error("network host `{0}` is not a lowercase DNS name or `*` alone")]
    Host(String),
    #[error("more than {MAX_HOSTS} network hosts")]
    TooManyHosts,
}

/// Why a call does not match the manifest's declarations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgError {
    #[error("extension has no tool `{0}`")]
    UnknownTool(String),
    #[error("extension has no panel `{0}`")]
    UnknownPanel(String),
    #[error("tool has no parameter `{0}`")]
    Unknown(String),
    #[error("parameter `{0}` is required")]
    Missing(String),
    #[error("parameter `{name}` must be {}", expected.label())]
    Type { name: String, expected: ParamKind },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub abi: u32,
    /// The union of what the tools and panels may use; requesting grants nothing.
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub panels: Vec<PanelSpec>,
    /// Where `network` may connect: exact host names, or `*` alone for any
    /// public HTTPS host. Shown to the user beside the permission.
    #[serde(default)]
    pub hosts: Vec<String>,
}
impl Manifest {
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let manifest: Self = toml::from_str(text).map_err(ManifestError::Parse)?;
        manifest.validate()?;
        Ok(manifest)
    }
    fn validate(&self) -> Result<(), ManifestError> {
        if self.abi != ABI {
            return Err(ManifestError::Abi { found: self.abi });
        }
        if !(1..=128).contains(&self.id.len())
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        {
            return Err(ManifestError::Id(self.id.clone()));
        }
        for (field, value) in [
            ("name", &self.name),
            ("description", &self.description),
            ("version", &self.version),
        ] {
            if value.trim().is_empty() {
                return Err(ManifestError::Empty(field));
            }
        }
        let tools = self
            .tools
            .iter()
            .map(|tool| (tool.name.as_str(), &tool.capabilities));
        let panels = self
            .panels
            .iter()
            .map(|panel| (panel.id.as_str(), &panel.capabilities));
        for (what, items) in [
            ("tool", tools.collect::<Vec<_>>()),
            ("panel", panels.collect()),
        ] {
            names(what, items.iter().map(|(name, _)| *name))?;
            for (name, capabilities) in items {
                if let Some(capability) = capabilities.difference(&self.capabilities).next() {
                    return Err(ManifestError::Undeclared {
                        item: name.into(),
                        capability: *capability,
                    });
                }
            }
        }
        for tool in &self.tools {
            names("parameter", tool.params.iter().map(|p| p.name.as_str()))?;
        }
        self.validate_hosts()
    }
    fn validate_hosts(&self) -> Result<(), ManifestError> {
        if self.capabilities.contains(&Capability::Network) == self.hosts.is_empty() {
            return Err(ManifestError::Hosts);
        }
        if self.hosts.len() > MAX_HOSTS {
            return Err(ManifestError::TooManyHosts);
        }
        let any = self.hosts.iter().any(|host| host == ANY_HOST);
        let mut seen = BTreeSet::new();
        for host in &self.hosts {
            let valid = if any {
                self.hosts.len() == 1
            } else {
                valid_host(host)
            };
            if !valid || !seen.insert(host) {
                return Err(ManifestError::Host(host.clone()));
            }
        }
        Ok(())
    }

    pub fn tool(&self, name: &str) -> Result<&ToolSpec, ArgError> {
        self.tools
            .iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| ArgError::UnknownTool(name.into()))
    }
    pub fn panel(&self, id: &str) -> Result<&PanelSpec, ArgError> {
        self.panels
            .iter()
            .find(|panel| panel.id == id)
            .ok_or_else(|| ArgError::UnknownPanel(id.into()))
    }
    /// What a client shows for this extension, with the user's choices.
    pub fn summary(&self, enabled: bool, granted: &BTreeSet<Capability>) -> PluginSummary {
        PluginSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            capabilities: self.capabilities.clone(),
            enabled,
            version: self.version.clone(),
            granted: granted.intersection(&self.capabilities).copied().collect(),
            tools: self.tools.clone(),
            panels: self.panels.clone(),
            hosts: self.hosts.clone(),
        }
    }
}

/// Whether declared `hosts` let `network` connect to `host` (lowercase, as
/// URL parsing leaves it).
pub fn allows_host(hosts: &[String], host: &str) -> bool {
    hosts
        .iter()
        .any(|allowed| allowed == ANY_HOST || allowed == host)
}

/// A lowercase DNS name: dot-separated labels of letters, digits and
/// inner hyphens. IP literals and wildcards within a name are refused.
fn valid_host(host: &str) -> bool {
    let label = |label: &str| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    host.len() <= 253
        && host.split('.').all(label)
        && host.contains('.')
        && !host
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
}

fn names<'a>(
    what: &'static str,
    names: impl Iterator<Item = &'a str>,
) -> Result<(), ManifestError> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !valid_name(name) {
            return Err(ManifestError::Name {
                what,
                name: name.into(),
            });
        }
        if !seen.insert(name) {
            return Err(ManifestError::Duplicate {
                what,
                name: name.into(),
            });
        }
        if seen.len() > MAX_DECLARATIONS {
            return Err(ManifestError::TooMany { what });
        }
    }
    Ok(())
}

/// Check arguments against a tool's declared parameters before the guest
/// sees them: no unknown names, required ones present, kinds exact.
pub fn check_args(tool: &ToolSpec, args: &Args) -> Result<(), ArgError> {
    if let Some(name) = args
        .keys()
        .find(|name| !tool.params.iter().any(|p| &p.name == *name))
    {
        return Err(ArgError::Unknown(name.clone()));
    }
    for param in &tool.params {
        match args.get(&param.name).and_then(|value| value.kind()) {
            None if param.required => return Err(ArgError::Missing(param.name.clone())),
            None => {}
            Some(kind) if param.kind.accepts(kind) => {}
            Some(_) => {
                return Err(ArgError::Type {
                    name: param.name.clone(),
                    expected: param.kind,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::extension::Value;

    fn manifest(extra: &str) -> Result<Manifest, ManifestError> {
        Manifest::parse(&format!(
            "id = \"test.ext\"\nname = \"Test\"\ndescription = \"d\"\nversion = \"1\"\nabi = {ABI}\ncapabilities = [\"read_workspace\"]\n{extra}"
        ))
    }

    #[test]
    fn bundled_manifests_are_valid() {
        assert!(!crate::builtins().unwrap().is_empty());
    }

    #[test]
    fn network_destinations_are_declared_exactly() {
        let network = |hosts: &str| {
            Manifest::parse(&format!(
                "id = \"test.ext\"\nname = \"Test\"\ndescription = \"d\"\nversion = \"1\"\nabi = {ABI}\ncapabilities = [\"network\"]\nhosts = {hosts}"
            ))
        };
        let host = format!("h{}.example.org", std::process::id());
        let parsed = network(&format!("[\"{host}\"]")).unwrap();
        assert!(allows_host(&parsed.hosts, &host));
        assert!(!allows_host(&parsed.hosts, &format!("x.{host}")));
        assert!(allows_host(&network("[\"*\"]").unwrap().hosts, &host));
        assert_eq!(parsed.summary(false, &BTreeSet::new()).hosts, parsed.hosts);
        for bad in [
            "[]".to_string(),
            format!("[\"{}\"]", host.to_uppercase()),
            format!("[\"*.{host}\"]"),
            format!("[\"*\", \"{host}\"]"),
            format!("[\"{host}\", \"{host}\"]"),
            "[\"127.0.0.1\"]".into(),
            "[\"localhost\"]".into(),
            format!("[\"-{host}\"]"),
            format!("[\"{host}:443\"]"),
        ] {
            assert!(network(&bad).is_err(), "{bad}");
        }
        assert!(matches!(
            manifest(&format!("hosts = [\"{host}\"]")),
            Err(ManifestError::Hosts)
        ));
        let many: Vec<_> = (0..=MAX_HOSTS)
            .map(|i| format!("\"h{i}.{host}\""))
            .collect();
        assert!(matches!(
            network(&format!("[{}]", many.join(","))),
            Err(ManifestError::TooManyHosts)
        ));
    }

    #[test]
    fn rejects_what_review_cannot_rely_on() {
        assert!(matches!(
            Manifest::parse(
                "id = \"x\"\nname = \"n\"\ndescription = \"d\"\nversion = \"1\"\nabi = 1"
            ),
            Err(ManifestError::Abi { found: 1 })
        ));
        assert!(matches!(
            manifest("unexpected = true"),
            Err(ManifestError::Parse(_))
        ));
        assert!(matches!(
            manifest("[[tools]]\nname = \"Bad-Name\"\ndescription = \"d\""),
            Err(ManifestError::Name { what: "tool", .. })
        ));
        assert!(matches!(
            manifest(
                "[[tools]]\nname = \"a\"\ndescription = \"d\"\n[[tools]]\nname = \"a\"\ndescription = \"d\""
            ),
            Err(ManifestError::Duplicate { what: "tool", .. })
        ));
        assert!(matches!(
            manifest("[[panels]]\nid = \"p\"\ntitle = \"t\"\ncapabilities = [\"execute\"]"),
            Err(ManifestError::Undeclared {
                capability: Capability::Execute,
                ..
            })
        ));
    }

    #[test]
    fn arguments_match_declarations_exactly() {
        let manifest = manifest(
            "[[tools]]\nname = \"t\"\ndescription = \"d\"\n[[tools.params]]\nname = \"path\"\ntype = \"string\"\ndescription = \"d\"\nrequired = true\n[[tools.params]]\nname = \"ratio\"\ntype = \"number\"\ndescription = \"d\"",
        )
        .unwrap();
        let tool = manifest.tool("t").unwrap();
        let args = |pairs: &[(&str, Value)]| -> Args {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        };
        let path = ("path", Value::Str("src".into()));
        assert_eq!(check_args(tool, &args(std::slice::from_ref(&path))), Ok(()));
        assert_eq!(
            check_args(tool, &args(&[path.clone(), ("ratio", Value::Int(2))])),
            Ok(())
        );
        assert_eq!(
            check_args(tool, &args(&[])),
            Err(ArgError::Missing("path".into()))
        );
        assert_eq!(
            check_args(tool, &args(&[("path", Value::Int(1))])),
            Err(ArgError::Type {
                name: "path".into(),
                expected: ParamKind::String
            })
        );
        assert_eq!(
            check_args(tool, &args(&[path, ("other", Value::Nil)])),
            Err(ArgError::Unknown("other".into()))
        );
        assert_eq!(
            manifest.tool("missing"),
            Err(ArgError::UnknownTool("missing".into()))
        );
    }
}
