//! Registry, layout, and warm-up-document behaviour for the live backends.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use rag_rat_base::language::Language;

use super::documents::enclosing_project_dir;
use super::layout::first_json_object;
use super::registry::LiveBackend;
use crate::OracleTool;

#[test]
fn live_backends_are_exactly_the_non_batch_tools() {
    // The two must stay in lockstep: a live tool with no backend entry would be enumerated by
    // the watcher and spawn nothing, and a backend for a batch tool would double-write edges
    // the batch pass already owns authoritatively.
    for &tool in OracleTool::ALL {
        assert_eq!(
            LiveBackend::for_tool(tool).is_some(),
            !tool.batch_capable(),
            "{} disagrees about being a live backend",
            tool.as_db_str()
        );
    }
}

#[test]
fn every_live_backend_copies_monikers_from_a_batch_tool_for_its_own_language() {
    // A live verdict's `scip_symbol` is its batch counterpart's moniker verbatim. If the two
    // resolved different languages the copy would be meaningless, so the pairing is asserted
    // rather than assumed.
    for backend in LiveBackend::all() {
        let source = backend
            .tool
            .batch_moniker_source()
            .unwrap_or_else(|| panic!("{} has no moniker source", backend.tool.as_db_str()));
        assert!(source.batch_capable(), "a moniker source must be a batch tool");
        let batch_languages = crate::ToolManifest::for_tool(source).languages;
        for language in backend.languages {
            assert!(
                batch_languages.contains(&language.as_str()),
                "{} resolves {language} but copies monikers from {}, which indexes \
                 {batch_languages:?}",
                backend.tool.as_db_str(),
                source.as_db_str(),
            );
        }
    }
}

#[test]
fn every_live_backend_declares_ids_for_the_extensions_its_language_claims() {
    // `claims_path` admits a file to the worklist and `language_id_for` decides how it is
    // opened; a gap between them means a file gets resolved under a fallback id.
    for backend in LiveBackend::all() {
        for extension in backend.languages.iter().flat_map(|l| l.target_extensions()) {
            let path = format!("src/file.{extension}");
            assert!(backend.claims_path(&path), "{path} must be claimed");
            assert!(
                backend.language_ids.iter().any(|(ext, _)| ext == extension),
                "{} claims .{extension} but declares no languageId for it",
                backend.tool.as_db_str(),
            );
        }
    }
}

#[test]
fn typescript_opens_tsx_as_typescriptreact() {
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    assert_eq!(ts.language_id_for("src/main.ts"), "typescript");
    assert_eq!(ts.language_id_for("src/App.tsx"), "typescriptreact");
    // An extension the table doesn't name still opens under the backend's fallback rather
    // than an empty id the server would reject.
    assert_eq!(ts.language_id_for("src/no-extension"), "typescript");
    assert!(!ts.claims_path("src/lib.rs"), "another language's file never enters the worklist");
}

/// The `--compile-commands-dir` argument for `dir`, built the way production builds it.
fn compdb_arg(dir: &Path) -> OsString {
    let mut arg = OsString::from("--compile-commands-dir=");
    arg.push(dir.as_os_str());
    arg
}

/// A compilation database with one real entry. `[]` is syntactically valid but describes no
/// project, and clangd emits no readiness cycle for it — writing that in a fixture would
/// assert the very bug `marker_is_usable` exists to catch.
const COMPDB: &str = r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c"}]"#;

/// A TypeScript project at `relative_dir` holding one `main.ts`.
fn write_project(root: &Path, relative_dir: &str) {
    let dir = root.join(relative_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
    std::fs::write(dir.join("main.ts"), "export function greet() {}\n").unwrap();
}

#[test]
fn enclosing_tsconfig_walks_up_to_the_nearest_project_and_stops_at_the_root() {
    // This is how tsserver assigns a file to a project, and it decides whether opening the
    // file produces an observable load. A file under no project opens as an inferred project
    // SILENTLY, so warming on it teaches the session nothing.
    let dir = rag_rat_base::test_scratch::ScratchDir::new("ts-lsp-enclosing");
    std::fs::create_dir_all(dir.join("packages/app/src")).unwrap();
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("packages/app/tsconfig.json"), "{}").unwrap();

    assert_eq!(
        enclosing_project_dir(&dir, &dir.join("packages/app/src/main.ts"), "tsconfig.json"),
        Some(dir.join("packages/app")),
        "the nearest enclosing project wins",
    );
    assert_eq!(
        enclosing_project_dir(&dir, &dir.join("scripts/tool.ts"), "tsconfig.json"),
        None,
        "a file under no project has none",
    );
}

#[test]
fn a_warmup_document_is_found_at_any_depth_and_only_inside_a_project() {
    // A project can sit arbitrarily deep in a monorepo; a depth limit would silently disable
    // those checkouts entirely, which is worse than the walk it saves.
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("ts-lsp-warmup-doc");
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/tool.ts"), "export function x() {}\n").unwrap();
    assert_eq!(
        ts.warmup_document(&dir, &ts.resolve_layout(&dir)),
        None,
        "a TypeScript file outside every project is not a warm-up document",
    );
    assert!(!ts.checkout_can_signal_readiness(&dir, &ts.resolve_layout(&dir)));

    write_project(&dir, "services/teams/foo/web");
    assert_eq!(
        ts.warmup_document(&dir, &ts.resolve_layout(&dir)),
        Some(dir.join("services/teams/foo/web/main.ts")),
        "a deeply nested project is still found",
    );
    assert!(ts.checkout_can_signal_readiness(&dir, &ts.resolve_layout(&dir)));
}

#[test]
fn the_warmup_search_ignores_vendored_and_vcs_directories() {
    // `node_modules` ships thousands of tsconfigs describing DEPENDENCIES. Warming on one
    // would report the checkout usable while none of ITS files ever resolve.
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("ts-lsp-vendored-warmup");
    write_project(&dir, "node_modules/some-dep");
    write_project(&dir, ".cache/tooling");
    assert_eq!(ts.warmup_document(&dir, &ts.resolve_layout(&dir)), None);
    assert!(!ts.checkout_can_signal_readiness(&dir, &ts.resolve_layout(&dir)));
}

#[test]
fn a_server_status_backend_needs_no_warmup_document_and_is_never_blocked_on_one() {
    // rust-analyzer reports quiescence for any checkout, so the whole notion is TS-specific
    // and must not leak into the other backend's gating.
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("ra-lsp-warmup-doc");
    assert_eq!(rust.warmup_document(&dir, &rust.resolve_layout(&dir)), None);
    assert!(
        rust.checkout_can_signal_readiness(&dir, &rust.resolve_layout(&dir)),
        "an empty checkout still signals"
    );
    assert!(
        rust.open_signals_readiness(&dir, "src/lib.rs", &rust.resolve_layout(&dir)),
        "any document will do"
    );
    assert!(rust.project_marker.is_none(), "session-level readiness needs no project");
}

#[test]
fn clangd_serves_c_and_cpp_from_one_backend() {
    // The first backend whose language set is not a singleton. Both languages must reach its
    // worklist, or half its files would silently never be resolved.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert!(clangd.resolves_language(Language::C));
    assert!(clangd.resolves_language(Language::Cpp));
    assert!(!clangd.resolves_language(Language::Rust));
    for path in ["src/a.c", "src/a.h", "src/a.cpp", "src/a.cc", "src/a.hpp"] {
        assert!(clangd.claims_path(path), "{path} must be claimed");
    }
    assert!(!clangd.claims_path("src/a.rs"));
    assert!(!clangd.claims_path("src/a.ts"));
}

#[test]
fn clangd_opens_each_dialect_under_its_own_language_id() {
    // A C++ file opened as `c` parses under the wrong dialect, so the extension decides.
    // `.h` follows the language registry's default owner (C), which clangd copes with.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert_eq!(clangd.language_id_for("src/a.c"), "c");
    assert_eq!(clangd.language_id_for("src/a.h"), "c");
    for path in ["src/a.cc", "src/a.cpp", "src/a.cxx", "src/a.hpp", "src/a.hh"] {
        assert_eq!(clangd.language_id_for(path), "cpp", "{path}");
    }
}

#[test]
fn a_backends_project_marker_is_the_file_its_prerequisite_looks_for() {
    // The warm-up search and the prerequisite gate must ask the SAME question, or a checkout
    // could pass the gate and still have nothing to warm on (or vice versa).
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-marker");
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert_eq!(clangd.project_marker.map(|m| m.file), Some("compile_commands.json"));
    assert!(
        !clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)),
        "no compdb ⇒ no signal possible"
    );

    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    assert!(
        !clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)),
        "sources alone are not a project"
    );
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    assert_eq!(
        clangd.warmup_document(&dir, &clangd.resolve_layout(&dir)),
        Some(dir.join("src/main.c"))
    );
    assert!(clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)));
    // The two live backends' project markers can coexist in one checkout, so a document
    // qualifies only if THIS backend could open it — not merely because a project contains it.
    assert!(clangd.open_signals_readiness(&dir, "src/main.c", &clangd.resolve_layout(&dir)));
    assert!(
        !clangd.open_signals_readiness(&dir, "src/app.ts", &clangd.resolve_layout(&dir)),
        "another language's file is not a clangd warm-up document, project or not",
    );
}

#[test]
fn an_out_of_tree_compilation_database_still_counts_as_a_project() {
    // A tsconfig DECLARES the sources beneath it; a compile_commands.json is a build artifact
    // that need not sit above anything. The standard out-of-tree CMake layout puts it under
    // `build/` with the sources in `src/` — measured, clangd resolves across translation units
    // there just fine, so requiring the marker to be an ancestor would report an ordinary
    // CMake project as Blocked.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-out-of-tree");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    assert!(
        !clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)),
        "sources with no compdb anywhere"
    );

    std::fs::write(dir.join("build/compile_commands.json"), COMPDB).unwrap();
    assert!(
        clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)),
        "a compdb ANYWHERE in the checkout makes the backend usable",
    );
    assert_eq!(
        clangd.warmup_document(&dir, &clangd.resolve_layout(&dir)),
        Some(dir.join("src/main.c"))
    );
    assert!(
        clangd.open_signals_readiness(&dir, "src/main.c", &clangd.resolve_layout(&dir)),
        "a source with no ancestor compdb still warms clangd",
    );
}

#[test]
fn clangd_is_told_where_a_compilation_database_it_could_not_find_lives() {
    // clangd searches only an opened file's ancestors and their `build/` subdirectory.
    // Measured: with the database in `out/` and no flag it emits no progress at all and
    // resolves calls to header declarations; with `--compile-commands-dir` it resolves across
    // translation units. Accepting the checkout is only honest because we pass the directory.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-compdb-dir");
    std::fs::create_dir_all(dir.join("out")).unwrap();
    std::fs::write(dir.join("out/compile_commands.json"), COMPDB).unwrap();

    let args = clangd.spawn_args(&["--background-index"], &clangd.resolve_layout(&dir));
    assert_eq!(args[0], "--background-index", "the static argv comes first");
    assert!(
        args.contains(&compdb_arg(&dir.join("out"))),
        "the discovered database directory must be passed: {args:?}",
    );
}

#[test]
fn a_file_whose_database_the_session_cannot_reach_is_not_resolvable() {
    // The sharpest failure this backend has: with several databases the session points at
    // none, so a file whose database clangd cannot find on its own gets heuristic flags —
    // measured, that resolves a cross-unit call to the callee's HEADER DECLARATION. The live
    // pass must skip such files rather than persist the wrong answer.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-unreachable-db");
    // `proj-a` keeps its database where clangd looks (`build/`); `proj-b` does not.
    std::fs::create_dir_all(dir.join("proj-a/build")).unwrap();
    std::fs::create_dir_all(dir.join("proj-b/out")).unwrap();
    std::fs::write(dir.join("proj-a/build/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("proj-b/out/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("proj-a/main.c"), "int a(void){return 0;}\n").unwrap();
    std::fs::write(dir.join("proj-b/main.c"), "int b(void){return 0;}\n").unwrap();
    let layout = clangd.resolve_layout(&dir);

    assert!(
        clangd.session_can_resolve(&dir, "proj-a/main.c", &layout),
        "clangd finds proj-a's database beside it",
    );
    assert!(
        !clangd.session_can_resolve(&dir, "proj-b/main.c", &layout),
        "proj-b's database is somewhere clangd will not look, and nothing points it there",
    );

    // With a SINGLE database the session is pointed at it, so every file is configured —
    // including one whose database is nowhere near it.
    let single = rag_rat_base::test_scratch::ScratchDir::new("clangd-single-db");
    std::fs::create_dir_all(single.join("out")).unwrap();
    std::fs::create_dir_all(single.join("src")).unwrap();
    std::fs::write(single.join("out/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(single.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    let single_layout = clangd.resolve_layout(&single);
    assert!(clangd.session_can_resolve(&single, "src/main.c", &single_layout));
}

#[test]
fn a_re_resolved_layout_reports_when_the_pinned_database_changed() {
    // The session caches this layout so it does not re-walk the checkout every pass, and
    // re-resolves once it ages out. What matters on re-resolution is whether the checkout
    // still pins the SAME database: the server was spawned with an argv derived from the old
    // one, so a change cannot be corrected in place. Both directions are dangerous — losing
    // the database leaves the server pointed at a directory that no longer exists, and gaining
    // one leaves the new project's files analysed with the old project's flags.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-relayout");
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::write(dir.join("build/compile_commands.json"), COMPDB).unwrap();
    let pinned = clangd.resolve_layout(&dir);
    assert!(pinned.pins_same_database_as(&clangd.resolve_layout(&dir)), "unchanged checkout");

    // A SECOND database appears: the checkout no longer pins one, so the session must go.
    std::fs::create_dir_all(dir.join("other/build")).unwrap();
    std::fs::write(dir.join("other/build/compile_commands.json"), COMPDB).unwrap();
    assert!(
        !pinned.pins_same_database_as(&clangd.resolve_layout(&dir)),
        "a database added mid-session must invalidate a pinned layout",
    );

    // And the losing direction: the sole database is removed.
    std::fs::remove_file(dir.join("other/build/compile_commands.json")).unwrap();
    std::fs::remove_file(dir.join("build/compile_commands.json")).unwrap();
    assert!(!pinned.pins_same_database_as(&clangd.resolve_layout(&dir)));

    // A backend with no project marker pins nothing and never goes stale.
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    assert!(rust.resolve_layout(&dir).pins_same_database_as(&rust.resolve_layout(&dir)));
}

#[test]
fn a_nearer_empty_database_does_not_make_a_file_configured() {
    // In a multi-database checkout the session points at none, so per-file discovery decides.
    // clangd picks up the NEAREST database — if that one is empty it configures nothing, and
    // the file must not count as resolvable merely because some other project has a real one.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-nearer-empty");
    // THREE projects, so the checkout stays multi-database after one is hollowed out —
    // otherwise it would collapse to the single-database case, where the session pins the one
    // remaining database and every file is configured by it.
    for project in ["good", "also-good", "hollow"] {
        std::fs::create_dir_all(dir.join(project).join("build")).unwrap();
        std::fs::write(dir.join(project).join("build/compile_commands.json"), COMPDB).unwrap();
        std::fs::write(dir.join(project).join("main.c"), "int f(void){return 0;}\n").unwrap();
    }
    let layout = clangd.resolve_layout(&dir);
    assert!(clangd.session_can_resolve(&dir, "hollow/main.c", &layout));

    // Hollow out the nearer database; the file is no longer configured, while its sibling
    // project is untouched.
    std::fs::write(dir.join("hollow/build/compile_commands.json"), "[]").unwrap();
    let layout = clangd.resolve_layout(&dir);
    assert!(
        !clangd.session_can_resolve(&dir, "hollow/main.c", &layout),
        "an empty nearest database configures nothing",
    );
    assert!(clangd.session_can_resolve(&dir, "good/main.c", &layout));
}

#[test]
fn a_databases_usability_is_read_once_per_layout() {
    // `session_can_resolve` runs once per worklist path per pass, and every source file in a
    // directory shares the same nearest database — so re-reading and re-parsing that database
    // per file multiplies the read by the worklist, while the maintenance pass holds the
    // repository write lock. The verdict is memoized for as long as the layout is trusted:
    // with the layout held, replacing the database with an unusable one cannot change the
    // answer, because the file is not read a second time.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-usability-memo");
    // Two databases, so the per-file discovery path is what decides — a single-database
    // checkout pins instead and never asks about a file's own database at all.
    for project in ["proj-a", "proj-b"] {
        std::fs::create_dir_all(dir.join(project).join("build")).unwrap();
        std::fs::write(dir.join(project).join("build/compile_commands.json"), COMPDB).unwrap();
        std::fs::write(dir.join(project).join("main.c"), "int f(void){return 0;}\n").unwrap();
    }
    let layout = clangd.resolve_layout(&dir);
    assert!(clangd.session_can_resolve(&dir, "proj-a/main.c", &layout));

    std::fs::write(dir.join("proj-a/build/compile_commands.json"), "[]").unwrap();
    assert!(
        clangd.session_can_resolve(&dir, "proj-a/main.c", &layout),
        "the answer must come from the memo, not from a second read of the database",
    );
    // The memo lives on the layout value, so a re-resolved layout sees the change — that is
    // what keeps LAYOUT_MAX_AGE the only staleness window this introduces.
    assert!(!clangd.session_can_resolve(&dir, "proj-a/main.c", &clangd.resolve_layout(&dir)));
}

#[test]
fn a_broken_second_database_still_disqualifies_global_pinning() {
    // `--compile-commands-dir` is GLOBAL. With one working database and one empty one,
    // recording only the working site would look like a single-database checkout and pin it —
    // handing its flags to the files of the broken project, which clangd would otherwise
    // resolve by stopping at their own nearer database. Both are wrong for those files, but
    // only pinning also makes them look configured.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-broken-second-db");
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    std::fs::create_dir_all(dir.join("sub/build")).unwrap();
    std::fs::write(dir.join("sub/build/compile_commands.json"), "[]").unwrap();
    std::fs::write(dir.join("sub/main.c"), "int s(void){return 0;}\n").unwrap();
    std::fs::write(dir.join("root.c"), "int r(void){return 0;}\n").unwrap();

    let layout = clangd.resolve_layout(&dir);
    assert_eq!(
        clangd.spawn_args(&["--background-index"], &layout),
        vec![OsString::from("--background-index")],
        "a second database disqualifies pinning even when it is unusable",
    );
    assert!(clangd.session_can_resolve(&dir, "root.c", &layout));
    assert!(
        !clangd.session_can_resolve(&dir, "sub/main.c", &layout),
        "files of the broken project are not resolvable by either route",
    );

    // Remove the broken one and the checkout is genuinely single-database again.
    std::fs::remove_file(dir.join("sub/build/compile_commands.json")).unwrap();
    let layout = clangd.resolve_layout(&dir);
    assert!(clangd.spawn_args(&["--background-index"], &layout).contains(&compdb_arg(&dir)));
    assert!(clangd.session_can_resolve(&dir, "sub/main.c", &layout));
}

#[test]
fn an_entry_missing_a_required_field_is_not_a_usable_database() {
    // clangd rejects an entry lacking `directory` or a compiler invocation and falls back to
    // generic flags, so a well-formed entry naming only a file is not a usable database.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-entry-fields");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    let incomplete = [
        r#"[{"file":"/x/a.c"}]"#,
        r#"[{"file":"/x/a.c","command":"cc -c a.c"}]"#,
        r#"[{"file":"/x/a.c","directory":"/x"}]"#,
    ];
    for entry in incomplete {
        std::fs::write(dir.join("compile_commands.json"), entry).unwrap();
        assert!(
            !clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)),
            "{entry} is missing a field clangd requires",
        );
    }
    // Either invocation form is accepted.
    for complete in [
        r#"[{"file":"/x/a.c","directory":"/x","command":"cc -c a.c"}]"#,
        r#"[{"file":"/x/a.c","directory":"/x","arguments":["cc","-c","a.c"]}]"#,
    ] {
        std::fs::write(dir.join("compile_commands.json"), complete).unwrap();
        assert!(
            clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)),
            "{complete} is a usable database",
        );
    }
}

#[test]
fn the_nearest_database_decides_even_when_it_is_unusable() {
    // clangd loads the first database it finds walking up and falls back to generic flags if
    // it configures nothing — it does NOT continue to a farther ancestor. Skipping past an
    // unusable nearer database would declare the file configured by one clangd never
    // consults, and the pass would trust a fallback-flags answer.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-nearest-wins");
    // A usable database at the root, plus a second project so the layout stays multi-database
    // (single-database checkouts pin instead of using per-file discovery).
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    std::fs::create_dir_all(dir.join("elsewhere/build")).unwrap();
    std::fs::write(dir.join("elsewhere/build/compile_commands.json"), COMPDB).unwrap();
    std::fs::create_dir_all(dir.join("sub/build")).unwrap();
    std::fs::write(dir.join("sub/main.c"), "int s(void){return 0;}\n").unwrap();
    let layout = clangd.resolve_layout(&dir);
    assert!(clangd.session_can_resolve(&dir, "sub/main.c", &layout), "falls back to the root");

    // Now `sub` has its own EMPTY database. clangd stops there, so the file is not configured
    // — even though the root database above it is perfectly good.
    std::fs::write(dir.join("sub/build/compile_commands.json"), "[]").unwrap();
    let layout = clangd.resolve_layout(&dir);
    assert!(
        !clangd.session_can_resolve(&dir, "sub/main.c", &layout),
        "an unusable NEARER database means fallback flags, not the ancestor's database",
    );
}

#[test]
fn the_first_database_entry_is_parsed_not_pattern_matched() {
    // Scanning for a token is wrong in BOTH directions: it rejects a valid database whose
    // first entry is larger than the window, and accepts a hollow one that merely contains
    // the token inside an unrelated string. Parsing the first entry settles both.
    assert_eq!(first_json_object(r#"[{"a":1},{"b":2}]"#), Some(r#"{"a":1}"#));
    assert_eq!(
        first_json_object(r#"[{"command":"cc -D'{' x.c","file":"x.c"}]"#),
        Some(r#"{"command":"cc -D'{' x.c","file":"x.c"}"#),
        "a brace inside a compile command must not end the object early",
    );
    assert_eq!(
        first_json_object(r#"[{"command":"cc \"{\" x.c","file":"x.c"}]"#),
        Some(r#"{"command":"cc \"{\" x.c","file":"x.c"}"#),
        "nor may an escaped quote end the string early",
    );
    assert_eq!(first_json_object("[]"), None);
    assert_eq!(first_json_object(r#"[{"unterminated": 1"#), None);

    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-db-shape");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    // A database whose text merely MENTIONS the key names no translation unit.
    let hollows = ["[]", "{}", r#"[{"note":"{"}]"#, r#"[{"command":"cc \"file\" x.c"}]"#];
    for hollow in hollows {
        std::fs::write(dir.join("compile_commands.json"), hollow).unwrap();
        assert!(
            !clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)),
            "{hollow} names no translation unit",
        );
    }
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    assert!(clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)));

    // A first entry that puts a large `arguments` array before `file` is still valid — a
    // fixed-size byte window would have rejected it.
    let bulky = format!(
        r#"[{{"directory":"/x","arguments":[{}],"file":"/x/a.c"}}]"#,
        (0..40_000).map(|i| format!(r#""-DBIG{i}=1""#)).collect::<Vec<_>>().join(","),
    );
    assert!(bulky.len() > 512 * 1024, "the fixture must exceed a small scan window");
    std::fs::write(dir.join("compile_commands.json"), &bulky).unwrap();
    assert!(
        clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)),
        "a valid database must not be rejected for putting `file` late in a big first entry",
    );
}

#[test]
fn a_symlinked_build_directory_is_still_searched() {
    // `build -> cmake-build-debug` is an ordinary layout, and the database is reachable
    // through the checkout path — but a symlink is not a directory to `DirEntry::file_type`.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-symlinked-build");
    std::fs::create_dir_all(dir.join("cmake-build-debug")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("cmake-build-debug/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("cmake-build-debug"), dir.join("build")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(dir.join("cmake-build-debug"), dir.join("build")).unwrap();

    assert!(clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)));
}

#[cfg(unix)]
#[test]
fn a_symlink_cycle_cannot_hang_the_marker_search() {
    // Following directory symlinks is what makes the case above work, and it is also what
    // makes a cycle possible. The search must terminate rather than recurse forever.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-symlink-cycle");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::os::unix::fs::symlink(dir.path(), dir.join("nested/loop")).unwrap();
    // Terminates; the checkout has no database, so it reports none.
    assert!(clangd.resolve_layout(&dir).sole_marker_dir().is_none());
}

#[cfg(unix)]
#[test]
fn two_symlink_cycles_cannot_make_the_marker_search_explode() {
    // ONE cycle costs the depth bound in visits; TWO links pointing at the same ancestor make
    // the walk branch at every level, so the number of paths through them is exponential in
    // that bound — replaying this traversal without a visited set over symlink targets did not
    // finish in 60 seconds on a tree of this shape. `resolve_layout` runs on every spawn
    // attempt while the maintenance pass holds the repository write lock, so that is a wedged
    // watcher, not a slow scan.
    //
    // No database anywhere, deliberately: the search stops at two sites, and a database
    // reachable through the links would supply the second one and mask the explosion.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-two-symlink-cycles");
    std::os::unix::fs::symlink(dir.path(), dir.join("loop-a")).unwrap();
    std::os::unix::fs::symlink(dir.path(), dir.join("loop-b")).unwrap();

    // Run the search off-thread with a bounded wait, so a regression fails this test in
    // seconds instead of hanging the suite for as long as CI allows.
    let root = dir.path().to_path_buf();
    let (done, resolved) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pinned = clangd.resolve_layout(&root).sole_marker_dir().map(Path::to_path_buf);
        let _ = done.send(pinned);
    });
    let pinned = resolved
        .recv_timeout(Duration::from_secs(20))
        .expect("the marker search must terminate with two symlink cycles present");
    assert_eq!(pinned, None, "the checkout holds no database, through the links or otherwise");
}

#[test]
fn a_hidden_build_under_dot_cache_is_still_a_database() {
    // Only clangd's OWN index is off-limits under `.cache` — excluding the whole subtree would
    // contradict supporting hidden build directories at all.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-dot-cache-build");
    std::fs::create_dir_all(dir.join(".cache/cmake-build")).unwrap();
    std::fs::create_dir_all(dir.join(".cache/clangd/index")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join(".cache/cmake-build/compile_commands.json"), COMPDB).unwrap();
    // clangd's own index directory must never be mistaken for a project of ours.
    std::fs::write(dir.join(".cache/clangd/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();

    let layout = clangd.resolve_layout(&dir);
    assert_eq!(
        layout.sole_marker_dir(),
        Some(dir.join(".cache/cmake-build").as_path()),
        "the hidden build counts, and clangd's own index does not",
    );
}

#[test]
fn an_empty_compilation_database_is_not_a_project() {
    // `[]` is valid JSON and a valid database file, but describes nothing to load: measured,
    // clangd emits no readiness cycle for it at all. Accepting it would report the backend
    // runnable while it could only ever sit in `Warming`.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-empty-db");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void){return 0;}\n").unwrap();
    std::fs::write(dir.join("compile_commands.json"), "[]").unwrap();
    assert!(!clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)));

    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    assert!(clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)));
}

#[test]
fn a_hidden_build_directory_still_counts_as_a_compilation_database() {
    // A build directory may legitimately be hidden (`.build/`), and a database there is as
    // real as one in `build/`. The DOCUMENT search still skips dot-directories — those hold
    // tooling state, not this checkout's sources — so the two searches differ on purpose.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-hidden-build");
    std::fs::create_dir_all(dir.join(".build")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join(".build/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void){return 0;}\n").unwrap();

    assert!(clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)));
    assert!(
        clangd
            .spawn_args(&["--background-index"], &clangd.resolve_layout(&dir))
            .contains(&compdb_arg(&dir.join(".build"))),
    );
    // The warm-up document still comes from the visible tree.
    assert_eq!(
        clangd.warmup_document(&dir, &clangd.resolve_layout(&dir)),
        Some(dir.join("src/main.c"))
    );
}

#[test]
fn a_vendored_or_vcs_database_is_never_mistaken_for_the_checkouts_own() {
    // Counting a stray database would be worse than missing one: it would flip a working
    // single-database checkout into the multi-database mode and drop the flag that makes it
    // resolvable.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-vendored-db");
    std::fs::create_dir_all(dir.join("node_modules/dep")).unwrap();
    std::fs::create_dir_all(dir.join(".git/weird")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("node_modules/dep/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join(".git/weird/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void){return 0;}\n").unwrap();

    assert!(
        clangd
            .spawn_args(&["--background-index"], &clangd.resolve_layout(&dir))
            .contains(&OsString::from(format!("--compile-commands-dir={}", dir.display()))),
        "the checkout's own database is still the single unambiguous one",
    );
}

#[test]
fn several_compilation_databases_are_left_to_the_servers_own_per_file_lookup() {
    // `--compile-commands-dir` is GLOBAL: it overrides clangd's per-file search for every
    // document. With one database that is exactly right; with several it would force one
    // project's flags onto another's files, and wrong `-D`/include flags select a different
    // `#ifdef` branch — a wrong definition, persisted. So the flag is only passed when it is
    // unambiguous, and otherwise clangd's own per-file lookup (ancestors and their `build/`)
    // decides, which is correct by construction.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-multi-db");
    for project in ["proj-a", "proj-b"] {
        std::fs::create_dir_all(dir.join(project).join("build")).unwrap();
        std::fs::write(dir.join(project).join("build/compile_commands.json"), COMPDB).unwrap();
        std::fs::write(dir.join(project).join("main.c"), "int main(void){return 0;}\n").unwrap();
    }
    assert_eq!(
        clangd.spawn_args(&["--background-index"], &clangd.resolve_layout(&dir)),
        vec![OsString::from("--background-index")],
        "no database may be forced globally when several exist",
    );
    // Each project's own file is still fine: clangd finds `<dir>/build/` beside it.
    assert!(clangd.open_signals_readiness(&dir, "proj-a/main.c", &clangd.resolve_layout(&dir)));
    // A file belonging to no project is not a usable warm-up document here, because nothing
    // points the session at a database on its behalf.
    std::fs::write(dir.join("stray.c"), "int stray(void){return 0;}\n").unwrap();
    assert!(!clangd.open_signals_readiness(&dir, "stray.c", &clangd.resolve_layout(&dir)));
    assert!(
        clangd.checkout_can_signal_readiness(&dir, &clangd.resolve_layout(&dir)),
        "the per-project files remain warmable, so the backend is not blocked",
    );
}

#[test]
fn a_backend_with_no_checkout_scoped_marker_gets_only_its_static_argv() {
    // The dynamic argument is clangd-shaped; the other backends must not acquire a stray flag
    // their server would reject.
    let dir = rag_rat_base::test_scratch::ScratchDir::new("static-argv");
    std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    assert_eq!(ts.spawn_args(&["--stdio"], &ts.resolve_layout(&dir)), vec![OsString::from(
        "--stdio"
    )]);
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    assert!(rust.spawn_args(&[], &rust.resolve_layout(&dir)).is_empty());
    // And with no database anywhere, clangd gets no directory to point at either.
    let empty = rag_rat_base::test_scratch::ScratchDir::new("static-argv-empty");
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert_eq!(clangd.spawn_args(&["--background-index"], &clangd.resolve_layout(&empty)), vec![
        OsString::from("--background-index")
    ],);
}

#[test]
fn a_typescript_project_still_has_to_enclose_its_documents() {
    // The other scope, asserted alongside so the two cannot be conflated: a tsconfig sibling
    // of the sources governs nothing, because tsserver resolves a file's project by walking UP
    // from the file.
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let dir = rag_rat_base::test_scratch::ScratchDir::new("ts-sibling-config");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(dir.join("src/main.ts"), "export const x = 1;\n").unwrap();
    std::fs::write(dir.join("config/tsconfig.json"), "{}").unwrap();
    assert!(
        !ts.open_signals_readiness(&dir, "src/main.ts", &ts.resolve_layout(&dir)),
        "a config in a SIBLING directory governs nothing under src/",
    );
    assert_eq!(
        ts.warmup_document(&dir, &ts.resolve_layout(&dir)),
        None,
        "and there is nothing to warm on"
    );
}

#[test]
fn rust_claims_only_rust_paths() {
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    assert_eq!(rust.language_id_for("src/lib.rs"), "rust");
    assert!(rust.claims_path("src/lib.rs"));
    assert!(!rust.claims_path("src/main.ts"));
    assert!(!rust.claims_path("Cargo.toml"));
}
