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

use super::*;

impl IndexDatabase {
    /// Recovery / freshness rebuild: repopulate the contentless `chunk_fts` from the durable text
    /// store and rebuild the external-content `commit_fts`, then record freshness. The hot
    /// full-rebuild path uses [`finalize_full_rebuild_fts`] instead (chunk_fts is written inline).
    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        // ONE transaction when standalone (#610): 'delete-all' + per-row reinserts + the
        // freshness stamps are a multi-statement sequence — interrupted mid-way it leaves an
        // EMPTY mirror silently missing from BM25 until the next refresh fires, and a
        // concurrent reader sees the torn intermediate. Inside a caller's fence (the corruption
        // heals) it runs bare; SQLite rejects nested BEGINs.
        fenced_when_autocommit(self.storage.connection(), || {
            self.rebuild_chunk_fts()?;
            schema::rebuild_commit_fts(self.storage.connection())?;
            self.record_content_revision()?;
            self.record_fts_current()?;
            self.set_meta("fts_dirty", "false")?;
            Ok(())
        })
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
        // NEVER rebuild while any chunk lacks its durable text row (A6, P2 review): a
        // generation-staged full rebuild commits its waves BEFORE `build_chunk_text_store` runs,
        // so mid-rebuild the staged chunks carry inline `chunk_fts` rows whose text exists only
        // in the REBUILDING connection's temp table. `rebuild_chunk_fts`'s 'delete-all' +
        // re-derive from `chunk_text` would silently drop those rows from BM25 and the freshness
        // stamp below would mark the loss clean — permanent missing rows in the published
        // generation. Degrade instead: serve the current (possibly stale) FTS and leave the
        // dirty/stale state in place, so the refresh re-fires once the text store is complete.
        //
        // The probe reads the GENERATION LEDGER, never chunk/FTS shapes (#582 review): the A6
        // guard used to check "HAS an fts row we cannot re-derive", but a docsize-corrupted
        // mirror SCANS AS EMPTY, blinding that shape exactly when a stale stamp coincides with
        // corruption — the rebuild then destroyed the staged rows the guard exists to protect.
        // Staging-above-live is corruption-independent, true precisely during rebuild windows
        // (an abandoned staging is swept by gc under the flock), so it cannot wedge freshness
        // healing permanently.
        if self.staged_files_generation_exists()? {
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

    /// The staging probe shared by `ensure_fts_fresh` and the corruption heals (#582 review): a
    /// generation-staged rebuild is in flight (or abandoned, pending gc's flock-guarded sweep)
    /// iff any repo carries `files` rows ABOVE its live generation pointer — the same ledger
    /// signal the #492 torn-window guard reads. Deliberately consults the generation ledger and
    /// never chunk/FTS shapes: a docsize-corrupted `chunk_fts` SCANS AS EMPTY (blinding any
    /// FTS-membership probe exactly when it matters), and "chunk without durable text"
    /// over-fires on never-searchable chunks that legitimately carry neither row. `chunk_fts` is
    /// GLOBAL, so ANY repo's staging defers the rebuild.
    fn staged_files_generation_exists(&self) -> anyhow::Result<bool> {
        let conn = self.storage.connection();
        let repos: Vec<String> = {
            let mut stmt = conn.prepare("SELECT DISTINCT repo_id FROM main.files")?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for repo_id in repos {
            let live = crate::index::schema::live_files_generation(conn, &repo_id)?;
            let staged: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM main.files WHERE repo_id = ?1 AND generation > ?2)",
                rusqlite::params![repo_id, live],
                |row| row.get(0),
            )?;
            if staged {
                return Ok(true);
            }
        }
        Ok(false)
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
    /// `repo_memories`, `papertrail_fts` from the papertrail tables), so over-healing costs one
    /// re-tokenize, not data. Healing through the querying connection is what makes the fix
    /// visible to a long-lived server: an out-of-band repair leaves the connection's cached
    /// corrupt pages serving `SQLITE_CORRUPT` indefinitely.
    /// Returns the mirrors whose repair was DEFERRED (today: only `chunk_fts`, behind a staged
    /// rebuild); every other mirror is rebuilt unconditionally.
    pub(crate) fn heal_corrupt_fts(&self) -> anyhow::Result<Vec<String>> {
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
        // Between a staged rebuild's committed waves, chunk_fts rows exist whose text is not in
        // the durable store yet — 'delete-all' + re-derive would drop them permanently and stamp
        // the loss clean (the same A6 guard `ensure_fts_fresh` applies). Defer ONLY that mirror:
        // commit_fts (git_commits), repo_memory_fts (repo_memories), and papertrail_fts (the
        // papertrail tables) rebuild from always-durable sources, so a staged window must not
        // leave their ranked surfaces broken too.
        let deferred = if self.staged_files_generation_exists()? {
            schema::rebuild_commit_fts(conn)?;
            vec!["chunk_fts".to_string()]
        } else {
            self.rebuild_fts()?;
            Vec::new()
        };
        crate::query::memory::heal_repo_memory_fts(conn)?;
        crate::index::papertrail::rebuild_fts(conn)?;
        fence.commit()?;
        Ok(deferred)
    }
}

impl IndexDatabase {
    /// `heal_index`'s FTS phase (#582): probe each mirror with a query that EXECUTES rank — only
    /// rank/bm25 decodes docsize, the corruption class BOTH `PRAGMA integrity_check` and FTS5's
    /// own `'integrity-check'` miss (a COUNT over the same MATCH passes; the ORDER BY is
    /// optimized away) — and rebuild the mirrors whose probe returns SQLITE_CORRUPT. Returns the
    /// rebuilt table names. The broad prefix disjunction matches essentially any text corpus; an
    /// empty mirror matches nothing and probes clean, which is right (nothing ranks it).
    pub fn heal_fts_if_corrupt(&self) -> anyhow::Result<FtsHealOutcome> {
        let conn = self.storage.connection();
        let mut outcome = FtsHealOutcome::default();
        // chunk_fts and commit_fts share one recovery (`rebuild_fts` repopulates both).
        let chunk_corrupt = ranked_probe_is_corrupt(conn, "chunk_fts")?;
        let commit_corrupt = ranked_probe_is_corrupt(conn, "commit_fts")?;
        let memory_corrupt = ranked_probe_is_corrupt(conn, "repo_memory_fts")?;
        let papertrail_corrupt = ranked_probe_is_corrupt(conn, "papertrail_fts")?;
        if !(chunk_corrupt || commit_corrupt || memory_corrupt || papertrail_corrupt) {
            return Ok(outcome);
        }
        // Same database-wide fence as `heal_corrupt_fts`: the rebuilds are global and
        // multi-statement, and per-repo flocks cannot serialize a consolidated DB's siblings.
        let fence =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        // Same staged-rows refusal as `heal_corrupt_fts` — the memory/papertrail heals below are
        // unaffected (their sources are always complete).
        if chunk_corrupt || commit_corrupt {
            if self.staged_files_generation_exists()? {
                // The staged-window hazard is CHUNK-only (chunk_fts re-derives from chunk_text,
                // which lags the staged waves); commit_fts rebuilds from durable git_commits and
                // must not stay broken behind unrelated staging. A silent skip of the deferred
                // mirror would let the report read healthy while ranked queries still fail —
                // surface it so the caller reruns after the staged rebuild completes.
                if commit_corrupt {
                    schema::rebuild_commit_fts(conn)?;
                    outcome.healed.push("commit_fts".to_string());
                }
                if chunk_corrupt {
                    outcome.deferred.push("chunk_fts".to_string());
                }
            } else {
                // The shared rebuild repopulates BOTH mirrors regardless of which probe failed
                // — report both, so the heal report states what was actually rebuilt.
                self.rebuild_fts()?;
                outcome.healed.push("chunk_fts".to_string());
                outcome.healed.push("commit_fts".to_string());
            }
        }
        if memory_corrupt {
            crate::query::memory::heal_repo_memory_fts(conn)?;
            outcome.healed.push("repo_memory_fts".to_string());
        }
        if papertrail_corrupt {
            crate::index::papertrail::rebuild_fts(conn)?;
            outcome.healed.push("papertrail_fts".to_string());
        }
        fence.commit()?;
        Ok(outcome)
    }
}

/// Run `op` inside its own IMMEDIATE transaction when the connection is in autocommit, bare when
/// a caller already holds one (SQLite rejects nested BEGINs). The atomicity seam for every
/// multi-statement FTS mirror maintenance sequence (#610): standalone callers get all-or-nothing
/// plus write serialization against sibling repos on a consolidated database (per-repo flocks
/// cannot serialize a GLOBAL mirror; the database write lock can), and fenced callers (the
/// corruption heals) keep their outer fence as the single atomic unit.
pub(crate) fn fenced_when_autocommit(
    conn: &rusqlite::Connection,
    op: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if !conn.is_autocommit() {
        return op();
    }
    let fence =
        rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    op()?;
    fence.commit()?;
    Ok(())
}

/// Probe one FTS mirror for the docsize corruption class with a query that EXECUTES rank — only
/// rank/bm25 decodes docsize, which both `PRAGMA integrity_check` and FTS5's own
/// `'integrity-check'` miss. The probe term is a REAL term pulled from the mirror's own
/// `fts5vocab`, so the ranked read matches at least one indexed row regardless of the corpus
/// alphabet (an ASCII-prefix disjunction goes blind on a corpus of non-ASCII-leading tokens —
/// review finding on #675). An empty mirror has no terms and probes clean, which is right
/// (nothing ranks it); a vocab read that itself hits corruption counts as corrupt.
fn ranked_probe_is_corrupt(conn: &rusqlite::Connection, table: &str) -> anyhow::Result<bool> {
    let vocab = format!("probe_vocab_{table}");
    let map_corrupt = |err: rusqlite::Error| -> anyhow::Result<bool> {
        let err: anyhow::Error = err.into();
        if error_is_fts_corruption(&err) { Ok(true) } else { Err(err) }
    };
    if let Err(err) = conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS temp.{vocab} USING fts5vocab(main, '{table}', 'row')"
    )) {
        return map_corrupt(err);
    }
    let term = conn
        .query_row(&format!("SELECT term FROM temp.{vocab} LIMIT 1"), [], |row| {
            row.get::<_, String>(0)
        })
        .optional();
    let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS temp.{vocab}"));
    let term = match term {
        Ok(Some(term)) => term,
        Ok(None) => return Ok(false),
        Err(err) => return map_corrupt(err),
    };
    // Exact-phrase match on the sampled term; internal quotes double per FTS5 phrase syntax.
    let phrase = format!("\"{}\"", term.replace('"', "\"\""));
    let sql = format!("SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY rank LIMIT 1");
    match conn.query_row(&sql, [phrase.as_str()], |_| Ok(())) {
        Ok(()) | Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(err) => map_corrupt(err),
    }
}

/// What one FTS corruption sweep did: the mirrors rebuilt, and the corrupt mirrors whose
/// repair had to wait for an in-flight generation-staged rebuild (#582).
#[derive(Debug, Default)]
pub struct FtsHealOutcome {
    pub healed: Vec<String>,
    pub deferred: Vec<String>,
}

/// #582: whether `err`'s chain contains SQLITE_CORRUPT — matched on the PRIMARY code
/// (`DatabaseCorrupt`), deliberately not the extended `SQLITE_CORRUPT_VTAB` (267): the incident
/// class itself reports extended code 11 (measured by truncating `chunk_fts_docsize` and reading
/// `sqlite3_extended_errcode` through the ranked probe), so requiring 267 would miss exactly the
/// corruption this exists to catch. The cost of the broad match is bounded: a non-FTS corruption
/// triggers one lossless mirror rebuild before the ORIGINAL error surfaces from the failed
/// retry.
pub fn error_is_fts_corruption(err: &anyhow::Error) -> bool {
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
    heal: impl FnOnce() -> anyhow::Result<Vec<String>>,
) -> anyhow::Result<T> {
    match op() {
        Ok(value) => Ok(value),
        Err(err) if error_is_fts_corruption(&err) => {
            let deferred = match heal() {
                Ok(deferred) => deferred,
                Err(heal_err) => {
                    // Keep the ORIGINAL corruption error as the chain root (callers and tests
                    // match on it); the heal's own failure rides along as context.
                    return Err(
                        err.context(format!("FTS self-heal did not complete: {heal_err:#}"))
                    );
                },
            };
            match op() {
                Ok(value) => Ok(value),
                // The retry failing right after a heal that DEFERRED a mirror is the deferral
                // biting: say so, instead of a bare "malformed" that reads like a fresh mystery.
                Err(retry_err) if !deferred.is_empty() => Err(retry_err.context(format!(
                    "FTS self-heal deferred {deferred:?} behind an in-flight staged rebuild; \
                     rerun once it completes"
                ))),
                other => other,
            }
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
                Ok(Vec::new())
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
            || Ok(Vec::new()),
        );
        assert!(error_is_fts_corruption(&result.unwrap_err()));
        assert_eq!(calls, 2, "exactly one retry, no loop");
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn the_ranked_probe_sees_docsize_corruption_on_a_non_ascii_corpus() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE probe_t USING fts5(body, content='', contentless_delete=1);
             INSERT INTO probe_t(rowid, body) VALUES (1, '\u{7d22}\u{5f15} \u{640d}\u{58ca} \
             \u{691c}\u{67fb}');",
        )
        .unwrap();
        assert!(!ranked_probe_is_corrupt(&conn, "probe_t").unwrap(), "intact mirror probes clean");
        conn.execute_batch("DELETE FROM probe_t_docsize").unwrap();
        assert!(
            ranked_probe_is_corrupt(&conn, "probe_t").unwrap(),
            "a real vocab term ranks the corpus regardless of alphabet — an ASCII-prefix probe \
             reports this corrupt mirror healthy"
        );
    }

    #[test]
    fn an_empty_mirror_probes_clean() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE probe_t USING fts5(body, content='')").unwrap();
        assert!(!ranked_probe_is_corrupt(&conn, "probe_t").unwrap());
    }
}

#[cfg(test)]
mod fence_tests {
    use super::*;

    fn conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(x INTEGER)").unwrap();
        conn
    }

    #[test]
    fn standalone_callers_get_all_or_nothing() {
        let conn = conn();
        let error = fenced_when_autocommit(&conn, || {
            conn.execute("INSERT INTO t(x) VALUES (1)", [])?;
            anyhow::bail!("interrupted mid-sequence")
        })
        .unwrap_err();
        assert!(error.to_string().contains("interrupted"), "{error:#}");
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 0, "the partial write must roll back — no torn mirror");
        assert!(conn.is_autocommit(), "the failed fence must not leave a transaction open");

        fenced_when_autocommit(&conn, || {
            conn.execute("INSERT INTO t(x) VALUES (2)", [])?;
            Ok(())
        })
        .unwrap();
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn fenced_callers_run_bare_inside_their_own_transaction() {
        let conn = conn();
        let outer =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
        fenced_when_autocommit(&conn, || {
            conn.execute("INSERT INTO t(x) VALUES (1)", [])?;
            Ok(())
        })
        .unwrap();
        // The outer fence is still the single atomic unit: rolling it back drops the write.
        outer.rollback().unwrap();
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 0, "no nested commit may escape the caller's fence");
    }
}
