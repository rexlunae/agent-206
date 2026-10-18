//! Named agent profiles: user-written presets a subagent can start from.
//!
//! A profile is a TOML file in the gateway's `agent-profiles` directory. It
//! names a mode, a model, the capabilities to give and instructions to add to
//! the subagent's prompt. A profile only narrows: the subagent's model must
//! still be one the user permitted for the delegating conversation's
//! subagents, and its grant is still the intersection of the parent's grant,
//! the request and the mode's ceiling. Choosing a profile never adds a model,
//! a capability or a tool permission.
use crate::{Capability, ErrorReport, Mode, ModelKey, ProviderConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The gateway profile directory holding one `<id>.toml` per agent profile.
pub const DIRECTORY: &str = "agent-profiles";
/// Longest profile ID, which is its file name without `.toml`.
pub const MAX_ID: usize = 64;
/// Longest display name, in bytes.
pub const MAX_NAME: usize = 80;
/// Longest description, in bytes.
pub const MAX_DESCRIPTION: usize = 400;
/// Longest instructions, in bytes.
pub const MAX_INSTRUCTIONS: usize = 16 * 1024;
/// Largest profile file read, in bytes.
pub const MAX_FILE: u64 = 32 * 1024;
/// Most profile files read from the directory.
pub const MAX_PROFILES: usize = 64;

/// One agent profile, as read from its file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    /// The file name without `.toml`: lowercase ASCII letters, digits, `-`, `_`.
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// The subagent's mode; when absent the delegating side chooses.
    pub mode: Option<Mode>,
    /// The model's name as its provider's API lists it. It is matched only
    /// against models the user permitted for the conversation's subagents.
    pub model: Option<String>,
    /// The connection's label, when several connections list `model`.
    pub connection: Option<String>,
    /// The most a subagent started from this profile may hold; absent means
    /// no narrowing beyond what the delegating side asks for.
    pub capabilities: Option<BTreeSet<Capability>>,
    /// Added to the subagent's prompt after its mode prompt.
    pub instructions: String,
    /// SHA-256 of the file, hex encoded.
    pub digest: String,
}

impl AgentProfile {
    /// Whether `key` is the model this profile names, on the connection it
    /// names; any model is when it names none.
    pub fn names(&self, key: &ModelKey, providers: &[ProviderConfig]) -> bool {
        let Some(model) = &self.model else {
            return true;
        };
        key.model == *model
            && self.connection.as_ref().is_none_or(|label| {
                providers
                    .iter()
                    .any(|p| p.id == key.provider && p.label == *label)
            })
    }
}

/// A profile file that could not be used, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileProblem {
    /// The file's name within the profile directory.
    pub file: String,
    pub error: ErrorReport,
}

/// Every profile the gateway could read, and every file it could not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfiles {
    /// In ID order.
    pub profiles: Vec<AgentProfile>,
    pub problems: Vec<ProfileProblem>,
}
impl AgentProfiles {
    pub fn get(&self, id: &str) -> Option<&AgentProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }
}

/// The profile a subagent was started from, as it was then. Later edits to
/// the file do not change a subagent already created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadProfile {
    pub id: String,
    pub name: String,
    pub digest: String,
    pub instructions: String,
}
impl From<&AgentProfile> for ThreadProfile {
    fn from(profile: &AgentProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            digest: profile.digest.clone(),
            instructions: profile.instructions.clone(),
        }
    }
}

/// Whether `id` can name a profile file.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}
