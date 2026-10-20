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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        plan_tool::{PlanHost, UPDATE},
        search_tool::{SEARCH, SearchHost},
        tools::{self, ToolHost, ToolScope},
    };

    #[tokio::test]
    async fn unreadable_services_are_reported_in_the_tool_listing() {
        let root = tempfile::tempdir().unwrap();
        let gateway = Gateway::open(root.path()).unwrap();
        let host = SearchHost::new(gateway.searcher());
        let store = gateway.store.clone();
        std::thread::spawn(move || {
            let _guard = store.lock().unwrap();
            panic!("simulate an interrupted state writer");
        })
        .join()
        .unwrap_err();
        let hosts: Vec<std::sync::Arc<dyn ToolHost>> = vec![
            std::sync::Arc::new(host),
            std::sync::Arc::new(PlanHost::new(gateway.plans())),
        ];
        let listing = tools::catalog(
            &hosts,
            &ToolScope {
                thread: a206_core::Id::new_v4(),
                workspace: None,
                capabilities: Default::default(),
                programs: vec![],
            },
        )
        .await;
        assert!(!listing.tools.contains_key(SEARCH));
        assert!(listing.tools.contains_key(UPDATE));
        assert_eq!(listing.unavailable.len(), 1);
        let unavailable = &listing.unavailable[0];
        assert_eq!(unavailable.source, SEARCH);
        assert_eq!(unavailable.error.kind, a206_core::ErrorKind::Internal);
        assert!(unavailable.error.message.contains("state lock poisoned"));
        assert!(unavailable.error.correlation.is_some());
    }
}
