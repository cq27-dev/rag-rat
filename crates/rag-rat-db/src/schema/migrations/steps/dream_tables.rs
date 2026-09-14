use rusqlite::Connection;

use crate::schema::migrations::{column_exists, sqlite_object_exists};

// Dream v2 pass 0 (memory verification sibling tables) — V046.
//
// The dream verify/compact passes need per-memory DERIVED state, and the load-bearing invariant is
// that dream NEVER mutates a `repo_memories` row (a human/strong agent confirms; dream only
// proposes). So the derived state lives in SIBLING tables, not new columns on `repo_memories`:
//
//  * `memory_reality` — one row per memory (keyed `(repo_id, memory_id)`), the verification
//    bookmark + verdict. `content_hash` and `checked_inputs_hash` are the churn-skip comparators
//    the verification queue reads: a memory is re-queued when its current content_hash differs from
//    the stored one (a TITLE or body edit — both prompts audit the whole note, so the hash covers
//    both) OR its recomputed evidence hash differs (source/identifier churn under the note).
//    `checked_inputs_hash` is a DELIBERATE addition to the plan's schema — a cheap sha comparison
//    over the memory's evidence pack beats a commit-ancestry walk. The verdict columns
//    (`verdict`/`direction`/`model_id`/`prompt_version`/`evidence_json`) are NULL in pass 0
//    (deterministic) and filled by the phase-B model verdict pass.
//  * `memory_summaries` — one row per `(repo_id, memory_id, content_hash)`, so a title or body edit
//    changes the key and self-invalidates the stale summary (a LEFT JOIN on the current
//    content_hash misses, the compaction pass in phase C regenerates).
//
// Both are STRICT and carry `repo_id` (repo-unique memory ids post-A5, mirroring how
// `repo_memory_bindings` scopes by `repo_id`); they hold regenerable data, so no FK to
// `repo_memories` — a deleted memory's orphan rows never surface (scoped reads join active
// memories) and gc sweeps them, matching the no-FK `dream_findings` periphery posture.
// ============================================================================================

/// The `memory_reality` table — also the V046 all-or-nothing SENTINEL (it exists iff the whole
/// atomic migration committed).
const MEMORY_REALITY_TABLE: &str = "memory_reality";

/// V046 (dream v2 pass 0): create the `memory_reality` / `memory_summaries` sibling tables that
/// hold the dream verification state, so the verify/compact passes persist derived per-memory data
/// WITHOUT ever touching a `repo_memories` row. Both are fresh CREATEs (no rebuild, no FK parent),
/// so unlike V040/V042 there is no `foreign_keys` toggle to do.
///
/// MECHANICS (the V044 no-FK-toggle recipe): the ladder runs apply fns in AUTOCOMMIT, so the two
/// CREATEs self-wrap in ONE `BEGIN IMMEDIATE` and commit together — `memory_reality` existing is
/// then the all-or-nothing sentinel (present iff both tables landed), so a `create_or_migrate` /
/// `rebuild` / `index --full` re-apply short-circuits before taking the write lock. The leading
/// `DROP TABLE IF EXISTS` re-converges after a hard-kill that bypassed a clean rollback (the tables
/// hold only regenerable data, so a drop-and-recreate is always safe here).
pub fn apply_memory_verification_tables(conn: &Connection) -> rusqlite::Result<()> {
    // All-or-nothing sentinel (see the doc comment): both CREATEs commit atomically, so
    // `memory_reality` present means the whole migration already ran.
    if sqlite_object_exists(conn, "table", MEMORY_REALITY_TABLE)? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = conn.execute_batch(
        "
        DROP TABLE IF EXISTS memory_reality;
        DROP TABLE IF EXISTS memory_summaries;
        CREATE TABLE memory_reality(
            memory_id              TEXT    NOT NULL,
            repo_id                TEXT    NOT NULL DEFAULT '__unassigned__',
            -- sha256 over the memory's NOTE content (trimmed title + body) — the churn-skip
            -- comparator that self-invalidates a stored verdict on any title OR body edit.
            content_hash           TEXT    NOT NULL,
            verdict                TEXT,
            direction              TEXT,
            -- Informational, for human review: the commit the note was checked against.
            checked_against_commit TEXT,
            -- sha256 over the sorted sha256s of the memory's bound files at check time — the
            -- churn-skip comparator (cheaper than a commit-ancestry walk).
            checked_inputs_hash    TEXT,
            evidence_json          TEXT,
            model_id               TEXT,
            prompt_version         TEXT,
            checked_at_ms          INTEGER NOT NULL,
            PRIMARY KEY (repo_id, memory_id)
        ) STRICT;
        CREATE TABLE memory_summaries(
            memory_id       TEXT    NOT NULL,
            repo_id         TEXT    NOT NULL DEFAULT '__unassigned__',
            -- Keyed WITH content_hash (trimmed title + body) so a title OR body edit changes the \
         key
            -- and self-invalidates the stale summary.
            content_hash    TEXT    NOT NULL,
            summary         TEXT    NOT NULL,
            model_id        TEXT,
            prompt_version  TEXT,
            generated_at_ms INTEGER NOT NULL,
            PRIMARY KEY (repo_id, memory_id, content_hash)
        ) STRICT;
        ",
    );
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

/// V047: record deterministic dream model failures in a derived sibling table.
///
/// The table is keyed one current row per `(repo_id, memory_id, pass)`, with the same freshness
/// stamps the queues already consult (`content_hash`, optional `checked_inputs_hash`,
/// `prompt_version`, `model_id`). A current row whose persisted enum reason is deterministic
/// suppresses another model call; content/evidence/prompt/model churn invalidates it.
pub fn apply_memory_model_failures_table(conn: &Connection) -> rusqlite::Result<()> {
    if column_exists(conn, "memory_model_failures", "reason")? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = conn.execute_batch(
        "
        DROP TABLE IF EXISTS memory_model_failures;
        CREATE TABLE memory_model_failures(
            memory_id           TEXT    NOT NULL,
            repo_id             TEXT    NOT NULL DEFAULT '__unassigned__',
            pass                TEXT    NOT NULL,
            content_hash        TEXT    NOT NULL,
            checked_inputs_hash TEXT,
            model_id            TEXT    NOT NULL,
            prompt_version      TEXT    NOT NULL,
            reason              TEXT    NOT NULL,
            detail              TEXT,
            failed_at_ms        INTEGER NOT NULL,
            attempts            INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (repo_id, memory_id, pass)
        ) STRICT;
        CREATE INDEX idx_memory_model_failures_reason
            ON memory_model_failures(repo_id, pass, reason);
        ",
    );
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}
