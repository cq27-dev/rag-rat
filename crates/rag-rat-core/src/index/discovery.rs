//! Discovery: compare configured targets against the index to find
//! indexed/unindexed/changed/removed files.

use super::*;

#[derive(Debug, Serialize)]
pub struct DiscoveryStatus {
    pub discovered_files: usize,
    pub indexed_files: usize,
    pub unindexed_files: usize,
    pub unindexed_source_files: usize,
    pub changed_indexed_files: usize,
    pub removed_indexed_files: usize,
    pub unindexed_sample: Vec<String>,
    pub warning: Option<String>,
}

#[derive(Debug)]
pub(crate) struct DiscoveryPlan {
    pub(crate) files: Vec<IndexFile>,
    pub(crate) deleted: BTreeSet<PathBuf>,
    pub(crate) unindexed: Vec<IndexFile>,
    pub(crate) changed: Vec<PathBuf>,
    pub(crate) discovered_files: usize,
    pub(crate) indexed_files: usize,
}

pub(crate) fn discovery_plan(
    conn: &rusqlite::Connection,
    config: &Config,
) -> anyhow::Result<DiscoveryPlan> {
    let discovered = collect_index_files(config)?;
    let mut indexed = indexed_file_map(conn)?;
    let mut current_paths = BTreeSet::new();
    let mut files = Vec::new();
    let mut unindexed = Vec::new();
    let mut changed = Vec::new();
    let discovered_files = discovered.len();
    let hashed = discovered
        .par_iter()
        .map(|file| -> anyhow::Result<(IndexFile, String)> {
            let text = fs::read(&file.full_path)?;
            Ok((file.clone(), hex_sha256(&text)))
        })
        .collect::<Vec<_>>();

    for hashed_file in hashed {
        let (file, current_hash) = hashed_file?;
        let relative = path_string(&file.relative_path);
        current_paths.insert(file.relative_path.clone());
        let Some(indexed) = indexed.remove(&relative) else {
            unindexed.push(file.clone());
            files.push(file);
            continue;
        };
        // Reindex on content drift OR target drift: the discovered (language, kind) can change with
        // no content change — e.g. after an upgrade or a binding edit moves a `.h` from a `c` to a
        // `cpp` target. The stored row would otherwise keep its old parse forever (sha unchanged),
        // so the `.h`→C++ upgrade would never take effect on an existing index without `--full`.
        let target_drift =
            indexed.language != file.language.as_str() || indexed.kind != file.kind.as_str();
        if current_hash != indexed.sha256 || target_drift {
            changed.push(file.relative_path.clone());
            files.push(file);
        }
    }

    let deleted = indexed
        .into_keys()
        .map(PathBuf::from)
        .filter(|path| !current_paths.contains(path))
        .collect::<BTreeSet<_>>();

    Ok(DiscoveryPlan {
        discovered_files,
        indexed_files: current_paths
            .len()
            .saturating_add(deleted.len())
            .saturating_sub(unindexed.len()),
        files,
        deleted,
        unindexed,
        changed,
    })
}

/// One indexed `files` row's identity for discovery: its content hash plus the (language, kind) it
/// was indexed under, so discovery can detect TARGET drift (a binding/precedence change that
/// re-languages a path) as well as content drift.
pub(crate) struct IndexedFileRow {
    pub(crate) sha256: String,
    pub(crate) language: String,
    pub(crate) kind: String,
}

pub(crate) fn indexed_file_map(
    conn: &rusqlite::Connection,
) -> anyhow::Result<BTreeMap<String, IndexedFileRow>> {
    let mut stmt = conn.prepare("SELECT path, sha256, language, kind FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, IndexedFileRow {
            sha256: row.get(1)?,
            language: row.get(2)?,
            kind: row.get(3)?,
        }))
    })?;
    let mut files = BTreeMap::new();
    for row in rows {
        let (path, indexed) = row?;
        files.insert(path, indexed);
    }
    Ok(files)
}

pub(crate) fn target_for_path(
    config: &Config,
    relative_path: &Path,
) -> Option<(Language, TargetKind)> {
    let relative = path_string(relative_path);
    // Skip extensionless / non-code files early; the per-target check below decides language by
    // which target CLAIMS the extension, so a `.h` lands on a `cpp` binding (as C++) or a `c`
    // binding (as C) — bare `from_path` detection would force every `.h` to C and starve C++
    // header resolution.
    relative_path.extension().and_then(|ext| ext.to_str())?;
    let mut targets = config.targets.iter().collect::<Vec<_>>();
    targets.sort_by_key(|target| target.index_precedence());
    targets.into_iter().find_map(|target| {
        if !target.language.claims_path(relative_path) {
            return None;
        }
        if !target.directories.iter().any(|directory| {
            directory.as_os_str().is_empty()
                || directory == Path::new(".")
                || relative_path.starts_with(directory)
        }) {
            return None;
        }
        if target.exclude.iter().any(|pattern| matches_simple_pattern(&relative, pattern)) {
            return None;
        }
        if !target.include.iter().any(|pattern| matches_simple_pattern(&relative, pattern)) {
            return None;
        }
        Some((target.language, target.kind))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::language::Language;

    #[test]
    fn h_resolves_to_cpp_when_both_c_and_cpp_bindings_cover_it() {
        // Both bindings claim `.h`; the `cpp` upgrade must win it (the deliberate intent), while a
        // `.c` stays C and a `.cpp` stays C++.
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("ragrat-disc-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nc = [\".\"]\ncpp = [\".\"]\n",
        )
        .unwrap();
        let config = crate::config::Config::load(root.join("rag-rat.toml")).unwrap();

        assert_eq!(target_for_path(&config, Path::new("a.h")).map(|(l, _)| l), Some(Language::Cpp));
        assert_eq!(target_for_path(&config, Path::new("a.c")).map(|(l, _)| l), Some(Language::C));
        assert_eq!(
            target_for_path(&config, Path::new("a.cpp")).map(|(l, _)| l),
            Some(Language::Cpp)
        );
        // A non-code file resolves to nothing.
        assert_eq!(target_for_path(&config, Path::new("README")), None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
