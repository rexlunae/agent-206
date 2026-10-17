//! Extension declarations as manifests state them and clients display them.
//!
//! The guest-facing wire values come from `a206-extension-abi`; this module adds
//! the capability-bearing declarations, which only the host interprets.
use crate::Capability;
pub use a206_extension_abi::{
    Args, Field, Node, ParamKind, Tone, ToolOutput, UiEvent, Value, View,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A tool an extension offers to agents and to the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// Every capability the tool may use. A call runs only when the
    /// effective grant holds all of them.
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    #[serde(default)]
    pub params: Vec<ParamSpec>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ParamKind,
    pub description: String,
    #[serde(default)]
    pub required: bool,
}
/// A panel an extension contributes to the Extensions page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelSpec {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
}
