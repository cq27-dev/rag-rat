//! `rag-rat status`: a read-only, cross-repo inventory of the consolidated multi-repo global store.
//!
//! Where `doctor` opens ONE repo and reports it deeply, `status` opens the store WITHOUT choosing a
//! repo (the repo-scoped opens refuse a multi-repo DB, #603) and rolls up every registered repo's
//! index health side by side. It mirrors [`IndexDatabase::global_store_overview`]: one read-only
//! connection, side-effect-free, never auto-migrates, and returns `exists: false` without touching
//! the file when the store is absent.
//!
//! Scoping is the whole point. Every per-repo count is filtered by `repo_id` so a sibling repo's
//! rows can NEVER bleed into another repo's totals:
//!   * The content tables (`chunks` / `symbols` / `edges_data`) carry NO `repo_id` of their own —
//!     they scope TRANSITIVELY through `files.repo_id` via `file_id` (`edges_data.source_file_id`),
//!     and embeddings scope one hop further through `chunk_embeddings.chunk_id -> chunks -> files`.
//!   * `repo_memories`, `papertrail_*`, and `repo_meta` carry `repo_id` directly.
//!   * FTS / `content_revision` freshness is deliberately GLOBAL (one FTS5 index over the whole
//!     `chunks` table), so it lives in the top-level rollup, not per repo — same classification the
//!     `index_status` shape uses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rag_rat_db::meta::{read_meta, repo_meta};
use rag_rat_db::schema::{self, RegisteredRepo};
use rag_rat_db::storage::IndexConnection;
use rusqlite::{Connection, params};
use serde::Serialize;

use super::db_file_health::{self, DatabaseFileHealth};
use crate::index::IndexDatabase;

/// The whole-store report: on-disk facts, the global FTS/freshness rollup, and one [`RepoStatus`]
/// per registered repo. `schema`/`file_health`/`fts` are `None` when the store file is absent or
/// its schema is not `Compatible` (no queryable tables); `repos` is then empty.
#[derive(Debug, Clone, Serialize)]
pub struct GlobalStatus {
    pub database: PathBuf,
    pub exists: bool,
    pub size_bytes: Option<u64>,
    pub schema: Option<schema::SchemaStatus>,
    /// Whole-file size / dead-space / VACUUM hint — the global rollup (reuses `db_file_health`).
    pub file_health: Option<DatabaseFileHealth>,
    /// GLOBAL content + FTS freshness (one FTS5 index over the whole store), not per repo.
    pub fts: Option<GlobalFtsStatus>,
    pub repo_count: usize,
    pub repos: Vec<RepoStatus>,
}

/// GLOBAL content-digest + FTS freshness markers, read straight from `index_meta` (cheap: the
/// digest is the cached value, never recomputed here).
#[derive(Debug, Clone, Serialize)]
pub struct GlobalFtsStatus {
    pub content_revision: Option<String>,
    pub fts_source_revision: Option<String>,
    pub fts_synced_at_ms: Option<i64>,
    pub fts_dirty: bool,
    pub fts_fresh: bool,
}

/// One registered repo's slice of the store: identity, index freshness, live worktree overlays,
/// memory counts, papertrail sync, and content sizing — all scoped by `repo_id`.
#[derive(Debug, Clone, Serialize)]
pub struct RepoStatus {
    pub repo_id: String,
    pub display_name: String,
    pub roots: Vec<String>,
    pub registered_at_ms: i64,
    pub freshness: RepoFreshness,
    pub worktree_overlays: Vec<WorktreeOverlay>,
    pub memories: MemoryCounts,
    pub papertrail: RepoPapertrail,
    pub content: RepoContent,
}

/// Per-repo index freshness from `repo_meta` (the per-repo keys) plus the A6 live-generation
/// pointer. `content_revision` / FTS state are NOT here — those are global (see
/// [`GlobalFtsStatus`]).
#[derive(Debug, Clone, Serialize)]
pub struct RepoFreshness {
    /// The commit this repo's base index was last built at (`repo_meta` `git_commit`).
    pub indexed_head: Option<String>,
    /// Whether the working tree was dirty at index time (`repo_meta` `git_dirty`).
    pub git_dirty: Option<bool>,
    pub indexed_at_ms: Option<i64>,
    /// The A6 live `files.generation` pointer — the generation a reader/incremental open scopes
    /// to.
    pub live_files_generation: i64,
}

/// A live linked-worktree overlay carried on top of the base index. Identified in the DB as a
/// DISTINCT non-empty `files.worktree_id` for this repo — the base checkout stores `worktree_id =
/// ''` (verified against the live store), so a non-empty id is always an overlay.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeOverlay {
    pub worktree_id: String,
    pub file_count: u64,
}

/// `repo_memories` counts for this repo, split by lifecycle status. The named buckets cover the
/// closed set of valid statuses (`active` / `obsolete` / `stale` / `rejected` — the set
/// `rag_rat_query::memory::validate_status` accepts), so the buckets reconcile with `total`.
/// `by_kind` breaks the same rows down per memory kind (`Invariant` / `Decision` / `Risk` / …).
/// `total` counts every row regardless of status, so even a status outside the closed set still
/// lands in the total.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryCounts {
    pub total: u64,
    pub active: u64,
    pub obsolete: u64,
    pub stale: u64,
    pub rejected: u64,
    pub by_kind: BTreeMap<String, MemoryKindCounts>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MemoryKindCounts {
    pub active: u64,
    pub obsolete: u64,
    pub stale: u64,
    pub rejected: u64,
}

/// This repo's papertrail mirror: item counts by kind (`issue` / `change_request` / …), comment and
/// ref totals, and the per-(tracker, project) sync cursors that record the tracker/project binding
/// and last-sync marks.
#[derive(Debug, Clone, Serialize)]
pub struct RepoPapertrail {
    pub items_by_kind: BTreeMap<String, u64>,
    pub comments: u64,
    pub refs: u64,
    pub cursors: Vec<PapertrailCursor>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailCursor {
    pub tracker: String,
    pub project: String,
    pub last_probe_ms: Option<i64>,
    pub last_full_sync_ms: Option<i64>,
    /// Whether the historical backfill descent has reached the oldest item.
    pub backfill_done: bool,
}

/// Row counts that dominate this repo's on-disk footprint, all scoped through `files.repo_id`.
#[derive(Debug, Clone, Serialize)]
pub struct RepoContent {
    pub files: u64,
    pub chunks: u64,
    pub symbols: u64,
    pub edges: u64,
    pub embeddings: u64,
}

impl IndexDatabase {
    /// Read-only cross-repo inventory of the consolidated global store at `path` — the `rag-rat
    /// status` engine. Opens ONE read-only connection (like [`Self::global_store_overview`]), never
    /// auto-migrates, and is side-effect-free when the file is absent (`exists: false`, nothing
    /// created). The per-repo rollup runs only when the schema is `Compatible` (a
    /// Missing/Newer/Dirty schema has no queryable tables); otherwise `repos` is empty and the
    /// size/schema are still reported. Every per-repo count is `repo_id`-scoped — a sibling's
    /// rows never bleed in.
    pub fn global_status(path: &Path) -> anyhow::Result<GlobalStatus> {
        if !path.is_file() {
            return Ok(GlobalStatus {
                database: path.to_path_buf(),
                exists: false,
                size_bytes: None,
                schema: None,
                file_health: None,
                fts: None,
                repo_count: 0,
                repos: Vec::new(),
            });
        }
        let size_bytes = std::fs::metadata(path).ok().map(|meta| meta.len());
        // READ-ONLY throughout: `status` must work against a read-only mount / backup, so it never
        // takes the read-WRITE `migration_check` path. A read-only open never auto-migrates, which
        // is correct — the report states the schema STATE, it does not change it.
        let storage = IndexConnection::open_read_only_blocking(path)?;
        let conn = storage.connection();
        let schema = schema::status(conn)?;
        if schema.state != schema::SchemaState::Compatible {
            return Ok(GlobalStatus {
                database: path.to_path_buf(),
                exists: true,
                size_bytes,
                schema: Some(schema),
                file_health: None,
                fts: None,
                repo_count: 0,
                repos: Vec::new(),
            });
        }
        // One DEFERRED read snapshot for the whole rollup. In WAL mode every SELECT below then sees
        // the same point-in-time view, so a concurrent rebuild that flips `live_files_generation`
        // and GCs the previous generation MID-report cannot make a repo's freshness name a
        // generation that its later content/overlay queries then read as empty. Read-only, so the
        // transaction only ever rolls back on drop.
        let snapshot = conn.unchecked_transaction()?;
        let file_health = db_file_health::database_file_health_from_conn(conn, path)?;
        let fts = read_global_fts_status(conn)?;
        let repos = schema::registered_repos(conn)?
            .into_iter()
            .map(|repo| repo_status(conn, repo))
            .collect::<anyhow::Result<Vec<_>>>()?;
        drop(snapshot);
        Ok(GlobalStatus {
            database: path.to_path_buf(),
            exists: true,
            size_bytes,
            schema: Some(schema),
            file_health: Some(file_health),
            fts: Some(fts),
            repo_count: repos.len(),
            repos,
        })
    }
}

/// The GLOBAL FTS/content freshness rollup — read from `index_meta` (`content_revision`,
/// `fts_source_revision`, `fts_synced_at_ms`, `fts_dirty` are all global keys, V040 round-5), never
/// recomputed. `fts_fresh` mirrors the `index_status` predicate: not dirty AND the FTS source
/// revision matches the current content digest.
fn read_global_fts_status(conn: &Connection) -> anyhow::Result<GlobalFtsStatus> {
    let content_revision = read_meta(conn, "content_revision")?;
    let fts_source_revision = read_meta(conn, "fts_source_revision")?;
    let fts_synced_at_ms =
        read_meta(conn, "fts_synced_at_ms")?.and_then(|value| value.parse().ok());
    let fts_dirty = read_meta(conn, "fts_dirty")?.as_deref() == Some("true");
    let fts_fresh =
        !fts_dirty && content_revision.is_some() && fts_source_revision == content_revision;
    Ok(GlobalFtsStatus {
        content_revision,
        fts_source_revision,
        fts_synced_at_ms,
        fts_dirty,
        fts_fresh,
    })
}

/// Assemble one repo's [`RepoStatus`] — orchestration only; each section is its own
/// `repo_id`-scoped reader below.
fn repo_status(conn: &Connection, repo: RegisteredRepo) -> anyhow::Result<RepoStatus> {
    let RegisteredRepo { repo_id, display_name, roots, registered_at_ms } = repo;
    let freshness = read_repo_freshness(conn, &repo_id)?;
    // Scope every file-derived count to the repo's LIVE generation. A full rebuild stages a fresh
    // generation and flips this pointer, leaving the previous generation's rows in place until GC,
    // and a rebuild in flight has a staged generation coexisting with the live one. Counting across
    // generations would inflate a count or surface a worktree overlay that exists only in a
    // dead/staged generation — the base index readers never see either.
    let live_generation = freshness.live_files_generation;
    let worktree_overlays = list_repo_worktree_overlays(conn, &repo_id, live_generation)?;
    let memories = count_repo_memories_by_kind(conn, &repo_id)?;
    let papertrail = read_repo_papertrail(conn, &repo_id)?;
    let content = count_repo_content(conn, &repo_id, live_generation)?;
    Ok(RepoStatus {
        repo_id,
        display_name,
        roots,
        registered_at_ms,
        freshness,
        worktree_overlays,
        memories,
        papertrail,
        content,
    })
}

fn read_repo_freshness(conn: &Connection, repo_id: &str) -> anyhow::Result<RepoFreshness> {
    Ok(RepoFreshness {
        indexed_head: repo_meta(conn, repo_id, "git_commit")?,
        git_dirty: repo_meta(conn, repo_id, "git_dirty")?.map(|value| value == "true"),
        indexed_at_ms: repo_meta(conn, repo_id, "indexed_at_ms")?
            .and_then(|value| value.parse().ok()),
        live_files_generation: schema::live_files_generation(conn, repo_id)?,
    })
}

/// The live linked-worktree overlays for `repo_id`: distinct non-empty `files.worktree_id` values
/// with their VISIBLE file counts, scoped to the repo's live `generation`. The base checkout's rows
/// carry `worktree_id = ''`, so filtering it out leaves exactly the overlays; scoping to the live
/// generation drops a dead/staged generation's rows so a stale overlay is neither reported nor
/// inflates a live overlay's count. `kind != 'deleted'` excludes overlay TOMBSTONES (a worktree
/// that only deletes a base file leaves a `kind='deleted'` marker with no visible file) — the same
/// predicate the scoped overlay reader uses, so `status` never reports an overlay whose only row is
/// a deletion marker.
fn list_repo_worktree_overlays(
    conn: &Connection,
    repo_id: &str,
    live_generation: i64,
) -> anyhow::Result<Vec<WorktreeOverlay>> {
    let mut stmt = conn.prepare(
        "SELECT worktree_id, COUNT(*) FROM main.files WHERE repo_id = ?1 AND generation = ?2 AND \
         worktree_id != '' AND kind != 'deleted' GROUP BY worktree_id ORDER BY worktree_id",
    )?;
    let overlays = stmt
        .query_map(params![repo_id, live_generation], |row| {
            Ok(WorktreeOverlay {
                worktree_id: row.get(0)?,
                file_count: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(overlays)
}

/// `repo_memories` counts for `repo_id`, grouped by (kind, status). Scoped by
/// `repo_memories.repo_id` DIRECTLY (the table carries the column since V042) — NOT through
/// `repo_memory_bindings`, which would double-count a multi-bound memory and drop unanchored ones.
fn count_repo_memories_by_kind(conn: &Connection, repo_id: &str) -> anyhow::Result<MemoryCounts> {
    let mut stmt = conn.prepare(
        "SELECT kind, status, COUNT(*) FROM repo_memories WHERE repo_id = ?1 GROUP BY kind, status",
    )?;
    let rows = stmt.query_map([repo_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
        ))
    })?;
    let mut counts = MemoryCounts {
        total: 0,
        active: 0,
        obsolete: 0,
        stale: 0,
        rejected: 0,
        by_kind: BTreeMap::new(),
    };
    for row in rows {
        let (kind, status, count) = row?;
        counts.total += count;
        let by_kind = counts.by_kind.entry(kind).or_default();
        // The closed set of valid statuses (`rag_rat_query::memory::validate_status`) — each has a
        // named bucket so the buckets reconcile with `total`.
        match status.as_str() {
            "active" => {
                counts.active += count;
                by_kind.active += count;
            },
            "obsolete" => {
                counts.obsolete += count;
                by_kind.obsolete += count;
            },
            "stale" => {
                counts.stale += count;
                by_kind.stale += count;
            },
            "rejected" => {
                counts.rejected += count;
                by_kind.rejected += count;
            },
            // A status outside the closed set still counts toward `total` (above) but no bucket.
            _ => {},
        }
    }
    Ok(counts)
}

fn read_repo_papertrail(conn: &Connection, repo_id: &str) -> anyhow::Result<RepoPapertrail> {
    let mut items_stmt = conn.prepare(
        "SELECT item_kind, COUNT(*) FROM papertrail_items WHERE repo_id = ?1 GROUP BY item_kind \
         ORDER BY item_kind",
    )?;
    let items_by_kind = items_stmt
        .query_map([repo_id], |row| {
            Ok((row.get::<_, String>(0)?, u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0)))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;

    let comments = count_by_repo(
        conn,
        "SELECT COUNT(*) FROM papertrail_comments WHERE repo_id = ?1",
        repo_id,
    )?;
    let refs =
        count_by_repo(conn, "SELECT COUNT(*) FROM papertrail_refs WHERE repo_id = ?1", repo_id)?;

    let mut cursor_stmt = conn.prepare(
        "SELECT tracker, project, last_probe_ms, last_full_sync_ms, backfill_done FROM \
         papertrail_sync_cursor WHERE repo_id = ?1 ORDER BY tracker, project",
    )?;
    let cursors = cursor_stmt
        .query_map([repo_id], |row| {
            Ok(PapertrailCursor {
                tracker: row.get(0)?,
                project: row.get(1)?,
                last_probe_ms: row.get(2)?,
                last_full_sync_ms: row.get(3)?,
                backfill_done: row.get::<_, i64>(4)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(RepoPapertrail { items_by_kind, comments, refs, cursors })
}

/// Content-sizing counts for `repo_id`, all scoped to the repo's live `generation`. `files` carries
/// `repo_id` + `generation`; the rest scope through it: `chunks`/`symbols` by `file_id`,
/// `edges_data` by `source_file_id` (file-level edges with a NULL source are unattributable and
/// excluded), embeddings one hop further through `chunks`. Filtering `files.generation` to the live
/// generation keeps a dead/staged generation's rows out of every count (see [`repo_status`]). The
/// `files` count also excludes `kind='deleted'` TOMBSTONES (a deleted file leaves a marker row with
/// no chunk/symbol/embedding content), so `files` matches the visible indexed content rather than
/// inflating it; the derived counts join through real content rows, which a tombstone never has.
fn count_repo_content(
    conn: &Connection,
    repo_id: &str,
    live_generation: i64,
) -> anyhow::Result<RepoContent> {
    Ok(RepoContent {
        files: count_live_files(
            conn,
            "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1 AND generation = ?2 AND kind != \
             'deleted'",
            repo_id,
            live_generation,
        )?,
        chunks: count_live_files(
            conn,
            "SELECT COUNT(*) FROM chunks c JOIN main.files f ON c.file_id = f.id WHERE f.repo_id \
             = ?1 AND f.generation = ?2",
            repo_id,
            live_generation,
        )?,
        symbols: count_live_files(
            conn,
            "SELECT COUNT(*) FROM symbols s JOIN main.files f ON s.file_id = f.id WHERE f.repo_id \
             = ?1 AND f.generation = ?2",
            repo_id,
            live_generation,
        )?,
        edges: count_live_files(
            conn,
            "SELECT COUNT(*) FROM edges_data e JOIN main.files f ON e.source_file_id = f.id WHERE \
             f.repo_id = ?1 AND f.generation = ?2",
            repo_id,
            live_generation,
        )?,
        embeddings: count_live_files(
            conn,
            "SELECT COUNT(*) FROM chunk_embeddings ce JOIN chunks c ON ce.chunk_id = c.id JOIN \
             main.files f ON c.file_id = f.id WHERE f.repo_id = ?1 AND f.generation = ?2",
            repo_id,
            live_generation,
        )?,
    })
}

/// Run a `(repo_id, generation)`-bound `SELECT COUNT(*)` and return it as a `u64` — the content
/// counts scope through `files` to the repo's live generation, so a dead/staged generation's rows
/// never inflate them. A negative count is impossible, so a failed cast floors at 0.
fn count_live_files(
    conn: &Connection,
    sql: &str,
    repo_id: &str,
    live_generation: i64,
) -> rusqlite::Result<u64> {
    conn.query_row(sql, params![repo_id, live_generation], |row| row.get::<_, i64>(0))
        .map(|count| u64::try_from(count).unwrap_or(0))
}

/// Run a single-`repo_id`-bound `SELECT COUNT(*)` and return it as a `u64` — for the papertrail
/// tables, which carry `repo_id` directly and are NOT generation-scoped. A negative count is
/// impossible, so a failed cast floors at 0.
fn count_by_repo(conn: &Connection, sql: &str, repo_id: &str) -> rusqlite::Result<u64> {
    conn.query_row(sql, [repo_id], |row| row.get::<_, i64>(0))
        .map(|count| u64::try_from(count).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rag_rat_base::repo_identity::{RepoIdentity, RepoIdentityClass};
    use rag_rat_db::schema::{self, register_repo};
    use rag_rat_db::storage::IndexConnection;
    use rusqlite::{Connection, params};

    use crate::index::IndexDatabase;

    static N: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> PathBuf {
        let id = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("ragrat-status-{}-{id}", std::process::id()))
            .join("rag-rat.sqlite")
    }

    fn register(conn: &Connection, repo_id: &str, root: &str) {
        register_repo(
            conn,
            &RepoIdentity {
                repo_id: repo_id.to_string(),
                display_name: repo_id.to_string(),
                class: RepoIdentityClass::Portable,
                shallow_boundary: Vec::new(),
            },
            std::path::Path::new(root),
            7,
            &crate::index::migration_hooks(),
        )
        .unwrap();
    }

    /// Seed one file for `repo_id` at `worktree_id` and return its id.
    fn seed_file(conn: &Connection, repo_id: &str, worktree_id: &str) -> i64 {
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             repo_id, worktree_id) VALUES (?1, 'rust', 'source', 'sha', 1, 1, ?2, ?3)",
            params![format!("f{}.rs", N.fetch_add(1, Ordering::Relaxed)), repo_id, worktree_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Seed a file at an explicit `generation` (the column defaults to 0) — for the live/dead
    /// generation scoping test.
    fn seed_file_gen(conn: &Connection, repo_id: &str, worktree_id: &str, generation: i64) -> i64 {
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             repo_id, worktree_id, generation) VALUES (?1, 'rust', 'source', 'sha', 1, 1, ?2, ?3, \
             ?4)",
            params![
                format!("f{}.rs", N.fetch_add(1, Ordering::Relaxed)),
                repo_id,
                worktree_id,
                generation
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Seed a `kind='deleted'` overlay tombstone (the marker `write_tombstone_in_scope` leaves when
    /// a linked worktree deletes a base file) — for the tombstone-exclusion test.
    fn seed_tombstone(conn: &Connection, repo_id: &str, worktree_id: &str, generation: i64) -> i64 {
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             repo_id, worktree_id, generation) VALUES (?1, 'unknown', 'deleted', '', 0, 0, ?2, \
             ?3, ?4)",
            params![
                format!("f{}.rs", N.fetch_add(1, Ordering::Relaxed)),
                repo_id,
                worktree_id,
                generation
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn seed_chunk(conn: &Connection, file_id: i64) -> i64 {
        conn.execute(
            "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
             text_hash) VALUES (?1, 'code', 0, 1, 0, 1, 'h')",
            params![file_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn seed_symbol(conn: &Connection, file_id: i64) {
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte) VALUES (?1, \
             'rust', 'sym', 'function', 0, 1)",
            params![file_id],
        )
        .unwrap();
    }

    fn seed_edge(conn: &Connection, file_id: i64) {
        conn.execute(
            "INSERT INTO edges_data(source_file_id, to_name_id, resolution_id, edge_kind_id, \
             confidence_id) VALUES (?1, 0, 0, 0, 0)",
            params![file_id],
        )
        .unwrap();
    }

    fn seed_embedding(conn: &Connection, chunk_id: i64) {
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, model_id, source_text_hash, vector_blob, \
             status, created_at_ms) VALUES (?1, 'm', 'h', X'00', 'ready', 1)",
            params![chunk_id],
        )
        .unwrap();
    }

    fn seed_memory(conn: &Connection, repo_id: &str, kind: &str, status: &str) {
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, repo_id) VALUES (?1, ?2, 't', 'b', 'high', \
             ?3, 1, 1, 'agent', 'v1', ?4)",
            params![format!("mem_{}", N.fetch_add(1, Ordering::Relaxed)), kind, status, repo_id],
        )
        .unwrap();
    }

    fn seed_item(conn: &Connection, repo_id: &str, item_kind: &str, item_key: &str) {
        conn.execute(
            "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, \
             title, body, synced_at_ms, repo_id) VALUES ('github', 'o/r', ?1, ?2, 'u', 'open', \
             't', 'b', 1, ?3)",
            params![item_kind, item_key, repo_id],
        )
        .unwrap();
    }

    /// The core correctness bar: a 2-repo store reports each repo's counts in isolation — a
    /// sibling's rows never leak into the other's totals.
    #[test]
    fn two_repo_store_scopes_every_count_per_repo() {
        let path = temp_db();
        {
            let conn = IndexConnection::open(&path).unwrap();
            let c = conn.connection();
            schema::apply(c, &crate::index::migration_hooks()).unwrap();
            register(c, "repo-a", "/src/a");
            register(c, "repo-b", "/src/b");

            // Repo A: a base file + a worktree overlay, 3 chunks (one embedded), 2 symbols, 2
            // edges.
            let a_base = seed_file(c, "repo-a", "");
            let a_overlay = seed_file(c, "repo-a", "/wt/a");
            let a_chunk = seed_chunk(c, a_base);
            seed_chunk(c, a_base);
            seed_chunk(c, a_overlay);
            seed_symbol(c, a_base);
            seed_symbol(c, a_overlay);
            seed_edge(c, a_base);
            seed_edge(c, a_overlay);
            seed_embedding(c, a_chunk);
            seed_memory(c, "repo-a", "Invariant", "active");
            seed_memory(c, "repo-a", "Invariant", "obsolete");
            seed_memory(c, "repo-a", "Risk", "active");
            seed_memory(c, "repo-a", "Risk", "rejected");
            seed_item(c, "repo-a", "issue", "1");
            seed_item(c, "repo-a", "issue", "2");
            seed_item(c, "repo-a", "change_request", "3");
            c.execute(
                "INSERT INTO papertrail_comments(tracker, project, item_kind, item_key, \
                 comment_id, body, synced_at_ms, repo_id) VALUES \
                 ('github','o/r','issue','1','c1', 'b',1,'repo-a')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO papertrail_sync_cursor(tracker, project, backfill_done, repo_id) \
                 VALUES ('github','o/r',1,'repo-a')",
                [],
            )
            .unwrap();
            rag_rat_db::meta::set_repo_meta(c, "repo-a", "git_commit", "deadbeef").unwrap();
            rag_rat_db::meta::set_repo_meta(c, "repo-a", "git_dirty", "true").unwrap();

            // Repo B: a DIFFERENT, larger shape — the poison set that must never leak into A.
            let b_file = seed_file(c, "repo-b", "");
            for _ in 0..5 {
                let ch = seed_chunk(c, b_file);
                seed_embedding(c, ch);
                seed_symbol(c, b_file);
                seed_edge(c, b_file);
            }
            for _ in 0..4 {
                seed_memory(c, "repo-b", "Decision", "active");
            }
            seed_item(c, "repo-b", "issue", "10");
        }

        let status = IndexDatabase::global_status(&path).unwrap();
        assert_eq!(status.repo_count, 2);
        let a = status.repos.iter().find(|r| r.repo_id == "repo-a").expect("repo-a present");
        let b = status.repos.iter().find(|r| r.repo_id == "repo-b").expect("repo-b present");

        // Content: A's counts exclude B's rows entirely.
        assert_eq!(a.content.files, 2);
        assert_eq!(a.content.chunks, 3);
        assert_eq!(a.content.symbols, 2);
        assert_eq!(a.content.edges, 2);
        assert_eq!(a.content.embeddings, 1);
        assert_eq!(b.content.files, 1);
        assert_eq!(b.content.chunks, 5);
        assert_eq!(b.content.symbols, 5);
        assert_eq!(b.content.edges, 5);
        assert_eq!(b.content.embeddings, 5);

        // Worktree overlays: only the non-empty worktree_id, base excluded.
        assert_eq!(a.worktree_overlays.len(), 1);
        assert_eq!(a.worktree_overlays[0].worktree_id, "/wt/a");
        assert_eq!(a.worktree_overlays[0].file_count, 1);
        assert!(b.worktree_overlays.is_empty());

        // Memories: per-repo, split by status and kind. Every valid status (incl. `rejected`) lands
        // in a bucket, so the buckets reconcile with `total`.
        assert_eq!(a.memories.total, 4);
        assert_eq!(a.memories.active, 2);
        assert_eq!(a.memories.obsolete, 1);
        assert_eq!(a.memories.rejected, 1);
        assert_eq!(
            a.memories.active + a.memories.obsolete + a.memories.stale + a.memories.rejected,
            a.memories.total,
            "the status buckets must reconcile with total"
        );
        assert_eq!(a.memories.by_kind["Invariant"].active, 1);
        assert_eq!(a.memories.by_kind["Invariant"].obsolete, 1);
        assert_eq!(a.memories.by_kind["Risk"].active, 1);
        assert_eq!(a.memories.by_kind["Risk"].rejected, 1);
        assert!(
            !a.memories.by_kind.contains_key("Decision"),
            "repo-b's Decision must not leak into A"
        );
        assert_eq!(b.memories.total, 4);
        assert_eq!(b.memories.by_kind["Decision"].active, 4);

        // Papertrail: item kinds, comments, cursor binding — scoped.
        assert_eq!(a.papertrail.items_by_kind["issue"], 2);
        assert_eq!(a.papertrail.items_by_kind["change_request"], 1);
        assert_eq!(a.papertrail.comments, 1);
        assert_eq!(a.papertrail.cursors.len(), 1);
        assert_eq!(a.papertrail.cursors[0].project, "o/r");
        assert!(a.papertrail.cursors[0].backfill_done);
        assert_eq!(b.papertrail.items_by_kind["issue"], 1);
        assert!(b.papertrail.comments == 0);
        assert!(b.papertrail.cursors.is_empty());

        // Freshness from repo_meta.
        assert_eq!(a.freshness.indexed_head.as_deref(), Some("deadbeef"));
        assert_eq!(a.freshness.git_dirty, Some(true));
        assert_eq!(b.freshness.indexed_head, None);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The file-derived counts and worktree overlays scope to the repo's LIVE generation: rows in a
    /// dead/staged generation (here generation 1, while the live pointer is 0) are excluded.
    /// Without the generation filter every count and the dead overlay would leak in, so this
    /// test fails on a scoping regression.
    #[test]
    fn content_and_overlays_scope_to_the_live_generation() {
        let path = temp_db();
        {
            let conn = IndexConnection::open(&path).unwrap();
            let c = conn.connection();
            schema::apply(c, &crate::index::migration_hooks()).unwrap();
            register(c, "repo-c", "/src/c");
            // A freshly-registered repo (no rebuild) has live generation 0. LIVE rows at gen 0:
            let live = seed_file(c, "repo-c", "");
            seed_file(c, "repo-c", "/wt/live");
            let live_chunk = seed_chunk(c, live);
            seed_symbol(c, live);
            seed_edge(c, live);
            seed_embedding(c, live_chunk);
            // DEAD rows at a non-live generation (1) — a stale generation not yet GC'd. Every one
            // must be excluded from the counts, and the dead overlay must not be reported.
            let dead = seed_file_gen(c, "repo-c", "/wt/dead", 1);
            let dead_chunk = seed_chunk(c, dead);
            seed_symbol(c, dead);
            seed_edge(c, dead);
            seed_embedding(c, dead_chunk);
        }
        let status = IndexDatabase::global_status(&path).unwrap();
        let c = status.repos.iter().find(|r| r.repo_id == "repo-c").expect("repo-c present");
        // Only the live generation: base + live overlay = 2 files, and one of each derived row.
        assert_eq!(c.content.files, 2, "a dead-generation file leaked into the count");
        assert_eq!(c.content.chunks, 1);
        assert_eq!(c.content.symbols, 1);
        assert_eq!(c.content.edges, 1);
        assert_eq!(c.content.embeddings, 1);
        // The dead-generation overlay is excluded; only the live overlay is reported.
        assert_eq!(c.worktree_overlays.len(), 1, "a dead-generation overlay was reported");
        assert_eq!(c.worktree_overlays[0].worktree_id, "/wt/live");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// `kind='deleted'` overlay tombstones are excluded from `content.files` and the overlay list:
    /// a worktree that only deletes a base file has a marker row but no visible content, and the
    /// scoped reader hides it — `status` must match. Without the `kind != 'deleted'` filter this
    /// reports 3 files and a phantom tombstone-only overlay, so the test fails on a regression.
    #[test]
    fn tombstones_are_excluded_from_content_and_overlays() {
        let path = temp_db();
        {
            let conn = IndexConnection::open(&path).unwrap();
            let c = conn.connection();
            schema::apply(c, &crate::index::migration_hooks()).unwrap();
            register(c, "repo-d", "/src/d");
            seed_file(c, "repo-d", ""); // live base source file
            seed_file(c, "repo-d", "/wt/real"); // an overlay with a real (visible) file
            seed_tombstone(c, "repo-d", "/wt/tomb", 0); // an overlay that ONLY deletes a base file
        }
        let status = IndexDatabase::global_status(&path).unwrap();
        let d = status.repos.iter().find(|r| r.repo_id == "repo-d").expect("repo-d present");
        // The tombstone is not visible content: 2 files, not 3.
        assert_eq!(d.content.files, 2, "a kind='deleted' tombstone leaked into content.files");
        // Only the overlay with a visible file is reported; the tombstone-only overlay is excluded.
        assert_eq!(d.worktree_overlays.len(), 1, "a tombstone-only overlay was reported");
        assert_eq!(d.worktree_overlays[0].worktree_id, "/wt/real");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// An ABSENT store reports zero repos without creating the file.
    #[test]
    fn absent_store_reports_no_repos_without_creating_it() {
        let path = temp_db();
        let status = IndexDatabase::global_status(&path).unwrap();
        assert!(!status.exists);
        assert_eq!(status.repo_count, 0);
        assert!(status.repos.is_empty());
        assert!(status.schema.is_none());
        assert!(status.file_health.is_none());
        assert!(!path.exists(), "reporting an absent store must not create it");
    }

    /// A schema-initialized store with NO registered repo reports an empty roster gracefully (with
    /// the file-health + FTS rollup still populated).
    #[test]
    fn empty_registry_reports_zero_repos() {
        let path = temp_db();
        {
            let conn = IndexConnection::open(&path).unwrap();
            schema::apply(conn.connection(), &crate::index::migration_hooks()).unwrap();
        }
        let status = IndexDatabase::global_status(&path).unwrap();
        assert!(status.exists);
        assert_eq!(status.repo_count, 0);
        assert!(status.repos.is_empty());
        assert!(status.file_health.is_some(), "the global rollup is still reported");
        assert!(status.fts.is_some());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The `--json` surface is stable: the top-level and per-repo field names the CLI serializes
    /// are present and shaped as expected.
    #[test]
    fn json_shape_is_stable() {
        let path = temp_db();
        {
            let conn = IndexConnection::open(&path).unwrap();
            let c = conn.connection();
            schema::apply(c, &crate::index::migration_hooks()).unwrap();
            register(c, "repo-a", "/src/a");
            seed_file(c, "repo-a", "");
        }
        let status = IndexDatabase::global_status(&path).unwrap();
        let value = serde_json::to_value(&status).unwrap();
        for key in [
            "database",
            "exists",
            "size_bytes",
            "schema",
            "file_health",
            "fts",
            "repo_count",
            "repos",
        ] {
            assert!(value.get(key).is_some(), "top-level field `{key}` missing from status JSON");
        }
        let repo = &value["repos"][0];
        for key in [
            "repo_id",
            "display_name",
            "roots",
            "registered_at_ms",
            "freshness",
            "worktree_overlays",
            "memories",
            "papertrail",
            "content",
        ] {
            assert!(repo.get(key).is_some(), "per-repo field `{key}` missing from status JSON");
        }
        assert_eq!(repo["content"]["files"], 1);
        assert_eq!(repo["repo_id"], "repo-a");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
