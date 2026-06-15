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
        let Some(indexed_hash) = indexed.remove(&relative) else {
            unindexed.push(file.clone());
            files.push(file);
            continue;
        };
        if current_hash != indexed_hash {
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

pub(crate) fn indexed_file_map(
    conn: &rusqlite::Connection,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut stmt = conn.prepare("SELECT path, sha256 FROM files ORDER BY path")?;
    let rows =
        stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    let mut files = BTreeMap::new();
    for row in rows {
        let (path, sha256) = row?;
        files.insert(path, sha256);
    }
    Ok(files)
}

pub(crate) fn target_for_path(
    config: &Config,
    relative_path: &Path,
) -> Option<(Language, TargetKind)> {
    let relative = path_string(relative_path);
    let language = Language::from_path(relative_path)?;
    let mut targets = config.targets.iter().collect::<Vec<_>>();
    targets.sort_by_key(|target| match target.kind {
        TargetKind::Generated => 0,
        TargetKind::Tests => 1,
        TargetKind::Docs => 2,
        TargetKind::Source => 3,
    });
    targets.into_iter().find_map(|target| {
        if target.language != language {
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
