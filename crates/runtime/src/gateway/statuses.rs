//! What `session_status` (`crate::status_tool`) reads: the gateway state as
//! it stands, under the store lock.
use super::Gateway;
use a206_core::{ErrorReport, Snapshot};

/// The state the status tool reads through.
#[derive(Clone)]
pub struct Statuses {
    /// The gateway without its tool hosts, so holding it forms no cycle.
    base: Gateway,
}
impl Gateway {
    pub(crate) fn statuses(&self) -> Statuses {
        let mut base = self.clone();
        base.tool_hosts = Default::default();
        base.delegation = Default::default();
        Statuses { base }
    }
}
impl Statuses {
    /// Apply `read` to the current state and gateway time.
    pub fn read<T>(&self, read: impl FnOnce(&Snapshot, u64) -> T) -> Result<T, ErrorReport> {
        let now = crate::provider::now();
        let store = self
            .base
            .lock()
            .map_err(|error| crate::error::report(&error))?;
        Ok(read(&store.state, now))
    }
}
