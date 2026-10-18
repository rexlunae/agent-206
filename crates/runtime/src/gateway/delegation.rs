//! The narrow gateway handle the subagent tools (`crate::delegation_tool`)
//! use to start and supervise a conversation's subagents.
//!
//! A [`Delegator`] can create a subagent (attenuated by `CreateSubagent`
//! exactly as a user-created one), send it a message, cancel its run and read
//! its state. It offers nothing that changes a grant, a permission, a model
//! set or an approval, so the tools holding it cannot widen any authority.
//!
//! The tool hosts hold the delegator and the gateway holds the tool hosts, so
//! the delegator keeps only a weak reference to them: dropping every gateway
//! still drops the hosts and everything they own. A gateway that gains a tool
//! host gets subagent tools of its own, bound to its own host list, so other
//! clones keep theirs.
//!
//! A subagent run is reserved, as a child of its parent's run token, before
//! the tool call that starts it returns, and only the delegator's own `Send`
//! for that reservation admits the run with it. Cancelling the parent
//! therefore also cancels a subagent whose run has not been admitted yet,
//! while other messages to the subagent neither take nor drop it.
use super::{Gateway, GatewayError, Result, poisoned};
use crate::{error::check, store::Store, tools::ToolHost};
use a206_core::*;
use a206_protocol::Request;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError, RwLock, Weak},
};
use tokio_util::sync::CancellationToken;

pub(super) type HostList = Vec<Arc<dyn ToolHost>>;
/// The gateway's current tool hosts, for the delegator to run subagents with.
pub(super) type HostLink = Arc<RwLock<Weak<HostList>>>;
/// Subagent runs reserved by their parent's run and not yet admitted: the
/// subagent and its run token, by reservation.
pub(super) type Reserved = Arc<Mutex<BTreeMap<Reservation, (Id, CancellationToken)>>>;

/// One reserved subagent run, admitted only by the `Send` made for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reservation(Id);

tokio::task_local! {
    /// The reservation the `Send` running in this task was made for.
    static ADMITTING: Reservation;
}

/// One gateway's subagent tools and the link to the hosts they run with.
#[derive(Clone, Default)]
pub(super) struct Binding {
    link: HostLink,
    host: Option<Arc<dyn ToolHost>>,
}

/// Why the delegator could not act.
#[derive(Debug, thiserror::Error)]
pub enum DelegatorError {
    #[error("the gateway is shutting down")]
    Closed,
    #[error("the conversation's run has ended")]
    NotRunning,
    #[error(transparent)]
    Gateway(#[from] GatewayError),
}
impl crate::error::Classified for DelegatorError {
    fn kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Closed => Some(ErrorKind::Unavailable),
            Self::NotRunning => Some(ErrorKind::Conflict),
            Self::Gateway(error) => crate::error::classify(error),
        }
    }
}

/// A conversation that may delegate, as of one read.
#[derive(Debug, Clone)]
pub struct Caller {
    pub mode: Mode,
    pub model: Option<ModelKey>,
    pub subagent_models: BTreeSet<ModelKey>,
    pub capabilities: BTreeSet<Capability>,
    /// How many conversations it was delegated from.
    pub ancestors: usize,
}
/// One direct subagent, as of one read.
#[derive(Debug, Clone)]
pub struct Child {
    pub id: Id,
    pub title: String,
    pub mode: Mode,
    pub model: Option<ModelKey>,
    pub state: RunState,
    pub capabilities: BTreeSet<Capability>,
    pub error: Option<ErrorReport>,
    /// The text of its latest assistant message.
    pub reply: Option<String>,
    /// Tool calls waiting for the user's approval.
    pub approvals: usize,
    /// The ID of the agent profile it was started from.
    pub profile: Option<String>,
}
impl Child {
    fn of(thread: &Thread) -> Self {
        Self {
            id: thread.id,
            title: thread.title.clone(),
            mode: thread.mode,
            model: thread.model.clone(),
            state: thread.state.clone(),
            capabilities: thread.grant.capabilities.clone(),
            error: thread.error.clone(),
            reply: thread
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.text.clone()),
            approvals: thread.approvals.len(),
            profile: thread.profile.as_ref().map(|p| p.id.clone()),
        }
    }
}

/// The subagent [`Delegator::create`] asks for, as `CreateSubagent` takes it.
#[derive(Debug, Clone)]
pub struct Spawn {
    pub title: String,
    pub mode: Mode,
    pub model: ModelKey,
    pub requested: BTreeSet<Capability>,
    /// The agent profile to start from.
    pub profile: Option<String>,
}

/// Starts and supervises subagents for the subagent tools.
pub struct Delegator {
    /// The gateway without its tool hosts, so holding it forms no cycle.
    base: Gateway,
    hosts: HostLink,
}

/// Threads delegated from `id`, directly or not.
pub(super) fn descendants(store: &Store, id: Id) -> Vec<Id> {
    let mut found = vec![];
    let mut seen = BTreeSet::from([id]);
    let mut next = vec![id];
    while let Some(parent) = next.pop() {
        for thread in &store.state.threads {
            if thread.parent == Some(parent) && seen.insert(thread.id) {
                found.push(thread.id);
                next.push(thread.id);
            }
        }
    }
    found
}

impl Gateway {
    pub(super) fn delegator(&self) -> Delegator {
        let mut base = self.clone();
        base.tool_hosts = Arc::default();
        base.delegation.host = None;
        Delegator {
            base,
            hosts: self.delegation.link.clone(),
        }
    }
    /// Give this gateway subagent tools bound to its current tool hosts,
    /// replacing any it shares with the gateway it was cloned from.
    pub(super) fn bind_delegation(&mut self) {
        self.delegation.link = HostLink::default();
        let host: Arc<dyn ToolHost> = Arc::new(crate::delegation_tool::DelegationHost::new(
            self.delegator(),
        ));
        let hosts = Arc::make_mut(&mut self.tool_hosts);
        if let Some(old) = self.delegation.host.take() {
            hosts.retain(|h| !std::ptr::addr_eq(Arc::as_ptr(h), Arc::as_ptr(&old)));
        }
        hosts.push(host.clone());
        self.delegation.host = Some(host);
        // The link is new and unshared, so no writer can have poisoned it.
        *self
            .delegation
            .link
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&self.tool_hosts);
    }
}

/// Record `thread`'s run as running and return its token: the one reserved
/// for the `Send` in progress, if it was made for one, else a fresh one. The
/// caller holds the store lock, as `cancel_run` does, so a cancellation
/// always finds the token either reserved or running.
pub(super) fn admit(gateway: &Gateway, thread: Id) -> Result<CancellationToken> {
    let mut running = gateway.running.lock().map_err(poisoned("run"))?;
    let token = match ADMITTING.try_with(|r| *r) {
        Ok(reservation) => {
            let mut reserved = gateway.reserved.lock().map_err(poisoned("run"))?;
            match reserved.get(&reservation) {
                Some((child, _)) if *child == thread => reserved.remove(&reservation).map(|r| r.1),
                _ => None,
            }
        }
        Err(_) => None,
    }
    .unwrap_or_default();
    running.insert(thread, token.clone());
    Ok(token)
}

impl Delegator {
    /// The full gateway, while it is open.
    fn gateway(&self) -> Result<Gateway, DelegatorError> {
        let hosts = self
            .hosts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade()
            .ok_or(DelegatorError::Closed)?;
        let mut gateway = self.base.clone();
        gateway.tool_hosts = hosts;
        Ok(gateway)
    }
    /// Run `body` as a client request would, so it is admitted, journaled and
    /// checked like one; a refusal is the gateway's own report.
    async fn request(&self, body: Command) -> Result<Reply, ErrorReport> {
        let gateway = self.gateway().map_err(|e| crate::error::report(&e))?;
        match gateway
            .request(Request {
                id: Id::new_v4(),
                body,
            })
            .await
            .body
        {
            Reply::Error(report) => Err(report),
            reply => Ok(reply),
        }
    }

    /// The user's agent profiles, as their files are now.
    pub async fn profiles(&self) -> Result<AgentProfiles, ErrorReport> {
        self.base
            .agent_profiles()
            .await
            .map_err(|e| crate::error::report(&DelegatorError::from(e)))
    }
    /// The user's agent profile `id`, as its file is now.
    pub async fn profile(&self, id: String) -> Result<AgentProfile, ErrorReport> {
        self.base
            .agent_profile(id)
            .await
            .map_err(|e| crate::error::report(&DelegatorError::from(e)))
    }
    /// The model of `permitted` that `profile` names, if it names one.
    pub fn profile_model(
        &self,
        profile: &AgentProfile,
        permitted: &BTreeSet<ModelKey>,
    ) -> Result<Option<ModelKey>, ErrorReport> {
        let store = self
            .base
            .lock()
            .map_err(|e| crate::error::report(&DelegatorError::from(e)))?;
        crate::agent_profiles::model(profile, permitted, &store.state.settings.providers)
            .map_err(|e| crate::error::report(&e))
    }

    /// `caller` and its direct subagents.
    pub fn family(&self, caller: Id) -> Result<(Caller, Vec<Child>), ErrorReport> {
        self.read_family(caller)
            .map_err(|e| crate::error::report(&DelegatorError::from(e)))
    }
    fn read_family(&self, caller: Id) -> Result<(Caller, Vec<Child>)> {
        let store = self.base.lock()?;
        let thread = store.thread(caller)?;
        let mut ancestors = 0;
        let mut parent = thread.parent;
        while let Some(id) = parent {
            ancestors += 1;
            check(
                ancestors <= store.state.threads.len(),
                GatewayError::Invalid("conversation delegation is circular"),
            )?;
            parent = store.thread(id)?.parent;
        }
        let children = store
            .state
            .threads
            .iter()
            .filter(|t| t.parent == Some(caller))
            .map(Child::of)
            .collect();
        Ok((
            Caller {
                mode: thread.mode,
                model: thread.model.clone(),
                subagent_models: thread.subagent_models.clone(),
                capabilities: thread.grant.capabilities.clone(),
                ancestors,
            },
            children,
        ))
    }

    /// Create a subagent of `caller` exactly as `CreateSubagent` does, then
    /// give it `caller`'s own tool permissions except `withheld`. Effective
    /// permissions are the minimum along the delegation chain, so the copy
    /// never permits more than `caller` has.
    pub async fn create(
        &self,
        caller: Id,
        spawn: Spawn,
        withheld: &[&str],
    ) -> Result<Child, ErrorReport> {
        let reply = self
            .request(Command::CreateSubagent {
                parent: caller,
                title: spawn.title,
                mode: spawn.mode,
                model: spawn.model,
                requested: spawn.requested,
                profile: spawn.profile,
            })
            .await?;
        let Reply::Thread(thread) = reply else {
            return Err(crate::error::report(&GatewayError::Invalid(
                "unexpected reply to CreateSubagent",
            )));
        };
        self.inherit(caller, thread.id, withheld)
            .map_err(|e| crate::error::report(&DelegatorError::from(e)))
    }
    fn inherit(&self, caller: Id, child: Id, withheld: &[&str]) -> Result<Child> {
        let mut store = self.base.lock()?;
        let permissions: BTreeMap<String, ToolPermission> = store
            .thread(caller)?
            .tool_permissions
            .iter()
            .filter(|(tool, _)| !withheld.contains(&tool.as_str()))
            .map(|(tool, permission)| (tool.clone(), *permission))
            .collect();
        let thread = store.thread_mut(child)?;
        thread.tool_permissions = permissions;
        let summary = Child::of(thread);
        store.persist(Some(EventKind::ThreadChanged { id: child }))?;
        Ok(summary)
    }

    /// Send `text` to `child` and wait for its run to end, as `Send` does,
    /// admitting the run with `reservation`'s token.
    pub async fn send(
        &self,
        child: Id,
        text: String,
        reservation: Reservation,
    ) -> Result<(), ErrorReport> {
        let send = self.request(Command::Send {
            thread: child,
            text,
            attachments: vec![],
        });
        ADMITTING.scope(reservation, send).await.map(drop)
    }

    /// Reserve `child`'s next run as part of `caller`'s current run, so
    /// cancelling `caller` cancels it even before it is admitted.
    pub fn reserve(&self, caller: Id, child: Id) -> Result<Reservation, ErrorReport> {
        self.reserve_run(caller, child)
            .map_err(|e| crate::error::report(&e))
    }
    fn reserve_run(&self, caller: Id, child: Id) -> Result<Reservation, DelegatorError> {
        let running = self.base.running.lock().map_err(poisoned("run"))?;
        let parent = running.get(&caller).ok_or(DelegatorError::NotRunning)?;
        let reservation = Reservation(Id::new_v4());
        self.base
            .reserved
            .lock()
            .map_err(poisoned("run"))?
            .insert(reservation, (child, parent.child_token()));
        Ok(reservation)
    }
    /// Drop `reservation` if its run was never admitted.
    pub fn release(&self, reservation: Reservation) {
        // A map of tokens is valid whatever a panicking writer left.
        self.base
            .reserved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&reservation);
    }

    /// Add `text` to `child`'s running native run.
    pub async fn steer(&self, child: Id, text: String) -> Result<(), ErrorReport> {
        self.request(Command::Steer {
            thread: child,
            text,
        })
        .await
        .map(drop)
    }
    /// Cancel `child`'s run and its own subagents' runs.
    pub async fn cancel(&self, child: Id) -> Result<(), ErrorReport> {
        self.request(Command::CancelRun { thread: child })
            .await
            .map(drop)
    }
}

/// Cancel `thread`'s run and those of every thread delegated from it,
/// including runs reserved and not yet admitted.
pub(super) fn cancel_run(gateway: &Gateway, thread: Id) -> Result<Reply> {
    let store = gateway.lock()?;
    let running = gateway.running.lock().map_err(poisoned("run"))?;
    let reserved = gateway.reserved.lock().map_err(poisoned("run"))?;
    let tokens = |id: Id| {
        running.get(&id).into_iter().chain(
            reserved
                .values()
                .filter(move |(child, _)| *child == id)
                .map(|(_, token)| token),
        )
    };
    let mut found = false;
    for token in tokens(thread) {
        token.cancel();
        found = true;
    }
    check(found, GatewayError::NotFound("thread is not running"))?;
    for id in descendants(&store, thread) {
        tokens(id).for_each(CancellationToken::cancel);
    }
    Ok(Reply::Ok)
}

#[cfg(test)]
mod tests;
