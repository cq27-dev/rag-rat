use rusqlite::Connection;

use crate::schema::migrations::{add_column_if_missing, column_exists};

/// V056 (#566): the windowed file-pair change-coupling table, derived from `git_file_changes` over
/// a bounded recency window of eligible commits. INVARIANTS:
///  * PURE GIT-HISTORY table: the stored rows are a function of `(git-history window, params)` ONLY
///    — no `files`-view dependence (generation / generated flag / worktree scope). This makes the
///    freshness stamp complete by construction; the generated / existence filter is a READ-time
///    concern (a generated or absent-at-HEAD partner is stored but not surfaced).
///  * `path_a < path_b` (BINARY) — exactly ONE symmetric row per unordered pair; the two
///    directional confidences (`P(B|A)` / `P(A|B)`) are derived at READ time from the endpoint
///    counts.
///  * DerivedIndex posture: rows are wholesale `DELETE` + `INSERT`ed per recompute inside one
///    transaction, stamped via `repo_meta` `'git_coupling_stamp'` (=
///    `history_freshness_key:params`). Rows are never patched incrementally; a stale/absent stamp
///    means "recompute or treat as absent", never "trust the rows". No FK to `git_file_changes` /
///    `git_commits`: the row aggregates over commits and must survive a history full-replace
///    between recompute passes (the stamp, not row integrity, is the freshness authority).
///  * WRITE-time storage floors (both pure git-history): `co_change_count >= MIN_COUPLING_SUPPORT`
///    AND `lift = co * N / (a_count * b_count) >= MIN_COUPLING_LIFT`. The lift floor bounds the
///    table without a per-file cap — a hub file scores `lift ~= 1` with everything and is dropped
///    here.
///  * Direct `repo_id` scope (V040): every reader joins AND filters on `repo_id` — a fork sharing
///    commit hashes must never surface a sibling repo's couplings. The PK covers `(repo_id,
///    path_a)` lookups; the secondary index covers `(repo_id, path_b)`, so one `OR` query serves
///    both directions of the symmetric row.
///
/// Additive + idempotent (`CREATE ... IF NOT EXISTS`); a fresh DB creates it empty and the first
/// git-inclusive `impact_surface` read fills it.
pub fn apply_git_change_couplings(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS git_change_couplings(
            repo_id TEXT NOT NULL,
            path_a TEXT NOT NULL,
            path_b TEXT NOT NULL,
            co_change_count INTEGER NOT NULL,
            path_a_change_count INTEGER NOT NULL,
            path_b_change_count INTEGER NOT NULL,
            window_commit_count INTEGER NOT NULL,
            last_co_change_at_s INTEGER NOT NULL,
            computed_at_ms INTEGER NOT NULL,
            PRIMARY KEY(repo_id, path_a, path_b)
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_git_change_couplings_b
            ON git_change_couplings(repo_id, path_b);
        ",
    )
}

pub(crate) fn apply_symbols_is_test(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "symbols", "is_test", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

/// V036 (#357): content-address embeddings so they SURVIVE reindex. `chunk_embeddings` is keyed by
/// `chunk_id` with `ON DELETE CASCADE`, so every reindex / branch-switch deletes a chunk and its
/// embedding — even when the content is unchanged — forcing a re-embed. `embedding_cache` keys the
/// vector by `input_hash` alone (which already folds model id + model version + the exact embedding
/// input text), so it is context-INDEPENDENT: reconcile reuses a vector for identical content
/// across reindexes, branches, and worktrees instead of paying the embedder. Seeded from the
/// current embeddings so existing vectors are preserved through the first post-migration reindex.
/// Idempotent (`CREATE TABLE IF NOT EXISTS` + `INSERT OR IGNORE`); seeding an empty table is a
/// no-op on a fresh DB.
pub(crate) fn apply_embedding_content_cache(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS embedding_cache(
             input_hash TEXT NOT NULL PRIMARY KEY,
             model_id TEXT NOT NULL,
             embedding_dim INTEGER NOT NULL,
             vector_blob BLOB NOT NULL,
             computed_at_ms INTEGER NOT NULL,
             last_used_at_ms INTEGER NOT NULL
         ) STRICT;
         -- Preserve existing vectors: seed the cache from the current embeddings by content so the
         -- first reindex after this migration reuses instead of re-embedding.
         INSERT OR IGNORE INTO embedding_cache(
             input_hash, model_id, embedding_dim, vector_blob, computed_at_ms, last_used_at_ms
         )
         SELECT input_hash, model_id, embedding_dim, vector_blob,
                COALESCE(computed_at_ms, created_at_ms, 0), COALESCE(computed_at_ms, \
         created_at_ms, 0)
         FROM chunk_embeddings
         WHERE status = 'Current' AND input_hash != '' AND length(vector_blob) > 0;",
    )?;
    Ok(())
}

/// V030 (#215 Plan 4a): add `clone_refinements.lcs_sampled` to indexes already recorded at V029.
///
/// Fresh DBs get the column from the V029 CREATE TABLE DDL (via baseline → apply_clone_fingerprint_
/// tables); this migration is the upgrade path for existing-V029 indexes where the column was
/// absent because V029 was applied before `lcs_sampled` landed. Idempotent: `add_column_if_missing`
/// is a no-op when the column already exists.
pub(crate) fn apply_clone_refinements_lcs_sampled(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "clone_refinements", "lcs_sampled", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

/// V031 (#248): rebuild `edge_oracle` content-anchored — drop the `edges_data` FK + the volatile
/// `edge_id` PK, key by the edge's CONTENT identity instead, so verdicts SURVIVE reindex.
///
/// THE BUG: V018 keyed `edge_oracle` on `edge_id` with `FOREIGN KEY(edge_id) REFERENCES
/// edges_data(id) ON DELETE CASCADE`. Every reindex rewrites `edges_data` (full rebuild +
/// `remove_file_in_scope`), so the cascade wiped every verdict; the oracle is opt-in (no auto-run)
/// so it never repopulated. The fix mirrors `logical_symbol_monikers`: content key + no
/// reindex-cascading FK, reads join LIVE edges so a dangling row never resolves.
///
/// DATA: the rebuild RENAME→CREATE→DROP **drops** any legacy verdicts. A DB where the oracle ran
/// but hasn't reindexed has rows, but the legacy rows lack the content-key columns and back-filling
/// them via `edge_id → edges_data` is lossy (the edges may already be gone). `edge_oracle` is
/// ephemeral + opt-in; the next `oracle run` repopulates with the new shape. We accept the one-time
/// drop — NO INSERT SELECT.
///
/// SQLite can't drop a FK in place, so this is a table REBUILD using the V020 recipe: `PRAGMA
/// foreign_keys=OFF` OUTSIDE `BEGIN IMMEDIATE`, RENAME→CREATE→DROP, recreate indexes, ROLLBACK on
/// error, then `COMMIT; PRAGMA foreign_keys=ON`.
///
/// IDEMPOTENT: a fresh DB ran the NEW `apply_oracle_tables` shape at V018, so V031 must be a no-op
/// there — short-circuit when `edge_oracle` already has the content-key columns (or no
/// `edges_data` FK). `migrate_forward` only replays unapplied steps, but a re-run after a partial
/// apply must still be safe, hence the guard.
pub(crate) fn apply_edge_oracle_content_anchor(conn: &Connection) -> rusqlite::Result<()> {
    // Short-circuit if already on the content-anchored shape (fresh DB at V018, or a re-run): the
    // new table has `source_path` and no `edges_data` FK. Detect via the column; the FK-list check
    // is the belt-and-suspenders companion (an empty foreign_key_list == no cascade to wipe).
    if column_exists(conn, "edge_oracle", "source_path")? {
        return Ok(());
    }

    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        conn.execute_batch(
            "
            ALTER TABLE main.edge_oracle RENAME TO edge_oracle_legacy;
            CREATE TABLE main.edge_oracle(
                source_path TEXT NOT NULL,
                source_start_byte INTEGER NOT NULL,
                source_end_byte INTEGER NOT NULL,
                callee_start_byte INTEGER NOT NULL,
                callee_end_byte INTEGER NOT NULL,
                edge_kind TEXT NOT NULL,
                file_sha TEXT NOT NULL,
                tool TEXT NOT NULL,
                tool_version TEXT NOT NULL,
                resolved_symbol_id INTEGER,
                scip_symbol TEXT NOT NULL,
                kind TEXT NOT NULL,
                computed_at INTEGER NOT NULL,
                PRIMARY KEY(
                    tool, tool_version, source_path,
                    source_start_byte, source_end_byte,
                    callee_start_byte, callee_end_byte, edge_kind
                )
            ) STRICT;
            -- NO INSERT SELECT: the legacy rows lack the content-key columns; we accept the
            -- one-time verdict drop (ephemeral/opt-in; the next oracle run repopulates).
            DROP TABLE main.edge_oracle_legacy;
            CREATE INDEX IF NOT EXISTS idx_edge_oracle_staleness
                ON edge_oracle(file_sha, tool, tool_version);
            CREATE INDEX IF NOT EXISTS idx_edge_oracle_symbol
                ON edge_oracle(resolved_symbol_id);
            CREATE INDEX IF NOT EXISTS idx_edge_oracle_anchor
                ON edge_oracle(source_path, callee_start_byte, callee_end_byte, edge_kind);
            ",
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
        return result;
    }
    conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// V032 (#231): BLOB-pack the clone token bag. Add `symbol_fingerprints.token_bag BLOB` and DROP
/// `symbol_token_postings` — collapsing the ~490k single-row postings INSERTs of a full rebuild
/// into ONE serialized `(token_hash, freq)` BLOB per fingerprint row
/// (`bag_blob::encode_token_bag`). The candidate read decodes the BLOB back into the same
/// `(token_hash, freq)` multiset, so recall is byte-identical on a fully (re)indexed DB;
/// `clone_token_df` is recomputed by aggregating the BLOBs.
///
/// SHAPE (R5): follows the V031 idempotency-guarded-transform pattern — V029's
/// `CLONE_FINGERPRINT_DDL` (which still CREATEs both tables) is NEVER edited. On a FRESH DB, V029
/// creates `symbol_token_postings` and this migration drops it; on an EXISTING DB it does the same.
/// Guard on the `token_bag` column so a re-run (or a fresh DB already transformed) is a clean
/// no-op.
///
/// NO BACK-FILL (R8): existing fingerprint rows get `token_bag = NULL` on `ADD COLUMN`. The
/// candidate read SKIPs NULL-bag rows, so clone recall is undefined for those symbols until the
/// post-migration reindex repopulates them — the same one-time-loss posture as V029/V031 (clone
/// data is rebuildable; no parse-the-whole-repo work belongs in a migration).
pub(crate) fn apply_token_bag_blob(conn: &Connection) -> rusqlite::Result<()> {
    // Both operations are individually idempotent — NO early-return guard. A short-circuit on the
    // column's presence would be a bug here: `apply` runs this transform from BOTH the baseline
    // (apply_baseline) AND the V032 migration step, and V029's `CREATE TABLE IF NOT EXISTS
    // symbol_token_postings` runs BETWEEN them (the migration replay re-creates the table the
    // baseline already dropped). An early return keyed on the now-present `token_bag` column would
    // skip the DROP and leave that re-created postings table behind. Running both ops
    // unconditionally (each a no-op when already in the target state) converges regardless of
    // call order.
    //
    // Additive column — STRICT-valid BLOB type; existing rows default to NULL (R8).
    add_column_if_missing(conn, "symbol_fingerprints", "token_bag", "BLOB")?;
    // The per-token inverted-index table is replaced by the BLOB; its indexes drop with it.
    conn.execute_batch("DROP TABLE IF EXISTS symbol_token_postings;")?;
    Ok(())
}

/// V033 (#122): the dream-mode worklist. Findings are ABOUT memories (a reviewable triage list),
/// NEVER mutations of them — dream mode proposes, a human/strong-agent confirms; nothing here ever
/// rewrites a `repo_memories` row. Identity is `(kind, subject, claim_hash)`: a re-run with the
/// same claim_hash REFRESHES (no duplicate), a materially-changed finding SUPERSEDES the prior one,
/// and a finding the run no longer reports is RESOLVED. `subject` is polymorphic (a
/// `repo_memories.id` for memory-scoped kinds, a symbol/path ref for `coverage_gap`) so it carries
/// NO FK. `status` drives the lifecycle; `base_rank` + the `first_seen_at_ms` clock drive age
/// decay. Additive + idempotent.
pub fn apply_dream_findings(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS dream_findings(
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            subject TEXT NOT NULL,
            claim_hash TEXT NOT NULL,
            evidence TEXT NOT NULL,
            base_rank REAL NOT NULL,
            status TEXT NOT NULL DEFAULT 'open',
            superseded_by TEXT,
            first_seen_at_ms INTEGER NOT NULL,
            last_seen_at_ms INTEGER NOT NULL,
            reviewed_at_ms INTEGER,
            UNIQUE(kind, subject, claim_hash)
        ) STRICT;

        CREATE INDEX IF NOT EXISTS idx_dream_findings_status ON dream_findings(status);
        CREATE INDEX IF NOT EXISTS idx_dream_findings_subject ON dream_findings(kind, subject);
        ",
    )?;
    Ok(())
}

/// V028 (#224): intern `symbols.qualified_name` + `logical_symbols.qualified_name` into the shared
/// `name_strings` pool (the pool `edges_data` already references; the `edge_strings → name_strings`
/// rename rides this version bump and is performed in `provision_baseline`, which runs BEFORE this
/// replay — so `name_strings` is guaranteed present here). Backfill-before-drop, like the V027
/// chunk-text precedent, so the column drop is the last step rather than an irreversible one-shot.
///
/// Idempotent + guarded so a fresh-baseline DB (already `qualified_name_id`, no `qualified_name`)
/// is a clean no-op and a re-run after a half-apply is safe:
/// 1. ADD `qualified_name_id INTEGER` to both tables (guarded — NULLABLE because an `ADD COLUMN …
///    NOT NULL` fails on a populated table, so the fresh baseline matches).
/// 2. Backfill ONLY while the old `qualified_name` column still exists: insert the not-yet-present
///    qnames (`INSERT OR IGNORE` — the ~85% already stored as edge-target names are skipped; the
///    new ~102K get fresh ids, and plain-PK reuse of prior gc gaps is safe because gc only deletes
///    orphans), then set `qualified_name_id` from the pool.
/// 3. Create the id-keyed indexes; drop the old string indexes.
/// 4. Drop the `qualified_name` column (guarded by `column_exists`).
pub(crate) fn apply_intern_symbol_qualified_names(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "symbols", "qualified_name_id", "INTEGER")?;
    add_column_if_missing(conn, "logical_symbols", "qualified_name_id", "INTEGER")?;
    // Backfill reads the OLD text column; on a fresh-baseline DB it is already gone (and the tables
    // are empty), so guard each table independently — one may have migrated and the other not on a
    // re-run after a partial apply.
    if column_exists(conn, "symbols", "qualified_name")? {
        conn.execute_batch(
            "
            INSERT OR IGNORE INTO name_strings(value) SELECT qualified_name FROM symbols;
            UPDATE symbols
               SET qualified_name_id =
                   (SELECT id FROM name_strings WHERE name_strings.value = symbols.qualified_name);
            ",
        )?;
    }
    if column_exists(conn, "logical_symbols", "qualified_name")? {
        conn.execute_batch(
            "
            INSERT OR IGNORE INTO name_strings(value) SELECT qualified_name FROM logical_symbols;
            UPDATE logical_symbols
               SET qualified_name_id =
                   (SELECT id FROM name_strings
                    WHERE name_strings.value = logical_symbols.qualified_name);
            ",
        )?;
    }
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_symbols_qualified_name_id
            ON symbols(qualified_name_id);
        CREATE INDEX IF NOT EXISTS idx_logical_symbols_qualified_name_id
            ON logical_symbols(qualified_name_id);
        DROP INDEX IF EXISTS idx_symbols_qualified_name;
        DROP INDEX IF EXISTS idx_logical_symbols_qualified_name;
        ",
    )?;
    if column_exists(conn, "symbols", "qualified_name")? {
        conn.execute("ALTER TABLE symbols DROP COLUMN qualified_name", [])?;
    }
    if column_exists(conn, "logical_symbols", "qualified_name")? {
        conn.execute("ALTER TABLE logical_symbols DROP COLUMN qualified_name", [])?;
    }
    Ok(())
}

pub(crate) fn apply_commit_addressable_worktrees(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "files", "commit_sha", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "files", "worktree_id", "TEXT NOT NULL DEFAULT ''")?;
    rebuild_files_table_for_commit_scopes(conn)?;
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_files_commit_path ON files(commit_sha, path);
        CREATE INDEX IF NOT EXISTS idx_files_worktree_path ON files(worktree_id, path);
        ",
    )?;
    Ok(())
}

/// Whether `files` already carries a UNIQUE index whose columns include `commit_sha` — i.e. it is
/// already commit-addressable (the V008 `UNIQUE(path, commit_sha, worktree_id)` or the V040
/// `UNIQUE(repo_id, path, commit_sha, worktree_id)` that supersedes it). Used to make the V008
/// files rebuild idempotent + non-clobbering on `apply`'s full re-run.
fn files_has_commit_scoped_unique(conn: &Connection) -> rusqlite::Result<bool> {
    let unique_indexes: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA index_list(files)")?;
        let rows = stmt.query_map([], |row| {
            // index_list columns: (seq, name, unique, origin, partial).
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
        })?;
        rows.filter_map(|row| match row {
            Ok((name, true)) => Some(Ok(name)),
            Ok((_, false)) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<rusqlite::Result<_>>()?
    };
    for index in unique_indexes {
        let mut stmt = conn.prepare(&format!("PRAGMA index_info({index})"))?;
        // index_info columns: (seqno, cid, name).
        let mut cols = stmt.query_map([], |row| row.get::<_, Option<String>>(2))?;
        if cols.any(|col| matches!(col, Ok(Some(name)) if name == "commit_sha")) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn rebuild_files_table_for_commit_scopes(conn: &Connection) -> rusqlite::Result<()> {
    // IDEMPOTENCE / NON-CLOBBER (load-bearing since V040): this V008 rebuild recreates `files` with
    // the columns it knew at V008 — it has NO `repo_id` column. `apply` re-runs EVERY migration
    // (the `create_or_migrate`/`rebuild` path), so on an already-V040 DB an unconditional rebuild
    // here would DROP the `repo_id` column (and its real values) BEFORE V040 re-adds it as the
    // placeholder — and a case-1 `register_repo` (already adopted) would not re-backfill it,
    // leaving every file row stranded under `__unassigned__`. The rebuild's ONLY job beyond the
    // additive columns (already added by `apply_commit_addressable_worktrees`) is the
    // commit-scoped UNIQUE; once `files` already carries a UNIQUE that includes `commit_sha`
    // (this V008 one, or the V040 `(repo_id, path, commit_sha, worktree_id)` that supersedes
    // it), the rebuild is redundant.
    if files_has_commit_scoped_unique(conn)? {
        return Ok(());
    }
    conn.execute_batch(
        "
        PRAGMA foreign_keys = OFF;

        CREATE TABLE IF NOT EXISTS files_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            kind TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            modified_at_ms INTEGER NOT NULL,
            generated INTEGER NOT NULL DEFAULT 0,
            indexed_at_ms INTEGER NOT NULL,
            indexed_revision TEXT NOT NULL DEFAULT '',
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            UNIQUE(path, commit_sha, worktree_id)
        );

        INSERT OR IGNORE INTO files_new(
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id
        )
        SELECT
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, COALESCE(commit_sha, ''), COALESCE(worktree_id, '')
        FROM files;

        DROP TABLE files;
        ALTER TABLE files_new RENAME TO files;

        PRAGMA foreign_keys = ON;
        ",
    )
}
