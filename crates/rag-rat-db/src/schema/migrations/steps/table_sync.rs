use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::schema::migrations::{add_column_if_missing, column_exists};

/// V085 (#691 A-pre): memory-sync provenance + edge tombstones — the write-path foundation for
/// projecting synced content into the read tables without corrupting the local reconcile.
///
/// - `origin` on `repo_memories` / `repo_node_edges` distinguishes a locally-authored row from one
///   projected from a synced sibling's `/3` content. It is LOAD-BEARING on the WRITE path: the
///   memory reconcile authors every read-table row MISSING from the accepted-`/3` projection, so a
///   synced row whose acceptance is later revoked must NOT be re-authored as local `/3` (that
///   forges local authorship and re-legitimizes revoked content). The reconcile gates on `origin =
///   'local'`. Every existing row is locally authored, so the `'local'` default is a correct
///   backfill.
/// - `present` on `content_projected_edges` retains edge TOMBSTONES (`present = 0`) rather than
///   dropping removed edges. Without it a foreign `EdgeRemove` leaves the local `repo_node_edges`
///   row a ghost, the reconcile re-authors it at a fresh Lamport, and the remove loses LWW forever
///   — a cross-device op-log growth loop. Live edges default `present = 1`; the projector-version
///   bump rebuilds the projection to write tombstones going forward.
///
/// Additive columns with correct defaults; atomic under one immediate txn; idempotent via
/// `add_column_if_missing`.
pub fn apply_sync_origin_and_edge_tombstone(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    add_column_if_missing(
        &tx,
        "repo_memories",
        "origin",
        "TEXT NOT NULL DEFAULT 'local' CHECK (origin IN ('local', 'synced'))",
    )?;
    add_column_if_missing(
        &tx,
        "repo_node_edges",
        "origin",
        "TEXT NOT NULL DEFAULT 'local' CHECK (origin IN ('local', 'synced'))",
    )?;
    add_column_if_missing(&tx, "content_projected_edges", "present", "INTEGER NOT NULL DEFAULT 1")?;
    tx.commit()
}

/// V086 (#828): stand up the incrementally-maintained `content_revision` digest.
///
/// Invariant established here: `content_digest_state.state` (one row, id = 1) is the 256-bit
/// additive multiset hash of `{(path, sha256) : main.files, kind != 'deleted'}` at every
/// transaction boundary, maintained by the three `files_content_digest_*` triggers
/// [`crate::content_digest::ensure_content_digest`] creates. `content_revision()` becomes an O(1)
/// read of that row (rendered `ms1-…`) instead of the O(N) `main.files` scan-sort-concat-hash.
///
/// The body runs in ONE immediate transaction, in order:
///  1. Create the state table + triggers (idempotent; a future `files`-rebuild migration MUST call
///     the same helper and reseed, because `DROP TABLE files` silently drops the triggers).
///  2. Seed the state row with a from-scratch Rust fold over the current non-deleted `files` — the
///     SAME per-row hash the trigger fold uses, so the trigger-maintained state and a recompute can
///     never disagree. Atomic with trigger creation, so no write slips between.
///  3. Re-stamp every freshness stamp that equals the FROZEN legacy digest (the pre-#828
///     `hex_sha256(group_concat(path||':'||sha256 ORDER BY path))`, inlined because migrations are
///     snapshots) to the new rendered digest. This is the ONLY place the digest value change is
///     absorbed: a stamp equal to the legacy value was fresh, so pointing it at the new value
///     avoids a one-time full FTS re-tokenize (`fts_source_revision`), a ~1 GB clone-graph rebuild
///     (`clone_graph_generations.source_revision`), and a reset of the clone quiet window
///     (`clone_graph_quiet_candidate_revision`). A stamp that did NOT equal the legacy digest was
///     already stale and is left for the normal freshness machinery — exactly as it would have
///     been.
///
/// If step 3 were dropped everything still self-heals (one FTS rebuild, one quiet-gated clone
/// rebuild); the re-stamp is one cheap legacy scan that avoids that first-use rebuild storm.
pub fn apply_content_digest_state(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    // 1. Table + triggers.
    crate::content_digest::ensure_content_digest(&tx)?;

    // 2. From-scratch seed fold over the current non-deleted rows (order-free — the fold is
    //    commutative, so no ORDER BY is needed).
    let mut state = [0u64; 4];
    let mut rows_folded: i64 = 0;
    {
        let mut stmt = tx.prepare("SELECT path, sha256 FROM main.files WHERE kind != 'deleted'")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let sha256: String = row.get(1)?;
            let hash = crate::content_digest::content_row_hash(&path, &sha256);
            crate::content_digest::fold_row(
                &mut state,
                &hash,
                crate::content_digest::FoldSign::Add,
            );
            rows_folded += 1;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO content_digest_state(id, state, rows_folded) VALUES (1, ?1, ?2)",
        params![crate::content_digest::encode_state(&state), rows_folded],
    )?;

    // 3. Re-stamp the frozen legacy digest -> the new rendered digest wherever it was still
    //    current.
    let legacy_concat: String = tx.query_row(
        "SELECT COALESCE(group_concat(pv, ','), '') FROM (SELECT path || ':' || sha256 AS pv FROM \
         main.files WHERE kind != 'deleted' ORDER BY path)",
        [],
        |row| row.get(0),
    )?;
    let legacy_digest = rag_rat_base::hash::hex_sha256(legacy_concat.as_bytes());
    let new_digest = crate::content_digest::render_revision(&state);
    // The GLOBAL freshness stamps (`content_revision` keeps `global_status`'s rollup consistent;
    // `fts_source_revision` prevents a full chunk-text re-tokenize on first `ensure_fts_fresh`).
    tx.execute(
        "UPDATE index_meta SET value = ?1
         WHERE key IN ('fts_source_revision', 'content_revision') AND value = ?2",
        params![new_digest, legacy_digest],
    )?;
    // Every clone-graph generation stamped at the legacy digest keeps the postings fast path
    // serving and skips a one-time full rebuild.
    tx.execute(
        "UPDATE clone_graph_generations SET source_revision = ?1 WHERE source_revision = ?2",
        params![new_digest, legacy_digest],
    )?;
    // A per-repo armed quiet-window candidate survives the upgrade instead of resetting its
    // stability clock (the key literal matches CLONE_GRAPH_QUIET_REVISION_META in rag-rat-core).
    tx.execute(
        "UPDATE repo_meta SET value = ?1
         WHERE key = 'clone_graph_quiet_candidate_revision' AND value = ?2",
        params![new_digest, legacy_digest],
    )?;

    tx.commit()
}

/// V087 — the table→log sync engine's bookkeeping tables (transport-independent).
///
/// The engine replicates derived/metadata rows as self-describing typed-CBOR ops on a signed
/// per-scope stream, folded by WHOLE-ROW last-writer-wins. The side tables carry the state the fold
/// and producer need; none holds authored content (that lives in the replicated tables themselves)
/// — these are pure sync bookkeeping.
///
/// - `sync_published_rows` is the anti-echo record: the post-apply hash of a row's SYNCED columns.
///   The producer skips a row whose current synced-hash already matches, so a remotely-applied row
///   is never re-signed and rebroadcast (the echo-republish loop). Local re-resolution churn must
///   never enter this hash, so it covers synced columns only.
/// - `sync_row_tombstones` is the per-row deletion clock: a `Remove` records `(lamport,
///   device_fingerprint)`, and a later `Upsert` older than it is suppressed (never resurrects a
///   deleted row) while a newer one overrides it. Without it, out-of-order delivery would let a
///   stale delete win and an even older insert resurrect.
/// - `sync_row_clocks` is the per-row latest-write clock — the whole-row LWW authority. An `Upsert`
///   wins the entire row, and a `Remove` deletes it, only when it beats this clock; the winner then
///   raises it. So convergence and delete/insert ordering hold regardless of arrival order.
/// - `table_sync_entries` is the engine's OWN signed hash-chained entry log — deliberately separate
///   from `oplog_entries`, whose upgrade re-fold (`reproject_all_streams`) decodes every stored
///   stream as a memory-content op and would choke on a table op. One chain per `(stream_id,
///   device_fingerprint)`, `lamport` strictly increasing.
///
/// All STRICT; `CREATE TABLE IF NOT EXISTS` so the migration is idempotent; one IMMEDIATE txn.
pub fn apply_table_sync_tables(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_entries(
             entry_hash         BLOB    NOT NULL PRIMARY KEY,
             stream_id          BLOB    NOT NULL,
             device_fingerprint BLOB    NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB,
             signed_bytes       BLOB    NOT NULL,
             received_at_ms     INTEGER NOT NULL,
             UNIQUE(stream_id, device_fingerprint, lamport)
         ) STRICT;
         -- Read the stream's Lamport tip (`MAX(lamport) WHERE stream_id = ?`, on every author and
         -- accept) from an index tail instead of scanning the stream; the UNIQUE index above leads
         -- with device_fingerprint, so it cannot answer a per-stream MAX.
         CREATE INDEX IF NOT EXISTS table_sync_entries_stream_lamport
             ON table_sync_entries(stream_id, lamport);
         CREATE TABLE IF NOT EXISTS sync_published_rows(
             repo_id     TEXT NOT NULL,
             table_name  TEXT NOT NULL,
             row_pk      TEXT NOT NULL,
             synced_hash TEXT NOT NULL,
             PRIMARY KEY(repo_id, table_name, row_pk)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS sync_row_tombstones(
             repo_id            TEXT    NOT NULL,
             table_name         TEXT    NOT NULL,
             row_pk             TEXT    NOT NULL,
             lamport            INTEGER NOT NULL,
             device_fingerprint TEXT    NOT NULL,
             PRIMARY KEY(repo_id, table_name, row_pk)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS sync_row_clocks(
             repo_id            TEXT    NOT NULL,
             table_name         TEXT    NOT NULL,
             row_pk             TEXT    NOT NULL,
             lamport            INTEGER NOT NULL,
             device_fingerprint TEXT    NOT NULL,
             PRIMARY KEY(repo_id, table_name, row_pk)
         ) STRICT;",
    )?;
    tx.commit()
}

/// V088 (#830): cache each clone generation's posting-row count on its generation row.
///
/// The #598 delta work budget is `max(100_000, 2 * postings_row_count(generation))`; deriving that
/// with `COUNT(*) FROM clone_subblock_postings WHERE build_generation = ?` scanned the whole
/// (generation-keyed) postings table on every delta pass. This adds a maintained
/// `postings_row_count` column so the budget reads one generation row instead. The count is kept
/// exact going forward — seeded at build (`complete_generation`) and adjusted by
/// (inserted − deleted) in each delta write-back, both inside the same transaction as the postings
/// change — so the column always equals `COUNT(*)` for that generation.
///
/// Additive + idempotent. The column type is STRICT-valid and defaulted so a row the backfill does
/// not reach reads back 0. The backfill is UNCONDITIONAL (not gated on the column being freshly
/// added): it recomputes the exact `COUNT(*)` the column is maintained to hold, so on a maintained
/// DB a re-apply is a value no-op, and a torn add-then-crash retry (which re-enters with the column
/// already present) still backfills instead of stranding an all-zero column. Re-scanning on the
/// rare `index --full` re-apply is the acceptable cost of that robustness — the per-delta scan this
/// migration removes is the hot one.
pub fn apply_clone_postings_row_count(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(
        conn,
        "clone_graph_generations",
        "postings_row_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch(
        "UPDATE clone_graph_generations
            SET postings_row_count = (
                SELECT COUNT(*) FROM clone_subblock_postings
                 WHERE build_generation = clone_graph_generations.generation);",
    )?;
    Ok(())
}

/// V089 (#945): durable, single-use enrollment invites.
pub fn apply_sync_invites(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_invites(
             nonce         BLOB    NOT NULL PRIMARY KEY CHECK(length(nonce) = 32),
             account_id    BLOB    NOT NULL CHECK(length(account_id) = 32),
             role          TEXT    NOT NULL CHECK(role IN ('read_only', 'member', 'owner')),
             label         TEXT,
             expires_at_ms INTEGER NOT NULL,
             created_at_ms INTEGER NOT NULL,
             used_at_ms    INTEGER,
             used_transport_node BLOB CHECK(
                 used_transport_node IS NULL OR length(used_transport_node) = 32
             ),
             used_ed25519_pubkey BLOB CHECK(
                 used_ed25519_pubkey IS NULL OR length(used_ed25519_pubkey) = 32
             ),
             used_x25519_pubkey BLOB CHECK(
                 used_x25519_pubkey IS NULL OR length(used_x25519_pubkey) = 32
             ),
             receipt_hash BLOB CHECK(receipt_hash IS NULL OR length(receipt_hash) = 32),
             receipt_signed BLOB,
             receipt_bytes BLOB,
             CHECK(
                 (used_at_ms IS NULL
                  AND used_transport_node IS NULL
                  AND used_ed25519_pubkey IS NULL
                  AND used_x25519_pubkey IS NULL
                  AND receipt_hash IS NULL
                  AND receipt_signed IS NULL
                  AND receipt_bytes IS NULL)
                 OR
                 (used_at_ms IS NOT NULL
                  AND used_transport_node IS NOT NULL
                  AND used_ed25519_pubkey IS NOT NULL
                  AND used_x25519_pubkey IS NOT NULL
                  AND receipt_hash IS NOT NULL
                  AND receipt_signed IS NOT NULL
                  AND receipt_bytes IS NOT NULL)
             )
         ) STRICT;
         CREATE INDEX IF NOT EXISTS sync_invites_account_expiry
             ON sync_invites(account_id, expires_at_ms);",
    )
}

/// V090 (#949): durable candidate-capacity reservations for outstanding enrollment invites.
pub fn apply_account_candidate_reservations(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_candidate_reservations(
             reservation_id BLOB    NOT NULL PRIMARY KEY CHECK(length(reservation_id) = 32),
             account_id     BLOB    NOT NULL CHECK(length(account_id) = 32),
             reserved_entries INTEGER NOT NULL CHECK(reserved_entries >= 0),
             reserved_bytes   INTEGER NOT NULL CHECK(reserved_bytes >= 0),
             expires_at_ms    INTEGER NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_candidate_reservations_account_expiry
             ON account_candidate_reservations(account_id, expires_at_ms);",
    )
}

/// V091 (#949): track the live key-target count each invite reservation covers, so any fold that
/// grows the target set — local authoring or REMOTELY synced `StreamOwn`/wrap entries — can top
/// the reservation up to the current mandatory redemption cost.
///
/// Additive + idempotent. The backfill is exact for every reservation written since V090: those
/// rows hold `reserved_entries = 1 (DeviceAdd) + covered_targets`, so `reserved_entries - 1`
/// recovers the covered target count.
pub fn apply_account_candidate_reservation_targets(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(
        conn,
        "account_candidate_reservations",
        "reserved_targets",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch(
        "UPDATE account_candidate_reservations
            SET reserved_targets = MAX(0, reserved_entries - 1);",
    )?;
    Ok(())
}

/// V093: maintain one O(1) Lens enrichment revision per repository. The revision is deliberately
/// a counter, not a timestamp: multiple writes in one millisecond, in-place promotions, and clone
/// graph publication must still invalidate connected editors.
pub fn apply_lens_enrichment_revision(conn: &Connection) -> rusqlite::Result<()> {
    // History imports and Oracle passes advance the clock once at their transaction boundary
    // instead of once per written row. Both write their table in bulk inside a single
    // transaction — an Oracle run rewrites a verdict for every resolved edge, hundreds of
    // thousands of them on a large index — and Lens freshness only needs the one revision change
    // the publication makes visible. `edge_oracle` gets that bump from the `oracle_runs` row its
    // run commits alongside the verdicts; the dead-checkout verdict sweep, which writes no run
    // row, bumps the clock itself. Always remove the per-row triggers so replaying this migration
    // repairs a database initialized by an older build.
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS git_file_changes_lens_revision_insert;
         DROP TRIGGER IF EXISTS git_file_changes_lens_revision_delete;
         DROP TRIGGER IF EXISTS git_file_changes_lens_revision_update;
         DROP TRIGGER IF EXISTS edge_oracle_lens_revision_insert;
         DROP TRIGGER IF EXISTS edge_oracle_lens_revision_delete;
         DROP TRIGGER IF EXISTS edge_oracle_lens_revision_update;",
    )?;
    for (table, trigger_prefix) in [
        ("repo_memories", "memories_lens_revision"),
        ("repo_memory_bindings", "memory_bindings_lens_revision"),
        ("memory_reality", "memory_reality_lens_revision"),
        ("memory_summaries", "memory_summaries_lens_revision"),
        ("papertrail_items", "papertrail_items_lens_revision"),
        ("papertrail_refs", "papertrail_refs_lens_revision"),
        ("papertrail_distill", "papertrail_distill_lens_revision"),
        ("papertrail_distill_anchors", "papertrail_distill_anchors_lens_revision"),
        ("clone_refinements", "clone_refinements_lens_revision"),
        ("oracle_runs", "oracle_runs_lens_revision"),
    ] {
        create_repo_scoped_lens_revision_triggers(
            conn,
            table,
            trigger_prefix,
            crate::meta::LENS_ENRICHMENT_REVISION_META,
        )?;
    }
    create_live_clone_graph_revision_triggers(
        conn,
        "clone_graph_generations_lens_revision",
        crate::meta::LENS_ENRICHMENT_REVISION_META,
    )?;
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS clone_graph_pointer_lens_revision_insert;
         DROP TRIGGER IF EXISTS clone_graph_pointer_lens_revision_delete;
         DROP TRIGGER IF EXISTS clone_graph_pointer_lens_revision_update;",
    )?;
    Ok(())
}

/// V102: split the aggregate Lens enrichment clock into the five editor data lanes.
pub fn apply_lens_lane_revisions(conn: &Connection) -> rusqlite::Result<()> {
    use crate::meta;

    for (table, trigger_prefix, key) in [
        ("repo_memories", "memories_lane_revision", meta::LENS_MEMORIES_REVISION_META),
        (
            "repo_memory_bindings",
            "memory_bindings_lane_revision",
            meta::LENS_MEMORIES_REVISION_META,
        ),
        ("memory_reality", "memory_reality_lane_revision", meta::LENS_MEMORIES_REVISION_META),
        ("memory_summaries", "memory_summaries_lane_revision", meta::LENS_MEMORIES_REVISION_META),
        ("papertrail_items", "papertrail_items_lane_revision", meta::LENS_PAPERTRAIL_REVISION_META),
        ("papertrail_refs", "papertrail_refs_lane_revision", meta::LENS_PAPERTRAIL_REVISION_META),
        (
            "papertrail_distill",
            "papertrail_distill_lane_revision",
            meta::LENS_PAPERTRAIL_REVISION_META,
        ),
        (
            "papertrail_distill_anchors",
            "papertrail_distill_anchors_lane_revision",
            meta::LENS_PAPERTRAIL_REVISION_META,
        ),
        ("clone_refinements", "clone_refinements_lane_revision", meta::LENS_CLONES_REVISION_META),
        ("oracle_runs", "oracle_symbols_lane_revision", meta::LENS_SYMBOLS_REVISION_META),
        ("oracle_runs", "oracle_clones_lane_revision", meta::LENS_CLONES_REVISION_META),
    ] {
        create_repo_scoped_lens_revision_triggers(conn, table, trigger_prefix, key)?;
    }
    create_live_clone_graph_revision_triggers(
        conn,
        "clone_graph_generations_lane_revision",
        meta::LENS_CLONES_REVISION_META,
    )
}

/// V092 (#949): stop duplicating every enrollment receipt in the invite row.
///
/// Each consumed invite used to store the complete signed account bootstrap (`receipt_bytes`),
/// so a fleet enrolling within the replay window kept one full history copy PER invite —
/// quadratic growth in a grow-only store. New redemptions persist only the joiner-specific
/// `DeviceAdd` envelope plus the manifest of receipt entry hashes (`receipt_entries`, 32 bytes
/// each in receipt order), and replay reconstructs the EXACT acknowledged receipt from the
/// grow-only candidate DAG. The legacy `receipt_bytes` column is RETAINED (never written again)
/// so invites consumed before this migration keep replaying through their 24h window; pruning
/// drops both forms. Rebuilds the table, preserving every row.
pub fn apply_sync_invites_normalized_receipts(conn: &Connection) -> rusqlite::Result<()> {
    // A re-apply (the sanctioned `index --full` recovery) sees the V092 shape: preserve its
    // `receipt_entries` manifests rather than nulling them into the consumed-row CHECK.
    let already_normalized = column_exists(conn, "sync_invites", "receipt_entries")?;
    let entries_expr = if already_normalized { "receipt_entries" } else { "NULL" };
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE sync_invites_v092(
             nonce         BLOB    NOT NULL PRIMARY KEY CHECK(length(nonce) = 32),
             account_id    BLOB    NOT NULL CHECK(length(account_id) = 32),
             role          TEXT    NOT NULL CHECK(role IN ('read_only', 'member', 'owner')),
             label         TEXT,
             expires_at_ms INTEGER NOT NULL,
             created_at_ms INTEGER NOT NULL,
             used_at_ms    INTEGER,
             used_transport_node BLOB CHECK(
                 used_transport_node IS NULL OR length(used_transport_node) = 32
             ),
             used_ed25519_pubkey BLOB CHECK(
                 used_ed25519_pubkey IS NULL OR length(used_ed25519_pubkey) = 32
             ),
             used_x25519_pubkey BLOB CHECK(
                 used_x25519_pubkey IS NULL OR length(used_x25519_pubkey) = 32
             ),
             receipt_hash BLOB CHECK(receipt_hash IS NULL OR length(receipt_hash) = 32),
             receipt_signed BLOB,
             receipt_entries BLOB CHECK(
                 receipt_entries IS NULL OR length(receipt_entries) % 32 = 0
             ),
             receipt_bytes BLOB,
             CHECK(
                 (used_at_ms IS NULL
                  AND used_transport_node IS NULL
                  AND used_ed25519_pubkey IS NULL
                  AND used_x25519_pubkey IS NULL
                  AND receipt_hash IS NULL
                  AND receipt_signed IS NULL
                  AND receipt_entries IS NULL
                  AND receipt_bytes IS NULL)
                 OR
                 (used_at_ms IS NOT NULL
                  AND used_transport_node IS NOT NULL
                  AND used_ed25519_pubkey IS NOT NULL
                  AND used_x25519_pubkey IS NOT NULL
                  AND receipt_hash IS NOT NULL
                  AND receipt_signed IS NOT NULL
                  AND (receipt_entries IS NOT NULL OR receipt_bytes IS NOT NULL))
              )
         ) STRICT;",
    )?;
    tx.execute_batch(&format!(
        "INSERT INTO sync_invites_v092(
             nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms,
             used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
             receipt_hash, receipt_signed, receipt_entries, receipt_bytes)
          SELECT nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms,
             used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
             receipt_hash, receipt_signed, {entries_expr}, receipt_bytes
            FROM sync_invites;
         DROP TABLE sync_invites;
         ALTER TABLE sync_invites_v092 RENAME TO sync_invites;
         CREATE INDEX IF NOT EXISTS sync_invites_account_expiry
             ON sync_invites(account_id, expires_at_ms);"
    ))?;
    tx.commit()
}

/// V093 (#1001): the table-sync forward-compat projection substrate — facts the engine could not
/// previously record, each of which silently corrupts a synced table once one registers.
///
/// - `table_sync_entries.pending_reason` / `.pending_projector_version`: an entry this binary
///   cannot fully project (unknown column, unknown op-kind, undecodable payload, table out of
///   scope) is retained but was never marked, so redelivery short-circuits on `entry_exists` and
///   the payload is unrecoverable. Marking it lets a later binary that understands it replay
///   exactly the outstanding set. NULL reason = fully projected.
/// - `table_sync_entries.quarantine_reason`: the TERMINAL counterpart. A payload rejected on its
///   own merits — a type mismatch, a constraint violation — is not a version gap, and no later
///   binary makes those data fit, so it must leave the replay worklist. Recording WHY keeps it
///   discoverable, instead of a retained entry that looks fully projected but was actually
///   rejected.
/// - `table_sync_streams`: `stream_id` is a ONE-WAY sha256 of `(repo_id, account_id, scope_id)`,
///   and entries store only the stream id. Replay needs `repo_id` to apply and the scope to resolve
///   the table spec, so without this directory a stored entry cannot be replayed at all.
/// - `sync_published_rows.projector_version`: the anti-echo hash is computed over the hashing
///   binary's `spec.columns`, so a stored hash means "this row under column set C" with C implicit.
///   Once a column set grows, every stored hash mismatches structurally and every row reads as a
///   local delta — re-authoring the whole table at fresh winning lamports on every upgrading
///   device. Recording the version makes a mismatched hash detectable as NOT COMPARABLE instead.
///
/// Additive and idempotent: the sanctioned `index --full` re-apply sees the V093 shape and skips
/// the column adds. Nothing backfills, because no table is registered yet — `SYNCABLE_TABLES` is
/// empty, so `table_sync_entries` and `sync_published_rows` are necessarily empty too, and the
/// `projector_version` default of 0 is therefore unreachable rather than a lie about existing rows.
pub fn apply_table_sync_projection_state(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    // Per-column, never one guard for the group: a store that applied an EARLIER shape of this
    // migration already has the first column, so a group guard would skip the rest forever — the
    // ladder records the migration as applied and never re-runs it, leaving a column missing on a
    // store that reports itself current.
    add_column_if_missing(&tx, "table_sync_entries", "pending_reason", "TEXT")?;
    add_column_if_missing(&tx, "table_sync_entries", "pending_projector_version", "INTEGER")?;
    add_column_if_missing(&tx, "table_sync_entries", "quarantine_reason", "TEXT")?;
    // V095 REPLACED this column with the per-table `spec_version`. `schema::apply` — the
    // `index --full` recovery — re-runs the WHOLE ladder over an existing store, so without this
    // check a store already at V095 would get the dead column back on every full reindex, and V095
    // (which can only restore its shape by rebuilding the table) would have to DROP a table that by
    // then holds live publication state. Losing that state is not merely churn: `produce_row_ops`
    // finds a locally-deleted row by its surviving published record, so wiping the table strands
    // every unsent deletion and leaves peers holding the row forever.
    if !column_exists(&tx, "sync_published_rows", "spec_version")? {
        add_column_if_missing(
            &tx,
            "sync_published_rows",
            "projector_version",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_streams(
             stream_id  BLOB NOT NULL PRIMARY KEY,
             repo_id    TEXT NOT NULL,
             account_id BLOB NOT NULL,
             scope_id   TEXT NOT NULL
         ) STRICT;
         -- The refold's worklist. Partial, so a projector bump costs O(outstanding entries) rather
         -- than a scan of the whole log, and the steady state (nothing pending) costs nothing.
         CREATE INDEX IF NOT EXISTS table_sync_entries_pending
             ON table_sync_entries(pending_reason)
             WHERE pending_reason IS NOT NULL;",
    )?;
    tx.commit()
}

fn create_repo_scoped_lens_revision_triggers(
    conn: &Connection,
    table: &str,
    trigger_prefix: &str,
    key: &str,
) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "DROP TRIGGER IF EXISTS {trigger_prefix}_insert;
         DROP TRIGGER IF EXISTS {trigger_prefix}_delete;
         DROP TRIGGER IF EXISTS {trigger_prefix}_update;"
    ))?;
    if !column_exists(conn, table, "repo_id")? {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "CREATE TRIGGER {trigger_prefix}_insert
                  AFTER INSERT ON {table}
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, '{key}', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
             CREATE TRIGGER {trigger_prefix}_delete
                 AFTER DELETE ON {table}
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, '{key}', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
             CREATE TRIGGER {trigger_prefix}_update
                 AFTER UPDATE ON {table}
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, '{key}', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, '{key}', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;"
    ))?;
    Ok(())
}

fn create_live_clone_graph_revision_triggers(
    conn: &Connection,
    trigger_prefix: &str,
    key: &str,
) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "DROP TRIGGER IF EXISTS {trigger_prefix}_insert;
         DROP TRIGGER IF EXISTS {trigger_prefix}_delete;
         DROP TRIGGER IF EXISTS {trigger_prefix}_update;"
    ))?;
    if !column_exists(conn, "clone_graph_generations", "repo_id")? {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "CREATE TRIGGER {trigger_prefix}_insert
         AFTER INSERT ON clone_graph_generations
         WHEN EXISTS (
             SELECT 1 FROM repo_meta
             WHERE repo_id = NEW.repo_id AND key = 'clone_graph_live_generation'
               AND CAST(value AS INTEGER) = NEW.generation
         )
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value) VALUES (NEW.repo_id, '{key}', '1')
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
         CREATE TRIGGER {trigger_prefix}_delete
         AFTER DELETE ON clone_graph_generations
         WHEN EXISTS (
             SELECT 1 FROM repo_meta
             WHERE repo_id = OLD.repo_id AND key = 'clone_graph_live_generation'
               AND CAST(value AS INTEGER) = OLD.generation
         )
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value) VALUES (OLD.repo_id, '{key}', '1')
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
         CREATE TRIGGER {trigger_prefix}_update
         AFTER UPDATE ON clone_graph_generations
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value)
             SELECT NEW.repo_id, '{key}', '1'
             WHERE EXISTS (
                 SELECT 1 FROM repo_meta
                 WHERE repo_id = NEW.repo_id AND key = 'clone_graph_live_generation'
                   AND CAST(value AS INTEGER) = NEW.generation
             )
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
             INSERT INTO repo_meta(repo_id, key, value)
             SELECT OLD.repo_id, '{key}', '1'
             WHERE (OLD.repo_id != NEW.repo_id OR OLD.generation != NEW.generation)
               AND EXISTS (
                 SELECT 1 FROM repo_meta
                 WHERE repo_id = OLD.repo_id AND key = 'clone_graph_live_generation'
                   AND CAST(value AS INTEGER) = OLD.generation
             )
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;"
    ))?;
    Ok(())
}

/// V095 (#1002): per-TABLE spec versioning, plus a direct pointer from a row to its winning entry.
///
/// `sync_published_rows.spec_version` replaces V093's `projector_version`. The anti-echo hash
///   covers one TABLE's synced column set, so recording the store-global projector version was too
///   coarse: registering an unrelated table (or learning a new op-kind) bumps that version and
/// would   mark EVERY table's rows incomparable, freezing their producers and forcing needless
/// winner   lookups. The projector version keeps its own job — deciding when parked entries are
/// replayed. The row's winning entry is NOT denormalized onto the clock: `sync_row_clocks` already
/// carries `(lamport, device_fingerprint)`, and `table_sync_entries` is UNIQUE on
/// `(stream_id, device_fingerprint, lamport)`, so the producer resolves it exactly once it knows
/// the stream — which it derives from the account it is already syncing. The only cost is
/// reconciling the two encodings of a fingerprint (hex TEXT on the clock, BLOB on the entry), which
/// the fingerprint type already round-trips.
///
/// The table is necessarily EMPTY: no table is registered (`SYNCABLE_TABLES` is empty) and there
/// is no transport, so nothing has ever written either. The published-rows table is therefore
/// rebuilt into its final shape rather than accumulating a dead column, and no backfill is owed.
pub fn apply_table_sync_spec_version(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    // The rebuild PRESERVES rows rather than dropping them, even though the table is provably empty
    // at this transition today. Emptiness is an emergent property of another crate
    // (`SYNCABLE_TABLES` has no entries yet) and the one-shot-ness depends on V093 declining to
    // re-add the column it replaced — a conjunction spanning two files that nothing enforces. A
    // bare DROP would make this migration's data safety rest on that conjunction holding
    // forever, and the failure would be silent and severe: `produce_row_ops` finds a
    // locally-deleted row by its surviving published record, so a wiped table strands every
    // unsent deletion and leaves peers holding the row. Copying instead demotes the guard below
    // to a performance detail, and costs nothing on an empty table.
    //
    // `spec_version` backfills to 0, which is below every real spec version (the registry lint
    // bounds those at >= 1), so a carried row reads as "column set unknown" — the not-comparable
    // path that resolves itself on the next produce. That is strictly better than deleting it.
    if !column_exists(&tx, "sync_published_rows", "spec_version")? {
        tx.execute_batch(
            "ALTER TABLE sync_published_rows RENAME TO sync_published_rows_pre_v095;
             CREATE TABLE sync_published_rows(
                 repo_id      TEXT    NOT NULL,
                 table_name   TEXT    NOT NULL,
                 row_pk       TEXT    NOT NULL,
                 synced_hash  TEXT    NOT NULL,
                 spec_version INTEGER NOT NULL,
                 PRIMARY KEY(repo_id, table_name, row_pk)
             ) STRICT;
             INSERT INTO sync_published_rows(repo_id, table_name, row_pk, synced_hash, \
             spec_version)
                 SELECT repo_id, table_name, row_pk, synced_hash, 0
                   FROM sync_published_rows_pre_v095;
             DROP TABLE sync_published_rows_pre_v095;",
        )?;
    }
    tx.commit()
}

/// V096 (#1058): retention for a table-sync entry whose chain predecessor has not arrived.
///
/// A verified entry that links to a predecessor this device does not hold used to be DROPPED, so a
/// chain delivered out of causal order — the normal condition on a transport — could only converge
/// through redelivery in exact order. It is now retained here until the predecessor is accepted,
/// then promoted through the ordinary accept-and-apply path.
///
/// This is deliberately its OWN table rather than a status column on `table_sync_entries`, because
/// six queries read that table as "the accepted chain" and every one of them must keep excluding an
/// entry that is not on a chain: the authoring Lamport clock and the lamport-advance bound (a
/// retained entry must not drag either), the chain tail (the promote target), entry existence (how
/// fork is told from gap), the LWW winner lookup, and the refold's pending set. A status column
/// would put a filter on each, and the one that got missed would fail silently.
///
/// `prev_hash` is NOT NULL: a genesis has no predecessor and so can never gap.
///
/// There is deliberately no `UNIQUE(stream_id, device_fingerprint, lamport)`. Two equivocating
/// entries at one lamport must both be storable, or the first one retained blocks the legitimate
/// one; identity here is the entry hash, and the conflict is resolved when the predecessor arrives
/// and one of them takes the successor slot. Accepted entries keep their own uniqueness — a losing
/// sibling never reaches `table_sync_entries`.
pub fn apply_table_sync_gapped_entries(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_gapped_entries(
             entry_hash         BLOB    NOT NULL PRIMARY KEY,
             stream_id          BLOB    NOT NULL,
             device_fingerprint BLOB    NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB    NOT NULL,
             signed_bytes       BLOB    NOT NULL,
             gapped_at_ms       INTEGER NOT NULL
         ) STRICT;
         -- Both walks of the promote loop: the child of the entry just accepted, and the siblings
         -- of its predecessor (which the acceptance just proved to be forks). The trailing
         -- (lamport, entry_hash) is the total order those walks take siblings in, so the index
         -- answers the ordering too instead of the query sorting a temp b-tree per probe.
         CREATE INDEX IF NOT EXISTS table_sync_gapped_entries_child
             ON table_sync_gapped_entries(
                 stream_id, device_fingerprint, prev_hash, lamport, entry_hash);
         -- The per-chain cap's count and its eviction victim, from an index tail instead of a \
         scan.
         CREATE INDEX IF NOT EXISTS table_sync_gapped_entries_chain_lamport
             ON table_sync_gapped_entries(stream_id, device_fingerprint, lamport);
         -- Children of a hash ACROSS devices: the abandoned-subtree walk and the cross-chain
         -- citation sweep, both of which run per accepted entry. The two indexes above lead with
         -- device_fingerprint and so answer neither — SQLite would fall back to the stream_id
         -- prefix and scan every held row on the stream, once per acceptance, which is quadratic
         -- over a reverse-delivered chain and is NOT bounded by the per-chain cap when several
         -- devices share the stream.
         --
         -- It carries device_fingerprint and entry_hash so it COVERS both probes. Without them the
         -- planner prefers the wider child index — which it can only use for its stream_id prefix,
         -- i.e. the scan this index exists to avoid — because that one is covering and this one
         -- would not be. Verified with EXPLAIN QUERY PLAN, not assumed.
         CREATE INDEX IF NOT EXISTS table_sync_gapped_entries_predecessor
             ON table_sync_gapped_entries(
                 stream_id, prev_hash, device_fingerprint, entry_hash);",
    )?;
    tx.commit()
}

/// V099 (#1049): account-authorized repository incarnations and incarnation-safe table streams.
///
/// The table-sync engine is still unreachable in production (`SYNCABLE_TABLES` is empty and no
/// transport exists), so legacy `/4` projection state has no peer-visible authority and cannot be
/// assigned an incarnation honestly. The migration retains only chain-tip witnesses, then clears
/// that unreachable state and rebuilds row bookkeeping with `stream_id` in every key.
pub fn apply_table_sync_repo_incarnations(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_repo_incarnation_current(
             account_id       BLOB NOT NULL CHECK(length(account_id) = 32),
             repository_id    TEXT NOT NULL,
             incarnation_ref  BLOB CHECK(incarnation_ref IS NULL OR length(incarnation_ref) = 32),
             PRIMARY KEY(account_id, repository_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS table_sync_chain_tips(
             stream_id          BLOB    NOT NULL CHECK(length(stream_id) = 32),
             device_fingerprint BLOB    NOT NULL CHECK(length(device_fingerprint) = 32),
             lamport            INTEGER NOT NULL,
             entry_hash         BLOB    NOT NULL CHECK(length(entry_hash) = 32),
             PRIMARY KEY(stream_id, device_fingerprint)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS table_sync_chain_tips_stream_lamport
             ON table_sync_chain_tips(stream_id, lamport);",
    )?;

    // Rebuild accepted-chain high-water independently of the directory shape. A sanctioned full
    // schema replay may be repairing a dropped/incomplete witness table on an already-V099 store,
    // where `table_sync_streams.incarnation_ref` is present and the shape conversion below skips.
    conn.execute_batch(
        "INSERT INTO table_sync_chain_tips(stream_id, device_fingerprint, lamport, entry_hash)
         SELECT e.stream_id, e.device_fingerprint, e.lamport, e.entry_hash
           FROM table_sync_entries e
          WHERE NOT EXISTS (
                SELECT 1 FROM table_sync_entries newer
                 WHERE newer.stream_id = e.stream_id
                   AND newer.device_fingerprint = e.device_fingerprint
                   AND newer.lamport > e.lamport
          )
         ON CONFLICT(stream_id, device_fingerprint) DO UPDATE SET
             lamport = excluded.lamport, entry_hash = excluded.entry_hash
         WHERE excluded.lamport > table_sync_chain_tips.lamport;",
    )?;

    if !column_exists(conn, "table_sync_streams", "incarnation_ref")? {
        conn.execute_batch(
            "DELETE FROM table_sync_gapped_entries;
             DELETE FROM table_sync_entries;
             DROP TABLE table_sync_streams;
             CREATE TABLE table_sync_streams(
                 stream_id       BLOB NOT NULL PRIMARY KEY CHECK(length(stream_id) = 32),
                 repo_id         TEXT NOT NULL,
                 account_id      BLOB NOT NULL CHECK(length(account_id) = 32),
                 incarnation_ref BLOB NOT NULL CHECK(length(incarnation_ref) = 32),
                 scope_id        TEXT NOT NULL,
                 UNIQUE(repo_id, account_id, incarnation_ref, scope_id)
             ) STRICT;",
        )?;
    }
    if !column_exists(conn, "sync_published_rows", "stream_id")? {
        conn.execute_batch(
            "DROP TABLE sync_published_rows;
             CREATE TABLE sync_published_rows(
                 stream_id BLOB NOT NULL CHECK(length(stream_id) = 32), repo_id TEXT NOT NULL,
                 table_name TEXT NOT NULL, row_pk TEXT NOT NULL, synced_hash TEXT NOT NULL,
                 spec_version INTEGER NOT NULL,
                 PRIMARY KEY(stream_id, table_name, row_pk)
             ) STRICT;",
        )?;
    }
    if !column_exists(conn, "sync_row_clocks", "stream_id")? {
        conn.execute_batch(
            "DROP TABLE sync_row_clocks;
             CREATE TABLE sync_row_clocks(
                 stream_id BLOB NOT NULL CHECK(length(stream_id) = 32), repo_id TEXT NOT NULL,
                 table_name TEXT NOT NULL, row_pk TEXT NOT NULL, lamport INTEGER NOT NULL,
                 device_fingerprint TEXT NOT NULL,
                 PRIMARY KEY(stream_id, table_name, row_pk)
             ) STRICT;",
        )?;
    }
    if !column_exists(conn, "sync_row_tombstones", "stream_id")? {
        conn.execute_batch(
            "DROP TABLE sync_row_tombstones;
             CREATE TABLE sync_row_tombstones(
                 stream_id BLOB NOT NULL CHECK(length(stream_id) = 32), repo_id TEXT NOT NULL,
                 table_name TEXT NOT NULL, row_pk TEXT NOT NULL, lamport INTEGER NOT NULL,
                 device_fingerprint TEXT NOT NULL,
                 PRIMARY KEY(stream_id, table_name, row_pk)
             ) STRICT;",
        )?;
    }
    Ok(())
}

/// V128: local observations, not replicated merge state. A row can be unpublishable without any
/// incoming pending entry. The repo key lets repository purge reclaim observations as well.
pub fn apply_table_sync_row_diagnostics(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_row_diagnostics(
             stream_id BLOB NOT NULL CHECK(length(stream_id) = 32),
             repo_id TEXT NOT NULL,
             table_name TEXT NOT NULL,
             row_pk TEXT NOT NULL,
             cause TEXT NOT NULL,
             self_apply_failed INTEGER NOT NULL DEFAULT 0 CHECK(self_apply_failed IN (0,1)),
             PRIMARY KEY(stream_id, repo_id, table_name, row_pk)
         ) STRICT;",
    )
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn row_diagnostics_upgrade_is_additive_retry_safe_and_repo_scoped() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        conn.execute_batch(
            "DROP TABLE table_sync_row_diagnostics; DELETE FROM schema_version WHERE id = \
             '128_table_sync_row_diagnostics';",
        )
        .unwrap();
        crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        for repo in ["main-checkout", "linked-sibling"] {
            conn.execute(
                "INSERT INTO \
                 table_sync_row_diagnostics(stream_id,repo_id,table_name,row_pk,cause) VALUES \
                 (?1, ?2, 't_demo', 'r1', 'future_cause')",
                rusqlite::params![[1_u8; 32].as_slice(), repo],
            )
            .unwrap();
        }
        apply_table_sync_row_diagnostics(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM table_sync_row_diagnostics", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        crate::schema::purge_repo_rows(&conn, "main-checkout").unwrap();
        assert_eq!(
            conn.query_row("SELECT repo_id FROM table_sync_row_diagnostics", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "linked-sibling"
        );
    }
}

/// V129: routing obligations, never accepted chain state or a Lamport clock. Older floors have
/// no recoverable promised tip; this table records only new observations, without inventing one.
pub fn apply_table_sync_suffix_coverage(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_suffix_coverage(
            stream_id BLOB NOT NULL CHECK(length(stream_id) = 32),
            device_fingerprint BLOB NOT NULL CHECK(length(device_fingerprint) = 32),
            floor_lamport INTEGER NOT NULL,
            tip_lamport INTEGER NOT NULL CHECK(tip_lamport >= floor_lamport),
            tip_hash BLOB NOT NULL CHECK(length(tip_hash) = 32),
            PRIMARY KEY(stream_id, device_fingerprint)
        ) STRICT;",
    )
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    #[test]
    fn suffix_coverage_migration_retries_and_preserves_obligations_across_repository_purge() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        conn.execute_batch(
            "DROP TABLE table_sync_suffix_coverage; DELETE FROM schema_version WHERE id = \
             '129_table_sync_suffix_coverage';",
        )
        .unwrap();
        crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        for (stream, repo) in [(1_u8, "main-checkout"), (2, "linked-sibling")] {
            conn.execute(
                "INSERT INTO \
                 table_sync_streams(stream_id,repo_id,account_id,scope_id,incarnation_ref) VALUES \
                 (?1,?2,?3,'overlay/1',?3)",
                params![[stream; 32].as_slice(), repo, [3_u8; 32].as_slice()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO table_sync_suffix_coverage VALUES (?1,?2,10,20,?3)",
                params![[stream; 32].as_slice(), [4_u8; 32].as_slice(), [5_u8; 32].as_slice()],
            )
            .unwrap();
        }
        apply_table_sync_suffix_coverage(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM table_sync_suffix_coverage", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        crate::schema::purge_repo_rows(&conn, "main-checkout").unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM table_sync_suffix_coverage", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT repo_id FROM table_sync_streams", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "linked-sibling"
        );
    }
}
