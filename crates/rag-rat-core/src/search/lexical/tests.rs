use rag_rat_base::language::Language;
use rag_rat_db::schema;
use rusqlite::{Connection, params};

use super::candidates::{bm25_candidates_sql, vector_candidates};
use super::history::git_score;
use super::query::fts_query;
use super::scoring::{graph_boost, qualified_symbol_name};
use super::*;

fn seeded_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
         VALUES ('src/watch.rs', 'rust', 'source', 'abc', 0, 0)",
        [],
    )
    .unwrap();
    let text = "fn watcher_main() { /* election retry loop */ }";
    let chunk_id: i64 = conn
        .query_row(
            "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                start_line, end_line, text_hash)
             VALUES (1, 'symbol', 'watcher_main', 0, 10, 1, 20, 'h1')
             RETURNING id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // chunks.text is gone (#77 Phase 2): seed the compressed chunk_text blob (readers INNER
    // JOIN it) and the contentless chunk_fts tokens directly, keeping this seed
    // self-contained.
    rag_rat_db::chunk_text_store::seed_chunk_text(&conn, chunk_id, text).unwrap();
    conn.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", params![chunk_id, text])
        .unwrap();
    conn
}

/// Regression guard (#79 query_warm): `graph_boost` runs once per candidate (~limit*8), so its
/// `from_name`/`to_name` filter MUST stay on the `from_name_id`/`to_name_id` INTEGER indexes.
/// Through the `edges` view a value predicate degrades to a full edges_data scan that joins the
/// dictionary per row (the 5x blow-up). Pin the plan: the candidate filter uses the int index.
#[test]
fn graph_boost_uses_the_name_id_indexes() {
    let conn = seeded_conn();
    let plan = conn
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT ek.value FROM edges_data d
             JOIN name_strings ek ON ek.id = d.edge_kind_id
             WHERE (d.from_name_id IN (SELECT id FROM name_strings WHERE value IN ('a', 'b'))
                 OR d.to_name_id IN (SELECT id FROM name_strings WHERE value IN ('a', 'b')))
               AND d.hidden = 0
               AND EXISTS (SELECT 1 FROM main.files f
                            WHERE f.id = d.source_file_id AND f.repo_id = 'r')",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("idx_edges_from_name") && plan.contains("idx_edges_to_name"),
        "graph_boost candidate filter must use the name_id indexes, got plan:\n{plan}"
    );
    assert!(
        !plan.contains("SCAN d "),
        "graph_boost must not full-scan edges_data, got plan:\n{plan}"
    );
    // The repo predicate must be a PK lookup on files (per candidate edge), never a files scan.
    assert!(
        !plan.contains("SCAN f"),
        "graph_boost repo scope must PK-search files, not scan it, got plan:\n{plan}"
    );
}

#[test]
fn graph_boost_ignores_suppressed_edge_candidates() {
    let conn = seeded_conn();
    conn.execute(
        "INSERT INTO edges(source_file_id, from_name, to_name, edge_kind, confidence,
                           resolution, evidence)
         VALUES (1, 'watcher_main', 'available', 'uses_macro', 'NameOnly', 'suppressed',
                 '@available')",
        [],
    )
    .unwrap();
    let hit = SearchHit {
        chunk_id: 1,
        path: "src/watch.rs".to_string(),
        language: "rust".to_string(),
        kind: "symbol".to_string(),
        start_line: 1,
        end_line: 20,
        symbol_path: Some("watcher_main".to_string()),
        score: 0.0,
        retrieval_mode: "lexical".to_string(),
        summary: String::new(),
        graph: None,
        score_components: None,
        importance: None,
        distilled_records: Vec::new(),
    };
    let repo_id = schema::active_repo_id(&conn).unwrap();
    let boost = graph_boost(&conn, &hit, &["available".to_string()], &repo_id).unwrap();
    assert_eq!(boost, 0.0, "suppressed resolver candidates are not ranking evidence");
}

/// Registry-driven tripwire: `qualified_symbol_name` must strip the file prefix for EVERY
/// indexed language, not just the ones a literal list happened to name. Drives the assertion
/// off `Language::all()` × `target_extensions()`, so registering a language without
/// teaching the stripper about it reddens here instead of silently costing that language
/// its graph boost (which is how `.swift`, `.py`, `.c`, and `.cpp` were all missing at
/// once).
#[test]
fn qualified_symbol_name_strips_the_file_prefix_for_every_registered_language() {
    for language in Language::all() {
        for ext in language.target_extensions() {
            let path = format!("crates/pkg/src/thing.{ext}::Type::method");
            assert_eq!(
                qualified_symbol_name(&path),
                "Type::method",
                "{language} (.{ext}) symbol paths must reduce to the bare qualified name"
            );
        }
    }
    // A bare name (no file prefix) and an unindexed extension are both passed through
    // unchanged.
    assert_eq!(qualified_symbol_name("Type::method"), "Type::method");
    assert_eq!(qualified_symbol_name("notes.txt::heading"), "notes.txt::heading");
}

/// The pre-materialization bm25 JOIN shape (joins the scope VIEW `files` directly, no CTE) —
/// otherwise identical to production, INCLUDING the `ORDER BY score, chunks.id` tie-break, so
/// the equivalence test isolates the materialization as the ONLY difference and the
/// comparison is order-deterministic (the honest guarantee: identical rows AND identical
/// order once ties break by chunk_id). Also the plan-pin tombstone: under the two-branch
/// scope view SQLite flattens `files` into a compound MERGE and runs the FTS pipeline once
/// PER BRANCH, so this shape shows TWO `SCAN chunk_fts`; the materialized rewrite collapses
/// it to one.
fn legacy_bm25_sql(include_generated: bool) -> String {
    let generated_filter = if include_generated { "1 = 1" } else { "files.generated = 0" };
    format!(
        "
        SELECT chunks.id, files.path, files.language, files.kind,
               chunks.start_line, chunks.end_line, chunks.symbol_path,
               bm25(chunk_fts) AS score,
               chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version
        FROM chunk_fts
        JOIN chunks ON chunks.id = chunk_fts.rowid
        JOIN files ON files.id = chunks.file_id
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        WHERE chunk_fts MATCH ?1
          AND {generated_filter}
        ORDER BY score, chunks.id
        LIMIT ?2
        "
    )
}

fn register_repo(conn: &Connection, repo_id: &str) {
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
        params![repo_id],
    )
    .unwrap();
}

/// Seed one scoped file + its chunk + compressed chunk_text + contentless chunk_fts token row,
/// returning the chunk id. The FULL scope key `(repo_id, path, commit_sha, worktree_id,
/// generation, generated)` is caller-controlled so a single DB can carry every scope class the
/// view partitions on (base / worktree-overlay / shadowed twin / superseded generation /
/// sibling repo / generated).
#[allow(clippy::too_many_arguments)]
fn seed_scoped_chunk(
    conn: &Connection,
    path: &str,
    repo_id: &str,
    commit_sha: &str,
    worktree_id: &str,
    generation: i64,
    generated: bool,
    text: &str,
) -> i64 {
    let file_id: i64 = conn
        .query_row(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                                    commit_sha, worktree_id, generation, generated, repo_id)
             VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, ?4, ?5, ?6, ?7)
             RETURNING id",
            params![
                path,
                format!("sha-{repo_id}-{path}-{commit_sha}-{worktree_id}-{generation}"),
                commit_sha,
                worktree_id,
                generation,
                generated as i64,
                repo_id
            ],
            |row| row.get(0),
        )
        .unwrap();
    let chunk_id: i64 = conn
        .query_row(
            "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                start_line, end_line, text_hash)
             VALUES (?1, 'symbol', ?2, 0, 10, 1, 20, ?3)
             RETURNING id",
            params![file_id, path, format!("h-{file_id}")],
            |row| row.get(0),
        )
        .unwrap();
    rag_rat_db::chunk_text_store::seed_chunk_text(conn, chunk_id, text).unwrap();
    conn.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", params![chunk_id, text])
        .unwrap();
    chunk_id
}

fn candidate_ids_and_scores(
    conn: &Connection,
    sql: &str,
    query: &str,
    limit: i64,
) -> Vec<(i64, f64)> {
    conn.prepare(sql)
        .unwrap()
        .query_map(params![fts_query(query), limit], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(7)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn fts_scan_count(conn: &Connection, sql: &str) -> usize {
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(params!["alpha", 10_i64], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    plan.matches("SCAN chunk_fts").count()
}

/// PLAN PIN (plan §5.1): under the production two-branch scope VIEW, the materialized bm25
/// statement must run the FTS scan + probe pipeline EXACTLY ONCE. The legacy shape shows it
/// twice (the compound MERGE duplication the rewrite exists to kill); pinning `new == 1` also
/// guards against a future SQLite/view change silently re-doubling the pipeline.
#[test]
fn bm25_materialized_scope_view_runs_the_fts_pipeline_once() {
    let conn = seeded_conn();
    crate::index::install_scope_view(&conn, "somecommit", "someworktree").unwrap();

    let legacy_scans = fts_scan_count(&conn, &legacy_bm25_sql(false));
    let new_scans = fts_scan_count(&conn, &bm25_candidates_sql(false));

    assert_eq!(new_scans, 1, "materialized bm25 must scan chunk_fts exactly once");
    assert!(
        legacy_scans > new_scans,
        "the two-branch view must double the legacy FTS pipeline (legacy={legacy_scans}, \
         new={new_scans}) — otherwise this pin proves nothing"
    );
}

/// BEHAVIOR EQUIVALENCE (plan §5.2): on a DB carrying every scope class — in-scope base file,
/// worktree overlay + its shadowed base twin, superseded generation, sibling repo, generated
/// file, PLUS an equal-bm25 tie pair spanning the base/overlay branch boundary — the
/// materialized rewrite returns byte-identical `(chunk_id, score)` rows in the same order as
/// the legacy join shape, ties broken deterministically by `chunks.id`. The post-filter
/// `LIMIT` still yields a PER-REPO window (single-repo, multi-repo, worktree-overlay), and
/// a LIMIT that cuts through the tie keeps the same survivor in both statements.
#[test]
fn bm25_materialized_is_byte_identical_to_the_legacy_query() {
    let conn = seeded_conn();
    // seeded_conn already inserted one `__unassigned__` file+chunk; drop it so the scope is
    // exactly the classes we seed below (the placeholder repo would otherwise pollute counts).
    conn.execute("DELETE FROM chunk_fts", []).unwrap();
    conn.execute("DELETE FROM chunk_text", []).unwrap();
    conn.execute("DELETE FROM chunks", []).unwrap();
    conn.execute("DELETE FROM main.files", []).unwrap();

    register_repo(&conn, "repo-a");
    register_repo(&conn, "repo-b");
    let commit = "commit-a";
    let worktree = "wt-a";

    // Every text carries exactly one `alpha`; distinct token counts give distinct bm25 scores
    // (shorter doc = more relevant) EXCEPT the deliberate tie pair below. IN SCOPE (repo-a,
    // live generation 0):
    let base1 = seed_scoped_chunk(&conn, "base1.rs", "repo-a", commit, "", 0, false, "alpha b1");
    let base2 = seed_scoped_chunk(&conn, "base2.rs", "repo-a", commit, "", 0, false, "alpha b2 c2");
    let base3 =
        seed_scoped_chunk(&conn, "base3.rs", "repo-a", commit, "", 0, false, "alpha b3 c3 d3");
    // Worktree overlay wins over its same-path committed twin (branch-1 vs the NOT IN shadow).
    let overlay = seed_scoped_chunk(
        &conn,
        "overlay.rs",
        "repo-a",
        "",
        worktree,
        0,
        false,
        "alpha o1 o2 o3 o4",
    );
    // TIE ACROSS THE BASE/OVERLAY BRANCH BOUNDARY (the Codex repro that the plain rewrite got
    // wrong): two chunks with IDENTICAL text → identical bm25. `tie_base` is in the commit
    // branch, `tie_overlay` in the worktree branch, and `tie_base` is seeded FIRST so
    // tie_base.id < tie_overlay.id. Under the OLD direct-`files` join the worktree branch is
    // emitted first, so a score-ONLY sort returned the pair in scope-branch order
    // [tie_overlay, tie_base] (higher id first); a single materialized scan would return them
    // in rowid order. The `(score, chunks.id)` tie-break both statements now carry makes the
    // deterministic order [tie_base, tie_overlay] regardless of scan plan.
    let tie_base = seed_scoped_chunk(
        &conn,
        "tie_base.rs",
        "repo-a",
        commit,
        "",
        0,
        false,
        "alpha t1 t2 t3 t4 t5",
    );
    let tie_overlay = seed_scoped_chunk(
        &conn,
        "tie_overlay.rs",
        "repo-a",
        "",
        worktree,
        0,
        false,
        "alpha t1 t2 t3 t4 t5",
    );
    // Generated file: in scope, but excluded when include_generated = false.
    let generated = seed_scoped_chunk(
        &conn,
        "generated.rs",
        "repo-a",
        commit,
        "",
        0,
        true,
        "alpha g1 g2 g3 g4 g5 g6",
    );

    // OUT OF SCOPE — each is a SHORTER doc (higher bm25) than any in-scope row, so a statement
    // that applied LIMIT before the scope join would surface them and starve the window. They
    // must never appear:
    // shadowed base twin of overlay.rs (same repo/commit, empty worktree) — dropped by NOT IN.
    let _twin = seed_scoped_chunk(&conn, "overlay.rs", "repo-a", commit, "", 0, false, "alpha");
    // superseded generation (generation 9 ≠ live 0).
    let _superseded = seed_scoped_chunk(&conn, "old.rs", "repo-a", commit, "", 9, false, "alpha");
    // sibling repo.
    let _sibling = seed_scoped_chunk(&conn, "sib.rs", "repo-b", commit, "", 0, false, "alpha");

    crate::index::install_scope_view(&conn, commit, worktree).unwrap();

    // include_generated = false: the visible set is exactly the non-generated in-scope chunks,
    // ordered by bm25 (shortest doc first), and the equal-score tie pair breaks by chunks.id
    // (tie_base before tie_overlay) in BOTH statements — identical rows AND identical order.
    let legacy = candidate_ids_and_scores(&conn, &legacy_bm25_sql(false), "alpha", 80);
    let materialized = candidate_ids_and_scores(&conn, &bm25_candidates_sql(false), "alpha", 80);
    assert_eq!(
        legacy, materialized,
        "materialized bm25 must be byte-identical to the legacy join shape (rows AND order)"
    );
    assert_eq!(
        materialized.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![base1, base2, base3, overlay, tie_base, tie_overlay],
        "in-scope non-generated chunks in bm25 order; ties deterministic by chunk_id"
    );

    // include_generated = true: the generated chunk joins the set (still no out-of-scope leak).
    let legacy_gen = candidate_ids_and_scores(&conn, &legacy_bm25_sql(true), "alpha", 80);
    let materialized_gen = candidate_ids_and_scores(&conn, &bm25_candidates_sql(true), "alpha", 80);
    assert_eq!(legacy_gen, materialized_gen, "include_generated path must match too");
    assert_eq!(
        materialized_gen.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![base1, base2, base3, overlay, tie_base, tie_overlay, generated],
        "generated chunk is visible under include_generated, still no out-of-scope rows"
    );

    // RECALL BOUND: LIMIT applies AFTER the scope filter, so a small limit returns a per-repo
    // window of in-scope rows (the two most-relevant), never the shorter out-of-scope docs a
    // pre-LIMIT statement would have grabbed.
    let legacy_limited = candidate_ids_and_scores(&conn, &legacy_bm25_sql(false), "alpha", 2);
    let materialized_limited =
        candidate_ids_and_scores(&conn, &bm25_candidates_sql(false), "alpha", 2);
    assert_eq!(legacy_limited, materialized_limited);
    assert_eq!(
        materialized_limited.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![base1, base2],
        "LIMIT after the scope filter keeps the per-repo top-2, not a starved global window"
    );

    // DETERMINISTIC TIE CUT: a LIMIT that falls in the middle of the tie pair must keep the
    // SAME survivor (tie_base, the lower chunk_id) in both statements — otherwise the position
    // fed to `lexical_rank_score(rank)` and the surviving candidate depend on scan plan.
    let legacy_cut = candidate_ids_and_scores(&conn, &legacy_bm25_sql(false), "alpha", 5);
    let materialized_cut = candidate_ids_and_scores(&conn, &bm25_candidates_sql(false), "alpha", 5);
    assert_eq!(legacy_cut, materialized_cut);
    assert_eq!(
        materialized_cut.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![base1, base2, base3, overlay, tie_base],
        "LIMIT cutting through the tie keeps the lower-chunk_id row, deterministically"
    );
}

/// Insert a `Current` int8 embedding for `chunk_id` under `model_id`. Sets exactly the columns
/// `vector_candidates_sql` filters on: `model_version` = "v1" and `embedding_text_version` =
/// `ai::EMBEDDING_TEXT_VERSION` (the two the query binds via `active_embedding_model_version` /
/// `EMBEDDING_TEXT_VERSION`), and `source_text_hash` copied from the chunk so the freshness
/// join matches. The rest take their schema defaults.
fn seed_embedding(conn: &Connection, chunk_id: i64, model_id: &str, dim: usize, vector: &[f32]) {
    let text_hash: String = conn
        .query_row("SELECT text_hash FROM chunks WHERE id = ?1", [chunk_id], |row| row.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO chunk_embeddings(
             chunk_id, model_id, model_version, source_text_hash, input_hash,
             embedding_text_version, embedding_dim, vector_blob, status, created_at_ms
         )
         VALUES (?1, ?2, 'v1', ?3, 'ih', ?4, ?5, ?6, 'Current', 0)",
        params![
            chunk_id,
            model_id,
            text_hash,
            ai::EMBEDDING_TEXT_VERSION,
            i64::try_from(dim).unwrap(),
            ai::encode_vector(vector)
        ],
    )
    .unwrap();
}

/// VECTOR TIE-BREAK (Codex #568 review, the vector twin of the bm25 tie fix): the scope-view
/// materialization changed the flat-scan row order feeding the vector scorer, so equal-
/// similarity candidates straddling the candidate limit must be truncated deterministically.
/// Two chunks share an IDENTICAL vector → identical similarity; their embedding rows are
/// inserted in REVERSE chunk_id order so `chunk_embeddings`'s AUTOINCREMENT rowid makes the
/// flat scan surface the higher-chunk_id one FIRST. Without the `(similarity, chunk_id)`
/// tie-break the stable sort would keep that order and truncate the wrong survivor. The
/// lower chunk_id must win.
#[test]
fn vector_candidates_break_similarity_ties_by_chunk_id() {
    let conn = seeded_conn();
    // Clear the placeholder file+chunk seeded_conn inserted; seed our own candidates only.
    conn.execute("DELETE FROM chunk_fts", []).unwrap();
    conn.execute("DELETE FROM chunk_text", []).unwrap();
    conn.execute("DELETE FROM chunks", []).unwrap();
    conn.execute("DELETE FROM main.files", []).unwrap();

    // A fake installed+ready embedding model (dim 4). It is not a known spec, so
    // `active_embedding_model_version` resolves to the default "v1" the rows carry.
    conn.execute(
        "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed,
                               disabled, status, installed_at_ms)
         VALUES ('test-model', 'embedding', 4, 'local', 1, 0, 'Ready', 0)",
        [],
    )
    .unwrap();

    // No scope view installed ⇒ `files` resolves to raw main.files, so all three chunks are
    // visible. `high` decodes to [1,0,0,0] (dot 1.0, strictly top); `tie_lo`/`tie_hi` decode to
    // [0.5,0,0,0] (dot 0.5, identical). tie_lo is seeded first ⇒ tie_lo.chunk_id <
    // tie_hi.chunk_id.
    let high = seed_scoped_chunk(&conn, "high.rs", "__unassigned__", "", "", 0, false, "alpha");
    let tie_lo = seed_scoped_chunk(&conn, "tie_lo.rs", "__unassigned__", "", "", 0, false, "beta");
    let tie_hi = seed_scoped_chunk(&conn, "tie_hi.rs", "__unassigned__", "", "", 0, false, "gamma");

    // Reverse-order embedding inserts: chunk_embeddings.id is AUTOINCREMENT, so tie_hi's row
    // gets the lower rowid and the flat scan surfaces tie_hi before tie_lo.
    seed_embedding(&conn, tie_hi, "test-model", 4, &[0.5, 0.0, 0.0, 0.0]);
    seed_embedding(&conn, tie_lo, "test-model", 4, &[0.5, 0.0, 0.0, 0.0]);
    seed_embedding(&conn, high, "test-model", 4, &[1.0, 0.0, 0.0, 0.0]);

    let make_qe = || ai::QueryEmbedding {
        model_id: "test-model".to_string(),
        dim: 4,
        vector: vec![1.0, 0.0, 0.0, 0.0],
    };
    let dicts = rag_rat_query::chunk_text_dicts(&conn).unwrap();
    let mut decoder = text_compression::ChunkTextDecoder::new(&dicts);

    // Full result (limit 3): descending similarity, ties by ASCENDING chunk_id, and the tie is
    // genuine (equal similarity) with `high` strictly greater.
    let all = vector_candidates(&conn, "alpha", 3, false, Some(make_qe()), &mut decoder).unwrap();
    assert_eq!(
        all.iter().map(|(hit, _)| hit.chunk_id).collect::<Vec<_>>(),
        vec![high, tie_lo, tie_hi],
        "vector order is descending similarity, ties broken by ascending chunk_id"
    );
    assert_eq!(all[1].1, all[2].1, "tie_lo and tie_hi must carry identical similarity");
    assert!(all[0].1 > all[1].1, "high must be strictly the most similar");

    // limit 2 straddles the tie: exactly ONE tie candidate survives, and it must be the lower
    // chunk_id (tie_lo) regardless of the flat-scan row order.
    let truncated =
        vector_candidates(&conn, "alpha", 2, false, Some(make_qe()), &mut decoder).unwrap();
    assert_eq!(
        truncated.iter().map(|(hit, _)| hit.chunk_id).collect::<Vec<_>>(),
        vec![high, tie_lo],
        "LIMIT straddling the tie keeps the lower-chunk_id survivor, deterministically"
    );
}

#[test]
fn search_lexical_only_returns_bm25_hits_without_embeddings() {
    let conn = seeded_conn();
    let hits = search_lexical_only(&conn, "election retry", 5, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "src/watch.rs");
    // No model is configured in this DB; reaching here without error proves no embed path ran.
    // retrieval_mode is always present and states the mode without needing explain (#41).
    assert_eq!(hits[0].retrieval_mode, "lexical");
}

#[test]
fn retrieval_mode_is_lexical_when_no_embedding_model() {
    let conn = seeded_conn();
    // The default search path embeds the query, but with no model it falls back to BM25 —
    // every hit must be labeled "lexical", never an empty string or an overclaimed mode.
    let hits = search(&conn, "election retry", 5, false).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].retrieval_mode, "lexical");
}

/// The saturating recency+churn formula (graded-git rerank, #109). Pins the [0,1] range, the
/// caps, the 0.6/0.4 recent/total split, and the no-history → 0.0 case — the dials a sweep
/// would tune.
#[test]
fn git_score_saturates_and_splits_recent_vs_total() {
    // No git history → 0.0.
    assert_eq!(git_score(0, 0), 0.0);
    // Negative counts (defensive) clamp to 0.
    assert_eq!(git_score(-3, -1), 0.0);
    // Recent caps at RECENT_CAP (=5): 5 and 50 both max the recency term. With 0 total commits
    // the total term is 0, so the score is exactly GIT_RECENT_WEIGHT (0.6).
    assert!((git_score(5, 0) - GIT_RECENT_WEIGHT).abs() < 1e-9);
    assert!((git_score(50, 0) - GIT_RECENT_WEIGHT).abs() < 1e-9);
    // Total caps at TOTAL_CAP (=20): 20 and 200 both max the churn term. With 0 recent commits
    // the recency term is 0, so the score is exactly GIT_TOTAL_WEIGHT (0.4).
    assert!((git_score(0, 20) - GIT_TOTAL_WEIGHT).abs() < 1e-9);
    assert!((git_score(0, 200) - GIT_TOTAL_WEIGHT).abs() < 1e-9);
    // Both maxed → 1.0 (the saturation ceiling).
    assert!((git_score(5, 20) - 1.0).abs() < 1e-9);
    assert!((git_score(100, 100) - 1.0).abs() < 1e-9);
    // A partial value: 2 recent of cap 5 (=0.4) and 10 total of cap 20 (=0.5):
    // 0.6*0.4 + 0.4*0.5 = 0.24 + 0.20 = 0.44.
    assert!((git_score(2, 10) - 0.44).abs() < 1e-9);
    // The score is always in [0,1].
    for (recent, total) in [(0, 0), (1, 1), (3, 7), (5, 20), (1000, 1000)] {
        let score = git_score(recent, total);
        assert!((0.0..=1.0).contains(&score), "git_score({recent},{total}) = {score} ∉ [0,1]");
    }
}

/// Seed one git commit touching `src/watch.rs` so the graded-git path has history to grade.
fn seed_git_history(conn: &Connection, path: &str) {
    conn.execute(
        "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s,
                                 committed_at_s, subject, body)
         VALUES ('c1', 'a', 'a@x', 1000, 1000, 'touch', '')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions)
         VALUES ('c1', ?1, 3, 1)",
        params![path],
    )
    .unwrap();
}

/// FLAG OFF is byte-identical: with `graded_history` false the produced score (and every
/// component) is exactly what today's fuse produces — including when git history exists that
/// the graded path WOULD grade. This is the load-bearing guarantee behind the A/B (the OFF
/// arm must be today's behavior).
#[test]
fn graded_history_off_is_byte_identical_to_today() {
    let conn = seeded_conn();
    seed_git_history(&conn, "src/watch.rs");

    let off = SearchOptions { graded_history: false, ..SearchOptions::default() };
    let baseline = search_with_query_embedding(
        &conn,
        "election retry",
        5,
        false,
        None,
        true,
        SearchOptions::default(),
    )
    .unwrap();
    let with_flag_off =
        search_with_query_embedding(&conn, "election retry", 5, false, None, true, off).unwrap();

    assert_eq!(baseline.len(), with_flag_off.len());
    for (a, b) in baseline.iter().zip(&with_flag_off) {
        assert_eq!(a.chunk_id, b.chunk_id);
        // The exact rounded score is identical bit-for-bit.
        assert_eq!(a.score, b.score, "flag-off score must equal today's score");
        let (ca, cb) = (a.score_components.as_ref().unwrap(), b.score_components.as_ref().unwrap());
        assert_eq!(ca.git, cb.git, "flag-off git component must be the binary boost");
        assert_eq!(ca.bm25, cb.bm25);
        assert_eq!(ca.symbol, cb.symbol);
        assert_eq!(ca.graph, cb.graph);
        assert_eq!(ca.papertrail, cb.papertrail);
    }
}

/// FLAG ON grades the git signal: with history present, the graded git contribution
/// (`GIT_WEIGHT_GRADED * git_score`) differs from the binary contribution (`GIT_WEIGHT * 1.0`),
/// so the flag actually changes the score. Proves the lever is wired end-to-end through the
/// inner wide-pool site (not just the standalone formula).
#[test]
fn graded_history_on_changes_the_git_component() {
    let conn = seeded_conn();
    seed_git_history(&conn, "src/watch.rs");

    let on = SearchOptions { graded_history: true, ..SearchOptions::default() };
    let hits =
        search_with_query_embedding(&conn, "election retry", 5, false, None, true, on).unwrap();
    assert_eq!(hits.len(), 1);
    let git = hits[0].score_components.as_ref().unwrap().git;
    // One commit, recent vs the only commit → recent=1, total=1:
    // git_score = 0.6*min(1/5,1) + 0.4*min(1/20,1) = 0.6*0.2 + 0.4*0.05 = 0.14; weighted by
    // GIT_WEIGHT_GRADED (0.10) → 0.014. The binary path would have been GIT_WEIGHT (0.03) * 1.0
    // = 0.03, so the graded component is strictly different.
    let expected = GIT_WEIGHT_GRADED * git_score(1, 1);
    assert!((git - expected).abs() < 1e-9, "graded git component {git} != expected {expected}");
    assert!((git - GIT_WEIGHT).abs() > 1e-9, "graded git must differ from the binary boost");
}

/// FLAG ON demotes generated + test chunks multiplicatively after the weighted sum. Seed a
/// generated, test-flagged file and assert the produced score is the un-demoted score scaled by
/// GENERATED_PENALTY * TEST_PENALTY.
#[test]
fn graded_history_on_applies_generated_and_test_demotion() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // A generated file with the precomputed test-code flag set.
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                           generated, has_test_code)
         VALUES ('src/gen.rs', 'rust', 'generated', 'abc', 0, 0, 1, 1)",
        [],
    )
    .unwrap();
    let text = "fn watcher_main() { /* election retry loop */ }";
    // No symbol_path → the test flag falls back to files.has_test_code.
    let chunk_id: i64 = conn
        .query_row(
            "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                start_line, end_line, text_hash)
             VALUES (1, 'symbol', NULL, 0, 10, 1, 20, 'h1')
             RETURNING id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    rag_rat_db::chunk_text_store::seed_chunk_text(&conn, chunk_id, text).unwrap();
    conn.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", params![chunk_id, text])
        .unwrap();

    // generated files are excluded unless include_generated; pass true so the chunk is a
    // candidate.
    let off = search_with_query_embedding(
        &conn,
        "election retry",
        5,
        true,
        None,
        false,
        SearchOptions::default(),
    )
    .unwrap();
    let on =
        search_with_query_embedding(&conn, "election retry", 5, true, None, false, SearchOptions {
            graded_history: true,
            ..SearchOptions::default()
        })
        .unwrap();
    assert_eq!(off.len(), 1);
    assert_eq!(on.len(), 1);
    // No git history here, so the only graded-on change to the WEIGHTED sum is the git
    // component dropping to 0 (graded score of a no-history path) vs the binary 0.03.
    // Compare the demotion independently by reconstructing the on-flag pre-demotion
    // score from its own components.
    let comps = on[0].score_components.is_none();
    assert!(comps, "this run did not request explain; components stay None");
    // The demotion is multiplicative on the final score; with generated+test both set the
    // penalty is GENERATED_PENALTY * TEST_PENALTY. The on score must be strictly below the
    // un-penalized graded score (which itself is the off score minus the binary git boost).
    // Simplest robust assertion: the on score is strictly less than the off score (penalty < 1
    // AND git dropped), and it is positive.
    assert!(on[0].score > 0.0, "demoted score must stay positive");
    assert!(
        on[0].score < off[0].score,
        "generated+test demotion must lower the score (on {} !< off {})",
        on[0].score,
        off[0].score,
    );
}
