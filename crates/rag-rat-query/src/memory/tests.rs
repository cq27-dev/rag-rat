use super::*;
use crate::memory::fixtures::{self, MemorySeed};

fn binding(kind: &str, anchor_status: &str, path: Option<&str>) -> RepoMemoryBinding {
    RepoMemoryBinding {
        memory_id: "mem_x".to_string(),
        binding_kind: kind.to_string(),
        binding_id: format!("{kind}-id"),
        resolved_binding_id: None,
        path: path.map(str::to_string),
        start_line: path.map(|_| 10),
        end_line: path.map(|_| 20),
        logical_symbol_id: Some(42),
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: None,
        signature_hash: None,
        moniker_tool: None,
        moniker_tool_version: None,
        relocation_reason: None,
        anchor_status: anchor_status.to_string(),
        created_at_ms: 0,
    }
}

fn memory(bindings: Vec<RepoMemoryBinding>) -> RepoMemory {
    RepoMemory {
        memory_id: "mem_x".to_string(),
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
        memory_version: String::new(),
        synced_anchor_drifted: false,
        bindings,
        call_paths: Vec::new(),
        tags: Vec::new(),
    }
}

#[test]
fn binding_kind_and_anchor_status_tokens_are_exact_and_round_trip() {
    let kinds = [
        (BindingKind::LogicalSymbol, "logical_symbol"),
        (BindingKind::Symbol, "symbol"),
        (BindingKind::Chunk, "chunk"),
        (BindingKind::Edge, "edge"),
        (BindingKind::CallPath, "call_path"),
        (BindingKind::ScipMoniker, "scip_moniker"),
        (BindingKind::Path, "path"),
        (BindingKind::Dir, "dir"),
        (BindingKind::Commit, "commit"),
        (BindingKind::Tracker, "tracker"),
    ];
    for (kind, token) in kinds {
        assert_eq!(kind.as_db_str(), token);
        assert_eq!(BindingKind::from_db_str(token).unwrap(), kind);
    }
    let statuses = [
        (AnchorStatus::Current, "current"),
        (AnchorStatus::Relocated, "relocated"),
        (AnchorStatus::Stale, "stale"),
        (AnchorStatus::Gone, "gone"),
        (AnchorStatus::Pending, "pending"),
        (AnchorStatus::Unverified, "unverified"),
    ];
    for (status, token) in statuses {
        assert_eq!(status.as_db_str(), token);
        assert_eq!(AnchorStatus::from_db_str(token).unwrap(), status);
    }
    assert!(BindingKind::from_db_str("repo").is_err());
    assert!(AnchorStatus::from_db_str("Current").is_err());
}

/// The memory-row and relocation tokens are persisted and replicate, so each variant's token is
/// pinned byte-for-byte: PascalCase kinds, lowercase status/confidence/source, kebab-case
/// relocation reasons.
#[test]
fn memory_row_and_relocation_tokens_are_exact_and_round_trip() {
    for (kind, token) in [
        (MemoryKind::Invariant, "Invariant"),
        (MemoryKind::Decision, "Decision"),
        (MemoryKind::RejectedAlternative, "RejectedAlternative"),
        (MemoryKind::Risk, "Risk"),
        (MemoryKind::BugPattern, "BugPattern"),
        (MemoryKind::TestExpectation, "TestExpectation"),
        (MemoryKind::PerformanceNote, "PerformanceNote"),
        (MemoryKind::SecurityNote, "SecurityNote"),
        (MemoryKind::FFIBoundary, "FFIBoundary"),
        (MemoryKind::PlatformQuirk, "PlatformQuirk"),
        (MemoryKind::FollowUp, "FollowUp"),
        (MemoryKind::OpenQuestion, "OpenQuestion"),
        (MemoryKind::Obsolete, "Obsolete"),
        (MemoryKind::Task, "Task"),
        (MemoryKind::Concept, "Concept"),
    ] {
        assert_eq!(kind.as_db_str(), token);
        assert_eq!(MemoryKind::from_db_str(token).unwrap(), kind);
        assert_eq!(kind.is_polymorphic_node(), matches!(token, "Task" | "Concept"));
    }
    for (status, token, live) in [
        (MemoryStatus::Active, "active", true),
        (MemoryStatus::Stale, "stale", true),
        (MemoryStatus::Obsolete, "obsolete", false),
        (MemoryStatus::Rejected, "rejected", false),
    ] {
        assert_eq!(status.as_db_str(), token);
        assert_eq!(MemoryStatus::from_db_str(token).unwrap(), status);
        assert_eq!(status.is_live(), live);
    }
    for (confidence, token) in [
        (MemoryConfidence::High, "high"),
        (MemoryConfidence::Medium, "medium"),
        (MemoryConfidence::Low, "low"),
    ] {
        assert_eq!(confidence.as_db_str(), token);
        assert_eq!(MemoryConfidence::from_db_str(token).unwrap(), confidence);
    }
    for (source, token) in [
        (MemorySource::Agent, "agent"),
        (MemorySource::Human, "human"),
        (MemorySource::Imported, "imported"),
        (MemorySource::Generated, "generated"),
    ] {
        assert_eq!(source.as_db_str(), token);
        assert_eq!(MemorySource::from_db_str(token).unwrap(), source);
    }
    for (reason, token) in [
        (RelocationReason::MonikerMatch, "moniker-match"),
        (RelocationReason::MonikerRefresh, "moniker-refresh"),
        (RelocationReason::Retargeted, "retargeted"),
    ] {
        assert_eq!(reason.as_db_str(), token);
        assert_eq!(RelocationReason::from_db_str(token).unwrap(), reason);
    }
    assert!(MemoryKind::from_db_str("invariant").is_err());
    assert!(MemoryStatus::from_db_str("Active").is_err());
    assert!(!is_polymorphic_node_kind("task"));
}

/// The live-status predicate is derived from `MemoryStatus::is_live`, and the SQL every memory
/// read runs must stay the exact text it has always been.
#[test]
fn live_memory_status_sql_is_the_live_variants() {
    assert_eq!(live_memory_status_sql("m"), "m.status IN ('active', 'stale')");
}

/// "This store's resolution when `resolved` is set, else the authored value" is written three
/// ways — the carry a partial writer pairs with its own assignment, the `binding_current` SQL
/// fragment, and `binding_row`'s Rust-side pick. All three must govern exactly the same shadow
/// columns, or a read that misses one returns the AUTHORED value of a relocated binding.
#[test]
fn binding_shadow_readers_and_the_carry_cover_the_same_columns() {
    const BINDING_SHADOWED_COLUMNS: [&str; 7] = [
        "binding_id",
        "path",
        "start_line",
        "end_line",
        "symbol_kind",
        "signature_hash",
        "moniker_tool_version",
    ];
    // The carry names every shadow but the identity, which its caller assigns itself.
    assert_eq!(
        BINDING_RESOLUTION_CARRY_SQL.matches("= IIF(").count(),
        BINDING_SHADOWED_COLUMNS.len() - 1
    );
    for column in BINDING_SHADOWED_COLUMNS.iter().filter(|column| **column != "binding_id") {
        let carried = format!("resolved_{column} = IIF(resolved, resolved_{column}, {column})");
        assert!(BINDING_RESOLUTION_CARRY_SQL.contains(&carried), "the carry misses {column}");
    }
    assert_eq!(BINDING_CURRENT_PATH, binding_current("repo_memory_bindings", "path"));
    assert_eq!(BINDING_CURRENT_BINDING_ID, binding_current("repo_memory_bindings", "binding_id"));

    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0)",
        [],
    )
    .unwrap();
    fixtures::seed_memory(&conn, MemorySeed {
        id: "m",
        created_by: None,
        created_at_ms: 0,
        updated_at_ms: 0,
        ..MemorySeed::default()
    });
    conn.execute_batch(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path,
                     start_line, end_line, symbol_kind, signature_hash, moniker_tool_version,
                     anchor_status, created_at_ms, repo_id, resolved, resolved_binding_id,
                     resolved_path, resolved_start_line, resolved_end_line, resolved_symbol_kind,
                     resolved_signature_hash, resolved_moniker_tool_version)
             VALUES ('m', 'symbol', 'moved', 'a.rs', 1, 2, 'fn', 's1', 'v1', 'relocated', 0, 'r',
                     1, 'there', 'b.rs', 10, 20, 'struct', 's2', 'v2'),
                    ('m', 'symbol', 'unmoved', 'a.rs', 1, 2, 'fn', 's1', 'v1', 'current', 0, 'r',
                     0, 'there', 'b.rs', 10, 20, 'struct', 's2', 'v2');",
    )
    .unwrap();
    for (binding_id, expected_path) in [("moved", "b.rs"), ("unmoved", "a.rs")] {
        let binding = conn
            .query_row(
                &format!(
                    "SELECT {} FROM repo_memory_bindings WHERE binding_id = ?1",
                    hydrate::BINDING_ROW_COLUMNS
                ),
                [binding_id],
                binding_row,
            )
            .unwrap();
        assert_eq!(binding.path.as_deref(), Some(expected_path));
        for column in BINDING_SHADOWED_COLUMNS {
            use rusqlite::types::Value;
            let from_sql: Value = conn
                .query_row(
                    &format!(
                        "SELECT {} FROM repo_memory_bindings AS b WHERE b.binding_id = ?1",
                        binding_current("b", column)
                    ),
                    [binding_id],
                    |row| row.get(0),
                )
                .unwrap();
            let from_row = match column {
                "binding_id" => Value::from(binding.current_binding_id().to_string()),
                "path" => binding.path.clone().into(),
                "start_line" => binding.start_line.into(),
                "end_line" => binding.end_line.into(),
                "symbol_kind" => binding.symbol_kind.clone().into(),
                "signature_hash" => binding.signature_hash.clone().into(),
                "moniker_tool_version" => binding.moniker_tool_version.clone().into(),
                other => panic!("binding_row does not hydrate the shadowed column `{other}`"),
            };
            assert_eq!(from_row, from_sql, "`{column}` on the `{binding_id}` row");
        }
    }
}

#[test]
fn compact_header_skips_a_lagging_moniker_binding_for_the_real_anchor() {
    // `attach_memory_children` orders bindings by `binding_kind`, so a `scip_moniker` companion
    // (which can be `unverified`/`gone` between oracle runs, and which `split_active_stale`
    // deliberately ignores) sorts BEFORE the real `symbol` anchor. The compact header must skip
    // it, or an ACTIVE memory reads as stale (Codex on #194).
    let compact = CompactRepoMemory::from(&memory(vec![
        binding("scip_moniker", "unverified", None),
        binding("symbol", "current", Some("src/lib.rs")),
    ]));
    assert_eq!(compact.binding_kind.as_deref(), Some("symbol"));
    assert_eq!(compact.anchor_status.as_deref(), Some("current"));
    assert_eq!(compact.path.as_deref(), Some("src/lib.rs"));
    assert_eq!(compact.span, Some([10, 20]));
}

#[test]
fn compact_header_falls_back_to_a_moniker_only_binding_set() {
    // A memory anchored ONLY by a moniker still gets a header (no non-moniker binding to
    // prefer).
    let compact = CompactRepoMemory::from(&memory(vec![binding("scip_moniker", "current", None)]));
    assert_eq!(compact.binding_kind.as_deref(), Some("scip_moniker"));
}

// ── dream-summary surfacing (`[memory] surface = "summary"`) ─────────────────

/// A fresh in-memory index scoped to repo `r` — the fixture for the summary-surfacing tests.
fn summary_conn() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &rag_rat_core::index::migration_hooks()).unwrap();
    c.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    c.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id','r')",
        [],
    )
    .unwrap();
    c
}

/// A minimal `RepoMemory` with a controlled id + body (no bindings).
fn memory_with_body(id: &str, body: &str) -> RepoMemory {
    RepoMemory {
        memory_id: id.to_string(),
        kind: "Invariant".to_string(),
        title: "t".to_string(),
        body: body.to_string(),
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
        memory_version: String::new(),
        synced_anchor_drifted: false,
        bindings: Vec::new(),
        call_paths: Vec::new(),
        tags: Vec::new(),
    }
}

/// A body one word OVER the compaction size gate — the queue takes it, so a missing summary
/// means "not compacted YET", not "never will be", and the summary surfaces must defer it.
fn over_envelope_body() -> String {
    vec!["word"; evidence::SUMMARY_MAX_WORDS + 1].join(" ")
}

/// A body WELL under the word ceiling but over the character one — the shape a word count
/// alone misreads: long tokens (absolute paths, URLs, a quoted stack line).
fn wide_token_body() -> String {
    let path = "/home/dev/src/repo/crates/rag-rat-core/src/index/query_api/memory.rs";
    let body = vec![path; 20].join(" ");
    assert!(body.split_whitespace().count() <= evidence::SUMMARY_MAX_WORDS);
    assert!(body.chars().count() > evidence::SUMMARY_MAX_CHARS);
    body
}

fn seed_summary(c: &Connection, id: &str, body: &str, summary: &str) {
    // Stamp the current content_hash (title `"t"`, matching `memory_with_body`) + the current
    // COMPACT_PROMPT_VERSION — the hydrator gates the summary read on both (like the compaction
    // queue's coverage check), so a mismatch drops the summary.
    c.execute(
        "INSERT INTO memory_note_summaries(memory_id, repo_id, content_hash, summary, \
         prompt_version, generated_at_ms) VALUES (?1,'r',?2,?3,?4,0)",
        params![
            id,
            crate::memory::evidence::note_content_hash("t", body),
            summary,
            crate::memory::evidence::COMPACT_PROMPT_VERSION
        ],
    )
    .unwrap();
}

fn seed_reality(c: &Connection, id: &str, body: &str, verdict: &str, commit: Option<&str>) {
    // Key the reality row on the memory's TRUE content_hash (title `"t"`, matching
    // `memory_with_body`), its current evidence hash, AND the current verdict PROMPT_VERSION —
    // the hydrator gates the marker on all three (like the queue/divergence finder), so a
    // mismatch on any silently drops it. These test memories have no bindings/identifiers, so
    // the evidence hash is the stable empty value `checked_inputs_hash` computes.
    let inputs =
        crate::memory::evidence::checked_inputs_hash(c, id, &Some("r".to_string())).unwrap();
    c.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, \
         checked_against_commit, checked_inputs_hash, prompt_version, checked_at_ms) VALUES \
         (?1,'r',?2,?3,?4,?5,?6,0)",
        params![
            id,
            crate::memory::evidence::note_content_hash("t", body),
            verdict,
            commit,
            inputs,
            crate::memory::evidence::VERDICT_PROMPT_VERSION
        ],
    )
    .unwrap();
}

fn evidence(memories: Vec<RepoMemory>) -> RepoMemoryEvidence {
    RepoMemoryEvidence { direct: memories, ..Default::default() }
}

#[test]
fn summary_surface_renders_summary_and_verdict_marker() {
    let c = summary_conn();
    let body = "the full body worth compacting";
    seed_summary(
        &c,
        "m1",
        body,
        "A compacted three-sentence summary. It preserves polarity. Done.",
    );
    seed_reality(&c, "m1", body, "diverged", None);

    let compact = evidence(vec![memory_with_body("m1", body)]).compact_summary_first(&c).unwrap();
    let header = &compact.direct[0];
    assert_eq!(
        header.summary.as_deref(),
        Some("A compacted three-sentence summary. It preserves polarity. Done."),
        "the compacted summary is hydrated under the summary surface"
    );
    assert_eq!(
        header.verdict.as_deref(),
        Some("[verdict: diverged]"),
        "the verdict marker renders"
    );
}

#[test]
fn summary_surface_falls_back_to_title_only_without_a_summary_row() {
    let c = summary_conn();
    // No memory_note_summaries / memory_reality rows, and a body too long to stand in for the
    // missing summary → summary + verdict stay None (title-only).
    let compact = evidence(vec![memory_with_body("m1", &over_envelope_body())])
        .compact_summary_first(&c)
        .unwrap();
    let header = &compact.direct[0];
    assert_eq!(header.summary, None, "no summary row → the title stands alone");
    assert_eq!(header.verdict, None, "no reality row → no verdict marker");
    assert_eq!(header.title, "t", "the title is still present");
}

#[test]
fn summary_surface_misses_a_stale_summary_after_a_body_edit() {
    let c = summary_conn();
    // A summary exists, but for the OLD body — the current content_hash differs, so the LEFT
    // JOIN misses and the header falls back to title-only (the summary self-invalidated). The
    // new body is over the envelope so the fallback is title-only rather than the body itself.
    seed_summary(
        &c,
        "m1",
        "old body",
        "A stale summary from before. It no longer applies. Ignore.",
    );
    let compact = evidence(vec![memory_with_body("m1", &over_envelope_body())])
        .compact_summary_first(&c)
        .unwrap();
    assert_eq!(
        compact.direct[0].summary, None,
        "a summary keyed on a stale content_hash is not surfaced"
    );
}

#[test]
fn verdict_marker_misses_a_stale_verdict_after_a_body_edit() {
    let c = summary_conn();
    // A verdict exists, but for the OLD body — the current content_hash differs, so the verdict
    // read misses and the header carries no marker. Symmetric to the stale-summary case: a body
    // edit self-invalidates the verdict just like the summary, so a just-edited memory never
    // renders the PRIOR body's verdict.
    seed_reality(&c, "m1", "old body", "diverged", None);
    let compact =
        evidence(vec![memory_with_body("m1", "new body")]).compact_summary_first(&c).unwrap();
    assert_eq!(
        compact.direct[0].verdict, None,
        "a verdict keyed on a stale content_hash is not surfaced after a body edit"
    );
}

#[test]
fn verdict_marker_misses_a_stale_verdict_after_a_bound_input_change() {
    // Regression (PR #428 Codex P2): the marker is gated on `checked_inputs_hash`, not only
    // `content_hash`. A stored verdict whose inputs hash no longer matches the memory's current
    // bound-file inputs (a bound file changed since the check) must drop, like the divergence
    // finder and queue treat an inputs mismatch. Seed a row with a deliberately-mismatched
    // inputs hash — the current inputs hash for this binding-less memory is the
    // empty-set value, which this arbitrary value is not.
    let c = summary_conn();
    let body = "a note whose stored verdict predates a bound-file change";
    // Stamp the CURRENT content hash + prompt version so the only mismatch is the inputs hash —
    // otherwise the marker would drop for the wrong reason and not exercise the inputs gate.
    c.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, \
         checked_inputs_hash, prompt_version, checked_at_ms) VALUES \
         ('m1','r',?1,'diverged','stale-inputs',?2,0)",
        params![
            crate::memory::evidence::note_content_hash("t", body),
            crate::memory::evidence::VERDICT_PROMPT_VERSION
        ],
    )
    .unwrap();
    let compact = evidence(vec![memory_with_body("m1", body)]).compact_summary_first(&c).unwrap();
    assert_eq!(
        compact.direct[0].verdict, None,
        "a verdict whose checked_inputs_hash no longer matches is not surfaced"
    );
}

#[test]
fn summary_and_marker_drop_under_an_obsolete_prompt_version() {
    // Regression (PR #428 Codex P2): the surfacing hydrator must apply the SAME prompt-version
    // gate the compaction queue / verification queue use. A summary from an obsolete compact
    // prompt or a verdict from an obsolete verdict prompt must not keep showing while the
    // memory waits behind the budget (or a model failure) for a fresh one.
    let c = summary_conn();
    // Over the summary envelope, so the obsolete-prompt drop leaves title-only and is not
    // masked by the show-it-whole fallback.
    let body = over_envelope_body();
    let body = body.as_str();
    let content_hash = crate::memory::evidence::note_content_hash("t", body);
    c.execute(
        "INSERT INTO memory_note_summaries(memory_id, repo_id, content_hash, summary, \
         prompt_version, generated_at_ms) VALUES ('m1','r',?1,'A three sentence summary. It \
         holds. Done.','compact-OLD',0)",
        params![content_hash],
    )
    .unwrap();
    let inputs =
        crate::memory::evidence::checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
    c.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, \
         checked_inputs_hash, prompt_version, checked_at_ms) VALUES \
         ('m1','r',?1,'diverged',?2,'verify-OLD',0)",
        params![content_hash, inputs],
    )
    .unwrap();
    let compact = evidence(vec![memory_with_body("m1", body)]).compact_summary_first(&c).unwrap();
    assert_eq!(
        compact.direct[0].summary, None,
        "a summary from an obsolete compact prompt is not surfaced"
    );
    assert_eq!(
        compact.direct[0].verdict, None,
        "a verdict from an obsolete verdict prompt is not surfaced"
    );
}

#[test]
fn verdict_marker_current_carries_the_short_commit() {
    let c = summary_conn();
    let body = "b";
    seed_summary(&c, "m1", body, "One sentence summary here. Two now. Three done.");
    seed_reality(&c, "m1", body, "current", Some("abcdef0123456789"));
    let compact = evidence(vec![memory_with_body("m1", body)]).compact_summary_first(&c).unwrap();
    assert_eq!(
        compact.direct[0].verdict.as_deref(),
        Some("[verdict: current @abcdef0]"),
        "a current verdict carries the 7-hex short commit"
    );
}

#[test]
fn full_surface_projection_carries_no_summary_or_verdict() {
    // The `full` compact projection is purely mechanical — no summary/verdict, even
    // when sibling rows exist (they are only read by `compact_summary_first`).
    let compact = CompactRepoMemory::from(&memory_with_body("m1", "body"));
    assert_eq!(compact.summary, None);
    assert_eq!(compact.verdict, None);
}

#[test]
fn memory_get_returns_the_full_body_even_when_a_summary_exists() {
    // `memory show` / `memory_show` is surface-independent: the expand path always carries the
    // full body regardless of any compacted summary.
    let c = summary_conn();
    let body = "the full body that memory show must always return";
    fixtures::seed_memory(&c, MemorySeed { body, ..MemorySeed::default() });
    seed_summary(&c, "m1", body, "A short summary stands in for surfacing. Not for show. Ok.");
    let fetched = memory_by_id(&c, "m1").unwrap().expect("memory present");
    assert_eq!(fetched.body, body, "memory_get returns the full body regardless of the summary");
}

#[test]
fn apply_memory_surface_summary_defers_the_body_and_hydrates_summary_and_verdict() {
    // The direct-query / read_chunk / grep counterpart to `compact_summary_first`: under
    // `Summary` the full body is emptied (deferred to `memory show`) and the current-body
    // summary + verdict marker are hydrated in its place.
    let c = summary_conn();
    let body = "the full body worth compacting";
    seed_summary(&c, "m1", body, "A compacted summary in place of the body.");
    seed_reality(&c, "m1", body, "diverged", None);
    let mut memories = vec![memory_with_body("m1", body)];
    apply_memory_surface(&c, &mut memories, rag_rat_base::config::MemorySurface::Summary).unwrap();
    assert_eq!(
        memories[0].body,
        body_elision_marker("m1"),
        "the deferred body is replaced by the elision marker, not blanked silently"
    );
    assert!(
        memories[0].body.contains("memory_get {memory_id: m1}"),
        "the marker names the expand path: {}",
        memories[0].body
    );
    assert_eq!(memories[0].summary.as_deref(), Some("A compacted summary in place of the body."));
    assert!(
        memories[0].verdict.as_deref().unwrap_or_default().contains("diverged"),
        "the verdict marker is set: {:?}",
        memories[0].verdict
    );
}

#[test]
fn apply_memory_surface_summary_shows_an_under_envelope_body_whole_and_defers_a_longer_one() {
    // The size gate, on both sides. A note compaction SKIPS (inside the envelope) never gets a
    // summary row, so deferring it would leave a permanent bare title — it surfaces whole. A
    // LONGER note with no summary row is merely uncompacted (dream disabled, never run, or
    // behind a prompt-version bump) and must still defer: without this half, the default
    // surface dumps every full body the moment COMPACT_PROMPT_VERSION is bumped.
    let c = summary_conn();
    let long = over_envelope_body();
    // Few words, many characters — paths, URLs, a quoted stack line. The envelope is a cost
    // bound, so the character ceiling has to hold on its own: a body the word count alone would
    // wave through is skipped by compaction forever and then dumped whole on every attachment.
    let wide = wide_token_body();
    let mut memories = vec![
        memory_with_body("m1", "some body"),
        memory_with_body("m2", &long),
        memory_with_body("m3", &wide),
    ];
    apply_memory_surface(&c, &mut memories, rag_rat_base::config::MemorySurface::Summary).unwrap();
    assert_eq!(
        memories[0].body, "some body",
        "a note inside the summary envelope keeps its full body"
    );
    assert_eq!(
        memories[1].body,
        body_elision_marker("m2"),
        "an over-envelope note with no summary row still defers its body"
    );
    assert_eq!(
        memories[2].body,
        body_elision_marker("m3"),
        "over the character ceiling defers too, however few words the body has"
    );
    assert_eq!(memories[0].summary, None);
    assert_eq!(memories[1].summary, None);
    assert_eq!(memories[2].summary, None);
    assert_eq!(memories[0].verdict, None);
}

#[test]
fn body_is_elided_reads_the_memory_own_marker_not_prose_quoting_it() {
    // A memory ABOUT the elision marker quotes the marker, so its prose opens with the marker
    // text — and under `Full` no marker is ever applied, so that body is prose and has to
    // render as prose. Only the marker this memory's own id would produce counts as elision.
    let c = summary_conn();
    let prose = format!(
        "{BODY_ELISION_PREFIX} …] is what a deferred body renders as. {}",
        over_envelope_body()
    );
    let mut memories = vec![memory_with_body("m1", &prose)];
    assert!(
        !body_is_elided(&memories[0]),
        "prose that merely opens with the marker text is not elided: {}",
        memories[0].body
    );
    assert!(
        !body_is_elided(&memory_with_body("m2", &body_elision_marker("m1"))),
        "another memory's marker, quoted verbatim, is not this memory's elision"
    );
    apply_memory_surface(&c, &mut memories, rag_rat_base::config::MemorySurface::Summary).unwrap();
    assert!(
        body_is_elided(&memories[0]),
        "a body the summary surface actually deferred still reads as elided: {}",
        memories[0].body
    );
}

#[test]
fn compact_summary_first_stands_an_under_envelope_body_in_for_the_missing_summary() {
    // `CompactRepoMemory` (impact_surface) carries no body field, so a memory compaction skips
    // would render title-only FOREVER without this fallback. Its body is inside the envelope by
    // construction, so it costs no more than the summary it replaces. An over-envelope note
    // with no summary row still falls back to title-only — it is waiting for a summary, not
    // ineligible for one.
    let c = summary_conn();
    let long = over_envelope_body();
    let compact = evidence(vec![
        memory_with_body("m1", "a short note worth showing"),
        memory_with_body("m2", &long),
    ])
    .compact_summary_first(&c)
    .unwrap();
    assert_eq!(
        compact.direct[0].summary.as_deref(),
        Some("a short note worth showing"),
        "an unsummarizable note stands in its own body"
    );
    assert_eq!(
        compact.direct[1].summary, None,
        "an over-envelope note with no summary row stays title-only"
    );
}

#[test]
fn apply_memory_surface_full_is_a_noop() {
    // `Full` keeps the body byte-identical and never hydrates a summary, even when one exists.
    let c = summary_conn();
    let body = "kept verbatim in full mode";
    seed_summary(&c, "m1", body, "would-be summary");
    let mut memories = vec![memory_with_body("m1", body)];
    apply_memory_surface(&c, &mut memories, rag_rat_base::config::MemorySurface::Full).unwrap();
    assert_eq!(memories[0].body, body, "full surface keeps the body");
    assert_eq!(memories[0].summary, None, "full surface never hydrates a summary");
    assert_eq!(memories[0].verdict, None);
}
