//! Path classification shared by the indexer and the query layer: which paths are tests, and
//! which are generated code. Pure string/path heuristics — the single notion both sides filter on.

use std::path::Path;

use crate::config;

pub fn is_generated_path(path: &str) -> bool {
    // Segment-based so a `generated` / `generated-web` directory is caught at ANY depth, INCLUDING
    // the repo root (`generated/bindings.rs`) — `contains("/generated/")` needed a leading
    // separator and silently missed root-level codegen dirs. Segment equality also avoids false
    // positives like `pre-generated-data/` that a bare substring match would catch.
    path.ends_with(".d.ts")
        || path.ends_with("_bg.wasm.d.ts")
        || path.split('/').any(|segment| segment == "generated" || segment == "generated-web")
}

/// Whether a file should be flagged `files.generated = 1` — the single notion of "generated" the
/// query layer filters on (search, orientation, tree, clusters, and symbol search all default to
/// `files.generated = 0`). A file is generated if its target is explicitly `kind = generated` OR
/// its path matches the codegen heuristic ([`is_generated_path`] — `/generated/`, `.d.ts`, ubrn
/// wasm-bindgen output). The path arm is what catches codegen that lives *under a source target*
/// (e.g. ubrn FFI bindings in `packages/.../src/generated/`): those still get full symbols (symbol
/// extraction is gated on `kind`, not this flag) so the graph keeps them, but they're filtered out
/// of default search/lookup results instead of burying the hand-written source (#202).
pub fn file_is_generated(kind: config::TargetKind, path: &str) -> bool {
    matches!(kind, config::TargetKind::Generated) || is_generated_path(path)
}

/// The CANONICAL cross-language test-path detector (#294) — the one reused by the indexer (the
/// `is_test` computation) AND the query layer (`staleness` test-file skip, `graph` test-callsite
/// filter, `repo_brief` support-path down-weight). A test path is any of: a test directory segment
/// (`tests`/`test`/`__tests__`/`__mocks__`/`spec`, case-insensitive), `conftest.py`, a `*.test.*` /
/// `*.spec.*` filename, or a stem like `test`/`tests` / `test_*` / `*_test` / `*_tests` / `*Test` /
/// `*Tests` / `*TestCase`. Takes `impl AsRef<Path>` so both `&Path` (parser) and `&str` (the query
/// callers' stored path strings) pass directly.
pub fn is_test_path(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    if path.components().filter_map(|component| component.as_os_str().to_str()).any(|segment| {
        matches!(
            segment.to_ascii_lowercase().as_str(),
            "tests" | "test" | "__tests__" | "__test__" | "__mocks__" | "spec" | "specs"
        )
    }) {
        return true;
    }
    let file = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if file == "conftest.py" {
        return true;
    }
    let lower = file.to_ascii_lowercase();
    if lower.contains(".test.") || lower.contains(".spec.") {
        return true;
    }
    let stem = file.split('.').next().unwrap_or(file);
    stem == "test"
        || stem == "tests"
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.ends_with("Test")
        || stem.ends_with("Tests")
        || stem.ends_with("TestCase")
}
