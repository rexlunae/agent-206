//! Agent profiles in the gateway's profile (`<data dir>/agent-profiles/`),
//! read when used so an edited file applies to the next subagent started
//! from it. A subagent keeps the profile it was created with.
use super::{Gateway, Result};
use crate::agent_profiles;
use a206_core::{AgentProfile, AgentProfiles, profiles::DIRECTORY};
use std::path::PathBuf;

impl Gateway {
    fn profiles_root(&self) -> Result<PathBuf> {
        Ok(self.lock()?.root.join(DIRECTORY))
    }
    pub(super) async fn agent_profiles(&self) -> Result<AgentProfiles> {
        let root = self.profiles_root()?;
        Ok(tokio::task::spawn_blocking(move || agent_profiles::load(&root)).await??)
    }
    pub(super) async fn agent_profile(&self, id: String) -> Result<AgentProfile> {
        let root = self.profiles_root()?;
        Ok(tokio::task::spawn_blocking(move || agent_profiles::find(&root, &id)).await??)
    }
}
