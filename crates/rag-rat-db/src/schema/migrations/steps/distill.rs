use rusqlite::Connection;

use crate::schema::migrations::{add_column_if_missing, sqlite_object_exists};

/// V073 (issue #702): the provider-attested closing-edge substrate for issue distillation.
/// `papertrail_closing_edges` is a FIRST-CLASS issue↔closer edge table, deliberately NOT more
/// `papertrail_refs` rows: refs are an annotation layer whose unique index coalesces on
/// `source_text`, while a closing edge's identity is the (issue, closer) PAIR and its trust
/// semantics differ by `source` — `provider` rows are attested by the tracker's own data
/// (GraphQL closing references, closed-event closers), `text` rows are mined from
/// commit/item/comment text and remain the degradation tier.
/// INVARIANT: `source` is an attribute, not part of the natural key — the same (issue, closer)
/// pair discovered by both tiers converges to ONE row, and a `provider` row is never downgraded
/// back to `text` (the store upsert enforces the precedence).
/// The new `papertrail_items` columns are all fillable from payloads the mirror already parses
/// (zero extra API calls):
///  - `closed_at` — the temporal axis for supersession ordering (`created_at` is NOT it);
///  - `resolution` — the provider-NEUTRAL outcome enum (`completed | not_planned | duplicate |
///    superseded | unknown`); GitHub `state_reason` maps in, Jira's resolution field maps in
///    richer, GitLab is mostly `unknown`;
///  - `merge_commit_sha` — INVARIANT: stored ONLY for merged change requests. GitHub returns a
///    non-null `merge_commit_sha` for closed-UNMERGED PRs too (its ephemeral test-merge commit,
///    possibly on no branch); the store write path must gate on `merged_at`/merged state, never
///    trust the field's presence;
///  - `state_normalized` — `open | closed | merged`. GitLab merged MRs carry `state='merged'`, so a
///    plain `WHERE state='closed'` silently drops every merged GitLab MR; consumers filter on THIS
///    column, never on raw `state`. Backfilled below from the provider-truthful pair (`state`,
///    `merged_at`); new rows are stamped at store time.
///  - `author_kind` / `author_association` (items AND comments) — thread-shape facets (`user.type:
///    "Bot"`, `OWNER`/`MEMBER`/…), already in the parsed payloads.
///
/// Additive only: `CREATE TABLE IF NOT EXISTS` + `add_column_if_missing` + an idempotent
/// backfill UPDATE, so a torn replay reconverges without a wrapping transaction.
pub fn apply_papertrail_distill_substrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS papertrail_closing_edges(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            issue_kind TEXT NOT NULL,
            issue_key TEXT NOT NULL,
            -- 'change_request' | 'commit' (ClosingEdgeCloserKind::as_db_str)
            closer_kind TEXT NOT NULL,
            -- change_request: the closer item's key in the same project; commit: the full sha.
            closer_key TEXT NOT NULL,
            -- The closing/merge commit sha when known (a change_request closer's merge commit).
            closer_commit TEXT,
            -- 'provider' | 'text' (ClosingEdgeSource::as_db_str). Attribute, NOT key: the store
            -- upsert converges both tiers onto one row and never downgrades provider -> text.
            source TEXT NOT NULL,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_closing_edges_natural_key
            ON papertrail_closing_edges(repo_id, tracker, project, issue_kind, issue_key, \
         closer_kind, closer_key);
        CREATE INDEX IF NOT EXISTS idx_papertrail_closing_edges_closer
            ON papertrail_closing_edges(repo_id, tracker, project, closer_kind, closer_key);
        ",
    )?;
    add_column_if_missing(conn, "papertrail_items", "closed_at", "TEXT")?;
    add_column_if_missing(conn, "papertrail_items", "resolution", "TEXT")?;
    // INVARIANT: non-null ONLY for merged change requests — see the migration doc above.
    add_column_if_missing(conn, "papertrail_items", "merge_commit_sha", "TEXT")?;
    add_column_if_missing(
        conn,
        "papertrail_items",
        "state_normalized",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(conn, "papertrail_items", "author_kind", "TEXT")?;
    add_column_if_missing(conn, "papertrail_items", "author_association", "TEXT")?;
    add_column_if_missing(conn, "papertrail_comments", "author_kind", "TEXT")?;
    add_column_if_missing(conn, "papertrail_comments", "author_association", "TEXT")?;
    // Backfill the normalized state for pre-existing rows from the provider-truthful pair:
    // 'merged' state (GitLab MRs) or a recorded merged_at (GitHub PRs) wins over raw 'closed';
    // anything else non-closed is 'open'. Idempotent: the predicate re-derives the same value.
    conn.execute_batch(
        "
        UPDATE papertrail_items SET state_normalized = CASE
            WHEN state = 'merged' OR merged_at IS NOT NULL THEN 'merged'
            WHEN state = 'closed' THEN 'closed'
            ELSE 'open'
        END
        WHERE state_normalized = '';
        ",
    )
}

/// V077 — the distillation RECORD STORE (issue #703): the derived, regenerable
/// `papertrail_distill` table plus its junction children, thread-keyed edges, work queue, and
/// run-stats. Consumes the V073 substrate; produced by the #704 LLM pass.
///
/// DESIGN INVARIANTS (locked here because these are the costliest columns to change post-landing):
/// - **Findings-not-facts.** Records are DERIVED and regenerable, never written into trusted
///   memories. Regeneration identity is `(distill_input_hash, pipeline_version)`; the record row is
///   replaced in place on its natural key `(repo_id, tracker, project, item_kind, item_key)`.
/// - **Confidence is provenance FACETS, never a fused label** (a fused high/med/low label does not
///   discriminate accuracy — measured). Any display label is computed in the read layer.
/// - **Edges key to the THREAD, not the record row** (`papertrail_distill_edges`), so LWW body
///   edits / regeneration replace the record while supersession/coalesce/promotion edges survive.
/// - **Mechanical status floors kept raw** (`revert_override`, `closing_keyword_floor`, and the
///   `fix_edge_source='none'` no-fix-edge floor); the EFFECTIVE status is computed read-layer with
///   precedence revert > closing-keyword > no-fix-edge > `outcome_status_model`. Fixing commits are
///   mechanical (`papertrail_distill_record_commits`), NEVER LLM-emitted.
/// - **No CSV-in-TEXT**: alternatives / commits / anchors / evidence are junction tables.
/// - **Anchors born as `sym_<hex>` bindings** (relocation-compatible) with EXACT file paths; no
///   basename fallback. `epistemic_status_*` makes proposed-not-landed / projected representable.
///
/// Additive: CREATE ... IF NOT EXISTS; nothing pre-existing to backfill.
pub fn apply_distill_record_store(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS papertrail_distill(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            -- the coalesced work-unit thread; the ISSUE side when an issue<->PR pair is coalesced.
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            -- regeneration identity: the row is replaced on the natural key when either changes.
            distill_input_hash TEXT NOT NULL,
            pipeline_version INTEGER NOT NULL,
            root_issue TEXT,
            -- NULL is an HONEST null (no failure, or none established) — not missing data.
            root_cause TEXT,
            -- free-text detail in v1; the induced-taxonomy FK is deferred (#705 non-goal).
            root_cause_class TEXT,
            decision_chosen TEXT,
            outcome_summary TEXT,
            -- MODEL-emitted status (OutcomeStatus::as_db_str). The EFFECTIVE status is computed in
            -- the read layer: revert_override > closing_keyword_floor > no-fix-edge > this.
            outcome_status_model TEXT,
            -- event-factuality (EpistemicStatus): \
         asserted_landed|projected|proposed_not_landed|superseded.
            epistemic_status_decision TEXT,
            epistemic_status_outcome TEXT,
            -- provenance FACETS (NOT a fused confidence label).
            fix_edge_source TEXT NOT NULL,               -- FixEdgeSource: provider|text|none
            quotes_materialized INTEGER NOT NULL DEFAULT 0,
            anchors_qualified_count INTEGER NOT NULL DEFAULT 0,
            thread_shape TEXT NOT NULL,                  -- ThreadShape: \
         investigation|review_stream|thin
            outcome_claim_verified INTEGER NOT NULL DEFAULT 0,
            decision_provenance_verified INTEGER NOT NULL DEFAULT 0,
            -- raw status-floor inputs (precedence applied in the read layer, never here).
            revert_override INTEGER NOT NULL DEFAULT 0,
            closing_keyword_floor TEXT,
            distilled_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_natural_key
            ON papertrail_distill(repo_id, tracker, project, item_kind, item_key);

        -- Evidence units: byte-span SELECTIONS with SNAPSHOTTED provenance + a MATERIALIZED quote
        -- (raw spans dangle under the mirror's per-row LWW body edits).
        CREATE TABLE IF NOT EXISTS papertrail_distill_evidence(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            field TEXT NOT NULL,                 -- record field supported: \
         root_cause|decision|outcome
            source_kind TEXT NOT NULL,           -- 'item' | 'comment'
            source_id TEXT NOT NULL,
            byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
            quote TEXT NOT NULL,
            author TEXT, author_kind TEXT, author_association TEXT,
            unit_created_at_ms INTEGER,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_evidence_thread
            ON papertrail_distill_evidence(repo_id, tracker, project, item_kind, item_key);

        -- Anchor candidates: index-validated, born as sym_<hex> bindings; EXACT file paths only.
        -- V078 adds their stable candidate ordinals and model-selection state.
        CREATE TABLE IF NOT EXISTS papertrail_distill_anchors(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            anchor_kind TEXT NOT NULL,           -- AnchorKind: \
         symbol|file|schema_object|crate|config_key
            logical_symbol_id TEXT,              -- sym_<hex> when anchor_kind='symbol' AND \
         resolved
            file_path TEXT,
            name TEXT NOT NULL,
            resolved INTEGER NOT NULL DEFAULT 0,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_thread
            ON papertrail_distill_anchors(repo_id, tracker, project, item_kind, item_key);
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_symbol
            ON papertrail_distill_anchors(repo_id, logical_symbol_id);

        -- Rejected alternatives (junction; ordinal-stable, no CSV).
        CREATE TABLE IF NOT EXISTS papertrail_distill_alternatives(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            alternative TEXT NOT NULL, reason TEXT,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_alternatives_key
            ON papertrail_distill_alternatives(repo_id, tracker, project, item_kind, item_key, \
         ordinal);

        -- Fixing commits: MECHANICAL, from the closing edge; outcome.commits is never LLM-emitted.
        CREATE TABLE IF NOT EXISTS papertrail_distill_record_commits(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_record_commits_key
            ON papertrail_distill_record_commits(repo_id, tracker, project, item_kind, item_key, \
         commit_sha);

        -- Thread-keyed edges: survive record regeneration.
        CREATE TABLE IF NOT EXISTS papertrail_distill_edges(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            src_item_kind TEXT NOT NULL, src_item_key TEXT NOT NULL,
            dst_item_kind TEXT NOT NULL, dst_item_key TEXT NOT NULL,
            edge_kind TEXT NOT NULL,             -- DistillEdgeKind: coalesced|supersedes|promoted
            created_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_edges_key
            ON papertrail_distill_edges(repo_id, tracker, project, src_item_kind, src_item_key,
                                        dst_item_kind, dst_item_key, edge_kind);

        -- Work queue: enqueued at mirror sync (cheap SQL), DRAINED only by the dream-lane pass.
        CREATE TABLE IF NOT EXISTS papertrail_distill_queue(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            enqueued_at_ms INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            raw_reply TEXT,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_queue_key
            ON papertrail_distill_queue(repo_id, tracker, project, item_kind, item_key);

        -- Per-run stats: the #704 verification bar (output-ladder rung + gate counters).
        CREATE TABLE IF NOT EXISTS papertrail_distill_runs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_at_ms INTEGER NOT NULL,
            threads INTEGER NOT NULL DEFAULT 0,
            rung_guided INTEGER NOT NULL DEFAULT 0,
            rung_serde INTEGER NOT NULL DEFAULT 0,
            rung_unguided INTEGER NOT NULL DEFAULT 0,
            rung_tolerant INTEGER NOT NULL DEFAULT 0,
            failed INTEGER NOT NULL DEFAULT 0,
            stats_json TEXT,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        ",
    )
}

/// V078 — separate mechanically mined anchor CANDIDATES from model SELECTED anchors (#704).
/// Existing V077 rows receive deterministic zero-based ordinals in row-id order within each
/// thread. The exact-path and `sym_<hex>` identity columns are deliberately untouched.
pub fn apply_distill_anchor_selection(conn: &Connection) -> rusqlite::Result<()> {
    // Key the backfill guard to its completion artifact, not merely column presence. If a process
    // dies after ADD COLUMN (whose default makes every legacy row ordinal 0) but before the
    // backfill/index, replay must backfill again rather than fail forever on duplicate ordinals.
    let candidate_index_exists =
        sqlite_object_exists(conn, "index", "idx_papertrail_distill_anchors_candidate")?;
    add_column_if_missing(
        conn,
        "papertrail_distill_anchors",
        "candidate_ordinal",
        "INTEGER NOT NULL DEFAULT 0 CHECK(candidate_ordinal >= 0)",
    )?;
    add_column_if_missing(
        conn,
        "papertrail_distill_anchors",
        "selected",
        "INTEGER NOT NULL DEFAULT 0 CHECK(selected IN (0, 1))",
    )?;
    if !candidate_index_exists {
        conn.execute_batch(
            "
        UPDATE papertrail_distill_anchors AS anchor
        SET candidate_ordinal = (
            SELECT COUNT(*)
            FROM papertrail_distill_anchors AS earlier
            WHERE earlier.repo_id = anchor.repo_id
              AND earlier.tracker = anchor.tracker
              AND earlier.project = anchor.project
              AND earlier.item_kind = anchor.item_kind
              AND earlier.item_key = anchor.item_key
              AND earlier.id < anchor.id
        );
        ",
        )?;
    }
    conn.execute_batch(
        "
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_candidate
            ON papertrail_distill_anchors(
                repo_id, tracker, project, item_kind, item_key, candidate_ordinal
            );
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_selected
            ON papertrail_distill_anchors(
                repo_id, tracker, project, item_kind, item_key, candidate_ordinal
            ) WHERE selected = 1;
        ",
    )
}

/// V079 — extraction-owned, immutable-for-one-input source and unit snapshots (#704).
///
/// There is deliberately no SQL backfill: old derived records must regenerate under the bumped
/// extraction pipeline and snapshot the mirror rows read in that same transaction. Backfilling
/// here would falsely attach today's mutable mirror text to an older `distill_input_hash`.
pub fn apply_distill_safe_input_snapshot(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "papertrail_distill", "prompt_version", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_distill", "model_input_hash", "TEXT")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS papertrail_distill_sources(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            source_ordinal INTEGER NOT NULL CHECK(source_ordinal >= 0),
            role TEXT NOT NULL CHECK(role IN ('primary', 'partner')),
            partner_ordinal INTEGER CHECK(partner_ordinal >= 0),
            source_item_kind TEXT NOT NULL,
            source_item_key TEXT NOT NULL,
            source_kind TEXT NOT NULL CHECK(source_kind IN ('item', 'comment')),
            source_part TEXT NOT NULL CHECK(source_part IN ('title', 'body', 'comment')),
            source_id TEXT NOT NULL,
            exact_text TEXT NOT NULL,
            author TEXT,
            author_kind TEXT,
            author_association TEXT,
            created_at_ms INTEGER,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            CHECK((role = 'primary' AND partner_ordinal IS NULL) OR
                  (role = 'partner' AND partner_ordinal IS NOT NULL)),
            CHECK((source_kind = 'item' AND source_part IN ('title', 'body')) OR
                  (source_kind = 'comment' AND source_part = 'comment'))
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_sources_ordinal
            ON papertrail_distill_sources(
                repo_id, tracker, project, item_kind, item_key, source_ordinal
            );
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_sources_identity
            ON papertrail_distill_sources(
                repo_id, tracker, project, source_item_kind, source_item_key, source_kind, \
         source_id
            );

        CREATE TABLE IF NOT EXISTS papertrail_distill_units(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            unit_ordinal INTEGER NOT NULL CHECK(unit_ordinal >= 0),
            source_ordinal INTEGER NOT NULL CHECK(source_ordinal >= 0),
            byte_start INTEGER NOT NULL CHECK(byte_start >= 0),
            byte_end INTEGER NOT NULL CHECK(byte_end > byte_start),
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_units_ordinal
            ON papertrail_distill_units(
                repo_id, tracker, project, item_kind, item_key, unit_ordinal
            );
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_units_source
            ON papertrail_distill_units(
                repo_id, tracker, project, item_kind, item_key, source_ordinal, unit_ordinal
            );
        ",
    )
}

/// V080 — extraction-owned enriched-context snapshots (#800): the fix diff (restricted to files
/// with symbol anchor candidates) and the thread's cross-referenced item titles + opening
/// paragraphs, snapshotted so the drain never reads mutable git/mirror state.
///
/// There is deliberately no SQL backfill, same doctrine as V079: old derived records must
/// regenerate under the bumped extraction pipeline and snapshot the git/mirror state read in that
/// same transaction.
pub fn apply_distill_enriched_context(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS papertrail_distill_fix_diffs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            path TEXT NOT NULL,
            patch TEXT NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_fix_diffs_file
            ON papertrail_distill_fix_diffs(
                repo_id, tracker, project, item_kind, item_key, commit_sha, path
            );

        CREATE TABLE IF NOT EXISTS papertrail_distill_xrefs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            xref_ordinal INTEGER NOT NULL CHECK(xref_ordinal >= 0),
            target_tracker TEXT NOT NULL,
            target_project TEXT NOT NULL,
            target_item_kind TEXT,
            target_item_key TEXT NOT NULL,
            ref_kind TEXT NOT NULL,
            title TEXT NOT NULL,
            opening TEXT NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_xrefs_ordinal
            ON papertrail_distill_xrefs(
                repo_id, tracker, project, item_kind, item_key, xref_ordinal
            );
        ",
    )
}

/// V081 — persist source-part identity on distilled evidence rows (#801). V077's
/// `papertrail_distill_evidence` stores `source_kind`/`source_id` but not WHICH part of an item a
/// citation came from: an item's title and body share the same `source_id` (the item key), so two
/// citations with identical or overlapping spans were indistinguishable in the persisted record —
/// even though the V079 snapshot substrate keeps full identity at drain time. Add `source_part`
/// (title|body|comment, matching the V079 CHECK), populated by the drain from its `SourceSnapshot`.
///
/// Nullable: existing rows predate the column and keep NULL (which passes the value CHECK). No SQL
/// backfill — evidence is derived and rewritten wholesale on every drain, so a re-drain repopulates
/// it (the derived-data doctrine of V079/V080). The source ITEM identity is deliberately NOT added:
/// distilled evidence is primary-only (the drain rejects partner units), so the source item is
/// always the record's own `(item_kind, item_key)` already stored on the row. Guarded by
/// `add_column_if_missing` so a torn replay (column added, migration row not yet recorded) is a
/// no-op rather than a duplicate-column error.
pub fn apply_distill_evidence_source_part(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(
        conn,
        "papertrail_distill_evidence",
        "source_part",
        "TEXT CHECK(source_part IN ('title', 'body', 'comment'))",
    )
}
