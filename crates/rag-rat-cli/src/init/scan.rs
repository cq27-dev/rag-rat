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
    scan_dir(root, root, 0, &mut scan)?;
    Ok(scan)
}
pub(crate) fn scan_dir(
    root: &Path,
    dir: &Path,
    depth: usize,
    scan: &mut RepoScan,
) -> anyhow::Result<()> {
    if depth > 10 {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, io::Error>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if should_skip_dir(&entry.file_name().to_string_lossy()) {
                continue;
            }
            scan_dir(root, &path, depth + 1, scan)?;
        } else if file_type.is_file()
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
    *scan
        .direct_dir_counts
        .entry(language)
        .or_default()
        .entry(relative_parent.to_path_buf())
        .or_default() += 1;
    *scan.dir_counts.entry(language).or_default().entry(PathBuf::from(".")).or_default() += 1;
    let mut current = PathBuf::new();
    for component in relative_parent.components() {
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
                    || directly_contains_source(scan, language, path)),
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
}
