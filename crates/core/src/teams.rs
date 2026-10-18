//! Agent teams: a coordinator and the agent profiles it delegates to.
//!
//! A team is a TOML file in the gateway's `agent-teams` directory naming a
//! coordinator profile, member profiles and instructions shared by all of
//! them. Starting a team on a conversation creates one subagent, the
//! coordinator, from its profile; the coordinator then starts members as
//! its own subagents, only from the team's member profiles. Everything a
//! profile does still applies, so a team only narrows: no model, capability
//! or tool permission is added by naming it.
use crate::{
    AgentProfile, AgentProfiles, Command, ProfileProblem, ProviderConfig, Thread,
    profiles::delegation,
};
use serde::{Deserialize, Serialize};

/// The gateway profile directory holding one `<id>.toml` per agent team.
pub const DIRECTORY: &str = "agent-teams";
/// Most member profiles in one team, besides the coordinator.
pub const MAX_MEMBERS: usize = 16;
/// Most team files read from the directory.
pub const MAX_TEAMS: usize = 64;

/// One agent team, as read from its file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTeam {
    /// The file name without `.toml`, under the same rules as a profile ID.
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// The agent profile the coordinator starts from.
    pub coordinator: String,
    /// The agent profiles the coordinator may start members from, in order.
    pub members: Vec<String>,
    /// Added to the prompt of the coordinator and of every member.
    pub instructions: String,
    /// SHA-256 of the file, hex encoded.
    pub digest: String,
}

/// Every team the gateway could read, and every file it could not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTeams {
    /// In ID order.
    pub teams: Vec<AgentTeam>,
    pub problems: Vec<ProfileProblem>,
}
impl AgentTeams {
    pub fn get(&self, id: &str) -> Option<&AgentTeam> {
        self.teams.iter().find(|t| t.id == id)
    }
}

/// One profile of a team, as it was when the team started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub profile: String,
    pub name: String,
    pub description: Option<String>,
}
impl From<&AgentProfile> for TeamMember {
    fn from(profile: &AgentProfile) -> Self {
        Self {
            profile: profile.id.clone(),
            name: profile.name.clone(),
            description: profile.description.clone(),
        }
    }
}

/// A conversation's part in a team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRole {
    /// Started from the team; its subagents start only from member profiles.
    Coordinator,
    /// Started by the coordinator from a member profile.
    Member,
}

/// The team a conversation belongs to, as the team was when it started.
/// Later edits to the file do not change a team already started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadTeam {
    pub id: String,
    pub name: String,
    pub digest: String,
    pub instructions: String,
    pub role: TeamRole,
    pub coordinator: TeamMember,
    pub members: Vec<TeamMember>,
}
impl ThreadTeam {
    /// Whether `profile` is one of the team's member profiles.
    pub fn has_member(&self, profile: &str) -> bool {
        self.members.iter().any(|m| m.profile == profile)
    }
    /// What a conversation of this team is told about it, after its mode
    /// prompt and profile instructions.
    pub fn prompt(&self) -> String {
        let mut text = format!("## Agent team: {}\n\n", self.name);
        match self.role {
            TeamRole::Coordinator => {
                text.push_str(
                    "You coordinate this team. Split the work into tasks and give each to a member with subagent_spawn, naming the member's profile; each member sees only the task you give it. Follow members with subagent_wait or subagent_status, and combine their results.\n\nMembers:\n",
                );
                for member in &self.members {
                    text.push_str(&format!("- {}: {}", member.profile, member.name));
                    if let Some(description) = &member.description {
                        text.push_str(&format!(" ({description})"));
                    }
                    text.push('\n');
                }
            }
            TeamRole::Member => text.push_str(&format!(
                "You are a member of this team, working on a task from its coordinator, {}. Reply with your result.\n",
                self.coordinator.name
            )),
        }
        if !self.instructions.is_empty() {
            text.push_str(&format!("\n{}\n", self.instructions));
        }
        text
    }
}

/// The `StartTeam` request for `team` on `parent`: the coordinator profile's
/// mode and model, as a delegated conversation from that profile would ask,
/// and `parent`'s capabilities for the gateway to narrow. `None` when the
/// coordinator profile or a member profile is not among the usable
/// `profiles`, which the gateway would refuse, or the coordinator names no
/// model `parent` permits.
pub fn start(
    team: &AgentTeam,
    parent: &Thread,
    profiles: &AgentProfiles,
    providers: &[ProviderConfig],
) -> Option<Command> {
    let coordinator = profiles.get(&team.coordinator)?;
    if team.members.iter().any(|m| profiles.get(m).is_none()) {
        return None;
    }
    let (mode, model) = delegation(Some(coordinator), &parent.subagent_models, providers)?;
    Some(Command::StartTeam {
        parent: parent.id,
        team: team.id.clone(),
        title: team.name.clone(),
        mode,
        model,
        requested: parent.grant.capabilities.clone(),
    })
}

#[cfg(test)]
mod tests;
