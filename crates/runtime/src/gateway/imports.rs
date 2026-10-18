//! User-owned import review; never exposed through agent tools.
use super::{Gateway, GatewayError, Result, poisoned};
use a206_core::{
    Id, Reply,
    imports::{Applied, PREVIEW_VERSION, REVIEW_VERSION, Receipt, Review, Selection},
};
use std::path::Path;

impl Gateway {
    pub(super) async fn preview_import(&self, source: &Path) -> Result<Reply> {
        super::check(
            source.is_absolute(),
            GatewayError::Invalid("Import source must be an absolute path on the gateway host."),
        )?;
        let settings = self.snapshot()?.settings;
        // No vault operation is offered here, so its availability stays unknown.
        let preview = crate::imports::preview_settings(source, &settings).await?;
        super::check(
            preview.providers.len() <= 200 && preview.messengers.len() <= 200,
            GatewayError::Invalid(
                "A review supports at most 200 providers and 200 messengers. Use a smaller source configuration.",
            ),
        )?;
        let review = Review {
            version: REVIEW_VERSION,
            id: Id::new_v4(),
            settings_revision: settings.revision,
            preview,
        };
        a206_protocol::encode(&a206_protocol::Response {
            id: Id::nil(),
            body: Reply::ImportPreview(Box::new(review.clone())),
        })
        .map_err(crate::transport::FrameError::from)?;
        let mut held = self
            .import_reviews
            .lock()
            .map_err(poisoned("import reviews"))?;
        if held.len() == 4 {
            held.pop_front();
        }
        held.push_back(review.clone());
        Ok(Reply::ImportPreview(Box::new(review)))
    }

    pub(super) async fn apply_import(&self, id: Id, selection: Selection) -> Result<Reply> {
        let review = {
            let mut held = self
                .import_reviews
                .lock()
                .map_err(poisoned("import reviews"))?;
            let index =
                held.iter()
                    .position(|review| review.id == id)
                    .ok_or(GatewayError::Conflict(
                        "Import review expired or was already used. Preview the source again.",
                    ))?;
            held.remove(index).ok_or(GatewayError::Conflict(
                "Import review is unavailable. Preview the source again.",
            ))?
        };
        super::check(
            selection.valid_for(&review.preview),
            GatewayError::Invalid("Select available providers or messengers from this review."),
        )?;
        let mut settings = self.snapshot()?.settings;
        super::check(
            settings.revision == review.settings_revision,
            GatewayError::Conflict("Target settings changed. Preview the source again."),
        )?;
        let current = crate::imports::preview_settings(&review.preview.source, &settings).await?;
        super::check(
            current == review.preview,
            GatewayError::Conflict("Import source changed. Preview the source again."),
        )?;
        let selected = selection.selected(&review.preview);
        let receipt = Receipt {
            version: PREVIEW_VERSION,
            id,
            tool: selected.tool,
            source: selected.source.clone(),
            config_sha256: selected.config_sha256.clone(),
            applied_at: super::now(),
            providers_added: selected.apply_providers(&mut settings),
            messengers_added: selected.apply_messengers(&mut settings),
            vault_imported: false,
        };
        // Reuses the same validation, revision check and policy notifications as
        // the CLI/settings editor. It checks revision again after the reader await.
        self.save_settings(Box::new(settings))?;
        let root = self.lock()?.root.clone();
        let receipt_error = a206_import::write_receipt(&root, &receipt)
            .err()
            .map(|error| crate::error::correlated("import.receipt", id, &error));
        Ok(Reply::ImportApplied(Box::new(Applied {
            receipt,
            receipt_error,
        })))
    }
}
