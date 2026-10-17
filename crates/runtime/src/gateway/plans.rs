//! A conversation's plan: the checklist its models keep with `plan_update`
//! (`crate::plan_tool`), stored on the thread so the user sees it and it
//! survives restarts and compaction.
use super::{Gateway, GatewayError, Result};
use crate::error::check;
use a206_core::*;

/// The plan store the plan tool writes through.
#[derive(Clone)]
pub struct Plans {
    /// The gateway without its tool hosts, so holding it forms no cycle.
    base: Gateway,
}
impl Gateway {
    pub(crate) fn plans(&self) -> Plans {
        let mut base = self.clone();
        base.tool_hosts = Default::default();
        base.delegation = Default::default();
        Plans { base }
    }
    fn set_plan(&self, thread: Id, plan: Vec<PlanStep>) -> Result<()> {
        let mut store = self.lock()?;
        let size = |plan: &[PlanStep]| rmp_serde::to_vec_named(plan).map(|bytes| bytes.len());
        let grows = size(&plan)?.saturating_sub(size(&store.thread(thread)?.plan)?);
        // A plan never takes the room the conversation's next reply needs.
        check(
            grows + super::run::NEXT_ROUND <= store.remaining()?,
            GatewayError::HistoryFull,
        )?;
        store.thread_mut(thread)?.plan = plan;
        store.persist(Some(EventKind::ThreadChanged { id: thread }))?;
        Ok(())
    }
}
impl Plans {
    /// Replace `thread`'s plan with `plan` (validated by the caller).
    pub fn set(&self, thread: Id, plan: Vec<PlanStep>) -> std::result::Result<(), ErrorReport> {
        self.base
            .set_plan(thread, plan)
            .map_err(|error| crate::error::report(&error))
    }
}
