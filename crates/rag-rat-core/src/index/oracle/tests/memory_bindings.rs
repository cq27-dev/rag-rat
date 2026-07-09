use super::*;
use crate::query::memory::{
    RepoMemoryBindTarget, RepoMemoryCreate, create_memory, memory_by_id, split_active_stale,
    validate_memories,
};

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
    let health = crate::query::memory::anchor_health_counts(&h.conn).unwrap();
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
