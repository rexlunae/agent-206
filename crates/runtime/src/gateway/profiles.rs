//! Agent profiles and teams in the gateway's profile (`<data dir>/agent-
//! profiles/`, `<data dir>/agent-teams/`), read when used so an edited file
//! applies to the next subagent started from it. A subagent keeps the
//! profile and team it was created with.
use super::*;
use crate::{agent_profiles, agent_teams};
use a206_core::{AgentProfile, AgentProfiles, AgentTeam, AgentTeams, ThreadTeam};
use std::path::PathBuf;

impl Gateway {
    fn state_dir(&self, name: &str) -> Result<PathBuf> {
        Ok(self.lock()?.root.join(name))
    }
    pub(super) async fn agent_profiles(&self) -> Result<AgentProfiles> {
        let root = self.state_dir(a206_core::profiles::DIRECTORY)?;
        Ok(tokio::task::spawn_blocking(move || agent_profiles::load(&root)).await??)
    }
    pub(super) async fn agent_profile(&self, id: String) -> Result<AgentProfile> {
        let root = self.state_dir(a206_core::profiles::DIRECTORY)?;
        Ok(tokio::task::spawn_blocking(move || agent_profiles::find(&root, &id)).await??)
    }
    pub(super) async fn agent_teams(&self) -> Result<AgentTeams> {
        let root = self.state_dir(a206_core::teams::DIRECTORY)?;
        Ok(tokio::task::spawn_blocking(move || agent_teams::load(&root)).await??)
    }
    /// Team `id`, with its coordinator profile and a record of its members
    /// as their profiles are now.
    async fn agent_team(&self, id: String) -> Result<(AgentTeam, AgentProfile, ThreadTeam)> {
        let teams = self.state_dir(a206_core::teams::DIRECTORY)?;
        let profiles = self.state_dir(a206_core::profiles::DIRECTORY)?;
        Ok(tokio::task::spawn_blocking(move || {
            let team = agent_teams::find(&teams, &id)?;
            let coordinator =
                agent_profiles::find(&profiles, &team.coordinator).map_err(|source| {
                    agent_teams::TeamError::Member {
                        member: team.coordinator.clone(),
                        source,
                    }
                })?;
            let members = team
                .members
                .iter()
                .map(|m| (m.clone(), agent_profiles::find(&profiles, m)));
            let record = agent_teams::coordinate(&team, &coordinator, members)?;
            Ok::<_, agent_teams::TeamError>((team, coordinator, record))
        })
        .await??)
    }
}

impl Gateway {
    pub(super) async fn create_subagent(&self, parent: Id, spawn: Spawn) -> Result<Reply> {
        let profile = match spawn.profile.clone() {
            Some(id) => Some(self.agent_profile(id).await?),
            None => None,
        };
        let mut store = self.lock()?;
        let thread = subagent(&store, parent, spawn, profile.as_ref())?;
        store.state.threads.push(thread.clone());
        store.persist(Some(EventKind::ThreadCreated { id: thread.id }))?;
        Ok(Reply::Thread(Box::new(thread)))
    }

    /// Start `team` on `parent`: a subagent from the coordinator profile,
    /// created as `CreateProfileSubagent` creates one, that may delegate to
    /// the subagent models `parent` permits, with `parent`'s tool
    /// permissions, only from the team's member profiles.
    pub(super) async fn start_team(&self, parent: Id, team: String, spawn: Spawn) -> Result<Reply> {
        let (_, coordinator, record) = self.agent_team(team).await?;
        let mut store = self.lock()?;
        let parent_thread = store.thread(parent)?;
        check(parent_thread.team.is_none(), agent_teams::TeamError::Nested)?;
        let (models, permissions) = (
            parent_thread.subagent_models.clone(),
            parent_thread.tool_permissions.clone(),
        );
        let mut thread = subagent(&store, parent, spawn, Some(&coordinator))?;
        thread.subagent_models = models;
        thread.tool_permissions = permissions;
        thread.team = Some(Box::new(record));
        store.state.threads.push(thread.clone());
        store.persist(Some(EventKind::ThreadCreated { id: thread.id }))?;
        Ok(Reply::Thread(Box::new(thread)))
    }
}

/// A subagent of `parent` as `spawn` asks for it, started from `profile`,
/// checked against `parent`'s subagent models, grant and team.
fn subagent(
    store: &crate::store::Store,
    parent: Id,
    spawn: Spawn,
    profile: Option<&AgentProfile>,
) -> Result<Thread> {
    let Spawn {
        title,
        mode,
        model,
        requested,
        ..
    } = spawn;
    validate_model(&store.state, &model)?;
    let parent_thread = store.thread(parent)?;
    check(
        parent_thread.subagent_models.contains(&model),
        GatewayError::Denied("model is not permitted for this thread's subagents"),
    )?;
    let team = agent_teams::join(
        parent_thread.team.as_deref(),
        profile.map(|p| p.id.as_str()),
    )?;
    let requested = match profile {
        Some(profile) => agent_profiles::admit(
            profile,
            mode,
            &model,
            &parent_thread.subagent_models,
            &store.state.settings.providers,
            requested,
        )?,
        None => requested,
    };
    let mut thread = Thread::new(title, mode);
    thread.project = parent_thread.project;
    thread.parent = Some(parent);
    thread.model = Some(model.clone());
    thread.profile = profile.map(|p| Box::new(p.into()));
    thread.team = team.map(Box::new);
    thread.grant = parent_thread.grant.attenuate(
        mode,
        &Grant {
            capabilities: requested,
            models: [model].into(),
        },
        &mode.ceiling(),
    );
    Ok(thread)
}
