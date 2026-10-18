//! Packages agents prepare for the user to review. A proposal is stored like
//! an installed package, but recorded apart from the installed extensions, so
//! it is never offered, loaded or granted anything. Only the user installs
//! it, and it then arrives disabled with nothing granted, even as an update.
use super::{ExtensionError, ExtensionHost, Grants, packages};
use a206_core::{
    EventKind, ExtensionProposal, Id, MAX_EXTENSION_PROPOSALS, PluginSource, PluginSummary,
};
use std::collections::BTreeSet;

impl ExtensionHost {
    /// Record a package an agent prepared in `thread`. Proposing the same
    /// package again refreshes its record; a package with the id of another
    /// waiting proposal replaces that proposal.
    pub async fn propose(
        &self,
        thread: Id,
        manifest: String,
        module: Vec<u8>,
        at: u64,
    ) -> Result<ExtensionProposal, ExtensionError> {
        let package = packages::prepare(manifest, module)?;
        let host = self.clone();
        tokio::task::spawn_blocking(move || host.propose_blocking(package, thread, at))
            .await
            .map_err(ExtensionError::Join)?
    }
    fn propose_blocking(
        &self,
        package: packages::Package,
        thread: Id,
        at: u64,
    ) -> Result<ExtensionProposal, ExtensionError> {
        let _serial = self.packages.lock().map_err(|_| ExtensionError::Poisoned)?;
        let installed = PluginSource::Installed {
            digest: package.digest.clone(),
        };
        // A digest is recorded once, as installed or as proposed, so removing
        // one record's files never takes another's.
        if let Some(plugin) = self.plugins()?.into_iter().find(|p| p.source == installed) {
            return Err(ExtensionError::AlreadyInstalled(plugin.name));
        }
        let dir = packages::directory(&self.data_dir);
        let created = packages::store(&dir, &package)?;
        let discard = |error: ExtensionError| {
            if created && let Err(cleanup) = packages::remove(&dir, &package.digest) {
                tracing::warn!(error = &cleanup as &dyn std::error::Error, extension = %package.manifest.id, "could not remove an unrecorded extension proposal");
            }
            error
        };
        let mut summary = package.manifest.summary(false, &BTreeSet::new());
        summary.source = installed;
        let proposal = ExtensionProposal {
            digest: package.digest.clone(),
            summary,
            thread,
            proposed_at: at,
        };
        let mut store = self
            .store
            .lock()
            .map_err(|_| discard(ExtensionError::Poisoned))?;
        let proposals = &mut store.state.extension_proposals;
        let superseded = |p: &ExtensionProposal| {
            p.digest == proposal.digest || p.summary.id == package.manifest.id
        };
        if proposals.iter().filter(|p| !superseded(p)).count() >= MAX_EXTENSION_PROPOSALS {
            return Err(discard(ExtensionError::TooManyProposals));
        }
        let replaced: Vec<String> = proposals
            .iter()
            .filter(|p| superseded(p) && p.digest != proposal.digest)
            .map(|p| p.digest.clone())
            .collect();
        proposals.retain(|p| !superseded(p));
        proposals.push(proposal.clone());
        // A refused persist restores the earlier state.
        store
            .persist(Some(EventKind::ExtensionChanged {
                id: package.manifest.id.clone(),
            }))
            .map_err(|error| discard(error.into()))?;
        drop(store);
        for digest in replaced {
            if let Err(error) = packages::remove(&dir, &digest) {
                // No longer recorded, so the next start prunes it.
                tracing::warn!(error = &error as &dyn std::error::Error, extension = %package.manifest.id, "could not remove a replaced extension proposal");
            }
        }
        Ok(proposal)
    }

    /// Install a waiting proposal (user operation). The stored package is
    /// verified against its digest first; an update keeps no earlier grants.
    pub async fn install_proposal(&self, digest: String) -> Result<PluginSummary, ExtensionError> {
        let host = self.clone();
        tokio::task::spawn_blocking(move || {
            let _serial = host.packages.lock().map_err(|_| ExtensionError::Poisoned)?;
            let proposal = host.proposal(&digest)?;
            let package = packages::load(
                &packages::directory(&host.data_dir),
                &proposal.summary.id,
                &digest,
            )?;
            host.install_locked(package, Grants::Clear)
        })
        .await
        .map_err(ExtensionError::Join)?
    }

    /// Discard a waiting proposal and its files.
    pub async fn dismiss_proposal(&self, digest: String) -> Result<(), ExtensionError> {
        let host = self.clone();
        tokio::task::spawn_blocking(move || {
            let _serial = host.packages.lock().map_err(|_| ExtensionError::Poisoned)?;
            let mut store = host.store.lock().map_err(|_| ExtensionError::Poisoned)?;
            let proposals = &mut store.state.extension_proposals;
            let index = proposals
                .iter()
                .position(|p| p.digest == digest)
                .ok_or(ExtensionError::NotProposed)?;
            let id = proposals.remove(index).summary.id;
            store.persist(Some(EventKind::ExtensionChanged { id }))?;
            drop(store);
            // On failure the files are no longer recorded and are pruned at
            // the next start; the error says so.
            Ok(packages::remove(
                &packages::directory(&host.data_dir),
                &digest,
            )?)
        })
        .await
        .map_err(ExtensionError::Join)?
    }

    fn proposal(&self, digest: &str) -> Result<ExtensionProposal, ExtensionError> {
        self.store
            .lock()
            .map_err(|_| ExtensionError::Poisoned)?
            .state
            .extension_proposals
            .iter()
            .find(|p| p.digest == digest)
            .cloned()
            .ok_or(ExtensionError::NotProposed)
    }
}
