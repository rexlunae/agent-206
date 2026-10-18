//! Reads the user's agent teams (`a206_core::teams`) from the gateway
//! profile's `agent-teams` directory, under the same rules as agent profiles
//! (`crate::agent_profiles`): bounded regular files in protected gateway
//! state, each problem reported beside the teams that can be used.
use crate::agent_profiles::{self, ProfileError, Source, optional};
use a206_core::{
    AgentProfile, AgentTeam, AgentTeams, ErrorKind, TeamMember, TeamRole, ThreadTeam,
    profiles::{MAX_DESCRIPTION, MAX_INSTRUCTIONS, MAX_NAME, valid_id},
    teams::{MAX_MEMBERS, MAX_TEAMS},
};
use serde::Deserialize;
use std::{collections::BTreeSet, path::Path};

/// Why teams, or one team, could not be read or used.
#[derive(Debug, thiserror::Error)]
pub enum TeamError {
    #[error(transparent)]
    File(#[from] ProfileError),
    #[error("the team file is not valid team TOML")]
    Parse(#[source] toml::de::Error),
    #[error("`{0}` is not an agent profile ID")]
    ProfileId(String),
    #[error("a team needs 1–{MAX_MEMBERS} `members`")]
    Members,
    #[error("`{0}` is listed more than once")]
    Duplicate(String),
    #[error("no agent team is named `{0}`")]
    Unknown(String),
    #[error("agent team `{0}` could not be read")]
    Unusable(String, #[source] Box<TeamError>),
    #[error("the profile of team member `{member}` could not be used")]
    Member {
        member: String,
        #[source]
        source: ProfileError,
    },
    #[error("a team's subagents start only from its member profiles, which do not include `{0}`")]
    NotAMember(String),
    #[error("a team's subagents start from one of its member profiles; name one")]
    MemberRequired,
    #[error("a conversation of a team cannot start another team")]
    Nested,
}
impl crate::error::Classified for TeamError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::File(error) => return crate::error::Classified::kind(error),
            Self::Member { source, .. } => return crate::error::Classified::kind(source),
            Self::Parse(_)
            | Self::ProfileId(_)
            | Self::Members
            | Self::Duplicate(_)
            | Self::Unusable(..)
            | Self::MemberRequired => ErrorKind::InvalidRequest,
            Self::Unknown(_) => ErrorKind::NotFound,
            Self::NotAMember(_) | Self::Nested => ErrorKind::PermissionDenied,
        })
    }
}

/// A team file's fields, as the user writes them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    name: String,
    description: Option<String>,
    coordinator: String,
    members: Vec<String>,
    #[serde(default)]
    instructions: String,
}

/// Every team in `dir`, by ID, with the files that could not be used. A
/// missing directory holds no teams.
pub fn load(dir: &Path) -> Result<AgentTeams, TeamError> {
    let (teams, problems) =
        agent_profiles::load_dir(dir, MAX_TEAMS, "Additional team files", parse)?;
    Ok(AgentTeams { teams, problems })
}

/// The team named `id` in `dir`.
pub fn find(dir: &Path, id: &str) -> Result<AgentTeam, TeamError> {
    match agent_profiles::source(dir, id) {
        Ok(None) => return Err(TeamError::Unknown(id.into())),
        Ok(Some(source)) => parse(source),
        Err(error) => Err(error.into()),
    }
    .map_err(|error| TeamError::Unusable(id.into(), Box::new(error)))
}

/// A team from its file.
pub(crate) fn parse(source: Source) -> Result<AgentTeam, TeamError> {
    let file: File = toml::from_str(&source.text).map_err(TeamError::Parse)?;
    let name = file.name.trim();
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(ProfileError::Name.into());
    }
    let description = optional(file.description, MAX_DESCRIPTION, ProfileError::Description)?;
    if file.instructions.len() > MAX_INSTRUCTIONS {
        return Err(ProfileError::Instructions.into());
    }
    if file.members.is_empty() || file.members.len() > MAX_MEMBERS {
        return Err(TeamError::Members);
    }
    let mut seen = BTreeSet::new();
    for id in std::iter::once(&file.coordinator).chain(&file.members) {
        if !valid_id(id) {
            return Err(TeamError::ProfileId(id.clone()));
        }
        if !seen.insert(id.as_str()) {
            return Err(TeamError::Duplicate(id.clone()));
        }
    }
    Ok(AgentTeam {
        id: source.id,
        name: name.into(),
        description,
        coordinator: file.coordinator,
        members: file.members,
        instructions: file.instructions.trim().to_owned(),
        digest: source.digest,
    })
}

/// The record a coordinator keeps of `team`, given its coordinator profile
/// and member profiles as they are now. Every member profile must be usable,
/// so a team never starts with a member it could not start later.
pub fn coordinate(
    team: &AgentTeam,
    coordinator: &AgentProfile,
    members: impl IntoIterator<Item = (String, Result<AgentProfile, ProfileError>)>,
) -> Result<ThreadTeam, TeamError> {
    let members = members
        .into_iter()
        .map(|(member, profile)| match profile {
            Ok(profile) => Ok(TeamMember::from(&profile)),
            Err(source) => Err(TeamError::Member { member, source }),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ThreadTeam {
        id: team.id.clone(),
        name: team.name.clone(),
        digest: team.digest.clone(),
        instructions: team.instructions.clone(),
        role: TeamRole::Coordinator,
        coordinator: coordinator.into(),
        members,
    })
}

/// The team a subagent of `parent` joins, if `parent` coordinates one, after
/// checking that the subagent starts from one of its member profiles.
pub fn join(
    parent: Option<&ThreadTeam>,
    profile: Option<&str>,
) -> Result<Option<ThreadTeam>, TeamError> {
    let Some(team) = parent.filter(|t| t.role == TeamRole::Coordinator) else {
        return Ok(None);
    };
    let profile = profile.ok_or(TeamError::MemberRequired)?;
    if !team.has_member(profile) {
        return Err(TeamError::NotAMember(profile.into()));
    }
    Ok(Some(ThreadTeam {
        role: TeamRole::Member,
        ..team.clone()
    }))
}

#[cfg(test)]
mod tests;
