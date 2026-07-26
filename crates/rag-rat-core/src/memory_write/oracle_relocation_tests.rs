//! Memory relocation under oracle evidence: `scip_moniker` bindings written by oracle runs must
//! keep repo memories anchored across file moves, reindex shapes, and content churn. These are
//! unit tests of this module's relocation internals (`resolve_moniker`, `validate_memories`,
//! `doctor_attention_count`, …) driven end-to-end through `rag_rat_oracle::run_oracle` and its
//! programmatic SCIP fixture harness.

use rag_rat_oracle::run_oracle;
use rag_rat_oracle::test_support::{
    COMMIT, Harness, TARGET_MONIKER, TOOL, VERSION, WORKTREE, move_target_with_edit, occurrence,
    scip_bytes_docs,
};
use rag_rat_query::memory::{
    RepoMemoryBindTarget, RepoMemoryCreate, doctor_attention_count, memory_by_id,
    split_active_stale, validate_memories,
};
use rusqlite::params;
use scip::types::SymbolRole;

use super::create_memory;

/// Create a memory bound to the harness symbol, asserting the automatic `scip_moniker` binding.
fn create_target_memory(h: &Harness, symbol_id: i64) -> String {
    use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};

    use crate::memory_write::create_memory;

    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "target invariant".to_string(),
        body: "target must stay reentrant".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget { symbol_id: Some(symbol_id), ..Default::default() },
    })
    .unwrap();
    assert!(!created.duplicate);
    let moniker_binding =
        created.memory.bindings.iter().find(|b| b.binding_kind == "scip_moniker").expect(
            "memory on a symbol with a known moniker gets the moniker binding automatically",
        );
    assert_eq!(moniker_binding.binding_id, TARGET_MONIKER);
    assert_eq!(moniker_binding.moniker_tool.as_deref(), Some(TOOL.as_db_str()));
    assert_eq!(moniker_binding.moniker_tool_version.as_deref(), Some(VERSION));
    created.memory.memory_id
}

/// Point the index `source_root` meta at the harness checkout so the off-index filesystem
/// existence fallback (#98) has a root to resolve a binding path against.
fn set_source_root(h: &Harness) {
    h.conn
        .execute(
            "INSERT OR REPLACE INTO repo_meta(repo_id, key, value)
             VALUES ('__unassigned__', 'source_root', ?1)",
            params![h.root().to_string_lossy()],
        )
        .unwrap();
}

/// Create a bare path-bound memory and return its id + the validated anchor status of the `path`
/// binding.
fn path_binding_status_after_validate(h: &Harness, path: &str) -> String {
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: format!("note about {path}"),
        body: "this guidance is still valid".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget { path: Some(path.to_string()), ..Default::default() },
    })
    .unwrap();
    // Twice: the #492 downgrade hysteresis defers a first gone observation, so the SETTLED
    // status needs two passes (every other status is a fixpoint under repeated validation).
    validate_memories(&h.conn, None).unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "path")
        .expect("path binding")
        .anchor_status
        .clone()
}

/// #98: a memory bound to a NON-INDEXED file that exists on disk (a Containerfile, shell script,
/// `.yml`, `.toml` — anything outside the indexed language set, so it has no `files` row by
/// construction) must validate `current`, not `gone`. Acting on a false `gone` would delete valid
/// guidance.
#[test]
fn path_binding_to_unindexed_file_present_on_disk_is_current() {
    let h = Harness::new();
    set_source_root(&h);
    std::fs::create_dir_all(h.root().join("tools")).unwrap();
    std::fs::write(h.root().join("tools/bench.Containerfile"), "FROM scratch\n").unwrap();
    // Deliberately NO `files` row — this file type is outside the indexed set.
    assert_eq!(
        path_binding_status_after_validate(&h, "tools/bench.Containerfile"),
        "current",
        "a path bound to a real-but-unindexed file is an area anchor, not gone"
    );
}

/// #98: a path binding whose target is absent from BOTH the index and the filesystem is genuinely
/// `gone`.
#[test]
fn path_binding_to_missing_file_is_gone() {
    let h = Harness::new();
    set_source_root(&h);
    assert_eq!(
        path_binding_status_after_validate(&h, "tools/deleted.Containerfile"),
        "gone",
        "a path that exists nowhere is genuinely gone"
    );
}

/// #98: a SPANNED path binding (`path:start-end`) to a non-indexed file has no chunk to hash, so it
/// is `unverified` rather than `gone` — it can't be content-validated but its target is alive.
#[test]
fn spanned_path_binding_to_unindexed_file_is_unverified() {
    let h = Harness::new();
    set_source_root(&h);
    std::fs::create_dir_all(h.root().join("tools")).unwrap();
    std::fs::write(h.root().join("tools/build.sh"), "#!/bin/sh\necho hi\n").unwrap();
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "spanned note".to_string(),
        body: "lines 1-2 matter".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget {
            path: Some("tools/build.sh".to_string()),
            start_line: Some(1),
            end_line: Some(2),
            ..Default::default()
        },
    })
    .unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    let pb = memory.bindings.iter().find(|b| b.binding_kind == "path").expect("path binding");
    assert_eq!(
        pb.anchor_status, "unverified",
        "a spanned binding to a non-indexed file can't be hashed but isn't gone"
    );
}

/// #98 (dir analogue): a `dir` binding to a directory that exists on disk but holds only
/// non-indexed files (so `dir_has_files` finds nothing) must validate `current`, not `gone`.
#[test]
fn dir_binding_to_unindexed_dir_present_on_disk_is_current() {
    let h = Harness::new();
    set_source_root(&h);
    std::fs::create_dir_all(h.root().join("scripts")).unwrap();
    std::fs::write(h.root().join("scripts/deploy.sh"), "#!/bin/sh\n").unwrap();
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "scripts dir note".to_string(),
        body: "deploy scripts live here".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget { dir: Some("scripts".to_string()), ..Default::default() },
    })
    .unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    let db = memory.bindings.iter().find(|b| b.binding_kind == "dir").expect("dir binding");
    assert_eq!(
        db.anchor_status, "current",
        "a dir present on disk with only non-indexed files is current, not gone"
    );
}

/// #98 (dir analogue): a `dir` binding to a directory absent from index and filesystem is `gone`.
#[test]
fn dir_binding_to_missing_dir_is_gone() {
    let h = Harness::new();
    set_source_root(&h);
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "ghost dir note".to_string(),
        body: "nothing here".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget {
            dir: Some("does/not/exist".to_string()),
            ..Default::default()
        },
    })
    .unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    let db = memory.bindings.iter().find(|b| b.binding_kind == "dir").expect("dir binding");
    assert_eq!(db.anchor_status, "gone", "a dir that exists nowhere is genuinely gone");
}

/// #98 review (Codex): a `path` binding names a FILE. If the file is deleted and a DIRECTORY now
/// occupies that name, the file is genuinely `gone` — the off-index fallback must use `is_file`,
/// not `exists`, so a directory at the path can't keep the file anchor alive.
#[test]
fn path_binding_to_a_dir_replacing_the_file_is_gone() {
    let h = Harness::new();
    set_source_root(&h);
    // A directory sits where the bound file used to be.
    std::fs::create_dir_all(h.root().join("tools/build.sh")).unwrap();
    assert_eq!(
        path_binding_status_after_validate(&h, "tools/build.sh"),
        "gone",
        "a directory occupying a file-bound path does not keep the file anchor alive"
    );
}

/// #98 review (Codex): path bindings are repo-root-relative by contract. An absolute path or one
/// with `..` could resolve OUTSIDE `source_root` (`root.join(abs)` replaces the root), letting an
/// unrelated out-of-repo file mark the anchor alive. Such a binding must be treated as `gone`.
#[test]
fn path_binding_escaping_source_root_is_gone() {
    let h = Harness::new();
    set_source_root(&h);
    // A real file OUTSIDE the source_root, reachable only by escaping it via `..`.
    let outside = h.root().parent().unwrap().join(format!("escape-{}.sh", std::process::id()));
    std::fs::write(&outside, "#!/bin/sh\n").unwrap();
    let traversal = format!("../{}", outside.file_name().unwrap().to_string_lossy());
    let status = path_binding_status_after_validate(&h, &traversal);
    let _ = std::fs::remove_file(&outside);
    assert_eq!(
        status, "gone",
        "a `..`-escaping path must not validate against an out-of-repo file"
    );
}

/// #98 review (Codex): under a shared DB across git worktrees, `index_meta.source_root` holds
/// whichever worktree last indexed. `validate_memories` must prefer the caller-supplied ACTIVE
/// checkout root so a sibling worktree checks its OWN filesystem, not the last indexer's.
#[test]
fn validate_prefers_active_root_over_persisted_meta() {
    let h = Harness::new();
    // Persisted meta points at a bogus root (a stale/sibling worktree); the active checkout is
    // h.root(), where the file actually lives.
    h.conn
        .execute(
            "INSERT OR REPLACE INTO repo_meta(repo_id, key, value)
             VALUES ('__unassigned__', 'source_root', ?1)",
            params![h.root().join("nonexistent-worktree").to_string_lossy()],
        )
        .unwrap();
    std::fs::create_dir_all(h.root().join("tools")).unwrap();
    std::fs::write(h.root().join("tools/notes.Containerfile"), "FROM scratch\n").unwrap();
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "worktree note".to_string(),
        body: "still valid".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget {
            path: Some("tools/notes.Containerfile".to_string()),
            ..Default::default()
        },
    })
    .unwrap();
    validate_memories(&h.conn, Some(h.root())).unwrap();
    let memory = memory_by_id(&h.conn, &created.memory.memory_id).unwrap().unwrap();
    let pb = memory.bindings.iter().find(|b| b.binding_kind == "path").expect("path binding");
    assert_eq!(
        pb.anchor_status, "current",
        "the active checkout root must win over the stale persisted source_root"
    );
}

/// `scip_moniker` binding statuses: `unverified` when the tool has no data at all, `gone` when
/// current data lacks the moniker, `stale` when the moniker's row dangles (its content-derived
/// logical id died after the symbol changed).
#[test]
fn moniker_binding_validation_statuses() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    let memory_id = create_target_memory(&h, sym);

    let moniker_status = |h: &Harness| -> String {
        // Twice: the #492 downgrade hysteresis defers a first gone observation, so the SETTLED
        // status needs two passes (every other status is a fixpoint under repeated validation).
        validate_memories(&h.conn, None).unwrap();
        validate_memories(&h.conn, None).unwrap();
        let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
        memory
            .bindings
            .iter()
            .find(|b| b.binding_kind == "scip_moniker")
            .expect("moniker binding")
            .anchor_status
            .clone()
    };

    assert_eq!(moniker_status(&h), "current");

    // Dangling: the symbol changed — its content-derived logical id died, the row points nowhere.
    h.conn.execute("DELETE FROM logical_symbols WHERE id = 1001", []).unwrap();
    assert_eq!(moniker_status(&h), "stale");

    // A lagging moniker anchor must NOT demote the memory's evidence — the symbol binding is
    // intact, and the moniker self-heals on the next oracle run.
    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let symbol_status = memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "symbol")
        .map(|b| b.anchor_status.clone())
        .unwrap();
    assert_eq!(symbol_status, "current");
    let (direct, stale) = split_active_stale(vec![memory]);
    assert_eq!(direct.len(), 1, "stale moniker anchor must not demote the memory");
    assert!(stale.is_empty());

    // ...and must not count toward the anchor-health totals that drive the "run memory doctor"
    // warnings — doctor hides moniker rows, so counting them would warn about nothing visible.
    let health = rag_rat_query::memory::anchor_health_counts(&h.conn).unwrap();
    assert_eq!(health.stale, 0, "auxiliary moniker anchors are excluded from health counts");

    // Gone: the tool has current data, but not this moniker.
    h.conn
        .execute(
            "UPDATE logical_symbol_monikers SET moniker = 'rust-analyzer cargo test_crate 0.1.0 \
             other().' WHERE logical_symbol_id = 1001",
            [],
        )
        .unwrap();
    assert_eq!(moniker_status(&h), "gone");

    // Unverified: no oracle data for the tool at all.
    h.conn.execute("DELETE FROM logical_symbol_monikers", []).unwrap();
    assert_eq!(moniker_status(&h), "unverified");
}

/// Several defs containment-map to one logical symbol (a struct's fields have no symbol row, so
/// they map up to the enclosing struct alongside its own def). The stored moniker must be the
/// DETERMINISTIC best — shortest, then lexicographic — i.e. the symbol's own moniker, never an
/// arbitrary member's (HashMap-order last-writer would silently break relocation between runs).
#[test]
fn moniker_for_symbol_with_member_defs_is_the_symbols_own() {
    let h = Harness::new();
    // `struct Config { db: u32 }` — one symbol row spanning the whole struct.
    let defs = h.add_file("defs.rs", "struct Config { db: u32 }\n");
    let sym = h.add_symbol_qualified(defs, "Config", "defs.rs::Config", "struct", 0, 25);
    h.add_logical_symbol(3003, "defs.rs", "Config", "defs.rs::Config", sym);

    let struct_moniker = "rust-analyzer cargo test_crate 0.1.0 Config#";
    let field_moniker = "rust-analyzer cargo test_crate 0.1.0 Config#db.";
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![
        // Field def first so a naive insertion-order winner would pick it.
        occurrence(0, 16, 18, field_moniker, SymbolRole::Definition as i32),
        occurrence(0, 7, 13, struct_moniker, SymbolRole::Definition as i32),
    ])]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    assert_eq!(report.monikers_written, 1, "one row per logical symbol, not per def");
    let (moniker, ..) = h.moniker(3003).expect("moniker row written");
    assert_eq!(moniker, struct_moniker, "the symbol's own (shortest) moniker wins");
}

/// A synthetic zero-width per-file definition (scip-typescript emits one ending in `/` at byte
/// `0..0`) must NOT become a symbol's moniker: it containment-maps to the first symbol (whose span
/// starts at 0) and, being shorter, would win shortest-moniker selection and clobber the real one.
/// The namespace-suffix filter in the moniker pass drops it.
#[test]
fn synthetic_file_definition_does_not_overwrite_a_symbols_moniker() {
    let h = Harness::new();
    // `greet`'s span starts at byte 0, so a 0..0 file def would containment-map to it.
    let defs = h.add_file("a.ts", "function greet() {}\n");
    let sym = h.add_symbol_qualified(defs, "greet", "a.ts::greet", "function", 0, 19);
    h.add_logical_symbol(4004, "a.ts", "greet", "a.ts::greet", sym);

    let greet_moniker = "rust-analyzer cargo test_crate 0.1.0 greet().";
    let file_moniker = "rust-analyzer cargo test_crate 0.1.0 `a.ts`/"; // shorter; ends in `/`
    let bytes = scip_bytes_docs(vec![("a.ts", vec![
        // File def first + at 0..0 so a naive containment+shortest winner would pick it.
        occurrence(0, 0, 0, file_moniker, SymbolRole::Definition as i32),
        occurrence(0, 9, 14, greet_moniker, SymbolRole::Definition as i32),
    ])]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    assert_eq!(report.monikers_written, 1, "the namespace symbol must not add a second row");
    let (moniker, ..) = h.moniker(4004).expect("moniker row written");
    assert_eq!(moniker, greet_moniker, "the symbol's own moniker wins, not the file's");
}

/// Moniker-STRING drift (Codex P2): rust-analyzer monikers embed the Cargo package version, so a
/// routine version bump rewrites every string while no symbol changes. The binding's stored
/// content-derived logical id is still live, so validation re-anchors the binding to the new
/// string (`relocated`, reason `moniker-refresh`) instead of marking it gone forever — and a
/// LATER file move can still relocate via the refreshed moniker.
#[test]
fn moniker_string_drift_rebinds_via_live_logical_symbol_then_survives_move() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    let memory_id = create_target_memory(&h, sym);

    // Crate version bump: same symbol, same location, NEW moniker string + tool version.
    let bumped_moniker = "rust-analyzer cargo test_crate 0.2.0 target().";
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        bumped_moniker,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, "v-bumped", COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let moniker_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "scip_moniker").expect("moniker binding");
    assert_eq!(moniker_binding.anchor_status, "relocated");
    assert_eq!(moniker_binding.binding_id, bumped_moniker, "rebound to the current string");
    assert_eq!(moniker_binding.moniker_tool_version.as_deref(), Some("v-bumped"));
    assert_eq!(moniker_binding.relocation_reason.as_deref(), Some("moniker-refresh"));

    // The refreshed anchor still does its job: a later move + content edit relocates via it.
    move_target_with_edit(&h, defs, "function");
    let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
        0,
        3,
        9,
        bumped_moniker,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, "v-bumped", COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    validate_memories(&h.conn, None).unwrap();
    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let symbol_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "symbol").expect("symbol binding");
    assert_eq!(symbol_binding.anchor_status, "relocated");
    assert_eq!(symbol_binding.path.as_deref(), Some("moved.rs"));
    assert_eq!(symbol_binding.relocation_reason.as_deref(), Some("moniker-match"));
}

/// The string-resolution path must NOT refresh the bind-time `moniker_tool_version` (Codex P1):
/// the cross-version corroboration gate compares the CURRENT data's version against bind-time
/// provenance, and a "last verified" refresh would silently downgrade a real cross-version match
/// to same-version.
#[test]
fn string_resolution_preserves_bind_time_tool_version() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    let memory_id = create_target_memory(&h, sym);

    // Move with the SAME moniker string but a NEWER tool version: the stored logical id is dead,
    // so validation takes the string-resolution path.
    move_target_with_edit(&h, defs, "function");
    let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, "v-newer", COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    validate_memories(&h.conn, None).unwrap();

    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let moniker_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "scip_moniker").expect("moniker binding");
    assert_eq!(moniker_binding.anchor_status, "relocated");
    assert_eq!(
        moniker_binding.moniker_tool_version.as_deref(),
        Some(VERSION),
        "bind-time provenance must survive a string-resolution relocate"
    );
}

/// Bare `path` bindings are AREA anchors (like `dir` bindings): a file edit must not stale them —
/// before this, every commit permanently staled every area-level note bound to a touched file,
/// burying real staleness signals. A SPANNED `path:start-end` binding claims specific content and
/// keeps the content-hash staleness.
#[test]
fn bare_path_binding_survives_file_edit_spanned_goes_stale() {
    let h = Harness::new();
    let file = h.add_file("notes.rs", "fn a() {}\nfn b() {}\n");

    let bind_path = |start: Option<i64>, end: Option<i64>, title: &str| {
        create_memory(&h.conn, RepoMemoryCreate {
            kind: "Decision".to_string(),
            title: title.to_string(),
            body: "area note".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: RepoMemoryBindTarget {
                path: Some("notes.rs".to_string()),
                start_line: start,
                end_line: end,
                ..Default::default()
            },
        })
        .unwrap()
        .memory
        .memory_id
    };
    let bare = bind_path(None, None, "bare path note");
    let spanned = bind_path(Some(1), Some(1), "spanned path note");

    // Edit the file: new content, new sha on the files row.
    h.set_file_sha(file, "edited-sha");
    validate_memories(&h.conn, None).unwrap();

    let status =
        |id: &str| memory_by_id(&h.conn, id).unwrap().unwrap().bindings[0].anchor_status.clone();
    assert_eq!(
        status(&bare),
        "current",
        "bare path binding is an area anchor — never content-stale"
    );
    assert_eq!(status(&spanned), "stale", "spanned path binding still claims content");

    // Deleting the file row sends both to gone — after TWO passes, per the #492 downgrade
    // hysteresis (the first observation only arms the marker).
    h.conn.execute("DELETE FROM files WHERE id = ?1", [file]).unwrap();
    validate_memories(&h.conn, None).unwrap();
    validate_memories(&h.conn, None).unwrap();
    assert_eq!(status(&bare), "gone");
    assert_eq!(status(&spanned), "gone");
}

// ---------------------------------------------------------------------------
// Moniker relocation under oracle runs (moved with the fixture harness split).
// ---------------------------------------------------------------------------

/// BEHAVIORAL LIFECYCLE GUARD (#248): the integration test the suite never had. Every prior oracle
/// test wrote+read its outputs with NO reindex in between — which is exactly why the CASCADE-on-
/// reindex bug was invisible for so long. This runs the oracle to populate BOTH oracle-derived
/// outputs (`edge_oracle` via a call/def join, `logical_symbol_monikers` via the def→logical map),
/// then exercises the two reindex shapes that rewrite the volatile parents, and asserts BOTH
/// outputs still resolve through their LIVE-JOIN reads afterward. The two shapes are a FULL reindex
/// (`DELETE FROM edges_data` + a wholesale `logical_symbols` rebuild — DELETE-all + reinsert with
/// the SAME content-derived id, modelling an UNCHANGED symbol) and a per-file INCREMENTAL reindex
/// on an UNCHANGED file (`remove_file_in_scope` deletes + the indexer re-inserts the same edge, the
/// `file_rows.rs` path). It is paired with a CHANGED-file/CHANGED-symbol staleness assertion, so it
/// pins BOTH directions: unchanged content survives, changed content goes stale.
#[test]
fn oracle_outputs_survive_full_and_incremental_reindex() {
    use rag_rat_query::memory::{MonikerResolution, resolve_moniker};

    let h = Harness::new();
    // A caller + a def file. The def's symbol is grouped into a logical symbol (content-derived id
    // 1001 here; stable across rebuilds in production), so the moniker pass writes a moniker for
    // it.
    let caller = h.add_file("caller.rs", "fn caller() { target(); }\n");
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let target_sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", target_sym);
    // The heuristic resolved the call; the oracle CONFIRMS it (an in-corpus verdict + a moniker).
    let edge_v1 = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));

    let bytes = scip_bytes_docs(vec![
        ("caller.rs", vec![occurrence(
            0,
            14,
            20,
            TARGET_MONIKER,
            SymbolRole::UnspecifiedSymbolRole as i32,
        )]),
        ("defs.rs", vec![occurrence(0, 3, 9, TARGET_MONIKER, SymbolRole::Definition as i32)]),
    ]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.rows_written, 1, "the run wrote a verdict");
    assert_eq!(report.monikers_written, 1, "the run wrote a moniker");

    // Both outputs resolve through their LIVE-JOIN reads before any reindex (sanity).
    assert_eq!(h.verdict_count(), 1, "verdict counted before reindex");
    assert!(
        matches!(
            resolve_moniker(&h.conn, TARGET_MONIKER, TOOL.as_db_str()).unwrap(),
            MonikerResolution::Unique { logical_symbol_id: 1001, .. }
        ),
        "moniker resolves to the live logical symbol before reindex"
    );

    // --- FULL reindex: rewrite edges_data (DELETE + re-insert the SAME edge → new rowid) AND
    // rebuild logical_symbols wholesale (DELETE-all + reinsert the SAME content-derived id 1001,
    // the unchanged-symbol case). The caller/def file shas are untouched. ---
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge_v1]).unwrap();
    let edge_v2 = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    assert_ne!(edge_v2, edge_v1, "full reindex minted a new edge rowid");
    // Wholesale logical_symbols rebuild: drop the members + the logical row, reinsert with the SAME
    // id (unchanged symbol → stable content-derived id).
    h.conn
        .execute("DELETE FROM logical_symbol_members WHERE logical_symbol_id = 1001", [])
        .unwrap();
    h.conn.execute("DELETE FROM logical_symbols WHERE id = 1001", []).unwrap();
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", target_sym);

    // BOTH outputs still resolve after the full reindex (re-anchored by content key / stable id).
    assert_eq!(
        h.verdict_count(),
        1,
        "edge_oracle survives the FULL reindex (re-anchored by content key)"
    );
    assert!(
        matches!(
            resolve_moniker(&h.conn, TARGET_MONIKER, TOOL.as_db_str()).unwrap(),
            MonikerResolution::Unique { logical_symbol_id: 1001, .. }
        ),
        "moniker survives the FULL logical_symbols rebuild (stable content-derived id)"
    );

    // --- INCREMENTAL reindex on the UNCHANGED caller.rs: the per-file path deletes the file's
    // edges then the indexer re-inserts the same one. Model `remove_file_in_scope`'s edge
    // delete + the re-insert; the file row + sha stay put. ---
    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge_v2]).unwrap();
    let edge_v3 = h.add_edge(caller, "target", 14, 20, "Exact", Some(target_sym));
    assert_ne!(edge_v3, edge_v2, "incremental reindex minted a new edge rowid");
    assert_eq!(
        h.verdict_count(),
        1,
        "edge_oracle survives the per-file INCREMENTAL reindex of an unchanged file"
    );

    // --- CHANGED-content staleness (the other direction): drift the caller's sha → its verdict
    // goes stale (file_sha mismatch); mint a new logical id for the symbol → its moniker
    // dangles. ---
    h.conn
        .execute("UPDATE files SET sha256 = 'caller-changed' WHERE id = ?1", params![caller])
        .unwrap();
    assert_eq!(
        h.verdict_count(),
        0,
        "a CHANGED file's verdict is stale (file_sha mismatch) — not counted"
    );
    // A changed symbol mints a NEW content-derived logical id; the old moniker row dangles.
    h.conn
        .execute("DELETE FROM logical_symbol_members WHERE logical_symbol_id = 1001", [])
        .unwrap();
    h.conn.execute("DELETE FROM logical_symbols WHERE id = 1001", []).unwrap();
    h.add_logical_symbol(2002, "defs.rs", "target", "defs.rs::target", target_sym);
    assert!(
        matches!(
            resolve_moniker(&h.conn, TARGET_MONIKER, TOOL.as_db_str()).unwrap(),
            MonikerResolution::Dangling
        ),
        "a CHANGED symbol's moniker dangles (its content-derived logical id died) — not resolved"
    );
}
/// The #70 acceptance test: a memory bound to a symbol survives a file move (with a content edit
/// the hash fallback can't survive) via moniker relocation — `relocated`, reason `moniker-match`.
#[test]
fn memory_survives_file_move_via_moniker_relocation() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
    h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
    let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let memory_id = create_target_memory(&h, sym);

    move_target_with_edit(&h, defs, "function");
    // The next oracle run sees the same moniker defined at its new home.
    let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
        0,
        3,
        9,
        TARGET_MONIKER,
        SymbolRole::Definition as i32,
    )])]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let report = validate_memories(&h.conn, None).unwrap();
    assert!(report.relocated >= 1, "expected a relocation, got {report:?}");

    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let symbol_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "symbol").expect("symbol binding");
    assert_eq!(symbol_binding.anchor_status, "relocated");
    assert_eq!(symbol_binding.binding_id, "moved.rs::target");
    assert_eq!(symbol_binding.path.as_deref(), Some("moved.rs"));
    assert_eq!(symbol_binding.relocation_reason.as_deref(), Some("moniker-match"));
    let moniker_binding =
        memory.bindings.iter().find(|b| b.binding_kind == "scip_moniker").expect("moniker binding");
    assert_eq!(moniker_binding.anchor_status, "relocated");
    assert_eq!(moniker_binding.logical_symbol_id, Some(2002));
}

/// A moniker match under a DIFFERENT current tool_version is lower confidence: it relocates only
/// when the stored `symbol_kind` corroborates the candidate.
#[test]
fn cross_version_moniker_match_requires_kind_corroboration() {
    for (new_kind, expect_status) in [("function", "relocated"), ("struct", "gone")] {
        let h = Harness::new();
        let defs = h.add_file("defs.rs", "fn target() {}\n");
        let sym = h.add_symbol_qualified(defs, "target", "defs.rs::target", "function", 0, 14);
        h.add_chunk(defs, "defs.rs::target", "fn target() {}\n");
        h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", sym);
        let bytes = scip_bytes_docs(vec![("defs.rs", vec![occurrence(
            0,
            3,
            9,
            TARGET_MONIKER,
            SymbolRole::Definition as i32,
        )])]);
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
        let memory_id = create_target_memory(&h, sym);

        move_target_with_edit(&h, defs, new_kind);
        // The re-run comes from an UPGRADED tool: same moniker, different tool_version.
        let bytes = scip_bytes_docs(vec![("moved.rs", vec![occurrence(
            0,
            3,
            9,
            TARGET_MONIKER,
            SymbolRole::Definition as i32,
        )])]);
        run_oracle(&h.conn, TOOL, "v-newer", COMMIT, WORKTREE, &bytes, h.root(), None, None)
            .unwrap();

        validate_memories(&h.conn, None).unwrap();
        if expect_status == "gone" {
            // The #492 downgrade hysteresis defers the first gone observation; the relocated arm
            // stays single-pass (a second pass would re-judge the freshly moved anchor).
            validate_memories(&h.conn, None).unwrap();
        }
        let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
        let symbol_binding =
            memory.bindings.iter().find(|b| b.binding_kind == "symbol").expect("symbol binding");
        assert_eq!(
            symbol_binding.anchor_status, expect_status,
            "cross-version match with new_kind={new_kind}"
        );
    }
}

/// #154: a `logical_symbol` binding must stay `current` across a reindex that merely SHIFTS the
/// symbol's lines (an edit elsewhere in the file). The logical symbol's id is content-derived and
/// stable, but chunk ids are reassigned on every re-chunk — so the stored `chunk_id` goes stale.
/// Before the fix the stable-id arm called `validate_bound_chunk`, which found the churned chunk_id
/// missing and returned `gone`; it must instead re-derive the chunk from the live logical symbol.
#[test]
fn logical_symbol_binding_survives_chunk_id_churn_on_reindex() {
    let h = Harness::new();
    let file = h.add_file("a.rs", "fn target() {}\n");
    let sym = h.add_symbol_qualified(file, "target", "a.rs::target", "function", 0, 14);
    h.add_chunk(file, "a.rs::target", "fn target() {}\n");
    h.add_logical_symbol(1001, "a.rs", "target", "a.rs::target", sym);

    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "target invariant".to_string(),
        body: "target stays reentrant".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget { logical_symbol_id: Some(1001), ..Default::default() },
    })
    .unwrap();
    let memory_id = created.memory.memory_id;
    let original_chunk_id = created
        .memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "logical_symbol")
        .expect("logical_symbol binding")
        .chunk_id
        .expect("chunk_id bound");

    // Re-chunk the file: chunk + symbol rows get NEW rowids (as a reindex reassigns them), but the
    // logical symbol keeps its content-derived id 1001 (the symbol is unchanged, just shifted). The
    // content is byte-identical, so the only thing that moved is the chunk_id.
    h.conn.execute("DELETE FROM logical_symbol_members", []).unwrap();
    h.conn.execute("DELETE FROM chunks", []).unwrap();
    h.conn.execute("DELETE FROM symbols", []).unwrap();
    let new_sym = h.add_symbol_qualified(file, "target", "a.rs::target", "function", 0, 14);
    h.add_chunk(file, "a.rs::target", "fn target() {}\n");
    h.conn
        .execute(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, cfg_expr, \
             signature_hash, start_line, end_line) VALUES (1001, ?1, NULL, NULL, 1, 1)",
            params![new_sym],
        )
        .unwrap();

    validate_memories(&h.conn, None).unwrap();

    let memory = memory_by_id(&h.conn, &memory_id).unwrap().unwrap();
    let binding = memory
        .bindings
        .iter()
        .find(|b| b.binding_kind == "logical_symbol")
        .expect("logical_symbol binding");
    assert_eq!(
        binding.anchor_status, "current",
        "a logical_symbol binding must survive chunk_id churn on reindex (#154)"
    );
    assert_ne!(
        binding.chunk_id,
        Some(original_chunk_id),
        "the binding's chunk_id should be refreshed to the re-chunked symbol's new chunk"
    );
}

/// The memory body cap is 8000 chars (raised from 4000 so detailed Invariant/Decision/BugPattern
/// memories aren't forced to drop content). Boundary: 8000 accepted, 8001 rejected.
#[test]
fn memory_body_cap_is_8000_chars() {
    let h = Harness::new();
    let make = |body: String| RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "cap test".to_string(),
        body,
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget { path: Some("a.rs".to_string()), ..Default::default() },
    };
    assert!(create_memory(&h.conn, make("x".repeat(8000))).is_ok(), "8000 chars is accepted");
    let err = create_memory(&h.conn, make("x".repeat(8001))).unwrap_err();
    assert!(err.to_string().contains("body exceeds 8000"), "8001 rejected with the cap: {err}");
}

/// `doctor_attention_count` (behind the MCP staleness nudge) counts active bindings whose anchor is
/// gone/stale, excludes obsolete memories, and matches the population `memory_doctor` lists.
#[test]
fn doctor_attention_count_counts_active_gone_and_stale_bindings() {
    let h = Harness::new();
    let created = create_memory(&h.conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "drift test".to_string(),
        body: "x".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget { path: Some("a.rs".to_string()), ..Default::default() },
    })
    .unwrap();
    let id = created.memory.memory_id;
    let set_status = |status: &str| {
        h.conn
            .execute(
                "UPDATE repo_memory_bindings SET anchor_status = ?2 WHERE memory_id = ?1",
                params![id, status],
            )
            .unwrap();
    };

    set_status("current");
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 0, "current is not counted");
    set_status("gone");
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 1, "gone is counted");
    set_status("stale");
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 1, "stale is counted");
    // An obsolete memory drops out even with a gone binding.
    h.conn.execute("UPDATE repo_memories SET status = 'obsolete' WHERE id = ?1", [&id]).unwrap();
    assert_eq!(doctor_attention_count(&h.conn).unwrap(), 0, "obsolete is excluded");
}

/// The public `memory_attention_count` (the MCP staleness nudge's source) reads from a file DB via
/// a bare read-only open and fails open to 0 on a missing DB — it must never block a tool call.
#[test]
fn memory_attention_count_reads_file_db_and_fails_open() {
    let dir = rag_rat_base::test_scratch::ScratchDir::new("attn");
    let db_path = dir.join("index.sqlite");

    // Missing DB → 0 (fail-open).
    assert_eq!(crate::memory_attention_count(&db_path), 0, "missing DB is 0, never an error");

    // A real file DB with one gone binding → 1.
    {
        let rw = rag_rat_db::storage::IndexConnection::open(&db_path).unwrap();
        rag_rat_db::schema::apply(rw.connection(), &crate::index::migration_hooks()).unwrap();
        let created = create_memory(rw.connection(), RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "drift".to_string(),
            body: "x".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: RepoMemoryBindTarget { path: Some("a.rs".to_string()), ..Default::default() },
        })
        .unwrap();
        rw.connection()
            .execute(
                "UPDATE repo_memory_bindings SET anchor_status = 'gone' WHERE memory_id = ?1",
                [&created.memory.memory_id],
            )
            .unwrap();
    }
    assert_eq!(crate::memory_attention_count(&db_path), 1, "counts the gone binding from disk");
}
