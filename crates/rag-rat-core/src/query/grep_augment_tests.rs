use std::collections::HashSet;

use rag_rat_db::schema;
use rag_rat_query::SearchHit;
use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};
use rusqlite::Connection;

use super::*;

fn lexical_hit(path: &str, start: i64, end: i64, score: f64) -> SearchHit {
    SearchHit {
        chunk_id: 0,
        path: path.to_string(),
        language: "rust".to_string(),
        kind: "chunk".to_string(),
        start_line: start,
        end_line: end,
        symbol_path: None,
        score,
        retrieval_mode: "lexical".to_string(),
        summary: format!("{path} summary"),
        graph: None,
        score_components: None,
        importance: None,
        distilled_records: Vec::new(),
    }
}

/// #139: the same chunk returned more than once (capped at MAX_LEXICAL_HITS) rendered as N
/// identical "Indexed hits" lines. The dedup keeps one line per (path, start, end); the
/// relevance floor still drops weak hits.
#[test]
fn lexical_lines_dedup_chunks_and_apply_floor() {
    let hits = vec![
        lexical_hit("a.rs", 1, 9, 1.0),
        lexical_hit("a.rs", 1, 9, 1.0), // exact duplicate chunk
        lexical_hit("a.rs", 1, 9, 1.0), // and again — would have filled all 3 slots
        lexical_hit("b.rs", 2, 3, 0.9), // distinct, above floor (0.6 * 1.0)
        lexical_hit("c.rs", 4, 5, 0.1), // below floor → dropped
    ];
    let lines = lexical_lines_from_hits(hits);
    assert_eq!(lines.len(), 2, "a.rs deduped to one, c.rs floored out: {lines:?}");
    assert!(lines[0].contains("a.rs:1-9"), "first line is a.rs once: {lines:?}");
    assert!(lines[1].contains("b.rs:2-3"), "second is b.rs: {lines:?}");
    assert!(!lines.iter().any(|l| l.contains("c.rs")), "weak hit dropped: {lines:?}");
}

/// #139 (symbol lane): two symbol rows sharing (path, qualified_name) — overloads / cfg
/// variants / re-export rows — must render once, not once per row.
#[test]
fn symbol_lane_dedups_duplicate_rows() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('src/a.rs', 'rust', 'source', 'h', 0, 0)",
        [],
    )
    .unwrap();
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('a::foo')", []).unwrap();
    for _ in 0..3 {
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                     end_byte, signature, docs)
                 VALUES (1, 'rust', 'foo', (SELECT id FROM name_strings WHERE value = 'a::foo'),
                         'function', 0, 10, 'fn foo()', NULL)",
            [],
        )
        .unwrap();
    }
    let out = compose(
        &conn,
        "foo",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("symbol lane augments");
    assert_eq!(
        out.context.matches("`a::foo`").count(),
        1,
        "duplicate symbol rows must render once:\n{}",
        out.context
    );
}

fn seeded_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('src/watch.rs', 'rust', 'source', 'abc', 0, 0)",
        [],
    )
    .unwrap();
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('watch::watcher_main')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                 end_byte, signature, docs)
             VALUES (1, 'rust', 'watcher_main',
                     (SELECT id FROM name_strings WHERE value = 'watch::watcher_main'),
                     'function', 0, 100, 'fn watcher_main(config: Config)', NULL)",
        [],
    )
    .unwrap();
    let chunk_text = "fn watcher_main() { /* election retry loop */ }";
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                start_line, end_line, text_hash)
             VALUES (1, 'symbol', 'watch::watcher_main', 0, 100, 1, 20, 'h1')",
        [],
    )
    .unwrap();
    let chunk_id = conn.last_insert_rowid();
    // chunks.text is gone (#77 Phase 2): seed the compressed chunk_text blob (readers INNER
    // JOIN it) and the contentless chunk_fts tokens.
    rag_rat_db::chunk_text_store::seed_chunk_text(&conn, chunk_id, chunk_text).unwrap();
    conn.execute("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)", rusqlite::params![
        chunk_id, chunk_text
    ])
    .unwrap();
    // One caller edge and one callee edge for the counts line.
    conn.execute(
        "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                               target_qualified_name, edge_kind, confidence)
             VALUES (1, NULL, 1, 'watcher_main', 'watch::watcher_main', 'calls_name', 'exact')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                               target_qualified_name, edge_kind, confidence)
             VALUES (1, 1, NULL, 'maintenance_pass', NULL, 'calls_name', 'name_only')",
        [],
    )
    .unwrap();
    crate::memory_write::create_memory(&conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "One watcher per worktree".to_string(),
        body: "The election lock guarantees a single watcher; never bind without it.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: None,
        tags: vec![],
        payload_json: None,
        bind: RepoMemoryBindTarget {
            symbol_id: Some(1),
            logical_symbol_id: None,
            chunk_id: None,
            edge_id: None,
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            tracker: None,
            project: None,
            item_key: None,
            start_logical_symbol_id: None,
            end_logical_symbol_id: None,
            edge_sequence_hash: None,
            path_summary: None,
            edge_path: None,
            dir: None,
        },
    })
    .unwrap();
    // Sync chunk_fts directly — external-content FTS5 needs explicit INSERT.
    conn.execute(
        "INSERT INTO chunk_fts(rowid, text)
             VALUES (?1, 'fn watcher_main() { /* election retry loop */ }')",
        [chunk_id],
    )
    .unwrap();
    conn
}

/// A memory reachable ONLY through the FTS lane: bound to the seeded file's path, which
/// `compose` never consults here because these tests pass `search_path: None`.
#[test]
fn a_drifted_memory_renders_its_drift_in_the_status_slot() {
    // grep- and read-augment render a raw list without partitioning it, so the status slot is
    // the only place a reader learns the anchor moved on. Kept next to the persisted status
    // rather than replacing it: the row really is active, and this says what is untrustworthy.
    let memory = memory::RepoMemory {
        memory_id: "m1".to_string(),
        kind: "Invariant".to_string(),
        title: "t".to_string(),
        body: "b".to_string(),
        summary: None,
        verdict: None,
        confidence: "high".to_string(),
        status: "active".to_string(),
        created_by: None,
        created_at_ms: 0,
        updated_at_ms: 0,
        source: "agent".to_string(),
        payload_json: None,
        source_text_hash: None,
        input_hash: None,
        memory_version: "v1".to_string(),
        synced_anchor_drifted: true,
        bindings: Vec::new(),
        call_paths: Vec::new(),
        tags: Vec::new(),
    };
    let rendered = memory_render_item(memory).line;
    assert!(
        rendered.contains("anchor drifted"),
        "the drift must reach the rendered line: {rendered}"
    );
}

#[test]
fn a_memory_found_only_by_prose_still_renders_its_anchor_drift() {
    // The lexical lane hydrates through plain `memory_by_id`, so a memory the pattern reaches
    // by prose alone arrives unmarked. Rendering it beside the path/symbol lanes would present
    // the same memory as current or drifted purely by how it was found.
    let conn = seeded_conn();
    let memory = seed_fts_memory(
        &conn,
        "Zebraglyph routing pins quokkaform",
        "zebraglyph quokkaform lorikeetwise — the zebraglyph router pins quokkaform.",
    );
    // Make it a synced memory whose stamped text this checkout no longer holds. The pattern
    // below matches its prose only: nothing names `src/watch.rs` or a symbol in it.
    conn.execute(
        "UPDATE repo_memories SET origin = 'synced', source_text_hash = 'stamped-then' WHERE id = \
         ?1",
        rusqlite::params![memory.memory_id],
    )
    .unwrap();

    let out = compose(
        &conn,
        "zebraglyph quokkaform lorikeetwise",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("payload expected");
    assert!(
        out.context.contains("Zebraglyph routing pins quokkaform"),
        "the prose match surfaces: {}",
        out.context
    );
    assert!(
        out.context.contains("anchor drifted"),
        "and carries its drift, though no drive-by reader hydrated it: {}",
        out.context
    );
}

fn seed_fts_memory(conn: &Connection, title: &str, body: &str) -> memory::RepoMemory {
    crate::memory_write::create_memory(conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: title.to_string(),
        body: body.to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: None,
        tags: vec![],
        payload_json: None,
        bind: RepoMemoryBindTarget {
            path: Some("src/watch.rs".to_string()),
            ..RepoMemoryBindTarget::default()
        },
    })
    .unwrap()
    .memory
}

/// #1200: the FTS memory lane had no relevance gate, so one common token in the normalized
/// pattern surfaced up to `MAX_MEMORIES` unrelated memories. The magnitudes are the ones
/// `compose_memory_lane_drops_weak_fts_co_matches` actually produces — the co-match's only
/// token is corpus-wide, so fts5 clamps its idf to 1e-6. bm25 is NEGATIVE and lower-is-better,
/// so a sign slip here would keep exactly the memory this drops.
#[test]
fn memory_floor_keeps_the_strong_match_and_drops_the_weak_tail() {
    let conn = seeded_conn();
    let strong = seed_fts_memory(&conn, "Strong bm25 match", "matches every query term");
    let weak = seed_fts_memory(&conn, "Weak bm25 match", "matches one corpus-wide term");
    let kept = memories_above_relative_floor(vec![(strong, -1.63), (weak, -1.02e-6)]);
    assert_eq!(kept.len(), 1, "only the strong match survives: {kept:?}");
    assert_eq!(kept[0].title, "Strong bm25 match");
}

/// The floor's VALUE is load-bearing, not just its existence: two orders of magnitude below the
/// best hit, so the tf/body-length spread between equally-relevant matches never reaches it.
#[test]
fn memory_floor_cuts_two_orders_of_magnitude_below_the_best_hit() {
    let conn = seeded_conn();
    let best = seed_fts_memory(&conn, "Best bm25 match", "the strongest match");
    let above = seed_fts_memory(&conn, "Just above the floor", "same terms, a longer body");
    let below = seed_fts_memory(&conn, "Just below the floor", "an incidental co-match");
    let kept = memories_above_relative_floor(vec![(best.clone(), -10.0), (above, -0.11)]);
    assert_eq!(kept.len(), 2, "0.011 of the best hit survives: {kept:?}");
    let kept = memories_above_relative_floor(vec![(best, -10.0), (below, -0.09)]);
    assert_eq!(kept.len(), 1, "0.009 of the best hit is dropped: {kept:?}");
}

/// The false-drop guard on the constant. These are the magnitudes measured on a 43-memory
/// corpus for the 3-term query `zebraglyph quokkaform rebuildpass`: the best hit is a short
/// all-terms memory, the second is an all-terms memory whose ~1 300-char body dilutes it to
/// 0.098 of the best. A floor anywhere near a tenth of the best hit discards that real match,
/// which is worse than the noise a tighter floor would remove.
#[test]
fn memory_floor_keeps_a_length_diluted_exact_match() {
    let conn = seeded_conn();
    let best = seed_fts_memory(&conn, "Short exact match", "every query term, briefly");
    let diluted = seed_fts_memory(&conn, "Long-bodied exact match", "every query term, at length");
    let kept = memories_above_relative_floor(vec![(best, -10.005), (diluted, -0.981)]);
    assert_eq!(kept.len(), 2, "body length must not disqualify an exact match: {kept:?}");
}

/// The gate is a RELATIVE floor, never a minimum score: the best hit is always its own
/// reference, so a lone match surfaces however weak it is in absolute terms.
#[test]
fn memory_floor_keeps_a_lone_hit_however_weak() {
    let conn = seeded_conn();
    let weak = seed_fts_memory(&conn, "Weak bm25 match", "matches one incidental term");
    assert_eq!(memories_above_relative_floor(vec![(weak, -0.01)]).len(), 1);
}

/// End-to-end: a pattern whose terms one memory answers fully and another merely brushes
/// injects only the former.
#[test]
fn compose_memory_lane_drops_weak_fts_co_matches() {
    let conn = seeded_conn();
    seed_fts_memory(
        &conn,
        "Zebraglyph routing pins quokkaform",
        "zebraglyph quokkaform lorikeetwise — the zebraglyph router pins quokkaform to \
         lorikeetwise on every zebraglyph rebuild.",
    );
    seed_fts_memory(
        &conn,
        "Unrelated cache eviction note",
        "The cache evicts on write; it happens to mention lorikeetwise once.",
    );
    let out = compose(
        &conn,
        "zebraglyph quokkaform lorikeetwise",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("payload expected");
    assert!(
        out.context.contains("Zebraglyph routing pins quokkaform"),
        "the strong match surfaces: {}",
        out.context
    );
    assert!(
        !out.context.contains("Unrelated cache eviction note"),
        "the weak co-match is floored out: {}",
        out.context
    );
}

/// End-to-end in the regime the two small-corpus tests never reach: a shared token present in
/// 10 of 43 memories keeps a POSITIVE idf, so the co-matches land at ~0.13 of the best hit
/// rather than the ~1e-6 a corpus-wide token clamps to. The gate is scoped to the clamped case
/// and deliberately does not fire here — bm25 magnitude cannot separate these co-matches from
/// a length-diluted exact match, so `MAX_MEMORIES` is what bounds them. This pins that scope:
/// a future tightening that starts dropping hits here is dropping real matches with them.
#[test]
fn compose_memory_lane_gate_is_scoped_to_corpus_wide_tokens() {
    let conn = seeded_conn();
    seed_fts_memory(
        &conn,
        "Zebraglyph routing pins quokkaform",
        "zebraglyph quokkaform rebuildpass — the zebraglyph router pins quokkaform on every \
         rebuildpass.",
    );
    for i in 0..10 {
        seed_fts_memory(
            &conn,
            &format!("Rebuildpass note {i}"),
            "The rebuildpass drains the queue before the next tick; unrelated to routing.",
        );
    }
    for i in 0..30 {
        seed_fts_memory(&conn, &format!("Filler note {i}"), "Nothing relevant lives here.");
    }
    let out = compose(
        &conn,
        "zebraglyph quokkaform rebuildpass",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("payload expected");
    assert!(
        out.context.contains("Zebraglyph routing pins quokkaform"),
        "the all-terms match surfaces: {}",
        out.context
    );
    assert!(
        out.context.contains("Rebuildpass note"),
        "a positive-idf co-match is NOT floored out — only the cap bounds it: {}",
        out.context
    );
}

/// End-to-end counterpart: the same weak memory is the ONLY hit for its own rare token, so it
/// still surfaces — proof the floor stayed relative rather than becoming an absolute gate.
#[test]
fn compose_memory_lane_surfaces_a_lone_weak_match() {
    let conn = seeded_conn();
    seed_fts_memory(
        &conn,
        "Unrelated cache eviction note",
        "The cache evicts on write; it happens to mention kestrelmark once.",
    );
    let out = compose(
        &conn,
        "kestrelmark",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("payload expected");
    assert!(
        out.context.contains("Unrelated cache eviction note"),
        "a lone weak match still surfaces: {}",
        out.context
    );
}

/// Session dedupe must not move the floor: the reference is the best hit for the QUERY, so the
/// weak co-match stays floored out even while the strong hit is suppressed as already-seen.
/// Gating the survivors instead would re-admit the weak tail for the whole resurface window.
#[test]
fn memory_floor_ignores_session_dedupe() {
    let conn = seeded_conn();
    let strong = seed_fts_memory(
        &conn,
        "Zebraglyph routing pins quokkaform",
        "zebraglyph quokkaform lorikeetwise — the zebraglyph router pins quokkaform to \
         lorikeetwise on every zebraglyph rebuild.",
    );
    seed_fts_memory(
        &conn,
        "Unrelated cache eviction note",
        "The cache evicts on write; it happens to mention lorikeetwise once.",
    );
    let dedupe = DedupeFilter {
        memory_ids: HashSet::from([strong.memory_id.clone()]),
        symbol_keys: HashSet::new(),
    };
    let out = compose(
        &conn,
        "zebraglyph quokkaform lorikeetwise",
        None,
        &dedupe,
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap();
    // Nothing else in the seeded corpus answers this pattern, so suppressing both the
    // already-seen hit and the floored co-match legitimately leaves no payload at all.
    let context = out.map(|payload| payload.context).unwrap_or_default();
    assert!(
        !context.contains("Unrelated cache eviction note"),
        "the weak co-match stays floored out while the strong hit is deduped: {context}"
    );
}

#[test]
fn compose_identifier_pattern_yields_symbol_and_memory() {
    let conn = seeded_conn();
    let out = compose(
        &conn,
        r"watcher_main\b",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("payload expected");
    assert!(out.context.contains("src/watch.rs"), "symbol location present");
    assert!(out.context.contains("One watcher per worktree"), "memory title present");
    let memory_pos = out.context.find("One watcher per worktree").unwrap();
    let symbol_pos = out.context.find("src/watch.rs").unwrap();
    assert!(memory_pos < symbol_pos, "memories render before symbols");
    assert_eq!(out.memory_ids.len(), 1);
    assert_eq!(out.symbol_keys.len(), 1);
    assert!(out.context.len() <= MAX_CONTEXT_CHARS);
}

#[test]
fn compose_summary_surface_renders_the_summary_and_verdict_not_the_full_body() {
    let conn = seeded_conn();
    // Seed a dream summary + verdict for the seeded memory, keyed on its id, repo scope, and
    // current content_hash / prompt versions — exactly what the surfacing hydrator gates on.
    let (id, repo_id): (String, String) = conn
        .query_row("SELECT id, repo_id FROM repo_memories LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    let title = "One watcher per worktree";
    let body = "The election lock guarantees a single watcher; never bind without it.";
    conn.execute(
        "INSERT INTO memory_note_summaries(memory_id, repo_id, content_hash, summary, \
         prompt_version, generated_at_ms) VALUES (?1,?2,?3,?4,?5,0)",
        rusqlite::params![
            id,
            repo_id,
            rag_rat_query::memory::evidence::note_content_hash(title, body),
            "Election lock ensures exactly one watcher; bind only under it.",
            rag_rat_query::memory::evidence::COMPACT_PROMPT_VERSION
        ],
    )
    .unwrap();
    let inputs =
        rag_rat_query::memory::evidence::checked_inputs_hash(&conn, &id, &Some(repo_id.clone()))
            .unwrap();
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, \
         checked_against_commit, checked_inputs_hash, prompt_version, checked_at_ms) VALUES \
         (?1,?2,?3,'diverged',NULL,?4,?5,0)",
        rusqlite::params![
            id,
            repo_id,
            rag_rat_query::memory::evidence::note_content_hash(title, body),
            inputs,
            rag_rat_query::memory::evidence::VERDICT_PROMPT_VERSION
        ],
    )
    .unwrap();

    let out = compose(
        &conn,
        r"watcher_main\b",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Summary,
    )
    .unwrap()
    .expect("payload expected");
    assert!(
        out.context.contains("Election lock ensures exactly one watcher"),
        "the summary renders in place of the body: {}",
        out.context
    );
    assert!(
        !out.context.contains("never bind without it"),
        "the full body is deferred under summary: {}",
        out.context
    );
    assert!(out.context.contains("diverged"), "the verdict marker renders: {}", out.context);
    assert!(out.context.contains(title), "the title still renders: {}", out.context);
}

/// The default surface with NO summary rows — dream disabled (the default), never run, or every
/// summary invalidated by a prompt-version bump. The digest gives a memory ONE line, so it has
/// one prose slot: a body the surface withheld is a pointer to `memory_show`, and spending that
/// slot on the marker costs the budget of a real gist to say nothing.
#[test]
fn compose_summary_surface_shows_a_short_body_and_renders_a_deferred_one_title_only() {
    let conn = seeded_conn();
    // A second memory on the same symbol, one word OVER the summary envelope, so the surface
    // defers its body instead of showing it whole.
    let long_body =
        vec!["padding"; rag_rat_query::memory::evidence::SUMMARY_MAX_WORDS + 1].join(" ");
    crate::memory_write::create_memory(&conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "Deferred until compaction runs".to_string(),
        body: long_body,
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: None,
        tags: vec![],
        payload_json: None,
        bind: RepoMemoryBindTarget { symbol_id: Some(1), ..Default::default() },
    })
    .unwrap();

    let out = compose(
        &conn,
        r"watcher_main\b",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Summary,
    )
    .unwrap()
    .expect("payload expected");
    assert!(
        out.context.contains("The election lock guarantees a single watcher"),
        "a note inside the envelope will never be summarized, so its body is its gist: {}",
        out.context
    );
    assert!(
        out.context.contains("Deferred until compaction runs"),
        "the deferred memory still renders its title: {}",
        out.context
    );
    assert!(
        !out.context.contains("body elided"),
        "the elision marker is a pointer, never a gist: {}",
        out.context
    );
    assert!(
        !out.context.contains("padding"),
        "the deferred body itself never renders: {}",
        out.context
    );
}

/// A summary is bounded in WORDS (150), which is worth ~1 kB — six times the per-line gist
/// budget the hook renders bodies under. Both sources are prose in the same slot, so both are
/// clamped: the digest costs the same whichever one filled it.
#[test]
fn compose_summary_surface_clamps_a_long_summary_to_the_line_budget() {
    let conn = seeded_conn();
    let (id, repo_id): (String, String) = conn
        .query_row("SELECT id, repo_id FROM repo_memories LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    let title = "One watcher per worktree";
    let body = "The election lock guarantees a single watcher; never bind without it.";
    let long_summary = format!("Election lock first. {} Tailmarker.", "filler ".repeat(60));
    assert!(long_summary.chars().count() > MAX_MEMORY_BODY_CHARS, "the clamp must engage");
    conn.execute(
        "INSERT INTO memory_note_summaries(memory_id, repo_id, content_hash, summary, \
         prompt_version, generated_at_ms) VALUES (?1,?2,?3,?4,?5,0)",
        rusqlite::params![
            id,
            repo_id,
            rag_rat_query::memory::evidence::note_content_hash(title, body),
            long_summary,
            rag_rat_query::memory::evidence::COMPACT_PROMPT_VERSION
        ],
    )
    .unwrap();

    let out = compose(
        &conn,
        r"watcher_main\b",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Summary,
    )
    .unwrap()
    .expect("payload expected");
    assert!(out.context.contains("Election lock first."), "the head renders: {}", out.context);
    assert!(
        !out.context.contains("Tailmarker"),
        "the tail past the clamp does not: {}",
        out.context
    );
    assert!(out.context.contains('…'), "the truncation is marked: {}", out.context);
}

#[test]
fn compose_respects_dedupe_filter_and_returns_none_when_everything_filtered() {
    let conn = seeded_conn();
    let first = compose(
        &conn,
        "watcher_main",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("first payload");
    let filter = DedupeFilter {
        memory_ids: first.memory_ids.iter().cloned().collect::<HashSet<_>>(),
        symbol_keys: first.symbol_keys.iter().cloned().collect::<HashSet<_>>(),
    };
    assert!(
        compose(&conn, "watcher_main", None, &filter, rag_rat_base::config::MemorySurface::Full)
            .unwrap()
            .is_none()
    );
}

#[test]
fn extract_symbol_identifier_handles_definition_patterns() {
    // Lone identifier passes through.
    assert_eq!(extract_symbol_identifier("watcher_main"), Some("watcher_main"));
    // Definition keywords are stripped, leaving the one target identifier.
    assert_eq!(extract_symbol_identifier("fn watcher_main"), Some("watcher_main"));
    assert_eq!(extract_symbol_identifier("pub struct SymbolIndex"), Some("SymbolIndex"));
    assert_eq!(
        extract_symbol_identifier("pub async fn resolve_all_edges"),
        Some("resolve_all_edges")
    );
    // Two real identifiers → ambiguous → lexical lane.
    assert_eq!(extract_symbol_identifier("election retry loop"), None);
    // Free text token (not keyword, not identifier-shaped) → lexical lane.
    assert_eq!(extract_symbol_identifier("foo == bar"), None);
}

/// Swift's declaration keywords reach the symbol lane like every other language's. Before they
/// were known keywords, `protocol Fetcher` read as two identifiers and fell to the lexical lane
/// — even though `func Fetcher` did not, which is the tell that the list, not the pattern, was
/// wrong.
#[test]
fn extract_symbol_identifier_handles_swift_definition_patterns() {
    assert_eq!(extract_symbol_identifier("protocol Fetcher"), Some("Fetcher"));
    assert_eq!(extract_symbol_identifier("actor Store"), Some("Store"));
    assert_eq!(extract_symbol_identifier("extension Client"), Some("Client"));
    assert_eq!(extract_symbol_identifier("public actor SyncStore"), Some("SyncStore"));
    assert_eq!(extract_symbol_identifier("mutating func reset"), Some("reset"));
    assert_eq!(extract_symbol_identifier("open class ViewModel"), Some("ViewModel"));
    assert_eq!(extract_symbol_identifier("macro stringify"), Some("stringify"));
}

#[test]
fn compose_definition_pattern_routes_to_symbol_lane_not_lexical() {
    let conn = seeded_conn();
    let out = compose(
        &conn,
        r"fn watcher_main",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("payload expected");
    // Resolves to the symbol + its bound memory; the redundant lexical echo is suppressed.
    assert!(out.context.contains("watch::watcher_main"), "symbol lane fired");
    assert!(out.context.contains("One watcher per worktree"), "bound memory surfaced");
    assert!(
        !out.context.contains("Indexed hits"),
        "lexical lane must be suppressed when the symbol lane has hits: {}",
        out.context
    );
    assert!(!out.symbol_keys.is_empty());
}

#[test]
fn compose_non_identifier_pattern_uses_lexical_lane() {
    let conn = seeded_conn();
    let out = compose(
        &conn,
        "election retry loop",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("lexical payload");
    assert!(out.context.contains("src/watch.rs"));
}

#[test]
fn compose_unknown_pattern_yields_none() {
    let conn = seeded_conn();
    assert!(
        compose(
            &conn,
            "zzqqyyxx_nothing",
            None,
            &DedupeFilter::default(),
            rag_rat_base::config::MemorySurface::Full
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn normalize_strips_regex_metacharacters_and_anchors() {
    assert_eq!(normalize_pattern(r"^fn\s+watcher_main\b"), "fn watcher_main");
    assert_eq!(normalize_pattern(r"Watcher::spawn(_with_fleet)?"), "Watcher::spawn _with_fleet");
    assert_eq!(normalize_pattern("plain words"), "plain words");
    assert_eq!(normalize_pattern(r".*[]()|+?^$\\"), "");
}

#[test]
fn normalize_preserves_dot_between_word_chars() {
    assert_eq!(normalize_pattern("foo.bar"), "foo.bar");
    assert_eq!(normalize_pattern(r"foo\.bar"), "foo.bar");
    // Leading/trailing dot is NOT between word chars → space.
    assert_eq!(normalize_pattern(".foo"), "foo");
    assert_eq!(normalize_pattern("foo."), "foo");
    // Dot between non-word chars → space.
    assert_eq!(normalize_pattern("foo. bar"), "foo bar");
}

#[test]
fn identifier_candidate_accepts_identifier_shapes_only() {
    assert_eq!(identifier_candidate("watcher_main"), Some("watcher_main"));
    assert_eq!(identifier_candidate("Watcher::spawn"), Some("Watcher::spawn"));
    assert_eq!(identifier_candidate("foo.bar"), Some("foo.bar"));
    assert_eq!(identifier_candidate("fn watcher_main"), None); // two words
    assert_eq!(identifier_candidate("ab"), None); // too short
    assert_eq!(identifier_candidate("1abc"), None); // leading digit
    assert_eq!(identifier_candidate(""), None);
}

#[test]
fn normalize_and_identifier_candidate_compose_for_dot_qualified() {
    // End-to-end: a grep pattern `foo.bar` reaches the symbol lane.
    let norm = normalize_pattern("foo.bar");
    assert_eq!(norm, "foo.bar");
    assert_eq!(identifier_candidate(&norm), Some("foo.bar"));

    // `r"foo\.bar"` (escaped) also reaches the symbol lane.
    let norm2 = normalize_pattern(r"foo\.bar");
    assert_eq!(norm2, "foo.bar");
    assert_eq!(identifier_candidate(&norm2), Some("foo.bar"));
}

#[test]
fn render_truncation_respects_cap_no_dangling_headers_ids_match() {
    // ── Setup ──────────────────────────────────────────────────────────────────────
    // seeded_conn() already contains one memory ("One watcher per worktree") bound to
    // symbol_id=1.  We add FOUR more memories, each with a body long enough to survive
    // the 240-char clamp as a full 241-char string (body trimmed = 300 ASCII words ≈
    // 1499 chars, collapses to 1499 chars, clamped to exactly 240 chars + `…`).
    //
    // Each rendered memory line is:
    //   "- [Invariant | active] <title≈80chars> — <241-char-body>\n"
    //   ≈ 24 + 80 + 4 + 241 + 1 = ~350 chars
    //
    // With four such lines:
    //   preamble(41) + header(39) + 4×350(1400) = 1480 chars for memories alone.
    //
    // The symbol section header+line needs ~151 chars more → 1480+151 = 1631 > 1500.
    // Therefore the render loop MUST drop the symbol section entirely, giving us a
    // genuine truncation scenario.  We assert below that the candidate total exceeds
    // the cap so the test is self-verifying.
    let conn = seeded_conn();

    // Body: 300 distinct English words × ~5 chars = ~1499 chars → collapses to 1499
    // chars → clamped to 240 chars + `…`.  All ASCII so char count == byte count.
    let long_body: String =
        (0u32..300).map(|i| format!("word{i:04}")).collect::<Vec<_>>().join(" ");
    assert!(long_body.len() > MAX_MEMORY_BODY_CHARS, "body must survive clamp");
    assert!(long_body.len() < 4000, "must not exceed validation cap");

    // Titles are ~80 chars — recognizable and unique, long enough to push each rendered
    // line to ~350 chars.
    let titles = [
        "Truncation memory one — extra padding words fill the title field here ok",
        "Truncation memory two — extra padding words fill the title field here ok",
        "Truncation memory three — extra padding words fill the title field here",
        "Truncation memory four — extra padding words fill the title field here ok",
    ];

    let mut created_ids: Vec<String> = Vec::new();
    for title in &titles {
        let result = crate::memory_write::create_memory(&conn, RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: title.to_string(),
            body: long_body.clone(),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: None,
            tags: vec![],
            payload_json: None,
            bind: RepoMemoryBindTarget {
                symbol_id: Some(1),
                logical_symbol_id: None,
                chunk_id: None,
                edge_id: None,
                path: None,
                start_line: None,
                end_line: None,
                commit_hash: None,
                tracker: None,
                project: None,
                item_key: None,
                start_logical_symbol_id: None,
                end_logical_symbol_id: None,
                edge_sequence_hash: None,
                path_summary: None,
                edge_path: None,
                dir: None,
            },
        })
        .unwrap();
        created_ids.push(result.memory.memory_id);
    }
    assert_eq!(created_ids.len(), 4, "all four memories must be created");

    // ── Sanity-check: verify the cap path triggers ─────────────────────────────────
    // A single memory render line ≈ 350 chars (conservative lower bound: 24+70+4+241+1).
    // Four lines + preamble + mem-header = min ~1480 chars; symbol section adds ~151.
    // Assert total candidate content exceeds MAX_CONTEXT_CHARS so truncation is forced.
    let per_mem_line_min: usize = "- [Invariant | active] ".len()  // 24
            + titles[0].len()                                            // ≥70
            + " — ".len()                                                // 4
            + MAX_MEMORY_BODY_CHARS + 1; // 241 (clamped+…)
    let preamble_len = "rag-rat index context for this search:\n".len();
    let mem_header_len = "**Repo memories bound to this code:**\n".len();
    let symbol_section_min: usize = "**Known symbols matching this pattern:**\n".len() + 80; // header + short line
    let candidate_total = preamble_len + mem_header_len + 4 * per_mem_line_min + symbol_section_min;
    assert!(
        candidate_total > MAX_CONTEXT_CHARS,
        "candidate_total={candidate_total} must exceed MAX_CONTEXT_CHARS={MAX_CONTEXT_CHARS} for \
         truncation to trigger",
    );

    // ── Run compose ────────────────────────────────────────────────────────────────
    let out = compose(
        &conn,
        "watcher_main",
        None,
        &DedupeFilter::default(),
        rag_rat_base::config::MemorySurface::Full,
    )
    .unwrap()
    .expect("payload expected");

    // (a) Context must not exceed the cap.
    assert!(
        out.context.len() <= MAX_CONTEXT_CHARS,
        "context.len()={} > MAX_CONTEXT_CHARS={}",
        out.context.len(),
        MAX_CONTEXT_CHARS,
    );

    // (b) No section header is the last line / every committed header is followed by
    //     at least one item line.
    let section_headers = [
        "**Repo memories bound to this code:**",
        "**Known symbols matching this pattern:**",
        "**Indexed hits (rag-rat semantic_search has more):**",
    ];
    let lines: Vec<&str> = out.context.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let is_header = section_headers.iter().any(|h| line.trim() == *h);
        if is_header {
            assert!(
                idx + 1 < lines.len(),
                "section header '{line}' is the last line — dangling header",
            );
        }
    }

    // (c) Exact two-way correspondence for every seeded memory:
    //     context.contains(title)  ⟺  memory_ids.contains(that_id)
    for (title, id) in titles.iter().zip(created_ids.iter()) {
        let in_context = out.context.contains(*title);
        let id_present = out.memory_ids.contains(id);
        assert_eq!(
            in_context, id_present,
            "mismatch for '{title}': in_context={in_context}, id_present={id_present}",
        );
    }

    // (d) Two-way correspondence for the symbol: symbol_keys non-empty ⟺
    //     "watch::watcher_main" appears in context.
    let sym_in_context = out.context.contains("watch::watcher_main");
    let sym_keys_non_empty = !out.symbol_keys.is_empty();
    assert_eq!(
        sym_in_context, sym_keys_non_empty,
        "symbol context/key mismatch: sym_in_context={sym_in_context}, \
         sym_keys_non_empty={sym_keys_non_empty}",
    );

    // (e) Truncation actually occurred: at least one seeded memory title OR the symbol
    //     must be absent from context (we have more content than the cap allows).
    let all_titles_present = titles.iter().all(|t| out.context.contains(*t));
    let symbol_present = out.context.contains("watch::watcher_main");
    assert!(
        !all_titles_present || !symbol_present,
        "no truncation detected: all memory titles and the symbol section all fit within \
         MAX_CONTEXT_CHARS — increase body/title size so the cap is actually exercised",
    );
}

#[test]
fn clamp_body_truncates_long_bodies_and_collapses_whitespace() {
    let short = "hello world";
    assert_eq!(clamp_body(short), "hello world");

    // Whitespace collapse.
    let multiline = "line one\nline two\n  indented";
    assert_eq!(clamp_body(multiline), "line one line two indented");

    // Long body truncation.
    let long = "x".repeat(300);
    let clamped = clamp_body(&long);
    assert!(clamped.ends_with('…'), "truncated body must end with ellipsis");
    // The char count of the non-ellipsis prefix must be exactly MAX_MEMORY_BODY_CHARS.
    let without_ellipsis: String = clamped.chars().take(MAX_MEMORY_BODY_CHARS).collect();
    assert_eq!(without_ellipsis.len(), MAX_MEMORY_BODY_CHARS);
}
