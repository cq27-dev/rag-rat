//! De-index a whole repo from the consolidated global store: delete EVERY row it owns, across every
//! repo-scoped table, in one place. The read-side counterpart is [`count_repo_rows`] (the
//! confirmation summary + `--dry-run` preview); the write side is [`purge_repo_rows`].
//!
//! COMPLETENESS is the whole job — a missed table strands orphaned rows that a later reindex of a
//! DIFFERENT repo can never reach, and (worse) a missed DELETE on a shared cache would corrupt a
//! sibling. The purge is built from three disjoint mechanisms, in ascending trust:
//!
//!  1. **Class-level `repo_id` sweep** ([`repo_scoped_table_names`]) — the BACKSTOP. Enumerate
//!     every base table that carries a `repo_id` column straight from `sqlite_master`/`PRAGMA
//!     table_info` and `DELETE FROM <t> WHERE repo_id = ?`. This is deliberately NOT a
//!     hand-maintained list: a future migration that adds a `repo_id`-bearing table is swept
//!     automatically, and the poison-sibling tripwire proves it. Regular FTS5 tables that carry
//!     `repo_id` (`repo_memory_fts`, `papertrail_fts`) support `DELETE ... WHERE repo_id = ?` and
//!     are swept the same way; their shadow tables never carry `repo_id`, so they are never
//!     enumerated.
//!  2. **Transitively-scoped children** ([`TRANSITIVE_SCOPED_TABLES`]) — the tables with NO
//!     `repo_id` column that are nonetheless owned by a repo, reached only through a
//!     `repo_id`-bearing parent (`files.id` → `chunks`/`symbols`, `chunks.id` → the chunk-child +
//!     FTS rows, `repo_memories.id` → the memory children, `clone_graph_generations.generation` →
//!     the clone postings). `edges_data` also has a legacy/malformed NULL-source escape hatch:
//!     those rows are reached by either symbol endpoint in the captured victim-symbol set. Every
//!     child is also `ON DELETE CASCADE` from its parent, so the sweep would reach them with
//!     `foreign_keys = ON` — but they are purged EXPLICITLY here (id sets captured up front) so the
//!     purge is correct even with FK enforcement off, and so the tripwire can assert each is
//!     emptied by name.
//!  3. **FTS / external-content fixups** — `chunk_fts` is a contentless FTS5 index keyed by
//!     `chunk.id` (no FK, no `repo_id`): its rows are deleted by rowid from the captured chunk set.
//!     `commit_fts` is external-content on `git_commits`, so deleting the repo's commits desyncs
//!     it; it is re-derived once at the end with [`rebuild_commit_fts`] (the desync-safe
//!     'rebuild').
//!
//! DELIBERATELY NOT PURGED (content-addressed / store-global shared state — deleting a repo's slice
//! would either be meaningless or damage siblings): `name_strings` / `chunk_text_dict` (shared
//! interning pools), `embedding_cache` (content-addressed vectors, keyed by `(input_hash,
//! model_id)`; a stale entry is harmless cache the importer itself carries with `INSERT OR
//! IGNORE`), `ai_models` (the machine model registry), `index_meta` / `reconcile_meta` (global
//! singletons), and the sync substrate `oplog_*` / `account_*` / `content_*` /
//! `sync_security_events` (a device's machine identity + crypto-stream-derived cross-device sync
//! log — no `repo_id`, keyed by device/account/`stream_id`, and excluded from the per-repo
//! consolidate import for the same reason). None of these carry a `repo_id` column, so the
//! class-level sweep leaves them untouched.
//!
//! `table_sync_entries` and its gapped rows are purged through the captured stream-directory ids.
//! The `/5` stream commits an account-authorized repository incarnation, so an explicit advance
//! derives a fresh stream and old entries cannot repopulate it. Local removal does NOT advance that
//! authority: a same-incarnation rejoin derives the same stream and must continue its old device
//! chains. `table_sync_chain_tips` therefore survives this purge as store-global high-water
//! witnesses (it deliberately has no `repo_id` column or directory FK); local authoring refuses a
//! second genesis until its witnessed tip is restored. The directory and row projection remain
//! repo-local and are still swept.

use rusqlite::{Connection, params};

use crate::schema::rebuild_commit_fts;

/// A transitively-scoped child table: no `repo_id` column of its own, reached through a
/// `repo_id`-bearing parent. `(table, id_column, parent_id_temp)` — the purge deletes
/// `table` rows whose `id_column` is in the captured `parent_id_temp` id set.
struct TransitiveTable {
    table: &'static str,
    id_column: &'static str,
    /// The temp table (one of the [`PurgeIds`] captures) holding the parent id set this child
    /// scopes through.
    parent_ids: &'static str,
}

/// The captured id sets a purge scopes its transitively-owned children through. Each is a
/// single-column temp table of the repo's ids, snapshotted BEFORE any delete so a child delete is
/// correct regardless of ordering (and regardless of FK cascade firing).
mod purge_ids {
    pub const FILES: &str = "temp.rmp_file_ids";
    pub const CHUNKS: &str = "temp.rmp_chunk_ids";
    pub const SYMBOLS: &str = "temp.rmp_symbol_ids";
    pub const MEMORIES: &str = "temp.rmp_memory_ids";
    pub const GENERATIONS: &str = "temp.rmp_generation_ids";
    pub const STREAMS: &str = "temp.rmp_stream_ids";
}

/// Every transitively-scoped child, child-first (a child appears before the parent whose id set it
/// scopes through). LOAD-BEARING completeness: the poison-sibling tripwire enumerates these plus
/// the `repo_id` tables and asserts the purge empties every one for the removed repo — so a NEW
/// transitive child that forgets to register here fails the tripwire rather than leaking in
/// production. `chunk_fts` is handled separately (it is an FTS5 virtual table deleted by rowid, not
/// a plain `DELETE ... WHERE id_column IN`).
const TRANSITIVE_SCOPED_TABLES: &[TransitiveTable] = &[
    // chunk children (chunks.id → chunk_id)
    TransitiveTable { table: "chunk_text", id_column: "chunk_id", parent_ids: purge_ids::CHUNKS },
    TransitiveTable {
        table: "chunk_embeddings",
        id_column: "chunk_id",
        parent_ids: purge_ids::CHUNKS,
    },
    TransitiveTable {
        table: "chunk_summaries",
        id_column: "chunk_id",
        parent_ids: purge_ids::CHUNKS,
    },
    TransitiveTable {
        table: "git_chunk_blame",
        id_column: "chunk_id",
        parent_ids: purge_ids::CHUNKS,
    },
    // symbol children (symbols.id → symbol_id)
    TransitiveTable {
        table: "symbol_facts",
        id_column: "symbol_id",
        parent_ids: purge_ids::SYMBOLS,
    },
    TransitiveTable {
        table: "symbol_fingerprints",
        id_column: "symbol_id",
        parent_ids: purge_ids::SYMBOLS,
    },
    TransitiveTable {
        table: "logical_symbol_members",
        id_column: "symbol_id",
        parent_ids: purge_ids::SYMBOLS,
    },
    // file children (files.id → source_file_id / file_id)
    TransitiveTable {
        table: "edges_data",
        id_column: "source_file_id",
        parent_ids: purge_ids::FILES,
    },
    TransitiveTable { table: "symbols", id_column: "file_id", parent_ids: purge_ids::FILES },
    TransitiveTable { table: "chunks", id_column: "file_id", parent_ids: purge_ids::FILES },
    // memory children (repo_memories.id → memory_id)
    TransitiveTable {
        table: "repo_memory_tags",
        id_column: "memory_id",
        parent_ids: purge_ids::MEMORIES,
    },
    TransitiveTable {
        table: "repo_memory_call_paths",
        id_column: "memory_id",
        parent_ids: purge_ids::MEMORIES,
    },
    TransitiveTable {
        table: "repo_memory_call_path_edges",
        id_column: "memory_id",
        parent_ids: purge_ids::MEMORIES,
    },
    // clone postings (clone_graph_generations.generation → build_generation)
    TransitiveTable {
        table: "clone_edges",
        id_column: "build_generation",
        parent_ids: purge_ids::GENERATIONS,
    },
    TransitiveTable {
        table: "clone_subblock_postings",
        id_column: "build_generation",
        parent_ids: purge_ids::GENERATIONS,
    },
    TransitiveTable {
        table: "clone_df_epoch",
        id_column: "build_generation",
        parent_ids: purge_ids::GENERATIONS,
    },
    // The table-sync entry log (table_sync_streams.stream_id → stream_id). Captured through the
    // directory because the stream id is a one-way hash and cannot be re-derived once the
    // directory row is swept. Retained chain-tip witnesses are intentionally not listed here.
    TransitiveTable {
        table: "table_sync_entries",
        id_column: "stream_id",
        parent_ids: purge_ids::STREAMS,
    },
    // Re-adoption work for removed writers on this repo's streams. It must not survive the
    // directory: stream identity is derived, so a re-registered repo must never let an old
    // removal re-author rows into its fresh stream.
    TransitiveTable {
        table: "table_sync_readoption_work",
        id_column: "stream_id",
        parent_ids: purge_ids::STREAMS,
    },
    // Re-adoption provenance for those same streams, retained only while the repo's accepted
    // history is retained.
    TransitiveTable {
        table: "table_sync_readoption_audit",
        id_column: "stream_id",
        parent_ids: purge_ids::STREAMS,
    },
    // The per-chain retained floor an accepted-entry compaction recorded. It rides the same
    // derived stream identity as the log it bounds, so it is swept with the directory for exactly
    // the same reason.
    TransitiveTable {
        table: "table_sync_retained_floors",
        id_column: "stream_id",
        parent_ids: purge_ids::STREAMS,
    },
    // Entries held awaiting a chain predecessor. Same reasoning as the accepted log above, and for
    // the same reason it must not be exempted: these are signed operations on a stream whose id is
    // derived, so retaining them across a re-registration of the repo would replay a removed
    // repo's history back into it.
    TransitiveTable {
        table: "table_sync_gapped_entries",
        id_column: "stream_id",
        parent_ids: purge_ids::STREAMS,
    },
];

/// Every base table (never a view, never an FTS shadow table) that carries a `repo_id` column, in
/// deterministic name order — the class-level backstop the purge sweeps and the tripwire
/// enumerates. An FTS5 virtual table that carries a `repo_id` column (`repo_memory_fts`,
/// `papertrail_fts`) is INCLUDED (it supports `DELETE ... WHERE repo_id = ?`); its shadow tables
/// carry no `repo_id`, so `PRAGMA table_info` never reports them here.
pub fn repo_scoped_table_names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT m.name FROM sqlite_master m
         WHERE m.type = 'table'
           AND m.name NOT LIKE 'sqlite_%'
           AND EXISTS (SELECT 1 FROM pragma_table_info(m.name) p WHERE p.name = 'repo_id')
         ORDER BY m.name",
    )?;
    let names = stmt.query_map([], |row| row.get::<_, String>(0))?;
    names.collect()
}

/// A per-table row count for the repo being removed — the confirmation summary + `--dry-run`
/// preview. `by_table` is dynamic (every `repo_id` table via [`repo_scoped_table_names`] plus the
/// transitively-scoped children), so a new scoped table is counted without a code change; only
/// non-zero entries are worth showing. `total_rows` is their exact sum.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RepoRowCounts {
    pub by_table: std::collections::BTreeMap<String, i64>,
    pub total_rows: i64,
}

impl RepoRowCounts {
    fn record(&mut self, table: &str, count: i64) {
        if count > 0 {
            *self.by_table.entry(table.to_string()).or_insert(0) += count;
            self.total_rows += count;
        }
    }
}

/// Count every row `repo_id` owns, across the class-level `repo_id` tables AND the transitively
/// scoped children — read-only, so it drives both the confirmation prompt and `--dry-run` without
/// mutating anything. The counts mirror exactly what [`purge_repo_rows`] deletes.
pub fn count_repo_rows(conn: &Connection, repo_id: &str) -> anyhow::Result<RepoRowCounts> {
    let mut counts = RepoRowCounts::default();
    for table in repo_scoped_table_names(conn)? {
        // `table` comes from `sqlite_master`, never user input.
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM \"{table}\" WHERE repo_id = ?1"),
            params![repo_id],
            |row| row.get(0),
        )?;
        counts.record(&table, count);
    }
    // Transitive children: count through the live parent scope. An older schema may predate a table
    // (e.g. `clone_df_epoch`, V051); this count runs BEFORE `rm` migrates (so `--dry-run` never
    // writes), so a table that does not exist is skipped — its row count is 0, and the destructive
    // purge runs post-migration (the `rm` command applies the schema after the preview, before
    // taking the write lock) where every listed table exists.
    //
    // BOTH halves are guarded, not just the child: a child can predate the parent that scopes it
    // (see [`parent_table`]), and an unguarded subquery against a missing parent fails the whole
    // plan on a store the preview is explicitly meant to tolerate.
    for transitive in TRANSITIVE_SCOPED_TABLES {
        if !crate::schema::table_exists(conn, transitive.table)?
            || !crate::schema::table_exists(conn, parent_table(transitive.parent_ids))?
        {
            continue;
        }
        let count: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM \"{}\" WHERE {} IN ({})",
                transitive.table,
                transitive.id_column,
                parent_id_select(transitive.parent_ids)
            ),
            params![repo_id],
            |row| row.get(0),
        )?;
        counts.record(transitive.table, count);
    }
    // `edges_data.source_file_id` is nullable. Normal index writers always stamp it, but legacy /
    // manually-created rows can carry NULL and therefore evade `NULL IN (victim file ids)`. Count
    // exactly the NULL-source rows whose FROM or TO endpoint is a captured victim symbol; a
    // non-NULL sibling-source cross-repo edge remains owned by that sibling and is not included.
    if crate::schema::table_exists(conn, "edges_data")? {
        let null_source_edges: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM edges_data WHERE source_file_id IS NULL AND (from_symbol_id \
                 IN ({symbols}) OR to_symbol_id IN ({symbols}))",
                symbols = parent_id_select(purge_ids::SYMBOLS),
            ),
            params![repo_id],
            |row| row.get(0),
        )?;
        counts.record("edges_data", null_source_edges);
    }
    // chunk_fts (contentless FTS, keyed by chunk.id) — counted like the other chunk children.
    if crate::schema::table_exists(conn, "chunk_fts")? {
        let chunk_fts: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM chunk_fts WHERE rowid IN ({})",
                parent_id_select(purge_ids::CHUNKS)
            ),
            params![repo_id],
            |row| row.get(0),
        )?;
        counts.record("chunk_fts", chunk_fts);
    }
    Ok(counts)
}

/// The live read-path subquery for a transitive parent's id set, mirroring the temp-table capture
/// in [`capture_purge_ids`] but evaluated inline against the current rows (used only by
/// [`count_repo_rows`], which runs before any delete).
fn parent_id_select(parent_ids: &str) -> &'static str {
    match parent_ids {
        purge_ids::FILES => "SELECT id FROM files WHERE repo_id = ?1",
        purge_ids::CHUNKS =>
            "SELECT id FROM chunks WHERE file_id IN (SELECT id FROM files WHERE repo_id = ?1)",
        purge_ids::SYMBOLS =>
            "SELECT id FROM symbols WHERE file_id IN (SELECT id FROM files WHERE repo_id = ?1)",
        purge_ids::MEMORIES => "SELECT id FROM repo_memories WHERE repo_id = ?1",
        purge_ids::GENERATIONS =>
            "SELECT generation FROM clone_graph_generations WHERE repo_id = ?1",
        purge_ids::STREAMS => "SELECT stream_id FROM table_sync_streams WHERE repo_id = ?1",
        other => unreachable!("unknown purge id set {other}"),
    }
}

/// The `repo_id`-bearing table each id set reads from — the PARENT half of the count-path existence
/// check. A child can outlive its parent's introduction: `table_sync_entries` arrived in V087 and
/// the `table_sync_streams` directory that scopes it only in V093, so on a store in between, the
/// child exists while the subquery's table does not. `count_repo_rows` runs on exactly such a store
/// (planning is read-only and pre-migration, so `--dry-run` never writes), and an unguarded
/// reference there fails the plan before the destructive path can migrate.
fn parent_table(parent_ids: &str) -> &'static str {
    match parent_ids {
        purge_ids::FILES | purge_ids::CHUNKS | purge_ids::SYMBOLS => "files",
        purge_ids::MEMORIES => "repo_memories",
        purge_ids::GENERATIONS => "clone_graph_generations",
        purge_ids::STREAMS => "table_sync_streams",
        other => unreachable!("unknown purge id set {other}"),
    }
}

/// Purge EVERY row `repo_id` owns. MUST run inside an IMMEDIATE transaction the caller opened
/// (all-or-nothing; a partial purge would leave the store internally inconsistent). Steps, in
/// order:
///  1. Capture the repo's file / chunk / symbol / memory / clone-generation id sets into temp
///     tables — snapshotted up front so every subsequent delete is correct regardless of ordering.
///  2. Delete the transitively-scoped children (including the contentless `chunk_fts` by rowid).
///  3. Sweep every `repo_id`-bearing table (the class-level backstop) with `DELETE ... WHERE
///     repo_id = ?` — reaches the direct tables plus, with `foreign_keys = ON`, cascades any child
///     not listed above.
///  4. Re-derive `commit_fts` (external-content on `git_commits`, desynced by the git-commit
///     delete).
///  5. Drop the temp id tables.
pub fn purge_repo_rows(conn: &Connection, repo_id: &str) -> anyhow::Result<()> {
    capture_purge_ids(conn, repo_id)?;

    // #782: `source_file_id` is nullable. Delete legacy / malformed NULL-source edges by either
    // endpoint BEFORE symbols are removed (the captured set makes this correct with foreign keys
    // both ON and OFF). Restricting this special case to NULL preserves a valid cross-repo edge
    // whose non-NULL source file belongs to a sibling.
    conn.execute(
        &format!(
            "DELETE FROM edges_data WHERE source_file_id IS NULL AND (from_symbol_id IN (SELECT \
             id FROM {symbols}) OR to_symbol_id IN (SELECT id FROM {symbols}))",
            symbols = purge_ids::SYMBOLS,
        ),
        [],
    )?;

    // Transitive children first (each keyed by a captured id set, so ordering vs. the sweep is
    // irrelevant — the temp tables decouple them from whether the parent rows still exist).
    for transitive in TRANSITIVE_SCOPED_TABLES {
        conn.execute(
            &format!(
                "DELETE FROM \"{}\" WHERE {} IN (SELECT id FROM {})",
                transitive.table, transitive.id_column, transitive.parent_ids
            ),
            [],
        )?;
    }
    // Re-adoption work parked under the all-zero placeholder stream has no directory row to scope
    // through and would otherwise survive forever. It exists only while its account has no
    // table-sync streams at all, so there is no authored history it could still repair, and any
    // removal it records is already folded into every peer's roster. Scoped to the accounts that
    // hold incarnation authority for THIS repository: a parked row is account-scoped, and an
    // unrelated account's repair must not be collateral of this purge. Rows already copied into a
    // real stream were handled above through purge_ids::STREAMS.
    if crate::schema::table_exists(conn, "table_sync_readoption_work")?
        && crate::schema::table_exists(conn, "account_repo_incarnation_current")?
    {
        conn.execute(
            "DELETE FROM table_sync_readoption_work
             WHERE stream_id = zeroblob(32)
               AND account_id IN (
                   SELECT account_id FROM account_repo_incarnation_current
                    WHERE repository_id = ?1
               )",
            params![repo_id],
        )?;
    }
    // chunk_fts is contentless FTS5 keyed by chunk.id: delete by rowid from the captured chunk set
    // (it carries no repo_id and no FK, so neither the sweep nor a cascade would ever reach it).
    conn.execute(
        &format!("DELETE FROM chunk_fts WHERE rowid IN (SELECT id FROM {})", purge_ids::CHUNKS),
        [],
    )?;

    // Class-level sweep: every repo_id table, DELETE WHERE repo_id = ?. This is the backstop that
    // makes a NEW scoped table purge automatically (the tripwire enforces coverage).
    for table in repo_scoped_table_names(conn)? {
        conn.execute(&format!("DELETE FROM \"{table}\" WHERE repo_id = ?1"), params![repo_id])?;
    }

    // commit_fts is external-content on git_commits, now desynced by the git-commit delete above.
    // 'rebuild' re-derives it from the remaining commits (all repos) — the desync-safe fixup.
    rebuild_commit_fts(conn)?;

    drop_purge_ids(conn)?;
    Ok(())
}

/// Snapshot the repo's id sets into temp tables (see [`purge_ids`]). Captured BEFORE any delete so
/// the transitive-child deletes are correct no matter the order or FK cascade behavior.
fn capture_purge_ids(conn: &Connection, repo_id: &str) -> anyhow::Result<()> {
    // Dropped first so a reused connection (a second purge on one process) starts clean.
    drop_purge_ids(conn)?;
    conn.execute(
        &format!(
            "CREATE TEMP TABLE {} AS SELECT id FROM files WHERE repo_id = ?1",
            temp_name(purge_ids::FILES)
        ),
        params![repo_id],
    )?;
    conn.execute(
        &format!(
            "CREATE TEMP TABLE {} AS SELECT id FROM chunks WHERE file_id IN (SELECT id FROM {})",
            temp_name(purge_ids::CHUNKS),
            purge_ids::FILES
        ),
        [],
    )?;
    conn.execute(
        &format!(
            "CREATE TEMP TABLE {} AS SELECT id FROM symbols WHERE file_id IN (SELECT id FROM {})",
            temp_name(purge_ids::SYMBOLS),
            purge_ids::FILES
        ),
        [],
    )?;
    conn.execute(
        &format!(
            "CREATE TEMP TABLE {} AS SELECT id FROM repo_memories WHERE repo_id = ?1",
            temp_name(purge_ids::MEMORIES)
        ),
        params![repo_id],
    )?;
    // The clone-generation id set uses the column name `id` in the temp table for a uniform
    // `SELECT id FROM <temp>` in the child deletes, even though the source column is `generation`.
    conn.execute(
        &format!(
            "CREATE TEMP TABLE {} AS SELECT generation AS id FROM clone_graph_generations WHERE \
             repo_id = ?1",
            temp_name(purge_ids::GENERATIONS)
        ),
        params![repo_id],
    )?;
    // Same aliasing for the sync stream directory. This capture is what makes the entry-log delete
    // possible at all: `stream_id` is a one-way hash of
    // `(repo_id, account_id, incarnation_ref, scope_id)`, so once the class sweep removes the
    // directory row there is no way back from an entry to its repo.
    conn.execute(
        &format!(
            "CREATE TEMP TABLE {} AS SELECT stream_id AS id FROM table_sync_streams WHERE repo_id \
             = ?1",
            temp_name(purge_ids::STREAMS)
        ),
        params![repo_id],
    )?;
    Ok(())
}

fn drop_purge_ids(conn: &Connection) -> anyhow::Result<()> {
    for temp in [
        purge_ids::FILES,
        purge_ids::CHUNKS,
        purge_ids::SYMBOLS,
        purge_ids::MEMORIES,
        purge_ids::GENERATIONS,
        purge_ids::STREAMS,
    ] {
        conn.execute(&format!("DROP TABLE IF EXISTS {temp}"), [])?;
    }
    Ok(())
}

/// The bare table name (without the `temp.` schema qualifier) for a `CREATE TEMP TABLE`, which
/// names the table unqualified while later reads qualify it `temp.<name>`.
fn temp_name(qualified: &str) -> &str {
    qualified.strip_prefix("temp.").unwrap_or(qualified)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `count_repo_rows` is the read-only planning path, and `rag-rat rm` runs it BEFORE migrating
    /// so `--dry-run` never writes. Every transitive child must therefore tolerate a schema
    /// that predates it — including one that predates its PARENT: `table_sync_entries` shipped
    /// in V087 and the `table_sync_streams` directory that scopes it only in V093, so on a
    /// store in between the child exists while the subquery's table does not. Dropping the
    /// parent reproduces that shape.
    #[test]
    fn counting_tolerates_a_transitive_child_whose_parent_table_does_not_exist_yet() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        conn.execute_batch("DROP TABLE table_sync_streams;").unwrap();

        let counts = count_repo_rows(&conn, "some-repo")
            .expect("planning must survive a store whose directory table predates the entry log");
        assert_eq!(counts.total_rows, 0, "nothing is counted for an unknown repo");
    }

    /// Parked re-adoption work is account-scoped: purging one account's repo must not delete
    /// another account's repair.
    #[test]
    fn purging_parked_readoption_work_leaves_unrelated_accounts_untouched() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        conn.execute_batch(
            "INSERT INTO account_repo_incarnation_current(
                 account_id, repository_id, incarnation_ref
             ) VALUES (zeroblob(32), 'repo-x', zeroblob(32)),
                      (x'1100000000000000000000000000000000000000000000000000000000000000',
                       'repo-y', zeroblob(32));
             INSERT INTO table_sync_readoption_work(
                 account_id, device_fingerprint, stream_id, roster_ref, removed_at_epoch,
                 enqueued_at_ms
             ) VALUES (zeroblob(32), zeroblob(32), zeroblob(32), zeroblob(32), 1, 2),
                      (x'1100000000000000000000000000000000000000000000000000000000000000',
                       zeroblob(32), zeroblob(32), zeroblob(32), 1, 2);
             INSERT INTO table_sync_readoption_work(
                 account_id, device_fingerprint, stream_id, roster_ref, removed_at_epoch,
                 enqueued_at_ms, processed_at_ms
             ) VALUES (zeroblob(32), \
             x'2200000000000000000000000000000000000000000000000000000000000000',
                       zeroblob(32), \
             x'2200000000000000000000000000000000000000000000000000000000000000',
                       1, 2, 3);",
        )
        .unwrap();

        purge_repo_rows(&conn, "repo-x").unwrap();

        let remaining: Vec<Vec<u8>> = conn
            .prepare("SELECT account_id FROM table_sync_readoption_work")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut expected = vec![0x11];
        expected.extend([0; 31]);
        assert_eq!(
            remaining,
            vec![expected],
            "repo-y's parked work survives; repo-x's rows are gone, stamped or not"
        );
    }
}
