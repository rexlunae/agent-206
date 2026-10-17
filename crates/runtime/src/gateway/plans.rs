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
    fn set_plan(&self, thread: Id, plan: Vec<PlanStep>, result: usize) -> Result<()> {
        let mut store = self.lock()?;
        let size = |plan: &[PlanStep]| rmp_serde::to_vec_named(plan).map(|bytes| bytes.len());
        let (new, old) = (size(&plan)?, size(&store.thread(thread)?.plan)?);
        // The plan changes only when the call's result still fits above the
        // run's reserve, so a call that changed the plan never reports failure.
        // Room a smaller plan frees counts toward it.
        check(
            new + result + super::run::RESERVE <= store.remaining()? + old,
            GatewayError::HistoryFull,
        )?;
        store.thread_mut(thread)?.plan = plan;
        store.persist(Some(EventKind::ThreadChanged { id: thread }))?;
        Ok(())
    }
}
impl Plans {
    /// Replace `thread`'s plan with `plan` (validated by the caller), whose
    /// call then reports a result of `result` bytes.
    pub fn set(
        &self,
        thread: Id,
        plan: Vec<PlanStep>,
        result: usize,
    ) -> std::result::Result<(), ErrorReport> {
        self.base
            .set_plan(thread, plan, result)
            .map_err(|error| crate::error::report(&error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step() -> PlanStep {
        PlanStep {
            text: Id::new_v4().to_string(),
            status: PlanStatus::Pending,
        }
    }

    /// Near the storage limit, a plan changes only if its call's result fits
    /// above the run's reserve; otherwise the plan stays as it was.
    #[test]
    fn a_plan_changes_only_when_its_result_still_fits() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Gateway::open(directory.path()).unwrap();
        let thread = Thread::new(Id::new_v4().to_string(), Mode::Chat);
        let id = thread.id;
        let kept = vec![step()];
        let next = vec![step(), step()];
        let grows = rmp_serde::to_vec_named(&next).unwrap().len()
            - rmp_serde::to_vec_named(&kept).unwrap().len();
        let result = 1 + (Id::new_v4().as_u128() % 4096) as usize;
        {
            let mut store = gateway.lock().unwrap();
            store.state.threads.push(thread);
            store.thread_mut(id).unwrap().plan = kept.clone();
            // Fill the conversation until exactly the plan's growth, its
            // result and the reserve remain.
            let mut message = Message::new(Role::User, String::new(), 0, CodingBackend::Native);
            store.thread_mut(id).unwrap().messages.push(message.clone());
            let target = grows + result + super::super::run::RESERVE;
            let excess = store.remaining().unwrap() - target;
            message.text = "x".repeat(excess);
            *store.thread_mut(id).unwrap().messages.last_mut().unwrap() = message;
            // A string's length prefix grows with it; trim to the target.
            while store.remaining().unwrap() < target {
                let text = &mut store
                    .thread_mut(id)
                    .unwrap()
                    .messages
                    .last_mut()
                    .unwrap()
                    .text;
                text.pop();
            }
            assert_eq!(store.remaining().unwrap(), target);
        }
        let plans = gateway.plans();
        let refused = plans.set(id, next.clone(), result + 1).unwrap_err();
        assert_eq!(refused.kind, ErrorKind::Storage);
        let plan = |gateway: &Gateway| gateway.lock().unwrap().thread(id).unwrap().plan.clone();
        assert_eq!(plan(&gateway), kept);
        plans.set(id, next.clone(), result).unwrap();
        assert_eq!(plan(&gateway), next);
        // Clearing frees the plan's room, which a result may then use.
        let freed = rmp_serde::to_vec_named(&next).unwrap().len()
            - rmp_serde::to_vec_named(&Vec::<PlanStep>::new())
                .unwrap()
                .len();
        let left = gateway.lock().unwrap().remaining().unwrap();
        let room = left + freed - super::super::run::RESERVE;
        assert!(plans.set(id, vec![], room + 1).is_err());
        assert_eq!(plan(&gateway), next);
        plans.set(id, vec![], room).unwrap();
        assert!(plan(&gateway).is_empty());
    }
}
