//! Index key-value meta (the `index_meta` table) and the content-revision digest.

use super::*;

impl IndexDatabase {
    pub(super) fn record_content_revision(&self) -> anyhow::Result<String> {
        let revision = self.content_revision()?;
        self.set_meta("content_revision", &revision)?;
        Ok(revision)
    }

    /// Write a meta key only if the stored value differs. Returns whether a write happened — so a
    /// no-change incremental/sweep pass can avoid dirtying a WAL page (see issue #63).
    pub(super) fn set_meta_if_changed(&self, key: &str, value: &str) -> anyhow::Result<bool> {
        if self.meta(key)?.as_deref() == Some(value) {
            return Ok(false);
        }
        self.set_meta(key, value)?;
        Ok(true)
    }

    pub(super) fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub(super) fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        meta_for(self.storage.connection(), key)
    }

    pub(super) fn content_revision(&self) -> anyhow::Result<String> {
        let value = self.storage.connection().query_row(
            "SELECT COALESCE(string_agg(path || ':' || sha256, ',' ORDER BY path), '') FROM files",
            [],
            |row| row.get::<_, String>(0),
        )?;
        Ok(hex_sha256(value.as_bytes()))
    }
}
