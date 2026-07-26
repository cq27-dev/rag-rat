//! The SwiftPM corpus (#636): deterministic expectations over a REAL, buildable Swift package.
//!
//! The fixture (`tests/fixtures/swift-corpus`) is a two-target SwiftPM package — `Renderer` depends
//! on `CoreKit` — so cross-MODULE calls genuinely exist. That matters because this file pins two
//! different things:
//!
//! 1. **What the tree-sitter baseline gets right**, deterministically: symbol kinds, containment
//!    (scope paths), candidate edges and their confidence, and test detection.
//! 2. **What it honestly CANNOT know** — the cross-module and overloaded calls it must leave
//!    unresolved rather than guess. Those are the edges the SourceKit-LSP oracle (#637) has to
//!    upgrade, and pinning them here is what makes that upgrade measurable instead of anecdotal.
//!
//! A test that only asserted (1) would let a resolver that CONFIDENTLY GUESSES look like an
//! improvement. Asserting (2) is what keeps the baseline honest.

use rag_rat_oracle::{LineIndex, LspEncoding};

use super::*;

/// Index the corpus. SwiftPM puts first-party code under `Sources/` and its test targets under
/// `Tests/`; both are bound so the fixture exercises test detection as well as source extraction.
fn corpus() -> (ScratchRoot, IndexDatabase) {
    let root = fixture_temp_root("swift-corpus");
    let config = source_config_dirs(root.clone(), Language::Swift, &["Sources", "Tests"]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    (root, db)
}

/// Every `(kind, scope_path)` for a symbol name, so an assertion can speak about overloads (two
/// rows sharing a scope path) as precisely as about unique declarations.
fn symbols_named(db: &IndexDatabase, name: &str) -> Vec<(String, String)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare("SELECT kind, scope_path FROM symbols WHERE name = ?1 ORDER BY scope_path")
        .unwrap();
    let rows = stmt
        .query_map([name], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// The `(confidence, resolution)` of every `calls_name` edge with this callee, from the function
/// named `from`. Edge `from_name` is the QUALIFIED name (`path/to/File.swift::fn`), so the caller
/// is matched on its trailing segment.
fn call_states(db: &IndexDatabase, from: &str, to: &str) -> Vec<(String, String)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT edges.confidence, edges.resolution
             FROM edges
             WHERE edges.edge_kind = 'calls_name'
               AND edges.to_name = ?2
               AND COALESCE(edges.from_name, '') LIKE '%::' || ?1",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![from, to], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn is_test_symbol(db: &IndexDatabase, name: &str) -> bool {
    db.storage
        .connection()
        .query_row("SELECT is_test FROM symbols WHERE name = ?1", [name], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or_else(|error| panic!("missing symbol {name}: {error}"))
        == 1
}

/// Swift's declaration kinds all reach the index, with the container relationships intact.
#[test]
fn swift_corpus_extracts_declarations_and_containment() {
    let (root, db) = corpus();

    // Nominal kinds, including the Swift-only ones.
    assert_eq!(symbols_named(&db, "Store"), vec![("actor".into(), "Store".into())]);
    assert_eq!(symbols_named(&db, "Repository"), vec![("protocol".into(), "Repository".into())]);
    // An `extension` is its own symbol, named for what it extends — never colliding with the type.
    assert_eq!(symbols_named(&db, "extension Repository"), vec![(
        "extension".into(),
        "extension Repository".into()
    )]);
    assert_eq!(symbols_named(&db, "Status"), vec![("enum".into(), "Status".into())]);
    assert_eq!(symbols_named(&db, "Service"), vec![("class".into(), "Service".into())]);
    assert_eq!(symbols_named(&db, "Renderer"), vec![("struct".into(), "Renderer".into())]);

    // Containment: members carry their container, and a protocol-extension method carries the
    // PROTOCOL it extends rather than sitting at file scope.
    // `Renderer.render` also binds a LOCAL named `cached`, so the name is not unique — the
    // protocol-extension METHOD is what must carry the protocol as its container.
    assert!(
        symbols_named(&db, "cached")
            .contains(&("function".to_string(), "Repository::cached".to_string())),
        "protocol-extension method carries the protocol: {:?}",
        symbols_named(&db, "cached")
    );
    assert_eq!(symbols_named(&db, "mapped"), vec![("function".into(), "Store::mapped".into())]);
    assert_eq!(symbols_named(&db, "idle"), vec![("enum_case".into(), "Status::idle".into())]);
    assert_eq!(symbols_named(&db, "subscript"), vec![(
        "function".into(),
        "Store::subscript".into()
    )]);

    // OVERLOADS stay independently indexed: two `fetch` declarations under one scope path. A
    // resolver that collapsed them would have nothing to be wrong about later.
    assert_eq!(symbols_named(&db, "fetch"), vec![
        ("function".into(), "Service::fetch".into()),
        ("function".into(), "Service::fetch".into()),
    ]);

    // Same bare name, two MODULES: this is the collision the oracle exists to settle.
    assert_eq!(symbols_named(&db, "load"), vec![
        ("function".into(), "Cache::load".into()),
        ("function".into(), "Repository::load".into()),
        ("function".into(), "Store::load".into()),
    ]);

    let _ = fs::remove_dir_all(&root);
}

/// Test-target symbols are marked `is_test` and production symbols in the same index are not — the
/// realistic SwiftPM layout, where tests live under `Tests/`.
///
/// This does NOT prove the Swift FRAMEWORK detectors (`XCTestCase` inheritance / `@Test` /
/// `@Suite`) on its own: `is_test = is_test_path(path) || backend.is_test_symbol(...)`, and every
/// symbol here sits under `Tests/`, so the path half alone would satisfy these assertions even if
/// the framework logic were removed. The framework detection that matters when test code lives
/// OUTSIDE a `Tests/` path (a sidecar `XCTestCase`, an inline `@Test`) is pinned separately by
/// `parser_tests::swift_test_symbols_are_detected_by_framework_not_only_by_path`, which places
/// those markers on a `Sources/…` path. What THIS test guards is that a realistic test target —
/// both frameworks, `test`-prefixed methods and un-prefixed helpers alike — is marked, and that
/// production code is left alone.
#[test]
fn swift_corpus_marks_test_target_symbols_and_leaves_production_alone() {
    let (root, db) = corpus();

    assert!(is_test_symbol(&db, "StoreTests"), "XCTestCase subclass");
    assert!(is_test_symbol(&db, "testLoadsSeed"), "XCTest method");
    assert!(is_test_symbol(&db, "makeItem"), "helper inside an XCTestCase is still test code");
    assert!(is_test_symbol(&db, "StatusSuite"), "@Suite type");
    assert!(is_test_symbol(&db, "idleIsNotRunning"), "@Test method");
    assert!(is_test_symbol(&db, "cachedDefaultsToLoad"), "free @Test function");

    // Production code in the same index is NOT test code.
    assert!(!is_test_symbol(&db, "Renderer"));
    assert!(!is_test_symbol(&db, "mapped"));

    let _ = fs::remove_dir_all(&root);
}

/// What the baseline resolves, and — the load-bearing half — what it refuses to.
#[test]
fn swift_corpus_pins_the_resolution_baseline_for_the_oracle() {
    let (root, db) = corpus();

    // RESOLVED, because the syntax carries the answer:
    // a qualified enum case (`Status.running`) binds by scope path…
    assert_eq!(call_states(&db, "status", "running"), vec![("Exact".into(), "scope_exact".into())]);
    // …and the shorthand `.idle` binds by bare name — the one shape that evidences a case.
    assert_eq!(call_states(&db, "status", "idle"), vec![(
        "Syntactic".into(),
        "target_name_fallback".into()
    )]);
    // A same-module call with a unique name resolves.
    assert_eq!(call_states(&db, "plainCall", "résumé"), vec![(
        "Syntactic".into(),
        "target_name_fallback".into()
    )]);

    // UNRESOLVED, because the syntax genuinely does NOT carry the answer. These are the edges
    // SourceKit-LSP must upgrade in #637 — and a "baseline improvement" that resolves them by
    // guessing (rather than by compiling) would be a regression in honesty, not a win.
    //
    // `render` calls BOTH `store.load(id:)` (CoreKit.Store.load, across the module boundary) and
    // `cache.load(id:)` (Renderer.Cache.load). Three `load` declarations exist; the receiver types
    // are the only way to tell them apart, and tree-sitter does not have types.
    let loads = call_states(&db, "render", "load");
    assert_eq!(loads.len(), 2, "both `load` call sites are recorded: {loads:?}");
    assert!(
        loads
            .iter()
            .all(|(confidence, resolution)| confidence == "NameOnly" && resolution == "unresolved"),
        "cross-module / same-name calls must stay UNRESOLVED, not be guessed: {loads:?}"
    );

    // Both OVERLOADED `service.fetch(…)` call sites are recorded, and both stay unresolved: only a
    // type-checker can say which `fetch` each reaches.
    let fetches = call_states(&db, "described", "fetch");
    assert_eq!(fetches.len(), 2, "both overload call sites are recorded: {fetches:?}");
    assert!(
        fetches.iter().all(|(_, resolution)| resolution == "unresolved"),
        "an overloaded call cannot be resolved by name: {fetches:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The call on the right of a binary operator reaches the graph at all.
///
/// tree-sitter-swift binds a call's argument list to the whole expression on the operator's left,
/// so `prefix + résumé()` parses with the OPERATOR as the call's target. The extractor used to
/// refuse that target and drop the call — silently losing every `total + item.price()` in a Swift
/// codebase. The corpus is what surfaced it, so the corpus is what pins it.
#[test]
fn swift_corpus_keeps_calls_on_the_right_of_an_operator() {
    let (root, db) = corpus();

    assert_eq!(call_states(&db, "offsetCall", "résumé"), vec![(
        "Syntactic".into(),
        "target_name_fallback".into()
    )]);
    // The method call at the end of a `+` chain, through a receiver.
    assert_eq!(call_states(&db, "described", "describe").len(), 1);

    let _ = fs::remove_dir_all(&root);
}

/// UTF-16 ↔ byte position conversion over real non-ASCII source — the semantic-readiness check for
/// #637, where LSP speaks UTF-16 code units and the index speaks bytes.
///
/// `offsetCall`'s line has `"🚀☕"` before the call, so the callee's byte offset and its LSP
/// `character` offset DIVERGE; `plainCall`'s line is ASCII-only, so they agree. Asserting both is
/// what proves the fixture actually exercises the conversion rather than merely containing an
/// emoji.
#[test]
fn swift_corpus_converts_utf16_positions_over_non_ascii_source() {
    let (root, db) = corpus();
    let source = fs::read_to_string(root.join("Sources/CoreKit/Text.swift")).unwrap();
    let index = LineIndex::new(source.as_bytes(), LspEncoding::Utf16);

    for (caller, expect_divergence) in [("plainCall", false), ("offsetCall", true)] {
        let (start, end) = callee_byte_range(&db, caller, "résumé", "calls_name")
            .unwrap_or_else(|| panic!("no callee range for the {caller} call"));
        let (start, end) = (start as usize, end as usize);

        // The range covers the callee — the accented identifier, not the operator expression.
        assert_eq!(&source[start..end], "résumé", "{caller}: callee range");

        // Round-trip: byte → LSP position → byte is the identity.
        let position = index.position_at_byte(start);
        assert_eq!(
            index.byte_at_position(position),
            Some(start),
            "{caller}: LSP position must round-trip back to the same byte"
        );

        // The column in BYTES, for comparison with the UTF-16 `character`.
        let line_start = source[..start].rfind('\n').map_or(0, |newline| newline + 1);
        let byte_column = start - line_start;
        let character = position.character as usize;
        if expect_divergence {
            assert!(
                character < byte_column,
                "{caller}: the emoji/accents before the call must make the UTF-16 character \
                 offset ({character}) smaller than the byte column ({byte_column}) — otherwise \
                 this fixture is not exercising the conversion at all"
            );
        } else {
            assert_eq!(
                character, byte_column,
                "{caller}: an ASCII-only line must have equal byte and UTF-16 offsets (the \
                 control)"
            );
        }
    }

    let _ = fs::remove_dir_all(&root);
}

/// Coverage floors. Deliberately floors, not exact counts: they catch a broken parse or a collapsed
/// extractor (the failure mode that matters) without turning every fixture edit into a
/// golden-number update. The RESOLUTION baseline above is where exactness lives.
#[test]
fn swift_corpus_meets_its_coverage_floors() {
    let (root, db) = corpus();
    let conn = db.storage.connection();

    let count = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();

    let files: i64 = count("SELECT COUNT(*) FROM files WHERE language = 'swift'");
    assert_eq!(files, 6, "every Swift file in the package is indexed");

    let symbols: i64 = count("SELECT COUNT(*) FROM symbols");
    assert!(symbols >= 50, "the corpus must carry real symbol mass, got {symbols}");

    let calls: i64 = count("SELECT COUNT(*) FROM edges WHERE edge_kind = 'calls_name'");
    assert!(calls >= 15, "the corpus must carry real call mass, got {calls}");

    // Parse coverage: a file that failed to parse yields NO symbols, and a silently-unparsed file
    // would make every other assertion here vacuous. (`Package.swift` is not indexed — it is not
    // under a bound directory — so every indexed Swift file is real source.)
    let symbolless: i64 = count(
        "SELECT COUNT(*) FROM files
         WHERE language = 'swift'
           AND id NOT IN (SELECT DISTINCT file_id FROM symbols)",
    );
    assert_eq!(symbolless, 0, "every indexed Swift file must parse into symbols");

    let _ = fs::remove_dir_all(&root);
}

/// The corpus is a REAL package, so it must actually build — the SourceKit-LSP oracle (#637)
/// resolves against a built module graph, and a corpus that only parses would send phase 3 chasing
/// its own fixture.
///
/// OPT-IN, because a cold SwiftPM build costs minutes and no CI runner is required to carry a Swift
/// toolchain: set `RAG_RAT_SWIFT_BUILD=1`. What it must NEVER do is report success it did not earn
/// — so the default path prints a visible SKIP, and with the flag set an absent toolchain is a
/// FAILURE, not a shrug. `swift --version` is echoed as the toolchain provenance for the run.
#[test]
fn swift_corpus_builds_with_swiftpm() {
    let required = std::env::var("RAG_RAT_SWIFT_BUILD").is_ok_and(|value| value == "1");
    let swift = Command::new("swift").arg("--version").output();
    if !required {
        eprintln!(
            "SKIP swift_corpus_builds_with_swiftpm: set RAG_RAT_SWIFT_BUILD=1 to build the corpus \
             with SwiftPM (a cold build takes minutes and needs a Swift toolchain). The corpus's \
             INDEXING assertions run unconditionally and need no toolchain."
        );
        return;
    }
    let version = match swift {
        Ok(output) if output.status.success() =>
            String::from_utf8_lossy(&output.stdout).to_string(),
        _ => panic!(
            "RAG_RAT_SWIFT_BUILD=1 but no working `swift` on PATH — refusing to report a Swift \
             build as passing without a toolchain. Install one (swiftly / swift.org) or unset the \
             flag."
        ),
    };
    eprintln!("swift toolchain provenance: {}", version.lines().next().unwrap_or("unknown"));

    let root = fixture_temp_root("swift-corpus");
    // `--build-tests` so the TEST target compiles too. Plain `swift build` skips it, which would
    // let a broken `CoreKitTests` — the half of the fixture that exercises XCTest and
    // swift-testing — sail through a check whose whole purpose is "this package really builds".
    let build = Command::new("swift")
        .args(["build", "--build-tests", "--package-path", &root.to_string_lossy()])
        .output()
        .expect("run swift build");
    assert!(
        build.status.success(),
        "the corpus must build with SwiftPM:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );

    let _ = fs::remove_dir_all(&root);
}
