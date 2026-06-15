//! Full-text search (FTS5) index lifecycle: rebuild/sync the FTS mirror and track its freshness.

use super::*;

impl IndexDatabase {
    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        schema::rebuild_fts(self.storage.connection())?;
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_meta("fts_dirty", "false")?;
        Ok(())
    }

    pub fn sync_fts(&self) -> anyhow::Result<()> {
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_meta("fts_dirty", "false")?;
        Ok(())
    }

    fn record_fts_current(&self) -> anyhow::Result<()> {
        self.set_meta("fts_synced_at_ms", &now_ms().to_string())?;
        let revision = self.content_revision()?;
        self.set_meta("fts_source_revision", &revision)?;
        Ok(())
    }

    pub(super) fn mark_fts_dirty(&self) -> anyhow::Result<()> {
        self.set_meta("fts_dirty", "true")
    }

    pub(super) fn ensure_fts_fresh(&self) -> anyhow::Result<()> {
        let content_revision = self.content_revision()?;
        let fts_source_revision = self.meta("fts_source_revision")?;
        if !self.fts_dirty()? && fts_source_revision.as_deref() == Some(content_revision.as_str()) {
            return Ok(());
        }
        self.rebuild_fts()?;
        let refreshed_revision = self.meta("fts_source_revision")?;
        if refreshed_revision.as_deref() != Some(content_revision.as_str()) {
            anyhow::bail!(
                "FTS freshness invariant failed: content_revision={content_revision}, \
                 fts_source_revision={}",
                refreshed_revision.unwrap_or_else(|| "<missing>".to_string())
            );
        }
        Ok(())
    }

    pub(super) fn fts_dirty(&self) -> anyhow::Result<bool> {
        Ok(self.meta("fts_dirty")?.as_deref() == Some("true"))
    }
}
