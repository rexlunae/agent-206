//! Agent profiles in the gateway's profile (`<data dir>/agent-profiles/`),
//! read when used so an edited file applies to the next subagent started
//! from it. A subagent keeps the profile it was created with.
use super::*;
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

impl Gateway {
    pub(super) async fn create_subagent(&self, parent: Id, spawn: Spawn) -> Result<Reply> {
        let Spawn {
            title,
            mode,
            model,
            requested,
            profile,
        } = spawn;
        let profile = match profile {
            Some(id) => Some(self.agent_profile(id).await?),
            None => None,
        };
        let mut store = self.lock()?;
        validate_model(&store.state, &model)?;
        let parent_thread = store.thread(parent)?.clone();
        check(
            parent_thread.subagent_models.contains(&model),
            GatewayError::Denied("model is not permitted for this thread's subagents"),
        )?;
        let requested = match &profile {
            Some(profile) => crate::agent_profiles::admit(
                profile,
                mode,
                &model,
                &store.state.settings.providers,
                requested,
            )?,
            None => requested,
        };
        let mut thread = Thread::new(title, mode);
        thread.project = parent_thread.project;
        thread.parent = Some(parent);
        thread.model = Some(model.clone());
        thread.profile = profile.as_ref().map(|p| Box::new(p.into()));
        thread.grant = parent_thread.grant.attenuate(
            mode,
            &Grant {
                capabilities: requested,
                models: [model].into(),
            },
            &mode.ceiling(),
        );
        store.state.threads.push(thread.clone());
        store.persist(Some(EventKind::ThreadCreated { id: thread.id }))?;
        Ok(Reply::Thread(Box::new(thread)))
    }
}
