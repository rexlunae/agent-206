//! The tool-host abstraction the gateway run engine calls.
//!
//! A [`ToolHost`] lists tools with JSON Schemas (the provider-facing standard)
//! and runs one call at a time. Hosts never decide whether a call may run:
//! the gateway checks the user's per-tool permission, the host's own policy
//! and the conversation's grant first, and asks the user for `Ask` tools.
//! Hosts receive no handle to the gateway, the vault or approvals, so a tool
//! cannot widen its own authority or approve a call.
//!
//! The WASM extension adapter (`crate::extension_tools`), the MCP client
//! adapter (`crate::mcp_tools`) and native exec (`crate::exec_tool`) are
//! registered by default.
use a206_core::{
    Capability, ErrorKind, ErrorReport, Id, Thread, ToolList, ToolOffer, ToolPermission,
    UnavailableTools,
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

/// Longest model-facing tool name; the strictest limit of the supported
/// provider protocols.
pub const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 4096;
const MAX_SCHEMA: usize = 32 * 1024;
/// Most tools offered to a model in one request.
pub const MAX_TOOLS: usize = 128;
/// Longest tool result text kept in the conversation and sent to the model.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// One tool a host offers.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// The model-facing name, unique across hosts. Hosts namespace their own
    /// tools (for example `mcp__server__tool`).
    pub name: String,
    pub description: String,
    /// A JSON Schema for the arguments object.
    pub parameters: Value,
    /// Capabilities the call uses; each must be in the conversation's grant.
    pub capabilities: BTreeSet<Capability>,
    /// The host's own policy, for example a user-configured MCP server entry.
    /// The gateway never enforces anything less restrictive than this.
    pub permission: ToolPermission,
}

/// The authority a call runs under.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolScope {
    pub thread: Id,
    /// The root of the conversation's project, if it has one.
    pub workspace: Option<PathBuf>,
    /// The conversation's grant within its mode's ceiling. A host must not use
    /// anything outside it, even for tools that declared nothing.
    pub capabilities: BTreeSet<Capability>,
}

/// What a host lists for a scope.
#[derive(Debug, Clone, Default)]
pub struct ToolListing {
    pub tools: Vec<ToolSpec>,
    /// Sources that could not list their tools, such as a server that failed
    /// to start; the rest of the listing is still usable.
    pub unavailable: Vec<UnavailableTools>,
}

/// One authorized call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRequest {
    pub call: String,
    pub name: String,
    pub arguments: Map<String, Value>,
    /// The user approved this exact call. Hosts with their own approval
    /// requirement (MCP `ask` tools) may rely on it; it is never set for a
    /// call the user did not approve.
    pub approved: bool,
}

/// A tool's result. `is_error` is the tool's own report of failure (for
/// example an MCP `isError` result), not a host failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

/// A source of tools.
///
/// Dropping a future returned by [`ToolHost::call`] cancels the call: the host
/// must stop the work it started (close the session, kill the worker), not
/// leave it running detached.
pub trait ToolHost: Send + Sync + 'static {
    fn list<'a>(&'a self, scope: &'a ToolScope) -> BoxFuture<'a, ToolListing>;
    /// Run one call. A host failure is an [`ErrorReport`] so its kind and
    /// cause chain survive the extension or server boundary intact.
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>>;
}

/// Why a listed tool is not offered.
#[derive(Debug, thiserror::Error)]
pub enum OfferError {
    #[error("tool names must be 1–64 ASCII letters, digits, `_` or `-`")]
    Name,
    #[error("tool `{0}` is offered by more than one host")]
    Duplicate(String),
    #[error("tool `{0}` has a description or schema that is too large")]
    TooLarge(String),
    #[error("tool `{0}` has a parameter schema that is not an object schema")]
    Schema(String),
    #[error("more than {MAX_TOOLS} tools are available; the rest are not offered")]
    TooMany,
}
impl crate::error::Classified for OfferError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(ErrorKind::Protocol)
    }
}

/// A tool the gateway can route a call to.
#[derive(Clone)]
pub(crate) struct Routed {
    pub spec: ToolSpec,
    pub host: Arc<dyn ToolHost>,
}

/// Every host's tools for one scope, keyed by model-facing name.
#[derive(Clone, Default)]
pub(crate) struct Catalog {
    pub tools: BTreeMap<String, Routed>,
    pub unavailable: Vec<UnavailableTools>,
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Collect and validate the tools of every host. A name offered twice is
/// offered by neither host: routing it to either would be a guess.
pub(crate) async fn catalog(hosts: &[Arc<dyn ToolHost>], scope: &ToolScope) -> Catalog {
    let mut catalog = Catalog::default();
    let mut duplicates = BTreeSet::new();
    let refuse = |catalog: &mut Catalog, error: OfferError| {
        catalog.unavailable.push(UnavailableTools {
            source: "gateway".into(),
            error: crate::error::handle("list_tools", &error),
        });
    };
    for host in hosts {
        let listing = host.list(scope).await;
        catalog.unavailable.extend(listing.unavailable);
        for spec in listing.tools {
            if !valid_name(&spec.name) {
                refuse(&mut catalog, OfferError::Name);
                continue;
            }
            let schema = serde_json::to_vec(&spec.parameters).map_or(usize::MAX, |s| s.len());
            if spec.description.len() > MAX_DESCRIPTION || schema > MAX_SCHEMA {
                refuse(&mut catalog, OfferError::TooLarge(spec.name));
                continue;
            }
            if spec.parameters.get("type").and_then(Value::as_str) != Some("object") {
                refuse(&mut catalog, OfferError::Schema(spec.name));
                continue;
            }
            if duplicates.contains(&spec.name) || catalog.tools.contains_key(&spec.name) {
                catalog.tools.remove(&spec.name);
                if duplicates.insert(spec.name.clone()) {
                    refuse(&mut catalog, OfferError::Duplicate(spec.name));
                }
                continue;
            }
            catalog.tools.insert(
                spec.name.clone(),
                Routed {
                    spec,
                    host: host.clone(),
                },
            );
        }
    }
    if catalog.tools.len() > MAX_TOOLS {
        let keep: BTreeMap<_, _> = std::mem::take(&mut catalog.tools)
            .into_iter()
            .take(MAX_TOOLS)
            .collect();
        catalog.tools = keep;
        refuse(&mut catalog, OfferError::TooMany);
    }
    catalog
}

/// The capabilities a conversation's tools may use: its grant within its
/// mode's ceiling, intersected with every ancestor's current grant within
/// that ancestor's ceiling. A delegated thread was attenuated when it was
/// created; intersecting again at each decision means a grant its parent
/// loses later is lost by the child too.
pub(crate) fn scope_capabilities(thread: &Thread, ancestors: &[&Thread]) -> BTreeSet<Capability> {
    let own = |t: &Thread| -> BTreeSet<Capability> {
        let ceiling = t.mode.ceiling();
        t.grant
            .capabilities
            .iter()
            .filter(|c| ceiling.contains(c))
            .copied()
            .collect()
    };
    ancestors.iter().fold(own(thread), |granted, ancestor| {
        granted.intersection(&own(ancestor)).copied().collect()
    })
}

/// The permission the gateway enforces for `spec` in `thread`: the most
/// restrictive of the user's setting (unset is deny), every ancestor's
/// setting, the host's policy, and deny if any capability is missing from
/// the grant of the thread or of any ancestor.
pub(crate) fn decide(
    spec: &ToolSpec,
    thread: &Thread,
    ancestors: &[&Thread],
) -> (ToolPermission, BTreeSet<Capability>) {
    let granted = scope_capabilities(thread, ancestors);
    let missing: BTreeSet<_> = spec.capabilities.difference(&granted).copied().collect();
    let setting = |t: &Thread| {
        t.tool_permissions
            .get(&spec.name)
            .copied()
            .unwrap_or_default()
    };
    let mut effective = spec.permission.min(setting(thread));
    for ancestor in ancestors {
        effective = effective.min(setting(ancestor));
    }
    if !missing.is_empty() {
        effective = ToolPermission::Deny;
    }
    (effective, missing)
}

/// The user-facing listing of a catalog for one thread.
pub(crate) fn offers(catalog: &Catalog, thread: &Thread, ancestors: &[&Thread]) -> ToolList {
    ToolList {
        tools: catalog
            .tools
            .values()
            .map(|routed| {
                let (effective, missing) = decide(&routed.spec, thread, ancestors);
                ToolOffer {
                    name: routed.spec.name.clone(),
                    description: routed.spec.description.clone(),
                    capabilities: routed.spec.capabilities.clone(),
                    host_permission: routed.spec.permission,
                    permission: thread.tool_permissions.get(&routed.spec.name).copied(),
                    effective,
                    missing,
                }
            })
            .collect(),
        unavailable: catalog.unavailable.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::Mode;
    fn spec(name: &str, permission: ToolPermission, caps: &[Capability]) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            capabilities: caps.iter().copied().collect(),
            permission,
        }
    }
    #[test]
    fn unset_permissions_deny_and_every_layer_can_only_restrict() {
        use ToolPermission::*;
        let name = Id::new_v4().simple().to_string();
        let mut thread = Thread::new(String::new(), Mode::Plan);
        assert_eq!(decide(&spec(&name, Allow, &[]), &thread, &[]).0, Deny);
        thread.tool_permissions.insert(name.clone(), Allow);
        assert_eq!(decide(&spec(&name, Allow, &[]), &thread, &[]).0, Allow);
        // The host's policy caps the user's setting.
        assert_eq!(decide(&spec(&name, Ask, &[]), &thread, &[]).0, Ask);
        // So does a parent's.
        let mut parent = Thread::new(String::new(), Mode::Plan);
        parent.tool_permissions.insert(name.clone(), Ask);
        assert_eq!(decide(&spec(&name, Allow, &[]), &thread, &[&parent]).0, Ask);
        parent.tool_permissions.clear();
        assert_eq!(
            decide(&spec(&name, Allow, &[]), &thread, &[&parent]).0,
            Deny
        );
        // A capability outside the grant, or outside the mode, denies.
        let read = spec(&name, Allow, &[Capability::ReadWorkspace]);
        let (effective, missing) = decide(&read, &thread, &[]);
        assert_eq!(
            (effective, missing),
            (Deny, [Capability::ReadWorkspace].into())
        );
        thread.grant.capabilities.insert(Capability::ReadWorkspace);
        assert_eq!(decide(&read, &thread, &[]).0, Allow);
        thread.grant.capabilities.insert(Capability::Network);
        let network = spec(&name, Allow, &[Capability::Network]);
        assert_eq!(decide(&network, &thread, &[]).0, Deny);
    }
    #[test]
    fn a_child_loses_every_capability_an_ancestor_loses() {
        use ToolPermission::*;
        let name = Id::new_v4().simple().to_string();
        let read = spec(&name, Allow, &[Capability::ReadWorkspace]);
        let with_read = |mode| {
            let mut thread = Thread::new(String::new(), mode);
            thread.grant.capabilities.insert(Capability::ReadWorkspace);
            thread.tool_permissions.insert(name.clone(), Allow);
            thread
        };
        let child = with_read(Mode::Plan);
        let mut parent = with_read(Mode::Plan);
        let mut root = with_read(Mode::Code);
        assert_eq!(decide(&read, &child, &[&parent, &root]).0, Allow);
        // The grandparent loses the grant after the child was created.
        root.grant.capabilities.clear();
        let (effective, missing) = decide(&read, &child, &[&parent, &root]);
        assert_eq!(
            (effective, missing),
            (Deny, [Capability::ReadWorkspace].into())
        );
        assert!(scope_capabilities(&child, &[&parent, &root]).is_empty());
        // So does an ancestor whose mode no longer allows it.
        root.grant.capabilities.insert(Capability::ReadWorkspace);
        parent.mode = Mode::Chat;
        assert_eq!(decide(&read, &child, &[&parent, &root]).0, Deny);
        parent.mode = Mode::Plan;
        assert_eq!(
            scope_capabilities(&child, &[&parent, &root]),
            [Capability::ReadWorkspace].into()
        );
    }
    struct Fixed(Vec<ToolSpec>);
    impl ToolHost for Fixed {
        fn list<'a>(&'a self, _: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
            Box::pin(async move {
                ToolListing {
                    tools: self.0.clone(),
                    unavailable: vec![],
                }
            })
        }
        fn call<'a>(
            &'a self,
            _: &'a ToolScope,
            _: ToolRequest,
        ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
            Box::pin(async { Err(ErrorReport::new(ErrorKind::Internal, "not called")) })
        }
    }
    #[tokio::test]
    async fn ambiguous_or_invalid_tools_are_not_offered() {
        let shared = Id::new_v4().simple().to_string();
        let own = Id::new_v4().simple().to_string();
        let mut bad_schema = spec(
            &Id::new_v4().simple().to_string(),
            ToolPermission::Allow,
            &[],
        );
        bad_schema.parameters = serde_json::json!({"type": "string"});
        let hosts: Vec<Arc<dyn ToolHost>> = vec![
            Arc::new(Fixed(vec![
                spec(&shared, ToolPermission::Allow, &[]),
                spec(&own, ToolPermission::Allow, &[]),
                spec("has space", ToolPermission::Allow, &[]),
                bad_schema,
            ])),
            Arc::new(Fixed(vec![spec(&shared, ToolPermission::Allow, &[])])),
        ];
        let scope = ToolScope {
            thread: Id::new_v4(),
            workspace: None,
            capabilities: BTreeSet::new(),
        };
        let catalog = catalog(&hosts, &scope).await;
        assert_eq!(catalog.tools.keys().collect::<Vec<_>>(), vec![&own]);
        assert_eq!(catalog.unavailable.len(), 3);
        assert!(
            catalog
                .unavailable
                .iter()
                .all(|u| u.error.kind == ErrorKind::Protocol)
        );
    }
}
