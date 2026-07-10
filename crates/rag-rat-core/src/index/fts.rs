//! Full-text search (FTS5) index lifecycle: rebuild/sync the FTS mirror and track its freshness.
//!
//! FTS FRESHNESS KEYS ARE GLOBAL, NOT PER-REPO (V040 reclassification). `chunk_fts` is ONE FTS5
//! index over the whole `chunks` table (never repo-scoped), and its freshness digest
//! (`content_revision()`) is computed over the GLOBAL `main.files`. So `fts_dirty`,
//! `fts_source_revision`, and `fts_synced_at_ms` track global infrastructure and MUST live in the
//! global `index_meta` (`self.meta` / `self.set_meta`), not per-repo `repo_meta`. V039 relocated
//! them to `repo_meta` under the one-DB-per-repo assumption; per-repo copies made a consolidated DB
//! pay a full FTS rebuild forever after a sibling synced (stale-dirty loop). Do NOT route these
//! back through `self.repo_meta` / `self.set_repo_meta`.

use anyhow::Context as _;

use super::*;

impl IndexDatabase {
    /// Recovery / freshness rebuild: repopulate the contentless `chunk_fts` from the durable text
    /// store and rebuild the external-content `commit_fts`, then record freshness. The hot
    /// full-rebuild path uses [`finalize_full_rebuild_fts`] instead (chunk_fts is written inline).
    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        self.rebuild_chunk_fts()?;
        schema::rebuild_commit_fts(self.storage.connection())?;
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_meta("fts_dirty", "false")?;
        Ok(())
    }

    /// Full-rebuild FTS finalize: `chunk_fts` was written inline during chunk
    /// insert, so only the external-content `commit_fts` needs the bulk 'rebuild' here. Records
    /// freshness like [`rebuild_fts`], without re-tokenizing every chunk.
    pub(super) fn finalize_full_rebuild_fts(&self) -> anyhow::Result<()> {
        schema::rebuild_commit_fts(self.storage.connection())?;
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_meta("fts_dirty", "false")?;
        Ok(())
    }

    /// Repopulate the contentless `chunk_fts` from scratch (#77 Phase 2): clear it, then
    /// re-tokenize every chunk's text. The text comes from the compressed `chunk_text` store
    /// (decompressed with one reused decompressor); the `chunks.text` column is gone, so this INNER
    /// JOINs `chunk_text`. This is the recovery path only — the full-rebuild and incremental paths
    /// write `chunk_fts` inline from the in-memory chunk text, so this never runs there.
    ///
    /// COST: unlike the old external-content `'rebuild'` (which re-tokenized in-engine from the
    /// content column), this decompresses the WHOLE store in Rust. It is gated by
    /// `ensure_fts_fresh` (only fires when the FTS is genuinely dirty/stale — the same trigger
    /// as before), so a steady-state query never pays it; but a stale-read heal is now a
    /// heavier one-off.
    fn rebuild_chunk_fts(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        // 'delete-all' is the FTS5 command to clear a contentless index (a bare DELETE is
        // unsupported; on an external-content table it also corrupts a desynced index — #51).
        conn.execute("INSERT INTO chunk_fts(chunk_fts) VALUES('delete-all')", [])?;
        let dicts = crate::query::chunk_text_dicts(conn)?;
        let mut decoder = crate::index::text_compression::ChunkTextDecoder::new(&dicts);
        // Collect (rowid, ChunkTextRow) first: decompress's anyhow::Result can't cross the rusqlite
        // closure, and the SELECT statement can't stay open while we INSERT into chunk_fts.
        let rows: Vec<(i64, crate::index::text_compression::ChunkTextRow)> = {
            let mut stmt = conn.prepare(
                "SELECT chunks.id, chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version
                 FROM chunks JOIN chunk_text ON chunk_text.chunk_id = chunks.id",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, crate::index::text_compression::ChunkTextRow {
                    blob: row.get(1)?,
                    raw_len: row.get(2)?,
                    dict_version: row.get(3)?,
                }))
            })?;
            let mut out = Vec::new();
            for row in mapped {
                out.push(row?);
            }
            out
        };
        let mut insert = conn.prepare("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)")?;
        for (chunk_id, text_row) in rows {
            let text = text_row.resolve(&mut decoder)?;
            insert.execute(rusqlite::params![chunk_id, text])?;
        }
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
        // NEVER rebuild while any SEARCHABLE chunk lacks its durable text row (A6, P2 review): a
        // generation-staged full rebuild commits its waves BEFORE `build_chunk_text_store` runs,
        // so on a FIRST index (no `chunk_text_dict` yet) the staged chunks carry inline
        // `chunk_fts` rows whose text exists only in the REBUILDING connection's temp table.
        // `rebuild_chunk_fts`'s 'delete-all' + re-derive from `chunk_text` would silently drop
        // those rows from BM25 and the freshness stamp below would mark the loss clean —
        // permanent missing rows in the published generation. Degrade instead: serve the current
        // (possibly stale) FTS and leave the dirty/stale state in place, so the refresh re-fires
        // once the text store is complete (the rebuild's own finalize records freshness for the
        // fast path). The probe is deliberately "HAS an fts row we cannot re-derive" — not a bare
        // "lacks text" — so a chunk with NEITHER row (never searchable; the re-derive would
        // exclude it regardless) can never wedge freshness healing permanently. Runs only on the
        // dirty/stale path, never steady-state.
        let irreplaceable_fts_rows: bool = self.storage.connection().query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM main.chunks
                 WHERE id IN (SELECT rowid FROM chunk_fts)
                   AND id NOT IN (SELECT chunk_id FROM main.chunk_text)
             )",
            [],
            |row| row.get(0),
        )?;
        if irreplaceable_fts_rows {
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

impl IndexDatabase {
    /// #582: rebuild EVERY FTS mirror from its durable source, THROUGH THIS CONNECTION — the
    /// corruption-recovery path behind [`retry_once_on_fts_corruption`]. All-of-them because the
    /// bare "database disk image is malformed" a corrupt read returns does not name the table
    /// (the #582 incident burned three wrong theories on exactly that); every mirror rebuilds
    /// losslessly (`chunk_fts`/`commit_fts` from the chunk/commit stores, `repo_memory_fts` from
    /// `repo_memories`, `github_fts` from the papertrail tables), so over-healing costs one
    /// re-tokenize, not data. Healing through the querying connection is what makes the fix
    /// visible to a long-lived server: an out-of-band repair leaves the connection's cached
    /// corrupt pages serving `SQLITE_CORRUPT` indefinitely.
    pub(crate) fn heal_corrupt_fts(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        // ONE `BEGIN IMMEDIATE` transaction fences the whole heal. The mirrors are GLOBAL on a
        // consolidated DB while write flocks are per-repo, so a flock cannot serialize sibling
        // repos' writers — the DATABASE-wide write lock can, and it also makes the
        // multi-statement rebuilds one atomic unit. An active writer makes the BEGIN fail
        // (busy timeout) and a connection already inside a transaction (a heal firing from
        // within a pass) cannot open one — both surface here and the caller returns the
        // ORIGINAL corruption error; the next query (or explicit `heal_index`) retries.
        let fence =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        self.rebuild_fts()?;
        crate::query::memory::heal_repo_memory_fts(conn)?;
        crate::index::papertrail::rebuild_fts(conn)?;
        fence.commit()?;
        Ok(())
    }
}

impl IndexDatabase {
    /// `heal_index`'s FTS phase (#582): probe each mirror with a query that EXECUTES rank — only
    /// rank/bm25 decodes docsize, the corruption class BOTH `PRAGMA integrity_check` and FTS5's
    /// own `'integrity-check'` miss (a COUNT over the same MATCH passes; the ORDER BY is
    /// optimized away) — and rebuild the mirrors whose probe returns SQLITE_CORRUPT. Returns the
    /// rebuilt table names. The broad prefix disjunction matches essentially any text corpus; an
    /// empty mirror matches nothing and probes clean, which is right (nothing ranks it).
    pub(crate) fn heal_fts_if_corrupt(&self) -> anyhow::Result<Vec<String>> {
        // Every a-z/0-9 prefix: any token leading with an ASCII alphanumeric matches, so the
        // ranked probe reads docsize for essentially every row a real query could rank. (A corpus
        // of exclusively non-ASCII-leading tokens would evade it; the query-layer retry still
        // covers those.)
        let probe_query =
            ('a'..='z').chain('0'..='9').map(|c| format!("{c}*")).collect::<Vec<_>>().join(" OR ");
        let conn = self.storage.connection();
        let probe_corrupt = |table: &str| -> anyhow::Result<bool> {
            let sql =
                format!("SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY rank LIMIT 1");
            match conn.query_row(&sql, [probe_query.as_str()], |_| Ok(())) {
                Ok(()) | Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
                Err(err) => {
                    let err: anyhow::Error = err.into();
                    if error_is_fts_corruption(&err) { Ok(true) } else { Err(err) }
                },
            }
        };
        let mut healed = Vec::new();
        // chunk_fts and commit_fts share one recovery (`rebuild_fts` repopulates both).
        let chunk_corrupt = probe_corrupt("chunk_fts")?;
        let commit_corrupt = probe_corrupt("commit_fts")?;
        let memory_corrupt = probe_corrupt("repo_memory_fts")?;
        let github_corrupt = probe_corrupt("github_fts")?;
        if !(chunk_corrupt || commit_corrupt || memory_corrupt || github_corrupt) {
            return Ok(healed);
        }
        // Same database-wide fence as `heal_corrupt_fts`: the rebuilds are global and
        // multi-statement, and per-repo flocks cannot serialize a consolidated DB's siblings.
        let fence =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        if chunk_corrupt || commit_corrupt {
            self.rebuild_fts()?;
            if chunk_corrupt {
                healed.push("chunk_fts".to_string());
            }
            if commit_corrupt {
                healed.push("commit_fts".to_string());
            }
        }
        if memory_corrupt {
            crate::query::memory::heal_repo_memory_fts(conn)?;
            healed.push("repo_memory_fts".to_string());
        }
        if github_corrupt {
            crate::index::papertrail::rebuild_fts(conn)?;
            healed.push("github_fts".to_string());
        }
        fence.commit()?;
        Ok(healed)
    }
}

/// #582: whether `err`'s chain contains SQLITE_CORRUPT — the FTS5 shadow-table variant surfaces
/// as extended `SQLITE_CORRUPT_VTAB` (267), whose primary code rusqlite maps to
/// `DatabaseCorrupt`, rendered as the bare "database disk image is malformed".
pub(crate) fn error_is_fts_corruption(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseCorrupt
        )
    })
}

/// Run `op`; when it fails with FTS corruption, run `heal` and retry ONCE. A non-corruption
/// error, a heal failure, and corruption that survives the heal all surface unchanged — there is
/// deliberately no loop (#582).
pub(crate) fn retry_once_on_fts_corruption<T>(
    mut op: impl FnMut() -> anyhow::Result<T>,
    heal: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<T> {
    match op() {
        Ok(value) => Ok(value),
        Err(err) if error_is_fts_corruption(&err) => {
            heal().context("healing corrupt FTS indexes (#582)")?;
            op()
        },
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod corruption_tests {
    use super::*;

    fn corrupt_error() -> anyhow::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseCorrupt,
                extended_code: 267, // SQLITE_CORRUPT_VTAB — the FTS5 shadow variant
            },
            Some("database disk image is malformed".to_string()),
        )
        .into()
    }

    #[test]
    fn corruption_retries_once_after_heal() {
        let mut calls = 0;
        let mut healed = false;
        let result = retry_once_on_fts_corruption(
            || {
                calls += 1;
                if calls == 1 { Err(corrupt_error()) } else { Ok(42) }
            },
            || {
                healed = true;
                Ok(())
            },
        );
        assert_eq!(result.unwrap(), 42);
        assert!(healed, "the heal ran between the attempts");
    }

    #[test]
    fn non_corruption_errors_do_not_heal() {
        let result: anyhow::Result<i32> = retry_once_on_fts_corruption(
            || anyhow::bail!("some other failure"),
            || panic!("a non-corruption error must not trigger the heal"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn corruption_that_survives_the_heal_surfaces_without_looping() {
        let mut calls = 0;
        let result: anyhow::Result<i32> = retry_once_on_fts_corruption(
            || {
                calls += 1;
                Err(corrupt_error())
            },
            || Ok(()),
        );
        assert!(error_is_fts_corruption(&result.unwrap_err()));
        assert_eq!(calls, 2, "exactly one retry, no loop");
    }
}
