use std::path::{Path, PathBuf};
use std::{fs, io};

use rag_rat_base::config::EmbeddingBackend;
use rag_rat_base::language::Language;
use rag_rat_core::index::ignore_rules::{IgnoreMatcher, is_virtualenv_dir};

use crate::plan::{DirCandidate, RepoScan, SKIPPED_DIRS};
use crate::render::display_rel;

/// Very rough chunk-count estimate from total indexable source bytes (~500 chars per chunk after
/// policy skips). Used only to *recommend* an embedding backend at init time.
pub fn estimated_chunks(total_source_bytes: u64) -> u64 {
    total_source_bytes / 500
}
/// Recommend an embedding backend by repo scale. The FastEmbed (MiniLM) cold backfill is CPU-bound
/// at ~10-100 chunks/sec, so it's only comfortable for repos that finish in a few minutes; larger
/// repos default to the static Model2Vec backend (orders of magnitude faster, some quality cost).
pub fn recommend_backend(estimated_chunks: u64) -> EmbeddingBackend {
    if estimated_chunks <= 5_000 {
        EmbeddingBackend::fast_embed()
    } else {
        EmbeddingBackend::model2vec()
    }
}
pub fn backend_label(backend: EmbeddingBackend) -> &'static str {
    if backend == EmbeddingBackend::NONE {
        "none — BM25 + structure only, no dense vectors"
    } else if backend == EmbeddingBackend::model2vec() {
        "model2vec — static embeddings; ~100-500x faster on CPU, some quality cost"
    } else {
        "minilm — MiniLM transformer; best quality, CPU backfill ~10-100 chunks/sec"
    }
}
pub fn scan_repo(root: &Path) -> anyhow::Result<RepoScan> {
    let mut scan = RepoScan::default();
    // The scan honors the SAME ignore rules as the index walk (gitignore + the unconditional floor)
    // so what it counts as candidate source matches what the index will actually contain (#181
    // review). Empty target dirs → the matcher governs the whole root.
    let ignore = IgnoreMatcher::compile(root, &[]);
    scan_dir(root, root, &ignore, &mut scan)?;
    assign_headers(root, &mut scan)?;
    Ok(scan)
}

/// Resolve the ambiguous `.h` headers deferred during the walk. A `.h` is C or C++; bare extension
/// detection picks C, but a repo with ANY C++ source (`.cpp`/`.cc`/…) almost certainly has C++
/// headers, so bind them to C++ there and to C otherwise. This is what lets `init` generate a `cpp`
/// binding that actually covers the header tree (so the indexer parses those `.h` as C++). A
/// genuinely mixed C-and-C++ repo gets its headers under C++ (a clear default; C++ subsumes C-style
/// headers) — the user can split them with an explicit `[[target]]` if needed.
pub fn assign_headers(root: &Path, scan: &mut RepoScan) -> anyhow::Result<()> {
    let header_lang = if scan.language_counts.get(&Language::Cpp).copied().unwrap_or(0) > 0 {
        Language::Cpp
    } else {
        Language::C
    };
    for path in std::mem::take(&mut scan.deferred_headers) {
        *scan.language_counts.entry(header_lang).or_default() += 1;
        add_file_to_dir_counts(root, &path, header_lang, scan)?;
    }
    Ok(())
}
pub fn scan_dir(
    root: &Path,
    dir: &Path,
    ignore: &IgnoreMatcher,
    scan: &mut RepoScan,
) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_file() && entry.file_name() == "go.mod" {
            // Record the manifest root by filename PRESENCE only — never parse `go.mod` content
            // (BIND-01). A malformed or empty `go.mod` still marks its parent dir as a Go
            // manifest root; content validity is irrelevant here.
            let parent = path.parent().unwrap_or(dir);
            let relative_parent = parent.strip_prefix(root).unwrap_or(parent);
            let relative_parent = if relative_parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                relative_parent
            };
            scan.manifest_roots
                .entry(Language::Go)
                .or_default()
                .insert(relative_parent.to_path_buf());
        }
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // A real Python VIRTUALENV (detected by content — a `pyvenv.cfg`, NOT by an ambiguous
            // name) is never project source: skip it and DON'T count its files, so a nested
            // `tools/env/` can't inflate `tools` into a candidate (#181 review). It also records
            // that a venv exists the index WOULD walk, so
            // `python_root_has_direct_source` refuses the `.` default — `python =
            // ["."]` would ingest the venv (the floor can't cover `env`/ `virtualenv`
            // names). Content detection keeps a same-named FIRST-PARTY package (the
            // `virtualenv` PyPI package's `src/virtualenv/`, which has no `pyvenv.cfg`) a normal
            // candidate. A gitignored venv is already `is_ignored` below (skipped + unindexed), so
            // it doesn't reach here.
            if !ignore.is_ignored(&path, true) && is_virtualenv_dir(&path) {
                scan.has_python_virtualenv = true;
                continue;
            }
            // Skip what the index won't walk: its hardcoded scan-skip names OR anything the shared
            // ignore matcher (gitignore + floor) excludes — so scan counts == index contents.
            if should_skip_dir(&name) || ignore.is_ignored(&path, true) {
                continue;
            }
            scan_dir(root, &path, ignore, scan)?;
        } else if file_type.is_file()
            && !ignore.is_ignored(&path, false)
            && let Some(language) = Language::from_path(&path)
        {
            scan.total_source_bytes += entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            // Defer the ambiguous bare `.h` header (bare detection calls it C): its language is
            // decided in `assign_headers` once we know whether the repo is C++. All other files
            // count immediately under their detected language.
            if language == Language::C && path.extension().is_some_and(|ext| ext == "h") {
                scan.deferred_headers.push(path);
            } else {
                *scan.language_counts.entry(language).or_default() += 1;
                add_file_to_dir_counts(root, &path, language, scan)?;
            }
        }
    }
    Ok(())
}
pub fn add_file_to_dir_counts(
    root: &Path,
    path: &Path,
    language: Language,
    scan: &mut RepoScan,
) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or(root);
    let relative_parent = parent.strip_prefix(root).unwrap_or(parent);
    // A root-level file (parent == root) strips to an empty path; key it under "." so `.` is
    // recognized as DIRECTLY containing source (root entrypoints like manage.py), not merely as the
    // aggregate bucket every file increments below (#173).
    let relative_parent =
        if relative_parent.as_os_str().is_empty() { Path::new(".") } else { relative_parent };
    *scan
        .direct_dir_counts
        .entry(language)
        .or_default()
        .entry(relative_parent.to_path_buf())
        .or_default() += 1;
    *scan.dir_counts.entry(language).or_default().entry(PathBuf::from(".")).or_default() += 1;
    let mut current = PathBuf::new();
    for component in relative_parent.components() {
        // The "." aggregate is counted once above; skip the CurDir component so a root file doesn't
        // double-count it.
        if component.as_os_str() == "." {
            continue;
        }
        current.push(component.as_os_str());
        *scan.dir_counts.entry(language).or_default().entry(current.clone()).or_default() += 1;
    }
    Ok(())
}
pub fn should_skip_dir(name: &str) -> bool {
    SKIPPED_DIRS.contains(&name)
}
pub fn candidate_dirs(scan: &RepoScan, language: Language) -> Vec<DirCandidate> {
    let Some(counts) = scan.dir_counts.get(&language) else {
        return Vec::new();
    };
    // Single source of truth for what's actually a default (BIND-02): membership here comes
    // from `default_dirs`, not a re-run of the heuristic — keeps this capped UI list and the
    // uncapped binding set from ever drifting apart.
    let resolved_defaults = default_dirs(scan, language);
    // `dir_counts` is populated only by `scan_dir`, whose walk already applies the shared
    // IgnoreMatcher and skips the hard floor. Do not rebuild that matcher here on every render; the
    // candidate set is derived from the filtered scan.
    let mut candidates = counts
        .iter()
        .filter(|(path, _)| path_depth(path) <= 4)
        .map(|(path, count)| DirCandidate {
            path: path.clone(),
            count: *count,
            default: resolved_defaults.contains(path),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        b.default
            .cmp(&a.default)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.path.cmp(&b.path))
    });
    if candidates.len() > 32 {
        tracing::debug!(
            "{} additional {language:?} directory candidates beyond the top 32 shown in the UI",
            candidates.len() - 32
        );
    }
    candidates.truncate(32);
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    candidates
}

/// Shared helper both plan-generation call sites (`run.rs::default_plan`,
/// `draft.rs::SetupDraft::from_scan`) use so they can no longer drift apart. Wraps the
/// uncapped `default_dirs`, applying the same "no safe default" edge case both call sites used
/// to implement separately: for Python with candidates present but none flagged default (an
/// env-only repo — every `.py` lives under a dependency tree, see `fallback_excluded`),
/// return empty so the caller omits the language rather than falling back to `["."]`, which
/// would index installed deps (#173/#181). Every other language falls back to `["."]`.
pub fn resolved_bindings(scan: &RepoScan, language: Language) -> Vec<PathBuf> {
    let defaults = default_dirs(scan, language);
    if !defaults.is_empty() {
        return defaults;
    }
    let has_candidates = scan.dir_counts.get(&language).is_some_and(|counts| !counts.is_empty());
    if language == Language::Python && has_candidates {
        return Vec::new();
    }
    vec![PathBuf::from(".")]
}
pub fn default_dir(scan: &RepoScan, language: Language, path: &Path) -> bool {
    let text = display_rel(path);
    match language {
        Language::Rust => text == "src" || text.ends_with("/src"),
        Language::TypeScript => text == "src" || text.ends_with("/src") || text.ends_with("/app"),
        Language::Kotlin =>
            text == "src"
                || text.ends_with("/src")
                || text.ends_with("/src/main/java")
                || text.ends_with("/src/main/kotlin"),
        Language::C | Language::Cpp =>
            text == "src"
                || text.ends_with("/src")
                || text == "include"
                || text.ends_with("/include")
                || directly_contains_source(scan, language, path),
        // Python packages typically sit at the repo root, under `src/`, or as a dir named after the
        // package that directly contains `.py` files — but NEVER a virtualenv / dependency tree
        // (`.venv/…/site-packages`), which would pollute the index with the whole dependency set.
        Language::Python =>
            !is_python_dependency_dir(&text)
                && (text == "src"
                    || text.ends_with("/src")
                    || directly_contains_source(scan, language, path)
                    || python_root_has_direct_source(scan, path)),
        Language::Swift =>
            !text.split('/').any(|component| component == ".build")
                && (text == "Sources"
                    || text.ends_with("/Sources")
                    || text == "src"
                    || text.ends_with("/src")),
        Language::Markdown => text == "docs" || text == ".",
        Language::Go =>
            text == "src"
                || text.ends_with("/src")
                || directly_contains_source(scan, language, path),
    }
}

/// `true` for a path under a Python virtualenv / dependency tree — these hold installed third-party
/// `.py` files (`site-packages`) that must never be indexed as project source.
fn is_python_dependency_dir(text: &str) -> bool {
    text.split('/').any(|component| {
        matches!(
            component,
            ".venv"
                | "venv"
                | "env"
                | ".env"
                | "virtualenv"
                | "site-packages"
                | "__pycache__"
                | ".tox"
                | ".nox"
                | "node_modules"
        )
    })
}

/// Whether the no-default fallback must NOT promote this candidate: a Python dependency/virtualenv
/// tree (`env`/`.env`/`venv`/`site-packages`/…). `is_python_dependency_dir` covers the names
/// `SKIPPED_DIRS` doesn't skip during the walk (e.g. `env`, which is too generic to skip globally).
fn fallback_excluded(language: Language, path: &Path) -> bool {
    language == Language::Python && is_python_dependency_dir(&display_rel(path))
}

/// The repo root (`.`) directly contains non-dependency Python source — e.g. `manage.py` /
/// `setup.py` / `main.py` at the top level (#173). `directly_contains_source` deliberately excludes
/// `.` (it's the aggregate bucket every file increments), so root entrypoints would otherwise never
/// make `.` a default — omitted entirely when a package dir is also present. Root-level files key
/// under `.` in `direct_dir_counts` (see `add_file_to_dir_counts`), so a positive direct count
/// means real source sits at the root (env-only repos have their `.py` under a dependency tree,
/// never at the root, so their `.` direct count is 0).
fn python_root_has_direct_source(scan: &RepoScan, path: &Path) -> bool {
    path == Path::new(".")
        // A real virtualenv (a `pyvenv.cfg` dir) the index would walk, found anywhere by the
        // gitignore-honoring scan, makes `python = ["."]` unsafe — it would ingest the venv. Omit the
        // `.` default; the user can bind `.` explicitly. A gitignored / floored venv doesn't set this
        // flag, so a `manage.py`-plus-gitignored-venv repo still binds `.` (#181 review).
        && !scan.has_python_virtualenv
        && scan
            .direct_dir_counts
            .get(&Language::Python)
            .and_then(|counts| counts.get(Path::new(".")))
            .copied()
            .unwrap_or_default()
            > 0
}

pub fn directly_contains_source(scan: &RepoScan, language: Language, path: &Path) -> bool {
    path != Path::new(".")
        && scan
            .direct_dir_counts
            .get(&language)
            .and_then(|counts| counts.get(path))
            .copied()
            .unwrap_or_default()
            > 0
}
pub fn path_depth(path: &Path) -> usize {
    if path == Path::new(".") { 0 } else { path.components().count() }
}

/// Uncapped, unbounded-depth derivation of the default binding dirs for a language (BIND-04,
/// BIND-08). Same natural-default + fallback-promotion logic `candidate_dirs` uses, minus its
/// UI-only `path_depth <= 4` filter and `truncate(32)`: this is the raw set init logic reasons
/// over, not the trimmed set the picker UI renders. Manifest-root promotion is a later task —
/// deliberately not implemented here.
pub fn default_dirs(scan: &RepoScan, language: Language) -> Vec<PathBuf> {
    let Some(counts) = scan.dir_counts.get(&language) else {
        return Vec::new();
    };
    // Step 1: every dir the per-language `default_dir` heuristic flags as a natural default —
    // uncapped depth, no top-32 truncation.
    let mut defaults: Vec<PathBuf> =
        counts.keys().filter(|path| default_dir(scan, language, path)).cloned().collect();
    // Step 2: promote each recorded manifest root (currently Go's `go.mod` dirs) as an
    // additional default — module roots absorb their leaf-package defaults via the
    // `dedup_ancestors` pass below, so a Go repo with a root `go.mod` and no root-level
    // `.go` file still collapses to a single recursive `.` binding (BIND-01).
    if let Some(manifest_roots) = scan.manifest_roots.get(&language) {
        defaults.extend(manifest_roots.iter().cloned());
    }
    // Step 3: nothing natural — fall back to the single highest-count candidate, same
    // Python-exclusion-aware promotion rule `candidate_dirs` applies.
    if defaults.is_empty()
        && let Some((best_path, _)) = counts
            .iter()
            .filter(|(path, _)| !fallback_excluded(language, path))
            .filter(|(path, _)| {
                language != Language::Python
                    || **path != Path::new(".")
                    || python_root_has_direct_source(scan, path)
            })
            .max_by_key(|(_, count)| **count)
    {
        defaults.push(best_path.clone());
    }
    // Step 4: collapse descendants into their shallowest kept ancestor.
    dedup_ancestors(defaults)
}

/// Drop any path that is a descendant of another path already in the set — e.g. `.` and
/// `src/foo` collapse to just `.`. Generalizes the ad hoc "Python `.` wins alone" special case
/// (see `python_root_has_direct_source` / the Python arm of `default_dir`) into a
/// language-agnostic pass usable for any binding set. Shallowest paths win: sort by depth
/// ascending first, then keep a path only if no already-kept path is an ancestor of it (via
/// `starts_with`). Output is sorted for deterministic, order-independent results.
pub fn dedup_ancestors(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort_by_key(|path| path_depth(path));
    let mut kept: Vec<PathBuf> = Vec::new();
    for path in paths {
        // `Path::starts_with` does NOT treat "." as an ancestor of a relative path (it compares
        // components literally, and "." has none once normalized) — so "." needs an explicit
        // check to act as the universal root every other relative path descends from.
        let has_kept_ancestor =
            kept.iter().any(|ancestor| ancestor == Path::new(".") || path.starts_with(ancestor));
        if !has_kept_ancestor {
            kept.push(path);
        }
    }
    kept.sort();
    kept
}

#[cfg(test)]
mod header_assignment_tests {
    use super::*;
    use crate::plan::default_plan;

    fn temp_root(tag: &str) -> rag_rat_base::test_scratch::ScratchDir {
        rag_rat_base::test_scratch::ScratchDir::new(&format!("hdr-{tag}"))
    }

    #[test]
    fn cpp_project_binds_h_headers_as_cpp() {
        // include/*.h + src/*.cpp: the headers must count as C++ (not C) so init can bind a `cpp`
        // target that covers the header tree.
        let root = temp_root("cpp");
        fs::create_dir_all(root.join("include/lib")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("include/lib/api.h"), "class Api { void run(); };\n").unwrap();
        fs::write(root.join("src/api.cpp"), "#include \"lib/api.h\"\nvoid Api::run() {}\n")
            .unwrap();

        let scan = scan_repo(&root).unwrap();
        assert_eq!(scan.language_counts.get(&Language::C).copied().unwrap_or(0), 0, "no C files");
        assert_eq!(
            scan.language_counts.get(&Language::Cpp).copied().unwrap_or(0),
            2,
            "header + src"
        );
        assert!(scan.dir_counts.get(&Language::Cpp).unwrap().contains_key(Path::new("include")));

        let plan = default_plan(".".to_string(), &scan);
        let cpp = &plan.bindings[&Language::Cpp];
        assert!(cpp.contains(&PathBuf::from("include")), "cpp must bind the header dir: {cpp:?}");
        assert!(!plan.bindings.contains_key(&Language::C), "no C binding for a C++-only repo");
    }

    #[test]
    fn pure_c_project_keeps_h_headers_as_c() {
        // .c + .h with NO C++ source: headers stay C.
        let root = temp_root("c");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.h"), "int f(void);\n").unwrap();
        fs::write(root.join("src/lib.c"), "int f(void){return 0;}\n").unwrap();

        let scan = scan_repo(&root).unwrap();
        assert_eq!(scan.language_counts.get(&Language::C).copied().unwrap_or(0), 2, ".c + .h");
        assert_eq!(scan.language_counts.get(&Language::Cpp).copied().unwrap_or(0), 0, "no C++");
    }
}

#[cfg(test)]
mod swift_dir_tests {
    use super::*;

    #[test]
    fn swiftpm_sources_is_the_default_binding() {
        let scan = RepoScan::default();
        assert!(default_dir(&scan, Language::Swift, Path::new("Sources")));
        assert!(default_dir(&scan, Language::Swift, Path::new("Packages/Feature/Sources")));
        assert!(!default_dir(
            &scan,
            Language::Swift,
            Path::new(".build/checkouts/Dependency/Sources")
        ));
        assert!(!default_dir(&scan, Language::Swift, Path::new("Tests")));
    }

    #[test]
    fn swiftpm_build_checkouts_are_not_scanned_as_project_source() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("Sources/App")).unwrap();
        fs::write(root.path().join("Sources/App/Main.swift"), "struct App {}\n").unwrap();
        fs::create_dir_all(root.path().join(".build/checkouts/Dep/Sources/Dep")).unwrap();
        fs::write(
            root.path().join(".build/checkouts/Dep/Sources/Dep/Dep.swift"),
            "struct Dependency {}\n",
        )
        .unwrap();

        let scan = scan_repo(root.path()).unwrap();
        assert_eq!(scan.language_counts.get(&Language::Swift), Some(&1));
        assert!(
            candidate_dirs(&scan, Language::Swift)
                .iter()
                .all(|candidate| !candidate.path.starts_with(".build")),
            "SwiftPM build checkouts must not become init candidates"
        );
    }

    #[test]
    fn swift_root_fallback_remains_safe_for_the_indexer() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("Main.swift"), "struct App {}\n").unwrap();
        fs::create_dir_all(root.path().join(".build/checkouts/Dep/Sources/Dep")).unwrap();
        fs::write(
            root.path().join(".build/checkouts/Dep/Sources/Dep/Dep.swift"),
            "struct Dependency {}\n",
        )
        .unwrap();

        let scan = scan_repo(root.path()).unwrap();
        let candidates = candidate_dirs(&scan, Language::Swift);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.path == Path::new(".") && candidate.default),
            "a non-SwiftPM layout should exercise the generated root fallback: {candidates:#?}"
        );

        let matcher = IgnoreMatcher::compile(root.path(), &[PathBuf::from(".")]);
        assert!(matcher.is_ignored(
            &root.path().join(".build/checkouts/Dep/Sources/Dep/Dep.swift"),
            false,
        ));
        assert!(!matcher.is_ignored(&root.path().join("Main.swift"), false));
    }
}

#[cfg(test)]
mod python_dir_tests {
    use super::*;

    #[test]
    fn python_dependency_dir_detection() {
        assert!(is_python_dependency_dir(".venv"));
        assert!(is_python_dependency_dir("env"));
        assert!(is_python_dependency_dir("project/.venv/lib/site-packages"));
        assert!(!is_python_dependency_dir("src"));
        assert!(!is_python_dependency_dir("app"));
    }

    #[test]
    fn virtualenv_detected_by_content_not_name() {
        let tmp = rag_rat_base::test_scratch::ScratchDir::new("venv-detect");
        // A real venv (any name) has a `pyvenv.cfg` → detected.
        fs::create_dir_all(tmp.join("env")).unwrap();
        fs::write(tmp.join("env/pyvenv.cfg"), "home = /usr\n").unwrap();
        assert!(is_virtualenv_dir(&tmp.join("env")), "a pyvenv.cfg dir is a venv");
        // A first-party package that merely shares a venv-ish NAME (no pyvenv.cfg) is NOT a venv.
        fs::create_dir_all(tmp.join("src/virtualenv")).unwrap();
        fs::write(tmp.join("src/virtualenv/__init__.py"), "").unwrap();
        assert!(
            !is_virtualenv_dir(&tmp.join("src/virtualenv")),
            "the virtualenv package dir has no pyvenv.cfg"
        );
    }

    #[test]
    fn fallback_does_not_promote_a_venv_only_python_repo() {
        // A repo whose only discovered .py files live under an `env/` virtualenv: the no-default
        // fallback must NOT promote it (else `init -y` writes `python = ["env"]`).
        let mut scan = RepoScan::default();
        let dir = PathBuf::from("env");
        scan.dir_counts.entry(Language::Python).or_default().insert(dir.clone(), 9);
        scan.direct_dir_counts.entry(Language::Python).or_default().insert(dir, 9);

        let candidates = candidate_dirs(&scan, Language::Python);
        assert!(
            candidates.iter().all(|candidate| !candidate.default),
            "a virtualenv dir must never be selected as the default Python target: {candidates:?}"
        );
    }

    /// #173 case 2: a realistic env-only repo — `add_file_to_dir_counts` always increments the `.`
    /// aggregate, so `.` carries the full count. The fallback must still NOT promote `.` (its only
    /// `.py` live under the dependency tree), so `init -y` writes no Python binding rather than
    /// `python = ["."]` over installed deps.
    #[test]
    fn fallback_does_not_promote_dot_when_python_lives_only_under_a_dependency_tree() {
        let root = Path::new("/repo");
        let mut scan = RepoScan::default();
        // Two `.py` files under `env/lib/site-packages/pkg` — nothing at the root.
        for name in ["a.py", "b.py"] {
            add_file_to_dir_counts(
                root,
                &root.join("env/lib/site-packages/pkg").join(name),
                Language::Python,
                &mut scan,
            )
            .unwrap();
        }
        let candidates = candidate_dirs(&scan, Language::Python);
        assert!(
            candidates.iter().all(|candidate| !candidate.default),
            "no binding for an env-only repo (not even `.`): {candidates:?}"
        );
    }

    /// #173 case 1: root entrypoints (`manage.py`) alongside a package dir. Both `.` (root source)
    /// and the package dir must be defaults, so `init -y` indexes the root entrypoints too — not
    /// only the package.
    #[test]
    fn root_entrypoints_default_alongside_a_package_dir() {
        let root = Path::new("/repo");
        let mut scan = RepoScan::default();
        // A root entrypoint + package-dir sources.
        add_file_to_dir_counts(root, &root.join("manage.py"), Language::Python, &mut scan).unwrap();
        for name in ["__init__.py", "views.py"] {
            add_file_to_dir_counts(
                root,
                &root.join("myapp").join(name),
                Language::Python,
                &mut scan,
            )
            .unwrap();
        }
        let candidates = candidate_dirs(&scan, Language::Python);
        let default_paths: Vec<String> =
            candidates.iter().filter(|c| c.default).map(|c| display_rel(&c.path)).collect();
        // `.` recursively covers `myapp`, so `dedup_ancestors` (BIND-07) collapses the
        // redundant descendant binding — `.` alone is the correct default set.
        assert_eq!(
            default_paths,
            vec![".".to_string()],
            "root ancestor `.` absorbs the package dir default: {candidates:?}"
        );
    }
}

#[cfg(test)]
mod go_manifest_root_tests {
    use super::*;

    #[test]
    fn go_mod_presence_records_its_parent_dir() {
        let root = rag_rat_base::test_scratch::ScratchDir::new("go-manifest-present");
        fs::create_dir_all(root.join("svc")).unwrap();
        fs::write(root.join("svc/go.mod"), "module example.com/svc\n\ngo 1.22\n").unwrap();
        fs::write(root.join("svc/main.go"), "package main\n").unwrap();

        let scan = scan_repo(&root).unwrap();

        assert!(
            scan.manifest_roots
                .get(&Language::Go)
                .is_some_and(|roots| roots.contains(&PathBuf::from("svc"))),
            "svc/go.mod must record svc as a Go manifest root: {:?}",
            scan.manifest_roots.get(&Language::Go)
        );
    }

    #[test]
    fn no_go_mod_anywhere_yields_an_empty_go_manifest_set() {
        let root = rag_rat_base::test_scratch::ScratchDir::new("go-manifest-absent");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();

        let scan = scan_repo(&root).unwrap();

        assert!(
            scan.manifest_roots.get(&Language::Go).is_none_or(|roots| roots.is_empty()),
            "no go.mod anywhere must leave the Go manifest set empty: {:?}",
            scan.manifest_roots.get(&Language::Go)
        );
    }

    #[test]
    fn malformed_or_empty_go_mod_still_records_its_parent_dir() {
        // Filename-presence only, never content-parsing (BIND-01): an empty file still counts.
        let root = rag_rat_base::test_scratch::ScratchDir::new("go-manifest-malformed");
        fs::create_dir_all(root.join("broken")).unwrap();
        fs::write(root.join("broken/go.mod"), "").unwrap();

        let scan = scan_repo(&root).unwrap();

        assert!(
            scan.manifest_roots
                .get(&Language::Go)
                .is_some_and(|roots| roots.contains(&PathBuf::from("broken"))),
            "an empty go.mod must still record its parent dir: {:?}",
            scan.manifest_roots.get(&Language::Go)
        );
    }
}

#[cfg(test)]
mod dedup_ancestors_tests {
    use super::*;

    #[test]
    fn a_descendant_collapses_into_its_ancestor() {
        let paths = vec![PathBuf::from("."), PathBuf::from("src/foo")];
        assert_eq!(dedup_ancestors(paths), vec![PathBuf::from(".")]);
    }

    #[test]
    fn unrelated_paths_are_kept_unchanged() {
        let paths = vec![PathBuf::from("pkg1"), PathBuf::from("pkg2"), PathBuf::from("pkg3")];
        assert_eq!(dedup_ancestors(paths.clone()), vec![
            PathBuf::from("pkg1"),
            PathBuf::from("pkg2"),
            PathBuf::from("pkg3")
        ]);
    }

    #[test]
    fn output_is_deterministic_across_repeated_runs() {
        let paths = vec![PathBuf::from("b/child"), PathBuf::from("a"), PathBuf::from("b")];
        let first = dedup_ancestors(paths.clone());
        let second = dedup_ancestors(paths);
        assert_eq!(first, second);
        assert_eq!(first, vec![PathBuf::from("a"), PathBuf::from("b")]);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let paths: Vec<PathBuf> = Vec::new();
        assert_eq!(dedup_ancestors(paths), Vec::<PathBuf>::new());
    }

    #[test]
    fn nested_descendants_collapse_into_the_shallowest_shared_ancestor() {
        let paths =
            vec![PathBuf::from("pkg"), PathBuf::from("pkg/sub"), PathBuf::from("pkg/sub/deep")];
        assert_eq!(dedup_ancestors(paths), vec![PathBuf::from("pkg")]);
    }
}

#[cfg(test)]
mod default_dirs_tests {
    use super::*;

    #[test]
    fn natural_default_dir_is_returned() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let scan = scan_repo(root.path()).unwrap();
        assert_eq!(default_dirs(&scan, Language::Rust), vec![PathBuf::from("src")]);
    }

    #[test]
    fn no_natural_default_falls_back_to_highest_count_candidate() {
        // No "src"/"app"-shaped dir for TypeScript, so the fallback-promotion rule picks the
        // highest count dir instead of leaving the language unbound. `.` is the aggregate bucket
        // every file increments (see `add_file_to_dir_counts`), so it wins the count race and, for
        // a non-Python language, is a legal fallback candidate (no Python-exclusion applies).
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("lib")).unwrap();
        fs::write(root.path().join("lib/a.ts"), "export const a = 1;\n").unwrap();
        fs::write(root.path().join("lib/b.ts"), "export const b = 2;\n").unwrap();
        fs::create_dir_all(root.path().join("other")).unwrap();
        fs::write(root.path().join("other/c.ts"), "export const c = 3;\n").unwrap();

        let scan = scan_repo(root.path()).unwrap();
        assert_eq!(default_dirs(&scan, Language::TypeScript), vec![PathBuf::from(".")]);
    }

    #[test]
    fn python_fallback_never_promotes_a_dependency_tree() {
        // Every .py file lives under a virtualenv-shaped dependency dir: the Python-exclusion-aware
        // fallback must promote nothing rather than write a binding into installed deps.
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".venv/lib/site-packages/pkg")).unwrap();
        fs::write(root.path().join(".venv/lib/site-packages/pkg/mod.py"), "def f():\n    pass\n")
            .unwrap();

        let scan = scan_repo(root.path()).unwrap();
        assert_eq!(default_dirs(&scan, Language::Python), Vec::<PathBuf>::new());
    }

    #[test]
    fn dedup_ancestors_is_applied_to_the_result() {
        // A default `.` (via python_root_has_direct_source) alongside a nested Python package dir
        // must collapse to just `.` — dedup_ancestors is exercised, not skipped.
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("manage.py"), "# entrypoint\n").unwrap();
        fs::create_dir_all(root.path().join("app")).unwrap();
        fs::write(root.path().join("app/main.py"), "def main():\n    pass\n").unwrap();

        let scan = scan_repo(root.path()).unwrap();
        assert_eq!(default_dirs(&scan, Language::Python), vec![PathBuf::from(".")]);
    }

    #[test]
    fn uncapped_depth_is_not_filtered_like_candidate_dirs() {
        // A default dir deeper than the candidate_dirs UI-only path_depth <= 4 filter must still
        // surface from default_dirs — it is the uncapped, unbounded-depth derivation.
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("a/b/c/d/src");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("main.rs"), "fn main() {}\n").unwrap();

        let scan = scan_repo(root.path()).unwrap();
        let deep_rel = PathBuf::from("a/b/c/d/src");
        assert!(
            path_depth(&deep_rel) > 4,
            "fixture must exceed the candidate_dirs depth filter to exercise uncapped depth"
        );
        assert_eq!(default_dirs(&scan, Language::Rust), vec![deep_rel]);
    }

    #[test]
    fn more_than_32_defaults_are_not_truncated() {
        // candidate_dirs truncates to the top 32 by count; default_dirs must not apply that
        // UI-only cap — all 40 top-level Rust `src` dirs must come back.
        let root = tempfile::tempdir().unwrap();
        let mut expected = Vec::new();
        for i in 0..40 {
            let dir = root.path().join(format!("pkg{i}/src"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("lib.rs"), "pub fn f() {}\n").unwrap();
            expected.push(PathBuf::from(format!("pkg{i}/src")));
        }
        expected.sort();

        let scan = scan_repo(root.path()).unwrap();
        assert_eq!(default_dirs(&scan, Language::Rust), expected);
    }

    #[test]
    fn go_module_root_absorbs_more_than_32_leaf_packages() {
        // Root go.mod, no root-level .go file, >32 package dirs: module root must absorb
        // every leaf package via manifest-root promotion + dedup_ancestors (BIND-01).
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("go.mod"), "module example.com/big\n\ngo 1.22\n").unwrap();
        for i in 0..40 {
            let dir = root.path().join(format!("pkg{i}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("file.go"), "package pkg\n").unwrap();
        }

        let scan = scan_repo(root.path()).unwrap();
        assert_eq!(default_dirs(&scan, Language::Go), vec![PathBuf::from(".")]);
    }

    #[test]
    fn multiple_independent_go_modules_are_each_promoted() {
        let root = tempfile::tempdir().unwrap();
        for name in ["svc-a", "svc-b"] {
            let dir = root.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("go.mod"), format!("module example.com/{name}\n\ngo 1.22\n"))
                .unwrap();
            fs::write(dir.join("main.go"), "package main\n").unwrap();
        }

        let scan = scan_repo(root.path()).unwrap();
        assert_eq!(default_dirs(&scan, Language::Go), vec![
            PathBuf::from("svc-a"),
            PathBuf::from("svc-b")
        ]);
    }

    #[test]
    fn go_default_dirs_without_manifest_root_is_unchanged() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(root.path().join("src/main.go"), "package main\n").unwrap();

        let scan = scan_repo(root.path()).unwrap();
        assert_eq!(default_dirs(&scan, Language::Go), vec![PathBuf::from("src")]);
    }
}
