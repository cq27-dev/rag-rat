//! The warm-up document search: which document a backend opens so its server signals
//! readiness.

use super::*;

/// A checkout whose indexed sources live only under a HIDDEN directory can still warm a session.
///
/// The warm-up document search excluded every dot-directory, so such a checkout had a usable
/// database and no findable document, and the prerequisite gate reported the backend `Blocked` for
/// a configuration that would have worked (#1011). The indexed corpus is the authority on where
/// this checkout's sources are; the search now asks it instead of guessing from the name.
#[test]
fn sources_under_a_hidden_directory_can_warm_the_session() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-hidden-sources");
    std::fs::create_dir_all(dir.join(".cache/generated")).unwrap();
    std::fs::write(dir.join(".cache/generated/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&dir, "build/compile_commands.json", &[".cache/generated/main.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &[".cache/generated"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);
    let layout = clangd.resolve_layout(&scope);

    assert_eq!(
        clangd.warmup_document(&scope, &layout),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().join(".cache/generated/main.c")),
        "a hidden directory the checkout indexes is an ordinary source location",
    );
    assert!(clangd.checkout_can_signal_readiness(&scope, &layout), "so it is not blocked");
}

/// The blanket dot-rule was right about tooling state, and dropping it must not lose that: a
/// document is refused because the checkout does not index it, not because of its name.
#[test]
fn a_document_the_checkout_does_not_index_is_never_warmed_on() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-unindexed-documents");
    std::fs::create_dir_all(dir.join(".cache/clangd")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    // Source-shaped files inside a machine-written tree, and one real source.
    std::fs::write(dir.join(".cache/clangd/stale.c"), "int stale(void) { return 0; }\n").unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&dir, "build/compile_commands.json", &["src/main.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);
    let layout = clangd.resolve_layout(&scope);

    assert_eq!(
        clangd.warmup_document(&scope, &layout),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().join("src/main.c")),
        "the indexed source is chosen, never the one under clangd's own index",
    );
}

#[test]
fn enclosing_tsconfig_walks_up_to_the_nearest_project_and_stops_at_the_root() {
    // This is how tsserver assigns a file to a project, and it decides whether opening the
    // file produces an observable load. A file under no project opens as an inferred project
    // SILENTLY, so warming on it teaches the session nothing.
    let (_dir_guard, dir) = checkout("ts-lsp-enclosing");
    std::fs::create_dir_all(dir.join("packages/app/src")).unwrap();
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("packages/app/tsconfig.json"), "{}").unwrap();

    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("packages/app/src/main.ts"), &[
            "tsconfig.json"
        ]),
        Some(dir.join("packages/app")),
        "the nearest enclosing project wins",
    );
    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("scripts/tool.ts"), &["tsconfig.json"]),
        None,
        "a file under no project has none",
    );
}

#[test]
fn a_warmup_document_is_found_under_a_project_declared_by_any_name() {
    // The second widened path. The warm-up search is what makes a backend usable on a checkout
    // whose changed files all sit outside a project, so a name it does not recognise there costs
    // the same thing as one the ancestor walk misses: the session never warms (#1042).
    let (_dir_guard, dir) = checkout("warmup-marker-alternates");
    std::fs::create_dir_all(dir.join("lib/src")).unwrap();
    // Only the SECOND declared name, and the document sits beneath it.
    std::fs::write(dir.join("lib/build.gradle"), "").unwrap();
    std::fs::write(dir.join("lib/src/main.ts"), "export function greet() {}\n").unwrap();

    let found = crate::backend::documents::find_document_in_project(
        &scope(&dir),
        &dir,
        &[Language::TypeScript],
        &["build.gradle.kts", "build.gradle"],
        false,
    );

    assert_eq!(
        found,
        Some(dir.join("lib/src/main.ts")),
        "a project declared by any name yields a warm-up document",
    );
}

#[test]
fn a_warmup_document_is_found_at_any_depth_and_only_inside_a_project() {
    // A project can sit arbitrarily deep in a monorepo; a depth limit would silently disable
    // those checkouts entirely, which is worse than the walk it saves.
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("ts-lsp-warmup-doc");
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/tool.ts"), "export function x() {}\n").unwrap();
    assert_eq!(
        ts.warmup_document(&scope(&dir), &ts.resolve_layout(&scope(&dir))),
        None,
        "a TypeScript file outside every project is not a warm-up document",
    );
    assert!(!ts.checkout_can_signal_readiness(&scope(&dir), &ts.resolve_layout(&scope(&dir))));

    write_project(&dir, "services/teams/foo/web");
    assert_eq!(
        ts.warmup_document(&scope(&dir), &ts.resolve_layout(&scope(&dir))),
        Some(dir.join("services/teams/foo/web/main.ts")),
        "a deeply nested project is still found",
    );
    assert!(ts.checkout_can_signal_readiness(&scope(&dir), &ts.resolve_layout(&scope(&dir))));
}

#[test]
fn a_warmup_document_is_found_when_the_project_marker_is_above_the_index_root() {
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("ts-lsp-warmup-ancestor-marker");
    let checkout = dir.join("repo");
    std::fs::create_dir_all(checkout.join("packages/app/src")).unwrap();
    rag_rat_base::test_git::run(&checkout, &["init"]);
    std::fs::write(checkout.join("tsconfig.json"), "{}").unwrap();
    std::fs::write(checkout.join("packages/app/src/main.ts"), "export function greet() {}\n")
        .unwrap();
    let scope = scope(&checkout.join("packages/app"));
    let layout = ts.resolve_layout(&scope);

    assert_eq!(
        ts.warmup_document(&scope, &layout),
        Some(checkout.join("packages/app/src/main.ts")),
        "a project marker above the index root still makes its indexed descendants warmable",
    );
    assert!(
        ts.checkout_can_signal_readiness(&scope, &layout),
        "the ancestor project marker must keep the checkout from being blocked",
    );
}

#[test]
fn a_warmup_document_is_not_found_when_marker_is_above_checkout_ceiling() {
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("ts-lsp-marker-above-ceiling");
    let checkout = dir.join("repo");
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    rag_rat_base::test_git::run(&checkout, &["init"]);
    std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
    std::fs::write(checkout.join("src/main.ts"), "export function greet() {}\n").unwrap();
    let scope = scope(&checkout);
    let layout = ts.resolve_layout(&scope);

    assert_eq!(
        ts.warmup_document(&scope, &layout),
        None,
        "a marker above the checkout ceiling must not govern checkout files",
    );
}

#[test]
fn the_warmup_search_refuses_a_document_the_checkout_does_not_index() {
    // `node_modules` ships thousands of tsconfigs describing DEPENDENCIES. Warming on one would
    // report the checkout usable while none of ITS files ever resolve.
    //
    // The test supplies a REAL corpus rather than the permissive one: the refusal is now the
    // corpus's, not a directory-name test's, so a corpus that claimed everything would assert
    // nothing. That is the point of the change — the indexer's own answer decides, so the warm-up
    // search cannot disagree with what the pass will actually resolve.
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("ts-lsp-vendored-warmup");
    write_project(&dir, "node_modules/some-dep");
    write_project(&dir, ".cache/tooling");
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    assert_eq!(ts.warmup_document(&scope, &ts.resolve_layout(&scope)), None);
    assert!(!ts.checkout_can_signal_readiness(&scope, &ts.resolve_layout(&scope)));

    // And a hidden directory this checkout DOES index is an ordinary source location — the case the
    // blanket dot-rule got wrong (#1011).
    write_project(&dir, ".cache/generated");
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &[".cache/generated"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    assert_eq!(
        ts.warmup_document(&scope, &ts.resolve_layout(&scope)),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().join(".cache/generated/main.ts")),
    );
}

#[test]
fn a_server_status_backend_needs_no_warmup_document_and_is_never_blocked_on_one() {
    // rust-analyzer reports quiescence for any checkout, so the whole notion is TS-specific
    // and must not leak into the other backend's gating.
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    let (_dir_guard, dir) = checkout("ra-lsp-warmup-doc");
    assert_eq!(rust.warmup_document(&scope(&dir), &rust.resolve_layout(&scope(&dir))), None);
    assert!(
        rust.checkout_can_signal_readiness(&scope(&dir), &rust.resolve_layout(&scope(&dir))),
        "an empty checkout still signals"
    );
    assert!(
        rust.open_signals_readiness(&scope(&dir), "src/lib.rs", &rust.resolve_layout(&scope(&dir))),
        "any document will do"
    );
    assert!(rust.project_model.is_none(), "session-level readiness needs no project");
}

#[test]
fn a_typescript_project_still_has_to_enclose_its_documents() {
    // The other scope, asserted alongside so the two cannot be conflated: a tsconfig sibling
    // of the sources governs nothing, because tsserver resolves a file's project by walking UP
    // from the file.
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("ts-sibling-config");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(dir.join("src/main.ts"), "export const x = 1;\n").unwrap();
    std::fs::write(dir.join("config/tsconfig.json"), "{}").unwrap();
    assert!(
        !ts.open_signals_readiness(&scope(&dir), "src/main.ts", &ts.resolve_layout(&scope(&dir))),
        "a config in a SIBLING directory governs nothing under src/",
    );
    assert_eq!(
        ts.warmup_document(&scope(&dir), &ts.resolve_layout(&scope(&dir))),
        None,
        "and there is nothing to warm on"
    );
}
