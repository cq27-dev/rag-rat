use rusqlite::Connection;

use crate::schema::migrations::{add_column_if_missing, column_exists, sqlite_object_exists};

// ============================================================================================
// Provider-neutral papertrail schema (#588) — V060.
//
// The seven GitHub-shaped cache tables (github_refs / github_issues / github_comments /
// github_pull_requests / github_reviews / github_review_comments / github_ref_sync) and the
// github_fts mirror normalize into the provider-neutral papertrail_* tables: items carry a
// `tracker` token (closed set, `papertrail::Tracker`) and an `item_kind` that is PART OF THE
// IDENTITY (`issue` | `change_request`), comments unify the three GitHub comment shapes behind
// nullable `review_state` / `anchor_path` markers, refs become a pure annotation layer, and the
// per-ref sync state machine is DELETED in favor of the per-(repo, tracker, project)
// `papertrail_sync_cursor` the mirror sync will drive. HARD RENAME — no legacy aliases, no
// compatibility views; the github_* tables are dropped after the backfill.
// ============================================================================================

/// Create the provider-neutral papertrail tables + indexes (idempotent `IF NOT EXISTS` DDL).
/// SHARED between `apply_baseline` (a fresh DB gets the current schema directly — no legacy
/// github_* tables are ever created) and [`apply_papertrail_provider_neutral_schema`] (so the
/// migration is self-contained when driven against an isolation fixture). V060 is these tables'
/// birth migration, so sharing the DDL cannot clobber an older shape; a future shape change must
/// land as its own migration, not an edit here.
pub(crate) fn create_papertrail_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        -- Every table carries repo_id from birth and folds it into its natural key (the V044/V045
        -- discipline). item_kind is part of an item's identity: GitHub's shared issue/PR
        -- numbering is the exception, not the rule (GitLab namespaces them separately).
        CREATE TABLE IF NOT EXISTS papertrail_items(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            url TEXT NOT NULL,
            state TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            merged_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_items_natural_key
            ON papertrail_items(repo_id, tracker, project, item_kind, item_key);

        -- One unified comment shape: a review event carries review_state, a file-anchored review
        -- comment carries anchor_path, a plain thread comment carries neither. The parent item is
        -- named by (item_kind, item_key); comment_id is source-qualified by providers whose
        -- thread-comment / review / review-comment resources have overlapping id spaces.
        CREATE TABLE IF NOT EXISTS papertrail_comments(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            comment_id TEXT NOT NULL,
            url TEXT,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            review_state TEXT,
            anchor_path TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_comments_natural_key
            ON papertrail_comments(repo_id, tracker, project, comment_id);
        CREATE INDEX IF NOT EXISTS idx_papertrail_comments_item
            ON papertrail_comments(repo_id, tracker, project, item_kind, item_key);
        CREATE INDEX IF NOT EXISTS idx_papertrail_comments_anchor_path
            ON papertrail_comments(anchor_path);

        -- Discovered path/commit/branch -> item links. Annotation layer ONLY (evidence ranking);
        -- refs no longer gate sync. No item_kind: a discovered `#N` cannot know the kind.
        CREATE TABLE IF NOT EXISTS papertrail_refs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_key TEXT NOT NULL,
            item_kind TEXT,
            ref_kind TEXT NOT NULL DEFAULT 'unknown',
            source_kind TEXT NOT NULL,
            source_path TEXT,
            source_commit TEXT,
            source_text TEXT NOT NULL,
            discovered_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_papertrail_refs_path ON papertrail_refs(source_path);
        CREATE INDEX IF NOT EXISTS idx_papertrail_refs_item
            ON papertrail_refs(repo_id, tracker, project, item_kind, item_key);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_refs_unique
            ON papertrail_refs(repo_id, tracker, project, COALESCE(item_kind, ''), item_key, \
         source_kind,
                               COALESCE(source_path, ''), COALESCE(source_commit, ''), \
         source_text);

        -- The mirror-sync resume cursor: ONE row per (repo, tracker, project) — REPLACES the
        -- per-ref github_ref_sync synced/not_found/failed state machine, which is deleted (not
        -- migrated). high_mark_at is the delta lane's newest-seen provider timestamp; low_mark_at
        -- is the LIFO backfill descent position; backfill_done flips once the descent reaches the
        -- oldest item; filter_fingerprint invalidates the cursor when the binding's tag filter
        -- changes. Created empty by this schema generation; the mirror sync that drives it lands
        -- separately.
        CREATE TABLE IF NOT EXISTS papertrail_sync_cursor(
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            high_mark_at TEXT,
            low_mark_at TEXT,
            probe_etag TEXT,
            backfill_done INTEGER NOT NULL DEFAULT 0,
            filter_fingerprint TEXT,
            last_probe_ms INTEGER,
            last_full_sync_ms INTEGER,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            PRIMARY KEY(repo_id, tracker, project)
        ) STRICT;

        -- Item -> tag junction (provider labels): client-side tag filtering, config-change
        -- pruning, and label surfacing in results. Populated by the mirror sync (the legacy cache
        -- never stored labels, so there is nothing to backfill).
        CREATE TABLE IF NOT EXISTS papertrail_item_tags(
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            tag TEXT NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, tag)
        ) STRICT;

        -- Standalone (own-content) FTS mirror of papertrail_items + papertrail_comments,
        -- maintained INCREMENTALLY by the store writers (each item/comment refreshes only its own
        -- row); the whole-table rebuild is reserved for the full re-walk / recovery paths.
        -- doc_kind is 'item' or 'comment'; comment_id is '' on item rows. repo_id is the
        -- V041-style UNINDEXED scope column every MATCH must filter on.
        CREATE VIRTUAL TABLE IF NOT EXISTS papertrail_fts USING fts5(
            tracker UNINDEXED,
            project,
            item_kind UNINDEXED,
            item_key UNINDEXED,
            doc_kind UNINDEXED,
            comment_id UNINDEXED,
            url UNINDEXED,
            title,
            body,
            classification,
            repo_id UNINDEXED,
            tokenize='porter'
        );
        ",
    )
}

/// V060 (#588): normalize the GitHub-shaped papertrail cache into the provider-neutral
/// papertrail_* tables and hard-rename the memory binding kind `github` -> `tracker`. Four steps,
/// all inside ONE self-wrapped `BEGIN IMMEDIATE` (the ladder runs apply fns in AUTOCOMMIT), each
/// individually conditional/idempotent so a replay converges:
///
///  1. [`create_papertrail_tables`] — `IF NOT EXISTS` no-ops on any modern DB (the baseline already
///     ran it); real work only for an isolation fixture driving this fn directly.
///  2. FIRST-APPLY-ONLY BACKFILL, gated on the legacy `github_issues` table existing: mechanical
///     copy — `tracker = 'github'`, `project = owner || '/' || repo`, `item_key = CAST(number AS
///     TEXT)`, `item_kind` from `is_pull_request` — with the GitHub issue-shadow DEDUPED (a change
///     request becomes ONE row: the pulls copy wins, a shadow-only row falls back via `INSERT OR
///     IGNORE` on the natural key); reviews / review comments fold into `papertrail_comments`
///     behind `review_state` / `anchor_path`; refs copy verbatim. `repo_id` copies VERBATIM
///     (placeholder rows stay placeholder for `register_repo` to adopt; the V044/V045 per-repo-copy
///     semantics carry over unchanged). The `papertrail_fts` mirror is re-derived from the
///     freshly-backfilled base tables (the V045 in-migration posture) via rag-rat-papertrail's
///     standing `rebuild_fts` (reached through the
///     [`rebuild_papertrail_fts`](crate::hooks::MigrationHooks::rebuild_papertrail_fts) hook),
///     which recomputes `classification` with the current classifier. The seven github_* tables +
///     `github_fts` are then DROPPED — the gate can never fire again, so the backfill is
///     structurally first-apply-only.
///  3. Memory bindings: gated on the legacy `github_owner` column existing — `binding_kind =
///     'github'` rows become `binding_kind = 'tracker'` with `binding_id = 'github:' || owner ||
///     '/' || repo || '#' || number` and the new `tracker` / `project` / `item_key` columns
///     populated; the three github_* columns are then dropped. The `github` binding kind ceases to
///     exist.
///  4. The `github_last_sync_ms` repo_meta key renames to `papertrail_last_sync_ms`.
pub fn apply_papertrail_provider_neutral_schema(
    conn: &Connection,
    hooks: &crate::hooks::MigrationHooks,
) -> rusqlite::Result<()> {
    // Fast path: nothing legacy left anywhere — the common post-V060 replay
    // (`create_or_migrate` / `rebuild` / `index --full`) short-circuits before the write lock.
    if sqlite_object_exists(conn, "table", "papertrail_items")?
        && !sqlite_object_exists(conn, "table", "github_issues")?
        && !column_exists(conn, "repo_memory_bindings", "github_owner")?
    {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        create_papertrail_tables(conn)?;
        if sqlite_object_exists(conn, "table", "github_issues")? {
            backfill_papertrail_from_github_tables(conn)?;
            // Re-derive the mirror from the freshly-backfilled base tables so the migrated cache
            // is scoped-searchable immediately (no sync required) — the V045 posture.
            (hooks.rebuild_papertrail_fts)(conn)?;
            conn.execute_batch(
                "
                DROP TABLE IF EXISTS github_refs;
                DROP TABLE IF EXISTS github_issues;
                DROP TABLE IF EXISTS github_comments;
                DROP TABLE IF EXISTS github_pull_requests;
                DROP TABLE IF EXISTS github_reviews;
                DROP TABLE IF EXISTS github_review_comments;
                DROP TABLE IF EXISTS github_ref_sync;
                DROP TABLE IF EXISTS github_fts;
                ",
            )?;
        }
        migrate_memory_bindings_to_tracker_kind(conn)?;
        if sqlite_object_exists(conn, "table", "repo_meta")? {
            conn.execute_batch(
                "
                INSERT OR REPLACE INTO repo_meta(repo_id, key, value)
                    SELECT repo_id, 'papertrail_last_sync_ms', value
                    FROM repo_meta WHERE key = 'github_last_sync_ms';
                DELETE FROM repo_meta WHERE key = 'github_last_sync_ms';
                ",
            )?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

pub(crate) fn apply_papertrail_ref_item_kind(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "papertrail_refs", "item_kind", "TEXT")?;
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_papertrail_refs_item;
         DROP INDEX IF EXISTS idx_papertrail_refs_unique;
         CREATE INDEX idx_papertrail_refs_item
             ON papertrail_refs(repo_id, tracker, project, item_kind, item_key);
         CREATE UNIQUE INDEX idx_papertrail_refs_unique
             ON papertrail_refs(repo_id, tracker, project, COALESCE(item_kind, ''), item_key,
                                source_kind, COALESCE(source_path, ''),
                                COALESCE(source_commit, ''), source_text);",
    )
}

/// V062 (#591): repo-wide comments have an independent timestamp lane and a page token that is
/// committed only after its page is stored. Existing cursors start with no comment watermark, so
/// the first native pass safely replays the comment streams instead of inheriting the item mark.
pub(crate) fn apply_papertrail_comment_cursor(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "papertrail_sync_cursor", "comment_high_mark_at", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "comment_page_token", "TEXT")?;
    Ok(())
}

/// V063 (#591): every multi-request mirror lane persists enough state to resume after a governed
/// pause. The processed-key sets are bounded by one provider page; `full_rewalk_seen` lives on the
/// item row so a full walk can mark/sweep across arbitrarily many invocations without a giant
/// in-memory set.
pub fn apply_papertrail_mirror_resume_state(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "papertrail_sync_cursor", "comment_scan_since", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "comment_stream_cursors", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "item_delta_page_token", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "item_delta_scan_since", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "item_delta_high_mark_at", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "backfill_page_cursor", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "item_thread_cursor", "TEXT")?;
    add_column_if_missing(
        conn,
        "papertrail_sync_cursor",
        "item_delta_in_progress",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "papertrail_sync_cursor",
        "item_delta_replay_required",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "delta_processed_keys", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "backfill_processed_keys", "TEXT")?;
    add_column_if_missing(
        conn,
        "papertrail_sync_cursor",
        "full_rewalk",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "papertrail_items",
        "full_rewalk_seen",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

/// V067 (#592): scheduling and failures are binding-local. Error classes are stable machine
/// values; detail is sanitized and bounded by the recording API rather than used for policy.
pub fn apply_papertrail_binding_health(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "papertrail_sync_cursor", "last_attempt_ms", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "last_successful_probe_ms", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "last_successful_mirror_ms", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "retry_not_before_ms", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "error_class", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "error_detail", "TEXT")?;
    conn.execute(
        "UPDATE papertrail_sync_cursor
         SET last_successful_probe_ms=last_probe_ms
         WHERE last_successful_probe_ms IS NULL AND last_probe_ms IS NOT NULL",
        [],
    )?;
    // Before V066, an ordinary initial backfill was a complete project walk but only forced
    // `--full` runs populated `last_full_sync_ms`. Preserve that completed-walk fact using the
    // cursor's last successful provider contact; incomplete cursors must remain due for healing.
    conn.execute(
        "UPDATE papertrail_sync_cursor
         SET last_full_sync_ms=last_probe_ms
         WHERE backfill_done=1 AND last_full_sync_ms IS NULL AND last_probe_ms IS NOT NULL",
        [],
    )?;
    conn.execute(
        "UPDATE papertrail_sync_cursor
         SET last_successful_mirror_ms=last_probe_ms
         WHERE backfill_done=1
           AND last_successful_mirror_ms IS NULL
           AND last_probe_ms IS NOT NULL",
        [],
    )?;
    Ok(())
}

/// The V060 base-table backfill (step 2 of [`apply_papertrail_provider_neutral_schema`]): runs
/// INSIDE the caller's transaction, only when the legacy tables exist. Every copy is
/// `INSERT OR IGNORE` on the new natural keys, so the ordering below defines the winner where the
/// legacy cache held two views of one item: the pulls-endpoint copy of a change request (which
/// carries `merged_at`) wins over its issues-endpoint shadow.
fn backfill_papertrail_from_github_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        -- Plain issues.
        INSERT OR IGNORE INTO papertrail_items(
            tracker, project, item_kind, item_key, url, state, title, body, author, created_at,
            updated_at, merged_at, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'issue', CAST(number AS TEXT), html_url, state,
               title, body, author, created_at, updated_at, NULL, synced_at_ms, repo_id
        FROM github_issues WHERE is_pull_request = 0;

        -- Change requests, from the richer pulls rows (merged_at) ...
        INSERT OR IGNORE INTO papertrail_items(
            tracker, project, item_kind, item_key, url, state, title, body, author, created_at,
            updated_at, merged_at, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'change_request', CAST(number AS TEXT), html_url,
               state, title, body, author, created_at, updated_at, merged_at, synced_at_ms, repo_id
        FROM github_pull_requests;

        -- ... and from issue-shadow rows whose pulls row never landed (a partial legacy cache):
        -- the OR IGNORE dedupes against the pulls copy above on the natural key.
        INSERT OR IGNORE INTO papertrail_items(
            tracker, project, item_kind, item_key, url, state, title, body, author, created_at,
            updated_at, merged_at, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'change_request', CAST(number AS TEXT), html_url,
               state, title, body, author, created_at, updated_at, NULL, synced_at_ms, repo_id
        FROM github_issues WHERE is_pull_request = 1;

        -- Thread comments: the parent kind resolves through the cached parent rows (a comment on
        -- a change request keeps its real parent kind); an orphan defaults to 'issue' — the
        -- provisional kind the comment mappers also use for GitHub's shared numbering.
        INSERT OR IGNORE INTO papertrail_comments(
            tracker, project, item_kind, item_key, comment_id, url, body, author, created_at,
            updated_at, review_state, anchor_path, synced_at_ms, repo_id
        )
        SELECT 'github', c.owner || '/' || c.repo,
               CASE WHEN EXISTS (
                        SELECT 1 FROM github_issues i
                        WHERE i.repo_id = c.repo_id AND i.owner = c.owner AND i.repo = c.repo
                          AND i.number = c.number AND i.is_pull_request = 1
                    ) OR EXISTS (
                        SELECT 1 FROM github_pull_requests p
                        WHERE p.repo_id = c.repo_id AND p.owner = c.owner AND p.repo = c.repo
                          AND p.number = c.number
                    ) THEN 'change_request' ELSE 'issue' END,
               CAST(c.number AS TEXT), 'comment:' || CAST(c.id AS TEXT), c.html_url, c.body, \
         c.author,
               c.created_at, c.updated_at, NULL, NULL, c.synced_at_ms, c.repo_id
        FROM github_comments c;

        -- Review events: review_state marks them; reviews only exist on change requests.
        INSERT OR IGNORE INTO papertrail_comments(
            tracker, project, item_kind, item_key, comment_id, url, body, author, created_at,
            updated_at, review_state, anchor_path, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'change_request', CAST(number AS TEXT),
               'review:' || CAST(id AS TEXT), html_url, body, author, submitted_at, submitted_at,
               state, NULL,
               synced_at_ms, repo_id
        FROM github_reviews;

        -- File-anchored review comments: anchor_path marks them.
        INSERT OR IGNORE INTO papertrail_comments(
            tracker, project, item_kind, item_key, comment_id, url, body, author, created_at,
            updated_at, review_state, anchor_path, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'change_request', CAST(number AS TEXT),
               'review_comment:' || CAST(id AS TEXT), html_url, body, author, created_at,
               updated_at, NULL, path,
               synced_at_ms, repo_id
        FROM github_review_comments;

        -- A legacy failed ref sync is an explicit retry marker. The referenced-only lane now uses
        -- the presence of a cached item as its sole completion signal, so carrying a partially
        -- cached item across V060 would turn that failure into a permanent skip. Remove both the
        -- item and any partial children; a successful legacy row keeps its complete cache.
        DELETE FROM papertrail_comments
        WHERE tracker = 'github' AND EXISTS (
            SELECT 1 FROM github_ref_sync s
            WHERE s.status = 'failed'
              AND s.repo_id = papertrail_comments.repo_id
              AND s.owner || '/' || s.repo = papertrail_comments.project
              AND CAST(s.number AS TEXT) = papertrail_comments.item_key
        );
        DELETE FROM papertrail_items
        WHERE tracker = 'github' AND EXISTS (
            SELECT 1 FROM github_ref_sync s
            WHERE s.status = 'failed'
              AND s.repo_id = papertrail_items.repo_id
              AND s.owner || '/' || s.repo = papertrail_items.project
              AND CAST(s.number AS TEXT) = papertrail_items.item_key
        );

        -- Refs copy verbatim (annotation layer). The per-ref github_ref_sync state machine is
        -- DELETED, not migrated — papertrail_sync_cursor starts empty.
        INSERT OR IGNORE INTO papertrail_refs(
            tracker, project, item_key, ref_kind, source_kind, source_path, source_commit,
            source_text, discovered_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, CAST(number AS TEXT), ref_kind, source_kind,
               source_path, source_commit, source_text, discovered_at_ms, repo_id
        FROM github_refs;
        ",
    )
}

/// The V060 memory-binding rename (step 3): `binding_kind = 'github'` -> `'tracker'`, the three
/// legacy columns fold into `tracker` / `project` / `item_key`, and the legacy columns are
/// dropped. Gated on the legacy `github_owner` column so a fresh DB (baseline already creates the
/// new columns) and a replay are clean no-ops. The `binding_id` rewrite keeps the PK 1:1 (every
/// legacy id `owner/repo#N` maps to exactly one `github:owner/repo#N`).
fn migrate_memory_bindings_to_tracker_kind(conn: &Connection) -> rusqlite::Result<()> {
    if !column_exists(conn, "repo_memory_bindings", "github_owner")? {
        return Ok(());
    }
    add_column_if_missing(conn, "repo_memory_bindings", "tracker", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "project", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "item_key", "TEXT")?;
    conn.execute_batch(
        "
        UPDATE repo_memory_bindings SET
            binding_kind = 'tracker',
            binding_id = 'github:' || github_owner || '/' || github_repo || '#' ||
                         CAST(github_number AS TEXT),
            tracker = 'github',
            project = github_owner || '/' || github_repo,
            item_key = CAST(github_number AS TEXT)
        WHERE binding_kind = 'github';
        ALTER TABLE repo_memory_bindings DROP COLUMN github_owner;
        ALTER TABLE repo_memory_bindings DROP COLUMN github_repo;
        ALTER TABLE repo_memory_bindings DROP COLUMN github_number;
        ",
    )
}
