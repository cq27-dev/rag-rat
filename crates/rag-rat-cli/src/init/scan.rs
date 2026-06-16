use super::*;

/// Very rough chunk-count estimate from total indexable source bytes (~500 chars per chunk after
/// policy skips). Used only to *recommend* an embedding backend at init time.
pub(crate) fn estimated_chunks(total_source_bytes: u64) -> u64 {
    total_source_bytes / 500
}
/// Recommend an embedding backend by repo scale. The FastEmbed (MiniLM) cold backfill is CPU-bound
/// at ~10-100 chunks/sec, so it's only comfortable for repos that finish in a few minutes; larger
/// repos default to the static Model2Vec backend (orders of magnitude faster, some quality cost).
pub(crate) fn recommend_backend(estimated_chunks: u64) -> EmbeddingBackend {
    if estimated_chunks <= 5_000 {
        EmbeddingBackend::FastEmbed
    } else {
        EmbeddingBackend::Model2Vec
    }
}
pub(crate) fn backend_label(backend: EmbeddingBackend) -> &'static str {
    match backend {
        EmbeddingBackend::FastEmbed =>
            "minilm — MiniLM transformer; best quality, CPU backfill ~10-100 chunks/sec",
        EmbeddingBackend::Model2Vec =>
            "model2vec — static embeddings; ~100-500x faster on CPU, some quality cost",
        EmbeddingBackend::None => "none — BM25 + structure only, no dense vectors",
    }
}
pub(crate) fn scan_repo(root: &Path) -> anyhow::Result<RepoScan> {
    let mut scan = RepoScan::default();
    // The scan honors the SAME ignore rules as the index walk (gitignore + the unconditional floor)
    // so what it counts as candidate source matches what the index will actually contain (#181
    // review). Empty target dirs → the matcher governs the whole root.
    let ignore = IgnoreMatcher::compile(root, &[]);
    scan_dir(root, root, &ignore, &mut scan)?;
    Ok(scan)
}
pub(crate) fn scan_dir(
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
            *scan.language_counts.entry(language).or_default() += 1;
            add_file_to_dir_counts(root, &path, language, scan)?;
            scan.total_source_bytes += entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        }
    }
    Ok(())
}
pub(crate) fn add_file_to_dir_counts(
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
pub(crate) fn should_skip_dir(name: &str) -> bool {
    SKIPPED_DIRS.contains(&name)
}
pub(crate) fn candidate_dirs(scan: &RepoScan, language: Language) -> Vec<DirCandidate> {
    let Some(counts) = scan.dir_counts.get(&language) else {
        return Vec::new();
    };
    let mut candidates = counts
        .iter()
        .filter(|(path, _)| path_depth(path) <= 4)
        .map(|(path, count)| DirCandidate {
            path: path.clone(),
            count: *count,
            default: default_dir(scan, language, path),
        })
        .collect::<Vec<_>>();
    // When nothing is a natural default, promote the largest candidate — but NEVER a Python
    // dependency tree (`env/`, `site-packages`, …): in a repo whose only `.py` files live under a
    // virtualenv, promoting it would write a binding into installed deps. If every candidate is a
    // dependency dir, leave none defaulted (no binding beats the wrong binding).
    if !candidates.iter().any(|candidate| candidate.default)
        && let Some(best) = candidates
            .iter_mut()
            .filter(|candidate| !fallback_excluded(language, &candidate.path))
            // For Python, don't promote the aggregate `.` bucket when the root holds no real source
            // directly: in an env-only repo every `.py` lives under a dependency tree (those dirs
            // are `fallback_excluded`), so `.` would win on aggregate count alone and write
            // `python = ["."]` over installed deps (#173). A root with genuine entrypoints has a
            // non-zero direct count and is already a default above, so it never reaches here.
            .filter(|candidate| {
                language != Language::Python
                    || candidate.path != Path::new(".")
                    || python_root_has_direct_source(scan, &candidate.path)
            })
            .max_by_key(|candidate| candidate.count)
    {
        best.default = true;
    }
    candidates.sort_by(|a, b| {
        b.default
            .cmp(&a.default)
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.path.cmp(&b.path))
    });
    candidates.truncate(32);
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    candidates
}
pub(crate) fn default_dir(scan: &RepoScan, language: Language, path: &Path) -> bool {
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
        Language::Markdown => text == "docs" || text == ".",
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

pub(crate) fn directly_contains_source(scan: &RepoScan, language: Language, path: &Path) -> bool {
    path != Path::new(".")
        && scan
            .direct_dir_counts
            .get(&language)
            .and_then(|counts| counts.get(path))
            .copied()
            .unwrap_or_default()
            > 0
}
pub(crate) fn path_depth(path: &Path) -> usize {
    if path == Path::new(".") { 0 } else { path.components().count() }
}
pub(crate) fn print_language_summary(scan: &RepoScan) {
    for language in supported_languages() {
        let count = scan.language_counts.get(&language).copied().unwrap_or_default();
        if count > 0 {
            println!("  {}: {count} files", language.as_str());
        }
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
        let tmp = std::env::temp_dir().join(format!("rag-rat-venv-detect-{}", std::process::id()));
        fs::remove_dir_all(&tmp).ok();
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
        fs::remove_dir_all(&tmp).ok();
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
        assert!(
            default_paths.contains(&".".to_string()),
            "root entrypoints make `.` a default: {candidates:?}"
        );
        assert!(
            default_paths.contains(&"myapp".to_string()),
            "the package dir is still a default: {candidates:?}"
        );
    }
}
