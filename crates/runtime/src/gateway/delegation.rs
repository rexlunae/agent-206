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
//! still drops the hosts and everything they own.
use super::{Gateway, GatewayError, Result, poisoned};
use crate::{error::check, store::Store, tools::ToolHost};
use a206_core::*;
use a206_protocol::Request;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, PoisonError, RwLock, Weak},
};

pub(super) type HostList = Vec<Arc<dyn ToolHost>>;
/// The gateway's current tool hosts, for the delegator to run subagents with.
pub(super) type HostLink = Arc<RwLock<Weak<HostList>>>;

/// Why the delegator could not act.
#[derive(Debug, thiserror::Error)]
pub enum DelegatorError {
    #[error("the gateway is shutting down")]
    Closed,
    #[error(transparent)]
    Gateway(#[from] GatewayError),
}
impl crate::error::Classified for DelegatorError {
    fn kind(&self) -> Option<ErrorKind> {
        match self {
            Self::Closed => Some(ErrorKind::Unavailable),
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
        }
    }
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
        Delegator {
            base,
            hosts: self.delegation.clone(),
        }
    }
    /// Point the delegator at the current tool hosts.
    pub(super) fn link_delegation(&self) {
        // The link is a single weak pointer, valid whatever a panicking
        // writer left, so a poisoned lock is still safe to overwrite.
        *self
            .delegation
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&self.tool_hosts);
    }
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
        title: String,
        mode: Mode,
        model: ModelKey,
        requested: BTreeSet<Capability>,
        withheld: &[&str],
    ) -> Result<Child, ErrorReport> {
        let reply = self
            .request(Command::CreateSubagent {
                parent: caller,
                title,
                mode,
                model,
                requested,
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

    /// Send `text` to `child` and wait for its run to end, as `Send` does.
    pub async fn send(&self, child: Id, text: String) -> Result<(), ErrorReport> {
        self.request(Command::Send {
            thread: child,
            text,
            attachments: vec![],
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

/// Cancel `thread`'s run and those of every thread delegated from it.
pub(super) fn cancel_run(gateway: &Gateway, thread: Id) -> Result<Reply> {
    let store = gateway.lock()?;
    let running = gateway.running.lock().map_err(poisoned("run"))?;
    running
        .get(&thread)
        .ok_or(GatewayError::NotFound("thread is not running"))?
        .cancel();
    for id in descendants(&store, thread) {
        if let Some(token) = running.get(&id) {
            token.cancel();
        }
    }
    Ok(Reply::Ok)
}
