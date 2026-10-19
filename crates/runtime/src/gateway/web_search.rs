//! What the native `web_search` tool reads from the gateway: the search
//! services the user configured, and where the vault broker that holds their
//! keys is. The tool host gets this handle, not the gateway itself.
use super::{Gateway, Result};
use a206_core::{BrokerPairing, web_search::SearchConfig};
use std::path::PathBuf;

/// The gateway's search settings and broker pairing.
pub struct Searcher {
    base: Gateway,
}
impl Gateway {
    pub(crate) fn searcher(&self) -> Searcher {
        let mut base = self.clone();
        base.tool_hosts = Default::default();
        base.delegation = Default::default();
        Searcher { base }
    }
}
impl Searcher {
    /// The configured services, in the user's order.
    pub(crate) fn services(&self) -> Result<Vec<SearchConfig>> {
        Ok(self.base.lock()?.state.settings.search_services.clone())
    }
    /// The profile whose broker holds the keys, and this unlock's pairing.
    pub(crate) fn broker(&self) -> Result<(PathBuf, Option<BrokerPairing>)> {
        let root = self.base.lock()?.root.clone();
        Ok((root, self.base.pairing()?))
    }
}
