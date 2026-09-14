//! The poison sibling's tripwire predicates and the intact check that reads them.

use rusqlite::Connection;

use super::seed::primary_is_real;
use super::*;

/// Each seeded tripwire as `(table, full-sentinel WHERE predicate)`. Asserting each still matches
/// EXACTLY one row is a row-count check (catches an unscoped DELETE) and a value checksum (the
/// predicate pins every seeded column, so an in-place UPDATE stops matching) in one. The transitive
/// children are matched through the poison file / logical symbol, exactly how a scoped reader would
/// have to reach them.
fn sibling_tripwires(conn: &Connection) -> anyhow::Result<Vec<(&'static str, String)>> {
    let file_scope =
        format!("file_id IN (SELECT id FROM main.files WHERE repo_id = '{POISON_REPO_ID}')");
    let mut tripwires = vec![
        ("git_commits", format!("repo_id = '{POISON_REPO_ID}' AND hash = '{POISON_COMMIT}'")),
        // Pinned to the distinct-path row's path so the same-path `git_file_changes` tripwire
        // (a second row under `POISON_REPO_ID`) doesn't inflate this count.
        (
            "git_file_changes",
            format!("repo_id = '{POISON_REPO_ID}' AND path = '{POISON_PREFIX}change.rs'"),
        ),
        ("main.files", format!("repo_id = '{POISON_REPO_ID}' AND path = '{POISON_PREFIX}file.rs'")),
        ("packages", format!("repo_id = '{POISON_REPO_ID}'")),
        // Pinned to the distinct-path row's path so the same-path `parser_failures` tripwire
        // doesn't inflate this count.
        (
            "parser_failures",
            format!("repo_id = '{POISON_REPO_ID}' AND path = '{POISON_PREFIX}fail.rs'"),
        ),
        ("docs", format!("repo_id = '{POISON_REPO_ID}'")),
        ("logical_symbols", format!("id = {POISON_LOGICAL_ID} AND repo_id = '{POISON_REPO_ID}'")),
        ("symbols", file_scope.clone()),
        ("chunks", file_scope.clone()),
        (
            "edges_data",
            format!(
                "source_file_id IN (SELECT id FROM main.files WHERE repo_id = '{POISON_REPO_ID}')"
            ),
        ),
        ("logical_symbol_members", format!("logical_symbol_id = {POISON_LOGICAL_ID}")),
        (
            "logical_symbol_monikers",
            format!(
                "logical_symbol_id = {POISON_LOGICAL_ID} AND repo_id = '{POISON_REPO_ID}' AND \
                 moniker = '{POISON_PREFIX}moniker'"
            ),
        ),
        // papertrail (V060): each base table + the fts mirror pinned by the sentinel item key.
        (
            "papertrail_closing_edges",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND issue_key = '{POISON_ITEM_KEY}' AND closer_key \
                 = '{POISON_PREFIX}sha'"
            ),
        ),
        (
            "papertrail_refs",
            format!("repo_id = '{POISON_REPO_ID}' AND item_key = '{POISON_ITEM_KEY}'"),
        ),
        (
            "papertrail_items",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND item_kind = 'issue' AND item_key = \
                 '{POISON_ITEM_KEY}' AND body = '{POISON_PREFIX}body'"
            ),
        ),
        (
            "papertrail_items",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND item_kind = 'change_request' AND item_key = \
                 '{POISON_ITEM_KEY}' AND body = '{POISON_PREFIX}prbody'"
            ),
        ),
        (
            "papertrail_comments",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND comment_id = '{POISON_PREFIX}comment_id_1' AND \
                 body = '{POISON_PREFIX}comment' AND review_state IS NULL AND anchor_path IS NULL"
            ),
        ),
        (
            "papertrail_comments",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND comment_id = '{POISON_PREFIX}comment_id_2' AND \
                 body = '{POISON_PREFIX}review' AND review_state = 'commented'"
            ),
        ),
        (
            "papertrail_comments",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND comment_id = '{POISON_PREFIX}comment_id_3' AND \
                 body = '{POISON_PREFIX}revcomment' AND anchor_path = '{POISON_PREFIX}anchored.rs'"
            ),
        ),
        (
            "papertrail_sync_cursor",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND project = '{POISON_PROJECT}' AND high_mark_at = \
                 '{POISON_PREFIX}mark'"
            ),
        ),
        (
            "papertrail_item_tags",
            format!("repo_id = '{POISON_REPO_ID}' AND tag = '{POISON_PREFIX}itemtag'"),
        ),
        // papertrail_fts: one derived mirror row per base row above, pinned by doc_kind +
        // item_kind/comment_id + body so a full mirror rebuild's re-derivation must reconverge
        // onto exactly this set (`classification` is derivation-owned and deliberately unpinned).
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'item' AND item_kind = 'issue' AND \
                 item_key = '{POISON_ITEM_KEY}' AND body = '{POISON_PREFIX}body'"
            ),
        ),
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'item' AND item_kind = \
                 'change_request' AND item_key = '{POISON_ITEM_KEY}' AND body = \
                 '{POISON_PREFIX}prbody'"
            ),
        ),
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'comment' AND comment_id = \
                 '{POISON_PREFIX}comment_id_1' AND body = '{POISON_PREFIX}comment' AND url = \
                 'http://x' AND title = ''"
            ),
        ),
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'comment' AND comment_id = \
                 '{POISON_PREFIX}comment_id_2' AND body = '{POISON_PREFIX}review' AND url = '' \
                 AND title = ''"
            ),
        ),
        (
            "papertrail_fts",
            format!(
                "repo_id = '{POISON_REPO_ID}' AND doc_kind = 'comment' AND comment_id = \
                 '{POISON_PREFIX}comment_id_3' AND body = '{POISON_PREFIX}revcomment' AND title = \
                 '{POISON_PREFIX}anchored.rs'"
            ),
        ),
        // SAME-PATH tripwires (V041): pinned by a path-INDEPENDENT sentinel column (the collision
        // path is fixture-dependent), so the intact check holds whichever primary path was chosen.
        (
            "main.files",
            format!("repo_id = '{POISON_REPO_ID}' AND sha256 = '{POISON_SAMEPATH_SHA}'"),
        ),
        (
            "git_file_changes",
            format!("repo_id = '{POISON_REPO_ID}' AND additions = {POISON_SAMEPATH_ADDITIONS}"),
        ),
        (
            "parser_failures",
            format!("repo_id = '{POISON_REPO_ID}' AND message = '{POISON_SAMEPATH_MSG}'"),
        ),
        (
            "papertrail_refs",
            format!("repo_id = '{POISON_REPO_ID}' AND item_key = '{POISON_SAMEPATH_ITEM_KEY}'"),
        ),
        // A5 periphery (V042): each directly-scoped table pinned by the sibling repo_id (plus its
        // sentinel key where a table's row is otherwise ambiguous). `repo_memory_tags` scopes
        // transitively through the poison memory.
        ("repo_memories", format!("repo_id = '{POISON_REPO_ID}' AND id = '{POISON_MEMORY_ID}'")),
        // Pinned to the distinct-path binding's `binding_id` so the same-path binding tripwire
        // (a SECOND binding under the same memory) doesn't inflate this count.
        (
            "repo_memory_bindings",
            format!("repo_id = '{POISON_REPO_ID}' AND binding_id = '{POISON_PREFIX}bind'"),
        ),
        ("repo_memory_tags", format!("memory_id = '{POISON_MEMORY_ID}'")),
        (
            "repo_memory_fts",
            format!("repo_id = '{POISON_REPO_ID}' AND memory_id = '{POISON_MEMORY_ID}'"),
        ),
        (
            "repo_node_edges",
            format!("repo_id = '{POISON_REPO_ID}' AND edge_key = '{POISON_PREFIX}edge_key'"),
        ),
        ("oracle_runs", format!("repo_id = '{POISON_REPO_ID}'")),
        // Pinned to the distinct-path edge's `scip_symbol` so the same-path edge tripwire doesn't
        // inflate this count.
        (
            "edge_oracle",
            format!("repo_id = '{POISON_REPO_ID}' AND scip_symbol = '{POISON_PREFIX}scip'"),
        ),
        (
            "clone_graph_generations",
            format!("repo_id = '{POISON_REPO_ID}' AND generation = {POISON_GENERATION}"),
        ),
        ("clone_token_df", format!("repo_id = '{POISON_REPO_ID}'")),
        ("clone_refinements", format!("repo_id = '{POISON_REPO_ID}'")),
        ("dream_findings", format!("repo_id = '{POISON_REPO_ID}'")),
        ("reconcile_attempts", format!("repo_id = '{POISON_REPO_ID}'")),
        // Dream v2 verification siblings: each pinned by the sibling repo_id + poison memory.
        (
            "memory_reality",
            format!("repo_id = '{POISON_REPO_ID}' AND memory_id = '{POISON_MEMORY_ID}'"),
        ),
        (
            "memory_summaries",
            format!("repo_id = '{POISON_REPO_ID}' AND memory_id = '{POISON_MEMORY_ID}'"),
        ),
        (
            "memory_note_summaries",
            format!("repo_id = '{POISON_REPO_ID}' AND memory_id = '{POISON_MEMORY_ID}'"),
        ),
        (
            "memory_model_failures",
            format!("repo_id = '{POISON_REPO_ID}' AND memory_id = '{POISON_MEMORY_ID}'"),
        ),
        // SAME-PATH tripwires (V042): the memory binding and oracle edge whose path (and, for the
        // oracle, path+sha) collide with a real primary row, pinned by their own sentinel keys.
        (
            "repo_memory_bindings",
            format!("repo_id = '{POISON_REPO_ID}' AND binding_id = '{POISON_SAMEPATH_BIND}'"),
        ),
        (
            "edge_oracle",
            format!("repo_id = '{POISON_REPO_ID}' AND scip_symbol = '{POISON_SAMEPATH_SCIP}'"),
        ),
    ];
    // Registry tripwires (A7): present ONLY when the sibling is a REAL registered repo (a git
    // fixture — see `primary_is_real`, which gates the matching seed). Each pins the sibling's own
    // registry row, so an unscoped `repos` / `repo_roots` / `repo_meta` read/count/delete trips.
    if primary_is_real(conn)? {
        tripwires.push(("repos", format!("repo_id = '{POISON_REPO_ID}'")));
        tripwires.push((
            "repo_roots",
            format!("repo_id = '{POISON_REPO_ID}' AND root = '{POISON_REPO_ROOT}'"),
        ));
        tripwires.push((
            "repo_meta",
            format!("repo_id = '{POISON_REPO_ID}' AND key = '{POISON_META_KEY}'"),
        ));
    }
    Ok(tripwires)
}

/// Post-condition for a MUTATING test: assert the poison sibling survived intact — every seeded
/// tripwire still present and unmodified. Any unscoped DELETE / UPDATE in the operation under test
/// (a GC that wipes a sibling's rows, an oracle clear that drops a sibling's monikers, an
/// incremental pass that stamps a sibling's file) trips this. Reads `main.*` directly (bypassing
/// the scope view), because the active connection is scoped to the fixture repo and would never see
/// the sibling through the view. Call it on the fixture's connection at the end of the mutating
/// step.
pub(crate) fn assert_sibling_intact(conn: &Connection) {
    for (table, predicate) in sibling_tripwires(conn).expect("read the sibling tripwire set") {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"), [], |row| {
                row.get(0)
            })
            .unwrap_or_else(|err| panic!("poison-sibling probe failed on {table}: {err}"));
        assert_eq!(
            count, 1,
            "poison sibling leaked/mutated in `{table}` (WHERE {predicate}): expected exactly 1 \
             row, found {count}. An unscoped read/count/delete in the operation under test \
             touched a sibling repo's rows — scope it by repo_id.",
        );
    }
}
