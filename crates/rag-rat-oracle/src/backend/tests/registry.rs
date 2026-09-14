//! The live-backend registry declarations: which tools are live, their languages and ids,
//! project markers, pins, and static argv.

use super::*;

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
        let source = backend.moniker_source;
        assert!(source.batch_capable(), "a moniker source must be a batch tool");
        let batch_languages = crate::ToolManifest::for_tool(source).languages;
        for language in backend.languages {
            assert!(
                batch_languages.contains(language),
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

#[test]
fn every_declared_marker_name_is_usable() {
    // `MarkerKind::Parsed` guarantees a parsed marker has exactly ONE name — that part of the old
    // invariant is now in the type. What the type does not say is that a name is non-empty, and an
    // empty one is worse than useless: `dir.join("")` is the directory itself, so a nameless
    // sentinel would match every directory in the checkout, and an empty pin would hand the server
    // a bare `=<dir>`. Every reader fails closed on it, but nothing should ever declare one.
    for backend in LiveBackend::all() {
        let Some(model) = backend.project_model else {
            continue;
        };
        let names = model.files();
        assert!(!names.is_empty(), "{:?} declares a marker with no name", backend.tool);
        for name in names {
            assert!(!name.is_empty(), "{:?} declares an empty marker name", backend.tool);
        }
        if let crate::backend::registry::MarkerKind::Parsed { pin, .. } = model.kind {
            assert!(!pin.is_empty(), "{:?} declares an empty marker pin flag", backend.tool);
        }
    }
}

#[test]
fn only_a_backend_that_declares_a_pin_receives_one() {
    // The marker pin is a property of the MARKER, not of spawning: it is meaningless without the
    // file it points at. Writing `--compile-commands-dir` into the shared argv builder meant any
    // backend whose layout happened to find a sole marker directory would receive clangd's flag —
    // harmless only because clangd is the one backend that produces such a layout today (#1042).
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("pin-is-declared");
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("main.c"), "int m(void){return 0;}\n").unwrap();

    // A layout that DOES name a sole marker directory.
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(layout.sole_marker_dir().is_some(), "the fixture really pins a directory");

    assert!(
        clangd.spawn_args(&layout).contains(&compdb_arg(&dir)),
        "the backend that declares a pin gets it: {:?}",
        clangd.spawn_args(&layout),
    );
    assert_eq!(
        ts.spawn_args(&layout),
        vec![OsString::from("--stdio")],
        "a backend whose marker is a sentinel declares no pin, so it receives none even from a \
         layout that has one",
    );
}

#[test]
fn any_declared_marker_name_identifies_a_project() {
    // A build system often accepts several spellings of the same declaration — Gradle takes
    // `build.gradle.kts` or `build.gradle`. With one name per marker a checkout using the other
    // spelling reads as having no project at all: the readiness signal never fires, so the session
    // can only sit in `Warming` while the prerequisite gate reports the project missing (#1042).
    let (_dir_guard, dir) = checkout("marker-alternates");
    std::fs::create_dir_all(dir.join("app/src")).unwrap();
    std::fs::create_dir_all(dir.join("lib/src")).unwrap();
    // One module declares itself with the first name, the other with the second.
    std::fs::write(dir.join("app/build.gradle.kts"), "").unwrap();
    std::fs::write(dir.join("lib/build.gradle"), "").unwrap();
    let names = &["build.gradle.kts", "build.gradle"];

    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("app/src/Main.kt"), names),
        Some(dir.join("app")),
        "the first name identifies its project",
    );
    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("lib/src/Lib.kt"), names),
        Some(dir.join("lib")),
        "and so does any other declared name — this is what one name per marker could not do",
    );
    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("src/Stray.kt"), names),
        None,
        "a file under none of them still has no project",
    );
}

#[test]
fn the_prerequisite_hint_names_every_spelling_that_would_satisfy_the_marker() {
    // The third widened path. An operator reading the hint must not be sent to create the one
    // spelling the backend happened to list first when another would have done (#1042).
    use crate::manifest::hint_marker_names;

    assert_eq!(
        hint_marker_names(&["build.gradle.kts", "build.gradle"]),
        "build.gradle.kts or build.gradle",
        "every declared name appears",
    );
    assert_eq!(
        hint_marker_names(&["tsconfig.json"]),
        "tsconfig.json",
        "and a single-name marker reads exactly as it did before",
    );
    assert_eq!(
        hint_marker_names(&[]),
        "project",
        "a marker declaring no name falls back to the generic wording rather than naming nothing",
    );

    // And through the real hint, for a shipped single-name backend: the rendering above is the
    // one an operator actually reads.
    let (_dir_guard, dir) = checkout("hint-single-name");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.ts"), "export function greet() {}\n").unwrap();
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let hint = crate::ToolManifest::for_tool(OracleTool::TsLsp)
        .prerequisite_blocked_with(&crate::backend::CheckoutScope::resolve(&dir, &corpus), None)
        .expect("a checkout with no tsconfig blocks the TypeScript backend");
    assert!(
        hint.contains("found no tsconfig.json project"),
        "the shipped single-name hint is unchanged: {hint}",
    );
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
    let (_dir_guard, dir) = checkout("clangd-marker");
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert_eq!(
        clangd.project_model.and_then(|m| m.parsed_file()),
        Some("compile_commands.json"),
        "the compilation database is PARSED, so it declares exactly one name",
    );
    assert!(
        !clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        "no compdb ⇒ no signal possible"
    );

    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    assert!(
        !clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        "sources alone are not a project"
    );
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    assert_eq!(
        clangd.warmup_document(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        Some(dir.join("src/main.c"))
    );
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir)))
    );
    // The two live backends' project markers can coexist in one checkout, so a document
    // qualifies only if THIS backend could open it — not merely because a project contains it.
    assert!(clangd.open_signals_readiness(
        &scope(&dir),
        "src/main.c",
        &clangd.resolve_layout(&scope(&dir))
    ));
    assert!(
        !clangd.open_signals_readiness(
            &scope(&dir),
            "src/app.ts",
            &clangd.resolve_layout(&scope(&dir))
        ),
        "another language's file is not a clangd warm-up document, project or not",
    );
}

#[test]
fn a_backend_with_no_checkout_scoped_marker_gets_only_its_static_argv() {
    // The dynamic argument is clangd-shaped; the other backends must not acquire a stray flag
    // their server would reject.
    let (_dir_guard, dir) = checkout("static-argv");
    std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    assert_eq!(ts.spawn_args(&ts.resolve_layout(&scope(&dir))), vec![OsString::from("--stdio")]);
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    assert!(rust.spawn_args(&rust.resolve_layout(&scope(&dir))).is_empty());
    // And with no database anywhere, clangd gets no directory to point at either.
    let (_empty_guard, empty) = checkout("static-argv-empty");
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert_eq!(clangd.spawn_args(&clangd.resolve_layout(&scope(&empty))), vec![OsString::from(
        "--background-index"
    )],);
}

#[test]
fn rust_claims_only_rust_paths() {
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    assert_eq!(rust.language_id_for("src/lib.rs"), "rust");
    assert!(rust.claims_path("src/lib.rs"));
    assert!(!rust.claims_path("src/main.ts"));
    assert!(!rust.claims_path("Cargo.toml"));
}
