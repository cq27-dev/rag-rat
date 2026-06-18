use rusqlite::Connection;

use crate::index::AnchorHealth;
use crate::query::tree::DirTree;

/// How many active non-dir memory titles are fetched and rendered in the digest.
/// Single source of truth for the display cap — the `(+N more)` overflow note is
/// computed against [`Orientation::active_memory_total`], not this cap.
const MEMORY_TITLES_SHOWN: usize = 3;

/// Flat, owned session-start digest — composed read-only from the indexed database.
///
/// Invariant: `orientation` never writes to the database.  It installs the per-connection
/// scope view (a TEMP table + TEMP VIEW) and then issues only SELECT queries.
#[derive(Debug)]
pub struct Orientation {
    /// Annotated directory tree (scoped to the active commit/worktree context).
    pub tree: DirTree,
    /// Top load-bearing files by fan_in, capped at 5: (path, fan_in).
    pub load_bearing: Vec<(String, u64)>,
    /// Subjects of the most recent indexed commits, newest first, capped at 5.
    pub recent_commits: Vec<String>,
    /// Paths of source files with the most recent changes, capped at 3.
    /// Empty when git history is not indexed.
    pub hot_files: Vec<String>,
    /// Titles of active repo memories that are NOT bound to a directory (i.e. not already
    /// annotated in the tree), newest first, capped at `MEMORY_TITLES_SHOWN` (3).
    pub active_memory_titles: Vec<String>,
    /// Total count of active non-dir memories — used for the `(+N more)` overflow note;
    /// may exceed `active_memory_titles.len()`.
    pub active_memory_total: u32,
    /// HEAD commit hash reported by git (live).
    pub head: String,
    /// HEAD commit hash recorded in the git-history index.
    pub indexed_head: String,
    /// Persisted anchor_status counts over active memory bindings (read-only).
    pub anchor: AnchorHealth,
    /// Non-generated indexed file count in the active scope.
    pub total_files: u32,
    /// Parser failure count (shared across all scopes — keyed by path, not commit).
    pub parser_failures: u64,
}

/// Compose a read-only session-start orientation digest from `conn`.
///
/// The caller supplies a connection (typically opened read-only); this function installs
/// the per-connection scope view so all subsequent queries reflect the active git context.
///
/// Invariant: purely READ — all writes go to TEMP objects (the scope view) which are
/// discarded when the connection closes.
pub fn orientation(
    conn: &Connection,
    config_root: &std::path::Path,
    cwd: &std::path::Path,
) -> anyhow::Result<Orientation> {
    // Step 1: install the WORKTREE-AWARE scoping view. `config_root` (the main worktree, where the
    // shared base index lives) anchors the base commit; `cwd` is the session's working dir — when
    // it is a linked worktree, orientation reflects that branch's overlay on the base,
    // otherwise the base scope (#219). Using the worktree's own HEAD as the base (the old
    // `resolve_git_context(cwd)`) was wrong: the index has no committed scope at a linked
    // worktree's HEAD, so orientation saw only the bare overlay delta.
    crate::index::install_worktree_scope_view(conn, config_root, cwd)?;

    // Step 2: directory tree (reads scoped `files` view).
    let tree = crate::query::tree::dir_tree(conn, &Default::default())?;

    // Step 3: load-bearing files — Spine mode, top 5 by fan_in.
    let load_bearing = spine_load_bearing(conn, 5)?;

    // Step 4: recent commit subjects + hot files from indexed git history.
    let recent_commits = recent_commit_subjects(conn, 5)?;
    let hot_files = recently_changed_source_files(conn, 3)?;

    // Step 5: active non-dir memory titles, newest first, capped at the display cap;
    // plus the true total count for the overflow note.
    let active_memory_titles = active_non_dir_memory_titles(conn, MEMORY_TITLES_SHOWN)?;
    let active_memory_total = active_non_dir_memory_count(conn)?;

    // Step 6: git-history index status (live HEAD + indexed HEAD).
    // The git working-tree status reflects the SESSION's checkout (the worktree's own dirty state),
    // so read it from `cwd`, not the base `config_root`.
    let git_status = crate::index::git_history::status(conn, cwd)?;
    let head = git_status.head.unwrap_or_default();
    let indexed_head = git_status.indexed_head.unwrap_or_default();

    // Step 7: anchor health (read-only, no validation pass).
    let anchor = crate::query::memory::anchor_health_counts(conn)?;

    // Step 8: scoped non-generated file count + parser failures.
    let total_files = scoped_file_count(conn)?;
    let parser_failures = crate::query::impact::parser_failure_count(conn)?;

    Ok(Orientation {
        tree,
        load_bearing,
        recent_commits,
        hot_files,
        active_memory_titles,
        active_memory_total,
        head,
        indexed_head,
        anchor,
        total_files,
        parser_failures,
    })
}

/// READ: top `limit` files by fan_in (graph in-degree) from the scoped `files` view.
///
/// Invariant: reads `files` (scoped TEMP VIEW) joined to `edges` / `symbols` — no writes.
/// Returns (path, fan_in) pairs ordered by fan_in DESC, path ASC.
fn spine_load_bearing(conn: &Connection, limit: usize) -> anyhow::Result<Vec<(String, u64)>> {
    let mut stmt = conn.prepare(
        "-- Top files by incoming graph edges (fan_in) within the active scope.
         -- Invariant: files resolves to the scoped TEMP VIEW; only non-generated source files.
         SELECT files.path,
                COUNT(*) AS fan_in
         FROM edges
         JOIN symbols AS target_symbols ON target_symbols.id = edges.to_symbol_id
         JOIN files ON files.id = target_symbols.file_id
         WHERE files.generated = 0
         GROUP BY files.path
         ORDER BY fan_in DESC, files.path ASC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit as i64], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (path, fan_in) = row?;
        out.push((path, u64::try_from(fan_in.max(0)).unwrap_or(0)));
    }
    Ok(out)
}

/// READ: subjects of the most recent indexed commits, newest first.
///
/// Invariant: reads `git_commits` — no writes.  Returns an empty Vec when no commits are
/// indexed (git_commits is empty).
fn recent_commit_subjects(conn: &Connection, limit: usize) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "-- Most recent indexed commit subjects, newest first.
         -- Invariant: git_commits rows are global (not scoped); subjects only.
         SELECT subject
         FROM git_commits
         ORDER BY authored_at_s DESC, committed_at_s DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// READ: paths of non-generated source files most recently changed, newest first.
///
/// Invariant: reads `git_file_changes` joined to `git_commits` — no writes.
/// Returns an empty Vec when git history is not indexed.
/// Only returns paths of files that are currently indexed (inner join to scoped `files`).
fn recently_changed_source_files(conn: &Connection, limit: usize) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "-- Most recently changed source paths that are currently indexed in the active scope.
         -- Invariant: files resolves to the scoped TEMP VIEW; git_file_changes is global.
         SELECT DISTINCT gfc.path
         FROM git_file_changes AS gfc
         JOIN git_commits AS gc ON gc.hash = gfc.commit_hash
         JOIN files ON files.path = gfc.path
         WHERE files.generated = 0
         ORDER BY gc.authored_at_s DESC, gfc.path ASC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// READ: titles of active repo memories NOT bound to a directory (`binding_kind != 'dir'`),
/// newest first, capped at `limit`.
///
/// Invariant: purely READ — no writes.
/// Dir-bound memories already appear as annotations on the tree nodes (or as
/// `root_memory_title`), so we exclude them here to avoid duplication.
fn active_non_dir_memory_titles(conn: &Connection, limit: usize) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "-- Active memories not bound to a directory, newest-updated first.
         -- Invariant: excludes binding_kind='dir' rows so tree-shown titles are not repeated.
         SELECT m.title
         FROM repo_memories AS m
         WHERE m.status = 'active'
           AND m.id NOT IN (
               SELECT b.memory_id
               FROM repo_memory_bindings AS b
               WHERE b.binding_kind = 'dir'
           )
         ORDER BY m.updated_at_ms DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// READ: total count of active repo memories NOT bound to a directory.
///
/// Invariant: purely READ — no writes.  Uses the SAME predicate as
/// [`active_non_dir_memory_titles`] (active + excludes `binding_kind='dir'`) so the
/// `(+N more)` overflow note reflects the true total, not the truncated title list.
fn active_non_dir_memory_count(conn: &Connection) -> anyhow::Result<u32> {
    let count: i64 = conn.query_row(
        "-- Total active memories not bound to a directory (mirrors the titles query).
         -- Invariant: excludes binding_kind='dir' rows so the count matches what the
         -- titles list draws from; only the LIMIT/ORDER differ.
         SELECT COUNT(*)
         FROM repo_memories AS m
         WHERE m.status = 'active'
           AND m.id NOT IN (
               SELECT b.memory_id
               FROM repo_memory_bindings AS b
               WHERE b.binding_kind = 'dir'
           )",
        [],
        |row| row.get(0),
    )?;
    Ok(u32::try_from(count.max(0)).unwrap_or(u32::MAX))
}

/// READ: count of non-generated files in the active scope.
///
/// Invariant: reads the scoped `files` TEMP VIEW — no writes.
fn scoped_file_count(conn: &Connection) -> anyhow::Result<u32> {
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM files WHERE generated = 0", [], |row| row.get(0))?;
    Ok(u32::try_from(count.max(0)).unwrap_or(u32::MAX))
}
