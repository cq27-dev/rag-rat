use std::fs;
use std::path::PathBuf;

use rag_rat_base::config::{Config, ResolvedTarget, TargetKind};
use rag_rat_base::language::Language;
use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};
use rusqlite::params;

use super::hops::adapt_hops;
use crate::index::{IndexDatabase, LensHopResolvedBy, LensHopSelector};

const SOURCE: &str = r#"
pub enum Req { Upsert { id: i64 } }

pub fn target(_id: i64) {}

pub fn caller() { target(1); }

#[test]
fn target_test() { target(2); }

pub fn enqueue() { send(Req::Upsert { id: 3 }); }
fn send(_req: Req) {}

pub fn handle(req: Req) {
    match req {
        Req::Upsert { id } => target(id),
    }
}
"#;

const OTHER_SOURCE: &str = "pub fn other() {}\n";

const UNICODE_PATH: &str = "src/дом.rs";
const UNICODE_UPPERCASE_PATH: &str = "src/ДОМ.rs";
const UNICODE_SOURCE: &str = "pub fn unicode_named() {}\n";

/// Medial and final sigma: two lowercase spellings a case-insensitive volume treats as one name,
/// which lowercasing alone leaves distinct.
const FOLDED_PATH: &str = "src/σ.rs";
const FOLDED_ALIAS_PATH: &str = "src/ς.rs";
const FOLDED_SOURCE: &str = "pub fn folded_name() {}\n";

/// Composed `ä` and its decomposed `a` + combining diaeresis: one name to a case-insensitive
/// volume, which also matches names irrespective of Unicode normalization.
const COMPOSED_PATH: &str = "src/\u{00e4}.rs";
const DECOMPOSED_PATH: &str = "src/a\u{0308}.rs";
const COMPOSED_SOURCE: &str = "pub fn composed_name() {}\n";

#[test]
fn file_compositions_are_ordered_and_batch_graph_signals() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::try_open_config_read_only(&config).unwrap().expect("read-only index");

    assert_eq!(
        db.lens_canonical_file_path("SRC/LIB.RS", true).unwrap().as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(db.lens_canonical_file_path("SRC/LIB.RS", false).unwrap(), None);
    // A case-insensitive volume folds the whole Unicode range, not the ASCII subset SQLite's
    // `NOCASE` collation covers — and it folds rather than lowercases, so two lowercase spellings
    // of one name still resolve to the indexed one.
    assert_eq!(
        db.lens_canonical_file_path(UNICODE_UPPERCASE_PATH, true).unwrap().as_deref(),
        Some(UNICODE_PATH)
    );
    assert_eq!(db.lens_canonical_file_path(UNICODE_UPPERCASE_PATH, false).unwrap(), None);
    assert_eq!(
        db.lens_canonical_file_path(FOLDED_ALIAS_PATH, true).unwrap().as_deref(),
        Some(FOLDED_PATH)
    );
    assert_eq!(db.lens_canonical_file_path(FOLDED_ALIAS_PATH, false).unwrap(), None);
    assert_eq!(
        db.lens_canonical_file_path(DECOMPOSED_PATH, true).unwrap().as_deref(),
        Some(COMPOSED_PATH)
    );
    assert_eq!(db.lens_canonical_file_path("src/missing.rs", true).unwrap(), None);

    let symbols = db.lens_file_symbols("src/lib.rs").unwrap().symbols;
    assert!(symbols.windows(2).all(|pair| pair[0].start_line <= pair[1].start_line));
    let target = symbols.iter().find(|symbol| symbol.name == "target").unwrap();
    assert!(target.qname.as_deref().is_some_and(|name| name.ends_with("::target")));
    assert!(target.signature.as_deref().is_some_and(|value| value.contains("target")));
    assert!(target.fan_in >= 3, "target graph counts: {target:?}");

    let graph = db.lens_file_graph("src/lib.rs").unwrap().symbols;
    assert!(graph.windows(2).all(|pair| pair[0].start_line <= pair[1].start_line));
    let target = graph.iter().find(|symbol| symbol.name == "target").unwrap();
    assert!(
        target.callers.exact + target.callers.syntactic >= 3,
        "confidence counts: {:?}",
        target.callers
    );
    assert!(target.callers.tests >= 1, "test counts: {:?}", target.callers);
    assert!(target.callers.dispatch >= 1, "dispatch counts: {:?}", target.callers);
    assert!(target.fan_in_score >= 2.0);
    assert!(target.dispatch.iter().any(|detail| {
        detail.direction == "handled"
            && detail.variant.as_deref() == Some("Req::Upsert")
            && detail.other_name.as_deref() == Some("enqueue")
    }));
}

#[test]
fn hops_and_chunk_text_keep_stable_compatibility_fields_on_read_only_open() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::try_open_config_read_only(&config).unwrap().expect("read-only index");
    let write_error = db
        .storage
        .connection()
        .execute("DELETE FROM chunks WHERE id = -1", [])
        .expect_err("read-only open must reject writes");
    assert!(write_error.to_string().contains("readonly"), "{write_error}");

    let target_qname = db
        .lens_file_symbols("src/lib.rs")
        .unwrap()
        .symbols
        .into_iter()
        .find(|symbol| symbol.name == "target")
        .and_then(|symbol| symbol.qname)
        .unwrap();
    let callers = db
        .lens_symbol_callers(&LensHopSelector::QualifiedName(target_qname), 50)
        .unwrap()
        .expect("a qualified name always resolves");
    assert!(callers.callers.iter().any(|hop| hop.name == "caller"));
    let wire = serde_json::to_value(&callers).unwrap();
    let first = wire["callers"].as_array().unwrap().first().unwrap();
    assert!(first.get("id").is_none());
    assert!(first.get("sid").is_none());
    assert!(first.get("source_file_id").is_none());

    let caller_qname = db
        .lens_file_symbols("src/lib.rs")
        .unwrap()
        .symbols
        .into_iter()
        .find(|symbol| symbol.name == "caller")
        .and_then(|symbol| symbol.qname)
        .unwrap();
    assert!(
        db.lens_symbol_callees(&LensHopSelector::QualifiedName(caller_qname.clone()), 50)
            .unwrap()
            .expect("a qualified name always resolves")
            .callees
            .iter()
            .any(|hop| hop.name == "target")
    );

    let chunk_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT chunks.id FROM chunks JOIN files ON files.id = chunks.file_id
             WHERE files.path = 'src/lib.rs' AND chunks.symbol_path = ?1",
            [caller_qname],
            |row| row.get(0),
        )
        .unwrap();
    let chunk = db.lens_chunk_text(chunk_id).unwrap().unwrap();
    assert_eq!(chunk.chunk_id, chunk_id);
    assert!(chunk.text.contains("target(1)"));
    assert!(db.lens_chunk_text(i64::MAX).unwrap().is_none());
}

#[test]
fn chunk_text_rejects_stale_source_without_healing_the_read_only_index() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::try_open_config_read_only(&config).unwrap().expect("read-only index");
    let chunk_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT chunks.id FROM chunks JOIN files ON files.id = chunks.file_id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    fs::write(config.root.join("src/lib.rs"), "completely different source\n").unwrap();

    let error = db.lens_chunk_text(chunk_id).unwrap_err().to_string();
    assert!(error.contains("StaleChunk"), "{error}");
}

#[test]
fn lens_version_tracks_in_place_enrichment_updates() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    let memory = crate::memory_write::create_memory(conn, RepoMemoryCreate {
        kind: "Invariant".into(),
        title: "Tracked binding".into(),
        body: "The editor must refresh this location".into(),
        confidence: "high".into(),
        created_by: Some("test".into()),
        source: None,
        payload_json: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget {
            path: Some("src/lib.rs".into()),
            start_line: Some(5),
            end_line: Some(5),
            ..RepoMemoryBindTarget::default()
        },
    })
    .unwrap()
    .memory;

    let before_binding_update = db.lens_version().unwrap();
    conn.execute(
        "UPDATE repo_memory_bindings SET start_line = 6, end_line = 6
         WHERE repo_id = ?1 AND memory_id = ?2",
        params![db.active_repo_id, memory.memory_id],
    )
    .unwrap();
    let after_binding_update = db.lens_version().unwrap();
    assert_ne!(before_binding_update.revision, after_binding_update.revision);

    let before_memory_update = db.lens_version().unwrap();
    conn.execute(
        "UPDATE repo_memories SET title = 'Tracked binding renamed'
         WHERE repo_id = ?1 AND id = ?2",
        params![db.active_repo_id, memory.memory_id],
    )
    .unwrap();
    let after_memory_update = db.lens_version().unwrap();
    assert_ne!(
        before_memory_update.revision, after_memory_update.revision,
        "memory content changes must invalidate even when updated_at_ms is unchanged"
    );

    conn.execute(
        "INSERT INTO memory_summaries(
             memory_id, repo_id, content_hash, summary, prompt_version, generated_at_ms
         ) VALUES (?1, ?2, 'same-time-summary', 'Before', '1', 7)",
        params![memory.memory_id, db.active_repo_id],
    )
    .unwrap();
    let before_summary_update = db.lens_version().unwrap();
    conn.execute(
        "UPDATE memory_summaries SET summary = 'After'
         WHERE repo_id = ?1 AND memory_id = ?2",
        params![db.active_repo_id, memory.memory_id],
    )
    .unwrap();
    let after_summary_update = db.lens_version().unwrap();
    assert_ne!(
        before_summary_update.revision, after_summary_update.revision,
        "summary changes must invalidate even when generated_at_ms is unchanged"
    );

    conn.execute(
        "INSERT INTO memory_reality(
             memory_id, repo_id, content_hash, verdict, prompt_version, checked_at_ms
         ) VALUES (?1, ?2, 'same-time-reality', 'current', '1', 7)",
        params![memory.memory_id, db.active_repo_id],
    )
    .unwrap();
    let before_reality_update = db.lens_version().unwrap();
    conn.execute(
        "UPDATE memory_reality SET verdict = 'diverged'
         WHERE repo_id = ?1 AND memory_id = ?2",
        params![db.active_repo_id, memory.memory_id],
    )
    .unwrap();
    let after_reality_update = db.lens_version().unwrap();
    assert_ne!(
        before_reality_update.revision, after_reality_update.revision,
        "verdict changes must invalidate even when checked_at_ms is unchanged"
    );

    conn.execute(
        "INSERT INTO papertrail_distill(
             tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
             root_issue, fix_edge_source, thread_shape, distilled_at_ms, repo_id
         ) VALUES ('github', 'owner/repo', 'issue', 'distill', 'hash', 1,
                   'Before', 'none', 'thin', 7, ?1)",
        [&db.active_repo_id],
    )
    .unwrap();
    let before_distill_update = db.lens_version().unwrap();
    conn.execute(
        "UPDATE papertrail_distill SET root_issue = 'After'
         WHERE repo_id = ?1 AND item_key = 'distill'",
        [&db.active_repo_id],
    )
    .unwrap();
    let after_distill_update = db.lens_version().unwrap();
    assert_ne!(
        before_distill_update.revision, after_distill_update.revision,
        "distill changes must invalidate even when distilled_at_ms is unchanged"
    );

    conn.execute(
        "INSERT INTO clone_graph_generations(
             generation, status, theta_floor, normalizer_kind, normalizer_version,
             source_revision, started_at_ms, repo_id
         ) VALUES (999, 'Complete', 0.7, 'baseline', ?1, 'revision', 7, ?2)",
        params![rag_rat_clones::NORM_VERSION, db.active_repo_id],
    )
    .unwrap();
    let before_clone_publish = db.lens_version().unwrap();
    db.set_repo_meta("clone_graph_live_generation", "999").unwrap();
    let after_clone_publish = db.lens_version().unwrap();
    assert_ne!(
        before_clone_publish.revision, after_clone_publish.revision,
        "publishing an already-created clone generation must invalidate Lens"
    );
    conn.execute(
        "UPDATE clone_graph_generations SET delta_files_applied = delta_files_applied + 1
         WHERE generation = 999",
        [],
    )
    .unwrap();
    let after_clone_delta = db.lens_version().unwrap();
    assert_ne!(
        after_clone_publish.revision, after_clone_delta.revision,
        "an in-place live clone-graph delta must invalidate Lens"
    );

    // Oracle results change graph scores and clone refinement mode, so Lens must see the pass —
    // but a pass writes one verdict per resolved edge, so the clock is advanced by the run row
    // those verdicts commit with, not by the verdicts themselves.
    let before_oracle_result = db.lens_version().unwrap();
    conn.execute(
        "INSERT INTO edge_oracle(
             repo_id, source_path, source_start_byte, source_end_byte, callee_start_byte,
             callee_end_byte, edge_kind, file_sha, tool, tool_version, scip_symbol, kind,
             computed_at
         ) VALUES (?1, 'src/lib.rs', 0, 1, 0, 1, 'calls_name', 'sha', 'scip-test', '1',
                   'symbol', 'confirm', 7)",
        [&db.active_repo_id],
    )
    .unwrap();
    let after_oracle_result = db.lens_version().unwrap();
    assert_eq!(
        before_oracle_result.revision, after_oracle_result.revision,
        "verdict rows are clocked by their transactional writer, not one bump per edge"
    );
    conn.execute(
        "INSERT INTO oracle_runs(
             repo_id, tool, tool_version, commit_sha, worktree_id, started_at, status, stats_json
         ) VALUES (?1, 'scip-test', '1', 'head', 'active-wt', 7, 'Completed', '{}')",
        [&db.active_repo_id],
    )
    .unwrap();
    let after_oracle_run = db.lens_version().unwrap();
    assert_ne!(
        after_oracle_result.revision, after_oracle_run.revision,
        "the run row that publishes a pass's verdicts must invalidate Lens"
    );

    conn.execute(
        "INSERT INTO papertrail_items(
             tracker, project, item_kind, item_key, url, state, \
         title, body, synced_at_ms, repo_id
         ) VALUES ('github', 'owner/repo', 'issue', '1', \
         'https://example.test/1', 'open',
                   'Issue', 'Body', 1, ?1)",
        [&db.active_repo_id],
    )
    .unwrap();
    let before_item_update = db.lens_version().unwrap();
    conn.execute(
        "UPDATE papertrail_items SET state = 'closed'
         WHERE repo_id = ?1 AND tracker = 'github' AND project = 'owner/repo'
           AND item_kind = 'issue' AND item_key = '1'",
        [&db.active_repo_id],
    )
    .unwrap();
    let after_item_update = db.lens_version().unwrap();
    assert_ne!(before_item_update.revision, after_item_update.revision);

    conn.execute(
        "DELETE FROM papertrail_items
         WHERE repo_id = ?1 AND tracker = 'github' AND project = 'owner/repo'
           AND item_kind = 'issue' AND item_key = '1'",
        [&db.active_repo_id],
    )
    .unwrap();
    let after_item_delete = db.lens_version().unwrap();
    assert_ne!(after_item_update.revision, after_item_delete.revision);

    conn.execute(
        "INSERT INTO papertrail_refs(
             tracker, project, item_key, item_kind, ref_kind, source_kind, source_path,
             source_text, discovered_at_ms, repo_id
         ) VALUES ('github', 'owner/repo', '1', 'issue', 'mentions', 'path', 'src/lib.rs',
                   'mentions #1', 1, ?1)",
        [&db.active_repo_id],
    )
    .unwrap();
    let before_ref_update = db.lens_version().unwrap();
    conn.execute(
        "UPDATE papertrail_refs SET ref_kind = 'fixes'
         WHERE repo_id = ?1 AND tracker = 'github' AND project = 'owner/repo'
           AND item_key = '1' AND source_path = 'src/lib.rs'",
        [&db.active_repo_id],
    )
    .unwrap();
    let after_ref_update = db.lens_version().unwrap();
    assert_ne!(before_ref_update.revision, after_ref_update.revision);
}

/// The dead-verdict sweep retires `edge_oracle` rows without writing an `oracle_runs` row, so it
/// is the one verdict writer the run-row clock does not cover. An editor showing an oracle-backed
/// call still has to learn that the evidence behind it is gone.
#[test]
fn lens_version_tracks_swept_oracle_verdicts() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    // No live edge carries this content key, so the global sweep retires it.
    conn.execute(
        "INSERT INTO edge_oracle(
             repo_id, source_path, source_start_byte, source_end_byte, callee_start_byte,
             callee_end_byte, edge_kind, file_sha, tool, tool_version, scip_symbol, kind,
             computed_at
         ) VALUES (?1, 'src/gone.rs', 0, 1, 0, 1, 'calls_name', 'sha', 'scip-test', '1',
                   'symbol', 'confirm', 7)",
        [&db.active_repo_id],
    )
    .unwrap();

    let before_sweep = db.lens_version().unwrap();
    db.garbage_collect().unwrap();
    let after_sweep = db.lens_version().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM edge_oracle WHERE source_path = 'src/gone.rs'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "the sweep must retire a verdict no live edge joins to"
    );
    assert_ne!(
        before_sweep.revision, after_sweep.revision,
        "retiring a verdict must invalidate Lens even though the sweep writes no run row"
    );

    // Idempotence: a sweep that retires nothing must not churn every connected editor.
    let after_empty_sweep = db.lens_version().unwrap();
    db.garbage_collect().unwrap();
    assert_eq!(
        after_empty_sweep.revision,
        db.lens_version().unwrap().revision,
        "a sweep with nothing to retire must leave the clock alone"
    );
}

#[test]
fn graph_compositions_exclude_edges_with_out_of_scope_endpoints() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    let (target_id, target_qname): (i64, String) = conn
        .query_row(
            "SELECT s.id, ns.value FROM symbols s
             JOIN files ON files.id = s.file_id
             JOIN name_strings ns ON ns.id = s.qualified_name_id
             WHERE s.name = 'target'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let (caller_id, caller_file_id, caller_qname): (i64, i64, String) = conn
        .query_row(
            "SELECT s.id, s.file_id, ns.value FROM symbols s
             JOIN files ON files.id = s.file_id
             JOIN name_strings ns ON ns.id = s.qualified_name_id
             WHERE s.name = 'caller'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let before_symbols = db.lens_file_symbols("src/lib.rs").unwrap().symbols;
    let before_target = before_symbols.iter().find(|symbol| symbol.name == "target").unwrap();
    let before_caller = before_symbols.iter().find(|symbol| symbol.name == "caller").unwrap();
    let before_target_fan_in = before_target.fan_in;
    let before_caller_fan_out = before_caller.fan_out;
    let before_dispatch = db
        .lens_file_graph("src/lib.rs")
        .unwrap()
        .symbols
        .into_iter()
        .find(|symbol| symbol.name == "target")
        .unwrap()
        .callers
        .dispatch;

    conn.execute(
        "INSERT INTO main.files(
             path, language, kind, sha256, modified_at_ms, indexed_at_ms,
             commit_sha, worktree_id, repo_id, generation
         ) VALUES ('dead.rs', 'rust', 'source', 'dead-sha', 0, 0, 'dead-commit', '', ?1, ?2)",
        params![db.active_repo_id, db.active_generation],
    )
    .unwrap();
    let dead_file_id = conn.last_insert_rowid();
    conn.execute_batch(
        "INSERT OR IGNORE INTO name_strings(value) VALUES ('dead::caller');
         INSERT OR IGNORE INTO name_strings(value) VALUES ('dead::target');",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO symbols(
             file_id, language, name, qualified_name_id, kind,
             start_byte, end_byte, start_line, end_line
         ) VALUES (
             ?1, 'rust', 'dead_caller',
             (SELECT id FROM name_strings WHERE value = 'dead::caller'),
             'function', 0, 10, 1, 1
         )",
        [dead_file_id],
    )
    .unwrap();
    let dead_caller_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO symbols(
             file_id, language, name, qualified_name_id, kind,
             start_byte, end_byte, start_line, end_line
         ) VALUES (
             ?1, 'rust', 'dead_target',
             (SELECT id FROM name_strings WHERE value = 'dead::target'),
             'function', 11, 20, 2, 2
         )",
        [dead_file_id],
    )
    .unwrap();
    let dead_target_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO edges(
             source_file_id, from_symbol_id, to_symbol_id, to_name,
             edge_kind, confidence, resolution, source_start_line
         ) VALUES (?1, ?2, ?3, 'target', 'calls_name', 'Exact', 'exact', 1)",
        params![dead_file_id, dead_caller_id, target_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(
             source_file_id, from_symbol_id, to_symbol_id, to_name,
             edge_kind, confidence, resolution, source_start_line
         ) VALUES (?1, ?2, ?3, 'dead_target', 'calls_name', 'Exact', 'exact', 1)",
        params![caller_file_id, caller_id, dead_target_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(
             source_file_id, from_symbol_id, to_symbol_id, to_name, evidence,
             edge_kind, confidence, resolution, source_start_line
         ) VALUES (
             ?1, ?2, ?3, 'target', 'Dead::Variant',
             'dispatches', 'Exact', 'dispatch', 1
         )",
        params![dead_file_id, dead_caller_id, target_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(
             source_file_id, from_symbol_id, to_name, target_qualified_name,
             edge_kind, confidence, resolution, source_start_line
         ) VALUES (
             ?1, ?2, 'missing', 'crate::missing',
             'calls_name', 'NameOnly', 'unresolved', 1
         )",
        params![caller_file_id, caller_id],
    )
    .unwrap();
    let unresolved_edge_id =
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get::<_, i64>(0)).unwrap();

    let compiler_hop = rag_rat_query::graph::GraphHop {
        edge_id: unresolved_edge_id,
        from_symbol: Some(caller_qname.clone()),
        to_symbol: Some(target_qname.clone()),
        edge_kind: "calls_name".into(),
        confidence: "compiler".into(),
        edge_confidence: "name_only".into(),
        target: Some("missing".into()),
        target_qualified_name: Some(target_qname.clone()),
        evidence: None,
        receiver_hint: None,
        resolution: "exact".into(),
        resolution_reason: Some("scip:test@1".into()),
        resolved_external: None,
        verified_target_symbol: true,
        shown_by_default: true,
        callsite: Some(rag_rat_query::graph::Callsite {
            path: "src/lib.rs".into(),
            line: 1,
            span: [1, 1],
        }),
        importance: None,
    };
    let adapted = adapt_hops(conn, vec![compiler_hop], false).unwrap();
    assert_eq!(adapted.len(), 1);
    assert_eq!(adapted[0].name, "target");
    assert_eq!(adapted[0].qname.as_deref(), Some(target_qname.as_str()));

    let symbols = db.lens_file_symbols("src/lib.rs").unwrap().symbols;
    let target = symbols.iter().find(|symbol| symbol.name == "target").unwrap();
    let caller = symbols.iter().find(|symbol| symbol.name == "caller").unwrap();
    assert_eq!(target.fan_in, before_target_fan_in);
    assert_eq!(caller.fan_out, before_caller_fan_out);
    let graph_target = db
        .lens_file_graph("src/lib.rs")
        .unwrap()
        .symbols
        .into_iter()
        .find(|symbol| symbol.name == "target")
        .unwrap();
    assert_eq!(graph_target.callers.dispatch, before_dispatch);
    assert!(graph_target.dispatch.iter().all(|detail| {
        detail.variant.as_deref() != Some("Dead::Variant")
            && detail.other_name.as_deref() != Some("dead_caller")
    }));
    assert!(
        db.lens_symbol_callers(&LensHopSelector::QualifiedName(target_qname), 50)
            .unwrap()
            .expect("a qualified name always resolves")
            .callers
            .iter()
            .all(|hop| hop.name != "dead_caller")
    );
    assert!(
        db.lens_symbol_callees(&LensHopSelector::QualifiedName(caller_qname), 50)
            .unwrap()
            .expect("a qualified name always resolves")
            .callees
            .iter()
            .all(|hop| hop.qname.as_deref() != Some("crate::missing") && hop.name != "dead_target")
    );
}

#[test]
fn symbol_callers_preserve_file_level_edges() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    let (target_id, target_qname): (i64, String) = conn
        .query_row(
            "SELECT symbol.id, name.value
             FROM symbols symbol
             JOIN name_strings name ON name.id = symbol.qualified_name_id
             WHERE symbol.name = 'target'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let source_file_id: i64 = conn
        .query_row("SELECT id FROM files WHERE path = 'src/lib.rs'", [], |row| row.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO edges(
             source_file_id, from_name, to_symbol_id, to_name,
             edge_kind, confidence, resolution, source_start_line
         ) VALUES (?1, 'src/lib.rs', ?2, 'target', 'calls_name', 'Exact', 'exact', 2)",
        params![source_file_id, target_id],
    )
    .unwrap();

    let callers = db
        .lens_symbol_callers(&LensHopSelector::QualifiedName(target_qname), 50)
        .unwrap()
        .expect("a qualified name always resolves")
        .callers;
    let file_caller = callers.iter().find(|hop| hop.name == "src/lib.rs").unwrap();
    assert_eq!(file_caller.path, "src/lib.rs");
    assert_eq!(file_caller.source_start_line, 2);
    assert_eq!(file_caller.qname, None);
    assert_eq!(file_caller.kind, None);
}

#[test]
fn file_enrichments_scope_memories_and_order_coupling() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();

    let path_memory = crate::memory_write::create_memory(conn, RepoMemoryCreate {
        kind: "Invariant".into(),
        title: "Path memory".into(),
        body: "Keep src/lib.rs stable".into(),
        confidence: "high".into(),
        created_by: Some("test".into()),
        source: None,
        payload_json: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget {
            path: Some("src/lib.rs".into()),
            start_line: Some(5),
            end_line: Some(5),
            ..RepoMemoryBindTarget::default()
        },
    })
    .unwrap()
    .memory;
    let dir_memory = crate::memory_write::create_memory(conn, RepoMemoryCreate {
        kind: "Decision".into(),
        title: "Source directory memory".into(),
        body: "Applies throughout src".into(),
        confidence: "medium".into(),
        created_by: Some("test".into()),
        source: None,
        payload_json: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget { dir: Some("src".into()), ..RepoMemoryBindTarget::default() },
    })
    .unwrap()
    .memory;
    crate::memory_write::create_memory(conn, RepoMemoryCreate {
        kind: "Risk".into(),
        title: "Other file memory".into(),
        body: "Must not leak into lib.rs".into(),
        confidence: "high".into(),
        created_by: Some("test".into()),
        source: None,
        payload_json: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget {
            path: Some("src/other.rs".into()),
            ..RepoMemoryBindTarget::default()
        },
    })
    .unwrap();

    let content_hash =
        rag_rat_query::memory::evidence::note_content_hash(&path_memory.title, &path_memory.body);
    conn.execute(
        "INSERT INTO memory_summaries(
             memory_id, repo_id, content_hash, summary, prompt_version, generated_at_ms
         ) VALUES (?1, ?2, ?3, 'Compact path summary', ?4, 0)",
        params![
            path_memory.memory_id,
            db.active_repo_id,
            content_hash,
            rag_rat_query::memory::evidence::COMPACT_PROMPT_VERSION
        ],
    )
    .unwrap();
    let inputs = rag_rat_query::memory::evidence::checked_inputs_hash(
        conn,
        &path_memory.memory_id,
        &Some(db.active_repo_id.clone()),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_reality(
             memory_id, repo_id, content_hash, verdict, direction, evidence_json,
             checked_against_commit, checked_inputs_hash, prompt_version, checked_at_ms
         ) VALUES (?1, ?2, ?3, 'diverged', 'code_ahead', '{\"line\":5}',
                   '0123456789abcdef', ?4, ?5, 0)",
        params![
            path_memory.memory_id,
            db.active_repo_id,
            content_hash,
            inputs,
            rag_rat_query::memory::evidence::VERDICT_PROMPT_VERSION
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_reality(
             memory_id, repo_id, content_hash, verdict, direction, evidence_json,
             checked_inputs_hash, prompt_version, checked_at_ms
         ) VALUES (?1, ?2, ?3, 'current', 'aligned', '{}', 'stale-inputs', ?4, 0)",
        params![
            dir_memory.memory_id,
            db.active_repo_id,
            rag_rat_query::memory::evidence::note_content_hash(&dir_memory.title, &dir_memory.body),
            rag_rat_query::memory::evidence::VERDICT_PROMPT_VERSION
        ],
    )
    .unwrap();

    let memories = db.lens_file_memories("src/lib.rs").unwrap().memories;
    assert_eq!(memories.len(), 2);
    assert_eq!(memories[0].title, "Path memory");
    assert_eq!(memories[0].line, Some(5));
    assert_eq!(memories[0].summary.as_deref(), Some("Compact path summary"));
    assert_eq!(memories[0].verdict.as_deref(), Some("diverged"));
    assert_eq!(memories[0].verdict_direction.as_deref(), Some("code_ahead"));
    assert_eq!(memories[0].verdict_evidence.as_deref(), Some("{\"line\":5}"));
    assert_eq!(memories[0].checked_against_commit.as_deref(), Some("0123456789abcdef"));
    let dir = memories.iter().find(|memory| memory.title == "Source directory memory").unwrap();
    assert_eq!(dir.binding_kind, "dir");
    assert_eq!(dir.path.as_deref(), Some("src"));
    assert_eq!(dir.verdict, None, "stale evidence must suppress structured verdict state");
    let treemap = db.lens_treemap().unwrap().files;
    let lib = treemap.iter().find(|file| file.path == "src/lib.rs").unwrap();
    assert_eq!(lib.memories, 2, "path and directory memories must both count");
    let other = treemap.iter().find(|file| file.path == "src/other.rs").unwrap();
    assert_eq!(other.loc, 1, "chunk end_line is already one-based");
    assert_eq!(other.memories, 2, "directory and direct memories must both count");

    for (hash, at_s, paths) in [
        ("pair-1", 123, &["src/lib.rs", "src/other.rs"][..]),
        ("pair-2", 122, &["src/lib.rs", "src/other.rs"][..]),
        ("lib-only-pair", 121, &["src/lib.rs", "history-only.rs"][..]),
        ("filler-1", 120, &["filler-a.rs", "filler-b.rs"][..]),
        ("filler-2", 119, &["filler-c.rs", "filler-d.rs"][..]),
    ] {
        conn.execute(
            "INSERT INTO git_commits(
                 hash, author_name, author_email, authored_at_s, committed_at_s,
                 subject, body, changed_file_count, repo_id
             ) VALUES (?1, 'a', 'a@b', ?2, ?2, 's', '', 2, ?3)",
            params![hash, at_s, db.active_repo_id],
        )
        .unwrap();
        for path in paths {
            conn.execute(
                "INSERT INTO git_file_changes(
                     commit_hash, path, additions, deletions, change_kind, repo_id
                 ) VALUES (?1, ?2, 0, 0, 'modified', ?3)",
                params![hash, path, db.active_repo_id],
            )
            .unwrap();
        }
    }
    conn.execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = 'git_coupling_stamp'", [
        &db.active_repo_id
    ])
    .unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM git_change_couplings", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0,
        "the read-only fallback must be exercised before the lazy table is materialized"
    );
    drop(db);
    let db = IndexDatabase::try_open_config_read_only(&config).unwrap().expect("read-only index");
    let coupling = db.lens_file_coupling("src/lib.rs").unwrap().coupling;
    assert_eq!(coupling.len(), 1);
    assert_eq!(coupling[0].path, "src/other.rs");
    assert_eq!(coupling[0].co_changes, 2);
    assert_eq!(coupling[0].my_changes, 3);
    assert_eq!(coupling[0].confidence, 0.667);
    assert_eq!(coupling[0].last_co_change_at_s, 123);
}

#[test]
fn file_memories_cap_the_editor_payload() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    for i in 0..(super::enrichments::MEMORY_LIMIT + 5) {
        crate::memory_write::create_memory(conn, RepoMemoryCreate {
            kind: "Invariant".into(),
            title: format!("Root memory {i:02}"),
            body: "A root-scoped memory included for every indexed file".into(),
            confidence: "high".into(),
            created_by: Some("test".into()),
            source: None,
            payload_json: None,
            tags: Vec::new(),
            bind: RepoMemoryBindTarget {
                dir: Some(String::new()),
                ..RepoMemoryBindTarget::default()
            },
        })
        .unwrap();
    }
    crate::memory_write::create_memory(conn, RepoMemoryCreate {
        kind: "Risk".into(),
        title: "Direct file memory".into(),
        body: "Must not be evicted by broader directory memories".into(),
        confidence: "high".into(),
        created_by: Some("test".into()),
        source: None,
        payload_json: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget {
            path: Some("src/lib.rs".into()),
            ..RepoMemoryBindTarget::default()
        },
    })
    .unwrap();

    let memories = db.lens_file_memories("src/lib.rs").unwrap().memories;
    assert_eq!(memories.len(), super::enrichments::MEMORY_LIMIT);
    assert_eq!(memories[0].title, "Direct file memory");
}

#[test]
fn treemap_drops_clone_edges_with_stale_content_anchors() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    let endpoint = |path: &str| {
        conn.query_row(
            "SELECT file.id, file.sha256, symbol.start_byte
             FROM files file
             JOIN symbols symbol ON symbol.file_id = file.id
             WHERE file.path = ?1
             ORDER BY symbol.start_byte LIMIT 1",
            [path],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)),
        )
        .unwrap()
    };
    let (_a_file_id, a_sha, a_start) = endpoint("src/lib.rs");
    let (b_file_id, b_sha, b_start) = endpoint("src/other.rs");
    conn.execute(
        "INSERT INTO clone_graph_generations(
             generation, status, theta_floor, normalizer_kind, normalizer_version,
             source_revision, cursor_symbol_id, edges_written, started_at_ms, repo_id
         ) VALUES (4242, 'Complete', 0.7, 'baseline', ?1, 'test', 0, 1, 0, ?2)",
        params![rag_rat_clones::NORM_VERSION, db.active_repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_edges(
             build_generation, a_path, a_start_byte, a_file_sha,
             b_path, b_start_byte, b_file_sha, overlap,
             a_token_len, b_token_len, similarity, edge_source
         ) VALUES (4242, 'src/lib.rs', ?1, ?2, 'src/other.rs', ?3, ?4,
                   10, 10, 10, 1.0, 'struct_hash')",
        params![a_start, a_sha, b_start, b_sha],
    )
    .unwrap();
    db.set_repo_meta("clone_graph_live_generation", "4242").unwrap();

    let before = db.lens_treemap().unwrap().files;
    let lib = before.iter().find(|file| file.path == "src/lib.rs").unwrap();
    assert_eq!(lib.dup_partners, 1);

    conn.execute(
        "UPDATE clone_graph_generations SET normalizer_version = ?1 WHERE generation = 4242",
        [rag_rat_clones::NORM_VERSION + 1],
    )
    .unwrap();
    let incompatible = db.lens_treemap().unwrap().files;
    let lib = incompatible.iter().find(|file| file.path == "src/lib.rs").unwrap();
    assert_eq!(lib.dup_partners, 0, "obsolete normalizer generations are ineligible");
    conn.execute(
        "UPDATE clone_graph_generations SET normalizer_version = ?1 WHERE generation = 4242",
        [rag_rat_clones::NORM_VERSION],
    )
    .unwrap();

    conn.execute("UPDATE main.files SET sha256 = 'edited' WHERE id = ?1", [b_file_id]).unwrap();
    let after = db.lens_treemap().unwrap().files;
    let lib = after.iter().find(|file| file.path == "src/lib.rs").unwrap();
    assert_eq!(lib.dup_partners, 0);
    assert_eq!(lib.dup_max_similarity, 0.0);
}

#[test]
fn treemap_excludes_same_file_clone_partners_but_keeps_similarity() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    let endpoint = |offset: i64| {
        conn.query_row(
            "SELECT file.sha256, symbol.start_byte
             FROM files file
             JOIN symbols symbol ON symbol.file_id = file.id
             WHERE file.path = 'src/lib.rs'
             ORDER BY symbol.start_byte LIMIT 1 OFFSET ?1",
            [offset],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap()
    };
    let (a_sha, a_start) = endpoint(0);
    let (b_sha, b_start) = endpoint(1);
    conn.execute(
        "INSERT INTO clone_graph_generations(
             generation, status, theta_floor, normalizer_kind, normalizer_version,
             source_revision, cursor_symbol_id, edges_written, started_at_ms, repo_id
         ) VALUES (4242, 'Complete', 0.7, 'baseline', ?1, 'test', 0, 1, 0, ?2)",
        params![rag_rat_clones::NORM_VERSION, db.active_repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_edges(
             build_generation, a_path, a_start_byte, a_file_sha,
             b_path, b_start_byte, b_file_sha, overlap,
             a_token_len, b_token_len, similarity, edge_source
         ) VALUES (4242, 'src/lib.rs', ?1, ?2, 'src/lib.rs', ?3, ?4,
                   7, 8, 8, 0.875, 'token_overlap')",
        params![a_start, a_sha, b_start, b_sha],
    )
    .unwrap();
    db.set_repo_meta("clone_graph_live_generation", "4242").unwrap();

    let treemap = db.lens_treemap().unwrap().files;
    let lib = treemap.iter().find(|file| file.path == "src/lib.rs").unwrap();
    assert_eq!(lib.dup_partners, 0);
    assert_eq!(lib.dup_max_similarity, 0.875);
}

#[test]
fn file_papertrail_resolves_refs_and_selected_decision_anchors() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    let repo_id = db.active_repo_id.as_str();

    conn.execute(
        "INSERT INTO papertrail_items(
             tracker, project, item_kind, item_key, url, state, \
         title, body,
             synced_at_ms, repo_id, state_normalized
         ) VALUES (
             \
         'github', 'o/r', 'issue', '5', 'https://github.com/o/r/issues/5',
             'open', 'Tracked issue', 'body', 0, ?1, 'open'
         )",
        [repo_id],
    )
    .unwrap();
    for (source_text, discovered_at_ms) in [("newest #5", 2), ("older #5", 1)] {
        conn.execute(
            "INSERT INTO papertrail_refs(
                 tracker, project, item_key, item_kind, ref_kind, source_kind,
                 source_path, source_text, discovered_at_ms, repo_id
             ) VALUES ('github', 'o/r', '5', NULL, 'annotation', 'file',
                       'src/lib.rs', ?1, ?2, ?3)",
            params![source_text, discovered_at_ms, repo_id],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO papertrail_refs(
             tracker, project, item_key, item_kind, ref_kind, source_kind,
             source_path, source_text, discovered_at_ms, repo_id
         ) VALUES ('github', 'o/r', '9', NULL, 'annotation', 'file',
                   'src/lib.rs', 'unmirrored #9', 3, ?1)",
        [repo_id],
    )
    .unwrap();

    let target_logical: i64 = conn
        .query_row(
            "SELECT member.logical_symbol_id
             FROM logical_symbol_members member
             JOIN symbols symbol ON symbol.id = member.symbol_id
             WHERE symbol.name = 'target'
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill(
             tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
             root_issue, root_cause, decision_chosen, outcome_summary, outcome_status_model,
             fix_edge_source, thread_shape, outcome_claim_verified,
             decision_provenance_verified, anchors_qualified_count, revert_override,
             distilled_at_ms, repo_id
         ) VALUES (
             'github', 'o/r', 'issue', '5', 'sha256:5', 3,
             'Root issue', 'Root cause', 'Chosen fix', 'Outcome', 'landed',
             'provider', 'investigation', 1, 1, 1, 1, 10, ?1
         )",
        [repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_anchors(
             tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id,
             name, resolved, candidate_ordinal, selected, repo_id
         ) VALUES ('github', 'o/r', 'issue', '5', 'symbol', ?1,
                   'target', 1, 0, 1, ?2)",
        params![rag_rat_base::serde_big_id::format_sym_handle(target_logical), repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill(
             tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
             root_issue, fix_edge_source, thread_shape, anchors_qualified_count,
             distilled_at_ms, repo_id
         ) VALUES ('github', 'o/r', 'issue', '7', 'sha256:7', 3,
                   'File decision', 'text', 'investigation', 1, 20, ?1)",
        [repo_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_anchors(
             tracker, project, item_kind, item_key, anchor_kind, file_path, name,
             resolved, candidate_ordinal, selected, repo_id
         ) VALUES ('github', 'o/r', 'issue', '7', 'file', 'src/lib.rs',
                   'src/lib.rs', 1, 0, 1, ?1)",
        [repo_id],
    )
    .unwrap();

    let papertrail = db.lens_file_papertrail("src/lib.rs").unwrap();
    assert_eq!(papertrail.refs.len(), 2, "duplicate textual refs collapse by item identity");
    let issue = papertrail.refs.iter().find(|reference| reference.item_key == "5").unwrap();
    assert_eq!(issue.item_kind, "issue", "a nullable GitHub ref probes issue before PR");
    assert_eq!(issue.source_text, "newest #5");
    assert_eq!(issue.title.as_deref(), Some("Tracked issue"));
    assert_eq!(issue.state_normalized.as_deref(), Some("open"));
    let unmirrored = papertrail.refs.iter().find(|reference| reference.item_key == "9").unwrap();
    assert_eq!(unmirrored.url.as_deref(), Some("https://github.com/o/r/issues/9"));

    assert_eq!(papertrail.decisions.len(), 2);
    let symbol = papertrail.decisions.iter().find(|decision| decision.item_key == "5").unwrap();
    assert_eq!(
        (symbol.tracker.as_str(), symbol.project.as_str(), symbol.item_kind.as_str()),
        ("github", "o/r", "issue")
    );
    assert!(symbol.line.is_some());
    assert_eq!(symbol.title.as_deref(), Some("Tracked issue"));
    assert_eq!(
        symbol.outcome_status_model, "reverted",
        "the compatibility field carries effective status, not the overridden model claim"
    );
    assert_eq!(symbol.outcome_claim_verified, 1);
    assert_eq!(symbol.decision_provenance_verified, 1);
    let file = papertrail.decisions.iter().find(|decision| decision.item_key == "7").unwrap();
    assert_eq!(file.line, None);
    assert_eq!(file.root_issue.as_deref(), Some("File decision"));
}

/// A symbol anchor is a claim about where a decision belongs, and the file lane presents it on a
/// line. It may only surface where its symbol lives in the CURRENT index.
#[test]
fn file_papertrail_rejects_decision_anchors_that_do_not_hold_for_the_file() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    let repo_id = db.active_repo_id.as_str();

    let logical_symbol = |name: &str| -> i64 {
        conn.query_row(
            "SELECT member.logical_symbol_id
             FROM logical_symbol_members member
             JOIN symbols symbol ON symbol.id = member.symbol_id
             WHERE symbol.name = ?1
             LIMIT 1",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    };
    // `11` is anchored to a symbol that lives in another file; `13` is anchored to a symbol in the
    // requested file, but a remap left its anchor unresolved.
    for (item_key, symbol, resolved) in [("11", "other", 1), ("13", "target", 0)] {
        conn.execute(
            "INSERT INTO papertrail_distill(
                 tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                 root_issue, fix_edge_source, thread_shape, anchors_qualified_count,
                 distilled_at_ms, repo_id
             ) VALUES ('github', 'o/r', 'issue', ?1, 'sha256:' || ?1, 3,
                       'Decision', 'provider', 'investigation', 1, 30, ?2)",
            params![item_key, repo_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_anchors(
                 tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id,
                 name, resolved, candidate_ordinal, selected, repo_id
             ) VALUES ('github', 'o/r', 'issue', ?1, 'symbol', ?2, ?3, ?4, 0, 1, ?5)",
            params![
                item_key,
                rag_rat_base::serde_big_id::format_sym_handle(logical_symbol(symbol)),
                symbol,
                resolved,
                repo_id
            ],
        )
        .unwrap();
    }

    let requested = db.lens_file_papertrail("src/lib.rs").unwrap();
    assert!(
        requested.decisions.is_empty(),
        "neither a foreign-file nor an unresolved anchor may surface: {:?}",
        requested.decisions
    );
    let owning = db.lens_file_papertrail("src/other.rs").unwrap();
    assert_eq!(
        owning.decisions.iter().map(|decision| decision.item_key.as_str()).collect::<Vec<_>>(),
        ["11"],
        "the anchor surfaces on the file its symbol currently belongs to"
    );
    assert!(owning.decisions[0].line.is_some(), "the line is the symbol's current position");
}

#[test]
fn file_papertrail_deduplicates_items_before_applying_the_limit() {
    let (_temp, config) = indexed_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let conn = db.storage.connection();
    for ordinal in 0..60 {
        conn.execute(
            "INSERT INTO papertrail_refs(
                 tracker, project, item_key, item_kind, ref_kind, source_kind,
                 source_path, source_text, discovered_at_ms, repo_id
             ) VALUES ('github', 'o/r', 'duplicate', 'issue', 'annotation', 'file',
                       'src/lib.rs', ?1, ?2, ?3)",
            params![format!("duplicate {ordinal}"), 1_000 + ordinal, db.active_repo_id],
        )
        .unwrap();
    }
    for ordinal in 0..49 {
        conn.execute(
            "INSERT INTO papertrail_refs(
                 tracker, project, item_key, item_kind, ref_kind, source_kind,
                 source_path, source_text, discovered_at_ms, repo_id
             ) VALUES ('github', 'o/r', ?1, 'issue', 'annotation', 'file',
                       'src/lib.rs', ?2, ?3, ?4)",
            params![
                format!("unique-{ordinal}"),
                format!("unique {ordinal}"),
                ordinal,
                db.active_repo_id
            ],
        )
        .unwrap();
    }

    let refs = db.lens_file_papertrail("src/lib.rs").unwrap().refs;
    assert_eq!(refs.len(), 50);
    assert_eq!(refs.iter().filter(|reference| reference.item_key == "duplicate").count(), 1);
    assert!(refs.iter().any(|reference| reference.item_key == "unique-0"));
}

/// Two `run` overloads in one file. Both qualify as `src/lib.rs::run`; only their signatures —
/// and therefore their logical-symbol handles — differ, which is exactly the case a qualified-name
/// selector cannot express.
const OVERLOAD_SOURCE: &str = r#"
pub struct Alpha;
pub struct Beta;

pub fn alpha_leaf() {}
pub fn beta_leaf() {}

impl Alpha {
    pub fn run(&self) { alpha_leaf(); }
}

impl Beta {
    pub fn run(&self, extra: i64) { beta_leaf(); let _ = extra; }
}

pub fn calls_alpha(alpha: &Alpha) { alpha.run(); }
pub fn calls_beta(beta: &Beta) { beta.run(1); }
"#;

/// Each overload's own callers and callees are reachable only through its handle: the qualified
/// name they share reports the union of both, in both directions.
#[test]
fn hop_selectors_separate_overloads_by_handle_and_unite_them_by_qualified_name() {
    let (_temp, config) = overloaded_config();
    let db = IndexDatabase::try_open_config_read_only(&config).unwrap().expect("read-only index");

    let (alpha, beta) = overload_handles(&db);
    let qname = "src/lib.rs::run".to_string();

    let alpha_callers =
        db.lens_symbol_callers(&LensHopSelector::Handle(alpha), 50).unwrap().expect("alpha");
    assert_eq!(hop_names(&alpha_callers.callers), ["calls_alpha"]);
    assert_eq!(alpha_callers.resolved_by, LensHopResolvedBy::Id);
    assert_eq!(alpha_callers.matched_symbols, 1);
    let beta_callers =
        db.lens_symbol_callers(&LensHopSelector::Handle(beta), 50).unwrap().expect("beta");
    assert_eq!(hop_names(&beta_callers.callers), ["calls_beta"]);

    let shared_callers = db
        .lens_symbol_callers(&LensHopSelector::QualifiedName(qname.clone()), 50)
        .unwrap()
        .expect("a qualified name always resolves");
    assert_eq!(hop_names(&shared_callers.callers), ["calls_alpha", "calls_beta"]);
    assert_eq!(shared_callers.resolved_by, LensHopResolvedBy::Ref);
    assert_eq!(
        shared_callers.matched_symbols, 2,
        "the fallback must report how many symbols the name it was given covers"
    );

    let alpha_callees =
        db.lens_symbol_callees(&LensHopSelector::Handle(alpha), 50).unwrap().expect("alpha");
    assert_eq!(hop_names(&alpha_callees.callees), ["alpha_leaf"]);
    let beta_callees =
        db.lens_symbol_callees(&LensHopSelector::Handle(beta), 50).unwrap().expect("beta");
    assert_eq!(hop_names(&beta_callees.callees), ["beta_leaf"]);
    let shared_callees = db
        .lens_symbol_callees(&LensHopSelector::QualifiedName(qname), 50)
        .unwrap()
        .expect("a qualified name always resolves");
    assert_eq!(hop_names(&shared_callees.callees), ["alpha_leaf", "beta_leaf"]);
    assert_eq!(shared_callees.matched_symbols, 2);
}

/// A handle that names nothing in the active checkout — held across a rename, or minted in another
/// checkout — is reported as absent. Answering it with an empty hop list would read as "this
/// symbol has no callers", and falling back to the qualified name would reintroduce the union.
#[test]
fn an_unresolvable_handle_is_absent_rather_than_empty() {
    let (_temp, config) = overloaded_config();
    let db = IndexDatabase::try_open_config_read_only(&config).unwrap().expect("read-only index");

    assert!(db.lens_symbol_callers(&LensHopSelector::Handle(i64::MAX), 50).unwrap().is_none());
    assert!(db.lens_symbol_callees(&LensHopSelector::Handle(i64::MAX), 50).unwrap().is_none());
}

/// The fallback's traversal also resolves a BARE SHORT NAME, through the unique-short-name arm its
/// predicate carries, so `matched_symbols` has to model that arm as well: counting only the
/// symbols whose QUALIFIED name is the selector reports zero beside a non-empty hop list, which
/// reads as "the selector matched nothing" — the opposite of the ambiguity signal the field
/// exists to carry. When the short name is ambiguous the arm is off and nothing is expanded, so
/// zero is then the truthful answer.
#[test]
fn a_short_name_fallback_counts_the_symbol_its_traversal_expanded_to() {
    let (_temp, config) = overloaded_config();
    let db = IndexDatabase::try_open_config_read_only(&config).unwrap().expect("read-only index");

    let unique = db
        .lens_symbol_callers(&LensHopSelector::QualifiedName("alpha_leaf".into()), 50)
        .unwrap()
        .expect("a qualified name always resolves");
    assert_eq!(hop_names(&unique.callers), ["run"], "the short name resolves through the index");
    assert_eq!(unique.resolved_by, LensHopResolvedBy::Ref);
    assert_eq!(
        unique.matched_symbols, 1,
        "a request answered with hops must not report that it matched no symbol"
    );

    let ambiguous = db
        .lens_symbol_callers(&LensHopSelector::QualifiedName("run".into()), 50)
        .unwrap()
        .expect("a qualified name always resolves");
    assert!(ambiguous.callers.is_empty(), "an ambiguous short name expands to nothing");
    assert_eq!(ambiguous.matched_symbols, 0);
}

/// Both selectors read the symbol table through the connection's `files` view, so a sibling
/// checkout's rows are invisible to them: its same-named symbol must not inflate the fallback's
/// ambiguity report, and its handle must not resolve here. The active checkout keeps answering.
#[test]
fn hop_selectors_see_only_the_active_checkouts_symbols() {
    let (_temp, config) = overloaded_config();
    let db = IndexDatabase::open_config(&config).unwrap();
    let (alpha, _) = overload_handles(&db);
    let conn = db.storage.connection();

    // A file belonging to another checkout of this repo, carrying a THIRD `src/lib.rs::run` under
    // its own logical symbol.
    conn.execute(
        "INSERT INTO main.files(
             path, language, kind, sha256, modified_at_ms, indexed_at_ms,
             commit_sha, worktree_id, repo_id, generation
         ) VALUES ('sibling.rs', 'rust', 'source', 'sibling-sha', 0, 0, 'sibling-commit', '', ?1, \
         ?2)",
        params![db.active_repo_id, db.active_generation],
    )
    .unwrap();
    let sibling_file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO symbols(
             file_id, language, name, qualified_name_id, kind,
             start_byte, end_byte, start_line, end_line
         ) VALUES (
             ?1, 'rust', 'run',
             (SELECT id FROM name_strings WHERE value = 'src/lib.rs::run'),
             'function', 0, 10, 1, 1
         )",
        [sibling_file_id],
    )
    .unwrap();
    let sibling_symbol_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO logical_symbols(language, path, logical_name, qualified_name_id, kind,
             variant_count, group_reason)
         VALUES ('rust', 'sibling.rs', 'run',
             (SELECT id FROM name_strings WHERE value = 'src/lib.rs::run'), 'function', 1, 'test')",
        [],
    )
    .unwrap();
    let sibling_logical_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, end_line)
         VALUES (?1, ?2, 1, 1)",
        [sibling_logical_id, sibling_symbol_id],
    )
    .unwrap();

    let shared = db
        .lens_symbol_callers(&LensHopSelector::QualifiedName("src/lib.rs::run".into()), 50)
        .unwrap()
        .expect("a qualified name always resolves");
    assert_eq!(
        shared.matched_symbols, 2,
        "a sibling checkout's symbol is not one this checkout's name covers"
    );
    assert_eq!(hop_names(&shared.callers), ["calls_alpha", "calls_beta"]);
    assert!(
        db.lens_symbol_callers(&LensHopSelector::Handle(sibling_logical_id), 50).unwrap().is_none(),
        "a handle whose only member lives in another checkout must read as absent"
    );
    assert!(db.lens_symbol_callers(&LensHopSelector::Handle(alpha), 50).unwrap().is_some());
}

/// The file lanes are where a CodeLens is built, so every row has to carry the handle its own hop
/// request needs — and two overloads must not share it.
#[test]
fn file_lanes_carry_a_distinct_symbol_handle_per_overload() {
    let (_temp, config) = overloaded_config();
    let db = IndexDatabase::try_open_config_read_only(&config).unwrap().expect("read-only index");

    let symbols = db.lens_file_symbols("src/lib.rs").unwrap().symbols;
    let runs = symbols.iter().filter(|symbol| symbol.name == "run").collect::<Vec<_>>();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|symbol| symbol.qname.as_deref() == Some("src/lib.rs::run")));
    assert_ne!(runs[0].logical_symbol_id, runs[1].logical_symbol_id);
    assert!(runs.iter().all(|symbol| symbol.logical_symbol_id.is_some()));

    let graph = db.lens_file_graph("src/lib.rs").unwrap().symbols;
    let graph_runs = graph
        .iter()
        .filter(|symbol| symbol.name == "run")
        .map(|symbol| symbol.logical_symbol_id)
        .collect::<Vec<_>>();
    assert_eq!(
        graph_runs,
        runs.iter().map(|symbol| symbol.logical_symbol_id).collect::<Vec<_>>(),
        "both file lanes must hand out the same handle for the same row"
    );

    // The handle crosses the wire as its opaque `sym_<hex>` token, never as a number a JSON client
    // could round past 2^53.
    let wire = serde_json::to_value(db.lens_file_graph("src/lib.rs").unwrap()).unwrap();
    let ids = wire["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "run")
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    assert!(ids.iter().all(|id| id.starts_with("sym_")), "{ids:?}");
    assert_ne!(ids[0], ids[1]);
}

fn hop_names(hops: &[super::LensSymbolHop]) -> Vec<String> {
    let mut names = hops.iter().map(|hop| hop.name.clone()).collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

/// The two handles behind `src/lib.rs::run`, alpha's first.
fn overload_handles(db: &IndexDatabase) -> (i64, i64) {
    let mut runs = db
        .lens_file_symbols("src/lib.rs")
        .unwrap()
        .symbols
        .into_iter()
        .filter(|symbol| symbol.name == "run")
        .collect::<Vec<_>>();
    runs.sort_by_key(|symbol| symbol.start_line);
    assert_eq!(runs.len(), 2, "the fixture must index both overloads");
    (runs[0].logical_symbol_id.unwrap(), runs[1].logical_symbol_id.unwrap())
}

/// Index [`OVERLOAD_SOURCE`] and bind each `run` call site to the overload it actually targets.
///
/// The heuristic resolver deliberately leaves a same-name method call unresolved — with two
/// candidates it refuses to guess — so on the caller side no natural edge distinguishes the
/// overloads. Binding them here is what a compiler oracle pass produces, and it is the only way
/// the caller direction can be observed at all.
fn overloaded_config() -> (tempfile::TempDir, Config) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), OVERLOAD_SOURCE).unwrap();
    let mut config = Config::minimal_for_database(root.join("index.sqlite"), root);
    config.database_key_pinned = true;
    config.targets = vec![ResolvedTarget {
        name: "rust".into(),
        language: Language::Rust,
        directories: vec![PathBuf::from("src")],
        include: vec!["**/*.rs".into()],
        exclude: Vec::new(),
        kind: TargetKind::Source,
    }];
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    for (caller, callee) in [("calls_alpha", "alpha_leaf"), ("calls_beta", "beta_leaf")] {
        // The overload each caller means is the one whose body calls that caller's leaf.
        let target: i64 = conn
            .query_row(
                "SELECT run.id
                 FROM symbols run
                 JOIN files ON files.id = run.file_id
                 JOIN edges body ON body.from_symbol_id = run.id
                 JOIN symbols leaf ON leaf.id = body.to_symbol_id
                 WHERE run.name = 'run' AND leaf.name = ?1",
                [callee],
                |row| row.get(0),
            )
            .unwrap();
        let edge: i64 = conn
            .query_row(
                "SELECT edges.id
                 FROM edges
                 JOIN symbols source ON source.id = edges.from_symbol_id
                 WHERE edges.edge_kind = 'calls_name' AND edges.to_name = 'run'
                   AND source.name = ?1",
                [caller],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute("UPDATE edges_data SET to_symbol_id = ?1 WHERE id = ?2", [target, edge])
            .unwrap();
    }
    drop(db);
    (temp, config)
}

fn indexed_config() -> (tempfile::TempDir, Config) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), SOURCE).unwrap();
    fs::write(root.join("src/other.rs"), OTHER_SOURCE).unwrap();
    // Paths whose case aliases only exist outside ASCII. Cyrillic and Greek have no canonical
    // decomposition, so the indexed spellings survive filesystems that normalize names.
    fs::write(root.join(UNICODE_PATH), UNICODE_SOURCE).unwrap();
    fs::write(root.join(FOLDED_PATH), FOLDED_SOURCE).unwrap();
    fs::write(root.join(COMPOSED_PATH), COMPOSED_SOURCE).unwrap();
    let mut config = Config::minimal_for_database(root.join("index.sqlite"), root);
    config.database_key_pinned = true;
    config.targets = vec![ResolvedTarget {
        name: "rust".into(),
        language: Language::Rust,
        directories: vec![PathBuf::from("src")],
        include: vec!["**/*.rs".into()],
        exclude: Vec::new(),
        kind: TargetKind::Source,
    }];
    drop(IndexDatabase::rebuild(&config).unwrap());
    (temp, config)
}
