use rusqlite::Connection;

use crate::schema::migrations::sqlite_object_exists;

// ============================================================================================
// Memory-sync phase A7 (GitHub natural-key widening) — V044.
//
// V041 gave the GitHub papertrail tables a `repo_id` column but DELIBERATELY left their
// `(owner, repo, number)`-style natural keys un-widened (one repo per DB until A7, so those keys
// stayed correct). A7 makes multi-repo the default: two distinct repos in one consolidated DB can
// each reference the SAME external `(owner, repo, number)` — a shared upstream issue, a fork — and
// the un-widened key would make the second repo's sync UPSERT OVER the first's row (stamping the
// wrong `repo_id`, so a scoped read then loses it). V044 folds `repo_id` into every such key.
// ============================================================================================

/// The unique index V044 creates on `github_issues` — also the migration's all-or-nothing SENTINEL
/// (it exists iff the whole atomic migration committed). Distinct from `idx_github_refs_unique`,
/// which predates V044 (baseline) and so cannot mark this migration's completion.
const GITHUB_ISSUES_REPO_UNIQUE_INDEX: &str = "idx_github_issues_repo_unique";

/// V044 (memory-sync phase A7): widen the `(owner, repo, number)`-style GitHub natural keys to fold
/// `repo_id`, so a consolidated multi-repo DB can cache the same external issue/PR/ref for two
/// repos without one repo's sync UPSERTing over the other's row. Three shapes:
///
///  * INLINE-UNIQUE REBUILD: `github_issues` / `github_pull_requests` carried an inline
///    `UNIQUE(owner, repo, number)` table constraint (droppable only by rebuild). Each is rebuilt
///    WITHOUT the inline constraint plus a NAMED unique index `(repo_id, owner, repo, number)` —
///    the named index doubles as the migration sentinel and as the writers' `ON CONFLICT` target.
///  * INLINE-PK REBUILD: `github_ref_sync` carried an inline `PRIMARY KEY(owner, repo, number)`;
///    rebuilt with `PRIMARY KEY(repo_id, owner, repo, number)`.
///  * INDEX SWAP: `github_refs` scopes uniqueness through the SEPARATE `idx_github_refs_unique`
///    index (its own PK is `id AUTOINCREMENT`), so it needs only a DROP + CREATE of that index with
///    a leading `repo_id` — no table rebuild.
///
/// The id-keyed caches (`github_comments` / `github_reviews` / `github_review_comments`) are NOT
/// touched here — V044 originally left them last-syncer-owns, which V045
/// ([`apply_github_child_key_widening`]) superseded with `(repo_id, id)` uniqueness after the
/// restamping proved to evict a sibling repo's scoped papertrail (see the V045 block comment).
///
/// SAFE ON EXISTING DATA: this migration only ever runs on a SINGLE-repo DB (multi-repo does not
/// exist until A7 registration, which post-dates the migration ladder), so every existing github
/// row shares one `repo_id` and the old `(owner, repo, number)` uniqueness already guarantees the
/// widened key is unique — a plain `INSERT ... SELECT` copies faithfully with no dup risk.
///
/// MECHANICS: the github base tables carry NO incoming FK (nothing REFERENCES them), so unlike V040
/// no `foreign_keys` toggle is needed — the DROP/RENAME rebuilds touch no FK parent. The whole
/// migration self-wraps in ONE `BEGIN IMMEDIATE` (the ladder runs apply fns in AUTOCOMMIT), so the
/// rebuilds and index swaps commit together; the named `github_issues` unique index present is the
/// all-or-nothing sentinel, so a `create_or_migrate` / `rebuild` / `index --full` re-apply
/// short-circuits before the write lock. Each rebuild's leading `DROP TABLE IF EXISTS
/// <scratch>_new` re-converges after a hard-kill that bypassed a clean rollback (the V040 recipe).
pub(crate) fn apply_github_natural_key_widening(conn: &Connection) -> rusqlite::Result<()> {
    // Post-V060 (and on every fresh DB) the legacy github_* tables do not exist — the baseline
    // creates the provider-neutral papertrail_* tables instead. The ladder still replays this
    // migration in order; with nothing to widen it must no-op rather than rebuild absent tables.
    if !sqlite_object_exists(conn, "table", "github_issues")? {
        return Ok(());
    }
    // All-or-nothing sentinel (see the doc comment): the whole migration commits atomically, so the
    // named github_issues unique index existing means every rebuild + index swap already landed.
    if sqlite_object_exists(conn, "index", GITHUB_ISSUES_REPO_UNIQUE_INDEX)? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        rebuild_github_issues_with_repo_scoped_key(conn)?;
        rebuild_github_pull_requests_with_repo_scoped_key(conn)?;
        rebuild_github_ref_sync_with_repo_scoped_key(conn)?;
        widen_github_refs_unique_index(conn)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

/// Rebuild `github_issues` dropping the inline `UNIQUE(owner, repo, number)` and adding a NAMED
/// unique index `(repo_id, owner, repo, number)` (the V044 sentinel + the `store_item` `ON
/// CONFLICT` target). The full V041 column set (through `repo_id`) is reproduced verbatim; `id`
/// values are copied so a mid-flight papertrail resync's `github_fts.item_id` stays stable. Runs
/// inside the caller's transaction (opens none of its own).
fn rebuild_github_issues_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_issues_new;
        CREATE TABLE github_issues_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            state TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            is_pull_request INTEGER NOT NULL DEFAULT 0,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_issues_new(
            id, owner, repo, number, html_url, state, title, body, author, created_at, updated_at,
            is_pull_request, synced_at_ms, repo_id
        )
        SELECT id, owner, repo, number, html_url, state, title, body, author, created_at,
               updated_at, is_pull_request, synced_at_ms, repo_id
        FROM github_issues;
        DROP TABLE github_issues;
        ALTER TABLE github_issues_new RENAME TO github_issues;
        CREATE UNIQUE INDEX idx_github_issues_repo_unique
            ON github_issues(repo_id, owner, repo, number);
        ",
    )
}

/// Rebuild `github_pull_requests` dropping the inline `UNIQUE(owner, repo, number)` and adding a
/// NAMED unique index `(repo_id, owner, repo, number)` (the `store_item` change-request `ON
/// CONFLICT` target). See [`rebuild_github_issues_with_repo_scoped_key`] for the shared rationale.
fn rebuild_github_pull_requests_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_pull_requests_new;
        CREATE TABLE github_pull_requests_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            state TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            merged_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_pull_requests_new(
            id, owner, repo, number, html_url, state, title, body, author, created_at, updated_at,
            merged_at, synced_at_ms, repo_id
        )
        SELECT id, owner, repo, number, html_url, state, title, body, author, created_at,
               updated_at, merged_at, synced_at_ms, repo_id
        FROM github_pull_requests;
        DROP TABLE github_pull_requests;
        ALTER TABLE github_pull_requests_new RENAME TO github_pull_requests;
        CREATE UNIQUE INDEX idx_github_pull_requests_repo_unique
            ON github_pull_requests(repo_id, owner, repo, number);
        ",
    )
}

/// Rebuild `github_ref_sync` widening its inline `PRIMARY KEY(owner, repo, number)` to
/// `PRIMARY KEY(repo_id, owner, repo, number)` (the `github/sync.rs` `ON CONFLICT` target). Copies
/// the full V041 column set verbatim.
fn rebuild_github_ref_sync_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_ref_sync_new;
        CREATE TABLE github_ref_sync_new(
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            status TEXT NOT NULL,
            synced_at_ms INTEGER NOT NULL,
            last_error TEXT,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            PRIMARY KEY(repo_id, owner, repo, number)
        );
        INSERT INTO github_ref_sync_new(owner, repo, number, status, synced_at_ms, last_error, \
         repo_id)
        SELECT owner, repo, number, status, synced_at_ms, last_error, repo_id FROM github_ref_sync;
        DROP TABLE github_ref_sync;
        ALTER TABLE github_ref_sync_new RENAME TO github_ref_sync;
        ",
    )
}

/// Widen `github_refs`' uniqueness index to lead with `repo_id`. `github_refs`' own PK is `id
/// AUTOINCREMENT`, so its `(owner, repo, number, source_kind, …)` uniqueness lives in the SEPARATE
/// `idx_github_refs_unique` index — a DROP + CREATE swap suffices, no table rebuild. The trailing
/// columns match `store_ref`'s widened `ON CONFLICT` target exactly.
fn widen_github_refs_unique_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP INDEX IF EXISTS idx_github_refs_unique;
        CREATE UNIQUE INDEX idx_github_refs_unique
            ON github_refs(repo_id, owner, repo, number, source_kind, COALESCE(source_path, ''),
                           COALESCE(source_commit, ''), source_text);
        ",
    )
}

// ============================================================================================
// Memory-sync phase A7 (GitHub id-keyed child widening) — V045.
//
// V044 widened the `(owner, repo, number)` natural keys but left the id-keyed CHILD caches
// (`github_comments` / `github_reviews` / `github_review_comments`) keyed by GitHub's own item id
// alone, documented as "last-syncer-owns". The consequence on a shared external issue/PR: each
// repo's sync RESTAMPS the child rows to itself, the sync-tail FTS rebuild copies that repo_id
// into the mirror, and every scoped reader filters `repo_id = active` — so repo A's papertrail
// LOSES the PR's comments/reviews the moment repo B syncs, oscillating on every re-sync. V045
// folds `repo_id` into the child keys so each repo owns its own copy.
// ============================================================================================

/// The V045 sentinel: the named unique index on `github_comments` (created inside the atomic
/// migration, so its existence ⇒ the whole migration committed).
const GITHUB_COMMENTS_REPO_UNIQUE_INDEX: &str = "idx_github_comments_repo_unique";

/// V045 (memory-sync phase A7): widen the id-keyed GitHub child caches to `(repo_id, id)`
/// uniqueness, so two repos sharing an external issue/PR each keep their own copy of its
/// comments/reviews instead of last-syncer-owns oscillation (see the block comment above). Each
/// table is rebuilt WITHOUT the inline `id INTEGER PRIMARY KEY` (the implicit rowid remains) plus
/// a NAMED unique index `(repo_id, id)` — the V044 recipe; the `github_comments` index doubles as
/// the all-or-nothing sentinel.
///
/// BACKFILL RULE (per row, lossless): a child row is copied once PER OWNING-PARENT repo — the
/// DISTINCT `repo_id`s of the parent rows matching its `(owner, repo, number)` (comments parent =
/// issues ∪ pull requests, both stored via the issues API; reviews / review comments parent =
/// pull requests) — because post-V044 a shared parent exists under MULTIPLE repos and each owner's
/// scoped papertrail must see the children. Additionally every row survives under its OWN stamped
/// `repo_id` when not already covered (`NOT EXISTS` second pass): orphans with no cached parent,
/// and the defensive case of a stamped repo whose parent row is missing. No row is ever dropped.
///
/// MECHANICS: no incoming FKs (no `foreign_keys` toggle needed); ONE self-wrapped
/// `BEGIN IMMEDIATE`; leading `DROP TABLE IF EXISTS <scratch>_new` re-converges a hard-killed
/// prior pass; the unique indexes are created AFTER the copies (the copy dedupes via UNION /
/// NOT EXISTS, never via constraint suppression). The writers keep `INSERT OR REPLACE`, which now
/// resolves through the widened unique index — a same-repo re-sync replaces in place, a sibling
/// repo's sync inserts its own row. The `github_fts` mirror is re-derived INSIDE the migration
/// ([`rebuild_github_fts_from_widened_bases`]) so the duplicated rows are scoped-searchable
/// immediately, not only after the next sync.
pub fn apply_github_child_key_widening(conn: &Connection) -> rusqlite::Result<()> {
    // Post-V060 / fresh-DB no-op, exactly like V041/V044: the legacy child caches don't exist.
    if !sqlite_object_exists(conn, "table", "github_comments")? {
        return Ok(());
    }
    if sqlite_object_exists(conn, "index", GITHUB_COMMENTS_REPO_UNIQUE_INDEX)? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        rebuild_github_comments_with_repo_scoped_key(conn)?;
        rebuild_github_reviews_with_repo_scoped_key(conn)?;
        rebuild_github_review_comments_with_repo_scoped_key(conn)?;
        rebuild_github_fts_from_widened_bases(conn)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

/// Re-derive the standalone `github_fts` mirror from the freshly-widened base tables, INSIDE the
/// V045 transaction — the V042 memory-FTS posture. Without this, the repos that just gained
/// duplicated child rows still cannot FIND them through the scoped FTS readers
/// (`rationale_search` / papertrail filter `repo_id = active`) until some later github sync
/// happens to run the sync-tail rebuild — the migration would fix the base tables while the
/// mirror kept serving the last-syncer state. The derivation mirrors `papertrail::rebuild_fts`'s
/// column mapping exactly (per-kind title slots; reviews' `COALESCE(html_url,'')`); the
/// `classification` column is a Rust-derived label (`classify_text`), so it is CARRIED from the
/// old mirror by `(item_kind, item_id)` — identical per item across per-repo duplicates — with
/// `'other'` for never-mirrored rows (refreshed to the real label at the next sync's rebuild).
/// GUARDED on `github_fts` presence: the schema-bootstrap isolation fixtures seed only the base
/// tables. Torn-state safe: runs inside the caller's single transaction; the temp scratch is
/// per-connection and re-created per run.
fn rebuild_github_fts_from_widened_bases(conn: &Connection) -> rusqlite::Result<()> {
    if !sqlite_object_exists(conn, "table", "github_fts")? {
        return Ok(());
    }
    conn.execute_batch(
        "
        CREATE TEMP TABLE IF NOT EXISTS v045_fts_class(
            item_kind TEXT NOT NULL,
            item_id TEXT NOT NULL,
            classification TEXT NOT NULL,
            PRIMARY KEY(item_kind, item_id)
        );
        DELETE FROM temp.v045_fts_class;
        INSERT OR IGNORE INTO temp.v045_fts_class(item_kind, item_id, classification)
            SELECT item_kind, item_id, classification FROM github_fts;
        DELETE FROM github_fts;
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT i.owner, i.repo, i.number, 'issue', CAST(i.id AS TEXT), i.html_url, i.title, \
         i.body, COALESCE(c.classification, 'other'), i.repo_id
        FROM github_issues i
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'issue' AND c.item_id = CAST(i.id AS TEXT);
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT m.owner, m.repo, m.number, 'comment', CAST(m.id AS TEXT), m.html_url, '', m.body,
               COALESCE(c.classification, 'other'), m.repo_id
        FROM github_comments m
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'comment' AND c.item_id = CAST(m.id AS TEXT);
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT p.owner, p.repo, p.number, 'pull', CAST(p.id AS TEXT), p.html_url, p.title, p.body, \
         COALESCE(c.classification, 'other'), p.repo_id
        FROM github_pull_requests p
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'pull' AND c.item_id = CAST(p.id AS TEXT);
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT r.owner, r.repo, r.number, 'review', CAST(r.id AS TEXT), COALESCE(r.html_url, ''), \
         '', r.body, COALESCE(c.classification, 'other'), r.repo_id
        FROM github_reviews r
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'review' AND c.item_id = CAST(r.id AS TEXT);
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT rc.owner, rc.repo, rc.number, 'review_comment', CAST(rc.id AS TEXT), rc.html_url, \
         COALESCE(rc.path, ''), rc.body, COALESCE(c.classification, 'other'), rc.repo_id
        FROM github_review_comments rc
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'review_comment' AND c.item_id = CAST(rc.id AS TEXT);
        DROP TABLE temp.v045_fts_class;
        ",
    )
}

/// Rebuild `github_comments` with `(repo_id, id)` uniqueness, backfilled once per owning parent
/// (issues ∪ pull requests — issue comments cover both) plus the own-repo_id lossless pass. Runs
/// inside the caller's transaction.
fn rebuild_github_comments_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_comments_new;
        CREATE TABLE github_comments_new(
            id INTEGER NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_comments_new(
            id, owner, repo, number, html_url, body, author, created_at, updated_at, synced_at_ms,
            repo_id
        )
        SELECT c.id, c.owner, c.repo, c.number, c.html_url, c.body, c.author, c.created_at,
               c.updated_at, c.synced_at_ms, p.repo_id
        FROM github_comments c
        JOIN (SELECT owner, repo, number, repo_id FROM github_issues
              UNION
              SELECT owner, repo, number, repo_id FROM github_pull_requests) p
          ON p.owner = c.owner AND p.repo = c.repo AND p.number = c.number;
        INSERT INTO github_comments_new(
            id, owner, repo, number, html_url, body, author, created_at, updated_at, synced_at_ms,
            repo_id
        )
        SELECT c.id, c.owner, c.repo, c.number, c.html_url, c.body, c.author, c.created_at,
               c.updated_at, c.synced_at_ms, c.repo_id
        FROM github_comments c
        WHERE NOT EXISTS (SELECT 1 FROM github_comments_new n
                          WHERE n.repo_id = c.repo_id AND n.id = c.id);
        DROP TABLE github_comments;
        ALTER TABLE github_comments_new RENAME TO github_comments;
        CREATE UNIQUE INDEX idx_github_comments_repo_unique ON github_comments(repo_id, id);
        ",
    )
}

/// Rebuild `github_reviews` with `(repo_id, id)` uniqueness, backfilled once per owning
/// pull-request repo plus the own-repo_id lossless pass.
fn rebuild_github_reviews_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_reviews_new;
        CREATE TABLE github_reviews_new(
            id INTEGER NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT,
            state TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            submitted_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_reviews_new(
            id, owner, repo, number, html_url, state, body, author, submitted_at, synced_at_ms,
            repo_id
        )
        SELECT r.id, r.owner, r.repo, r.number, r.html_url, r.state, r.body, r.author,
               r.submitted_at, r.synced_at_ms, p.repo_id
        FROM github_reviews r
        JOIN (SELECT DISTINCT owner, repo, number, repo_id FROM github_pull_requests) p
          ON p.owner = r.owner AND p.repo = r.repo AND p.number = r.number;
        INSERT INTO github_reviews_new(
            id, owner, repo, number, html_url, state, body, author, submitted_at, synced_at_ms,
            repo_id
        )
        SELECT r.id, r.owner, r.repo, r.number, r.html_url, r.state, r.body, r.author,
               r.submitted_at, r.synced_at_ms, r.repo_id
        FROM github_reviews r
        WHERE NOT EXISTS (SELECT 1 FROM github_reviews_new n
                          WHERE n.repo_id = r.repo_id AND n.id = r.id);
        DROP TABLE github_reviews;
        ALTER TABLE github_reviews_new RENAME TO github_reviews;
        CREATE UNIQUE INDEX idx_github_reviews_repo_unique ON github_reviews(repo_id, id);
        ",
    )
}

/// Rebuild `github_review_comments` with `(repo_id, id)` uniqueness, backfilled once per owning
/// pull-request repo plus the own-repo_id lossless pass.
fn rebuild_github_review_comments_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_review_comments_new;
        CREATE TABLE github_review_comments_new(
            id INTEGER NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            path TEXT,
            html_url TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_review_comments_new(
            id, owner, repo, number, path, html_url, body, author, created_at, updated_at,
            synced_at_ms, repo_id
        )
        SELECT c.id, c.owner, c.repo, c.number, c.path, c.html_url, c.body, c.author,
               c.created_at, c.updated_at, c.synced_at_ms, p.repo_id
        FROM github_review_comments c
        JOIN (SELECT DISTINCT owner, repo, number, repo_id FROM github_pull_requests) p
          ON p.owner = c.owner AND p.repo = c.repo AND p.number = c.number;
        INSERT INTO github_review_comments_new(
            id, owner, repo, number, path, html_url, body, author, created_at, updated_at,
            synced_at_ms, repo_id
        )
        SELECT c.id, c.owner, c.repo, c.number, c.path, c.html_url, c.body, c.author,
               c.created_at, c.updated_at, c.synced_at_ms, c.repo_id
        FROM github_review_comments c
        WHERE NOT EXISTS (SELECT 1 FROM github_review_comments_new n
                          WHERE n.repo_id = c.repo_id AND n.id = c.id);
        DROP TABLE github_review_comments;
        ALTER TABLE github_review_comments_new RENAME TO github_review_comments;
        CREATE UNIQUE INDEX idx_github_review_comments_repo_unique
            ON github_review_comments(repo_id, id);
        ",
    )
}
