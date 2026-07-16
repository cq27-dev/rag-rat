//! Discovery: compare configured targets against the index to find
//! indexed/unindexed/changed/removed files.

use rag_rat_base::hash::hex_sha256;
use rag_rat_base::paths::path_string;

use super::*;

#[derive(Debug, Serialize)]
pub struct DiscoveryStatus {
    pub discovered_files: usize,
    pub indexed_files: usize,
    pub unindexed_files: usize,
    pub unindexed_source_files: usize,
    /// Files a discover pass would CARRY onto the current HEAD instead of re-deriving (#502):
    /// their retained rows are not in the active scope yet, so until that pass runs, queries at
    /// the new HEAD miss them — pending work (with a cheap remedy), not an indexed state.
    pub carryable_files: usize,
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
    /// `files.id` of retained committed rows to ADOPT into the active commit scope instead of
    /// re-deriving (#502): rows of a previous HEAD's scope (same repo + generation,
    /// `worktree_id = ''`, a different `commit_sha`) whose (sha256, language, kind) match the
    /// discovered file exactly. Re-stamping a row's `commit_sha` in place preserves its id and
    /// every chunk/symbol/edge/embedding/memory-binding hanging off it, so a `git pull` or
    /// branch checkout costs roughly its diff instead of a full re-derive. Discovery is a pure
    /// read (the status command shares it), so the plan carries the ids and the indexing pass
    /// applies the re-stamps inside its own transaction.
    pub(crate) carried: Vec<i64>,
    pub(crate) discovered_files: usize,
    pub(crate) indexed_files: usize,
}

/// `changes` is the working tree's git status (dirty + untracked paths) — the caller computes it
/// once and shares it with the scope assignment that follows the plan. The carry (#502) consults
/// it because retained-row adoption writes the COMMITTED scope: a dirty or untracked path whose
/// disk bytes happen to match an older commit must fall through to normal indexing (which stamps
/// it into the worktree OVERLAY scope) — a carry would record uncommitted content as committed,
/// and committed rows are shared with every linked worktree's base view.
pub(crate) fn discovery_plan(
    conn: &rusqlite::Connection,
    config: &Config,
    changes: &GitChangedPaths,
) -> anyhow::Result<DiscoveryPlan> {
    let discovered = collect_index_files(config)?;
    let mut indexed = indexed_file_map(conn)?;
    let retained = retained_committed_file_map(conn)?;
    let mut current_paths = BTreeSet::new();
    let mut files = Vec::new();
    let mut unindexed = Vec::new();
    let mut changed = Vec::new();
    let mut carried = Vec::new();
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
            // Absent from the ACTIVE scope, but a retained committed row (a previous HEAD's
            // scope) holds this exact content under the same target: adopt it instead of
            // re-deriving (#502). The match mirrors the drift checks below — sha for content,
            // (language, kind) for target drift — so a carry can never smuggle a stale parse
            // into the new scope; among several matching retained rows the most recently
            // derived (highest id) wins. A DIRTY or UNTRACKED path is never carried even on a
            // sha match: its disk bytes are working-tree content, not the new HEAD's committed
            // content (a deleted-then-recreated file, or a revert to an old commit's bytes),
            // so it falls through to normal indexing and lands in the overlay scope.
            if !changes.changed.contains(&file.relative_path)
                && let Some(rows) = retained.get(&relative)
                && let Some(matching) = rows.iter().rev().find(|row| {
                    row.sha256 == current_hash
                        && row.language == file.language.as_str()
                        && row.kind == file.kind.as_str()
                })
            {
                carried.push(matching.file_id);
                continue;
            }
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
        // Carried paths count as indexed: no re-derive is owed for them, only the scope
        // re-stamp the indexing pass applies.
        indexed_files: current_paths
            .len()
            .saturating_add(deleted.len())
            .saturating_sub(unindexed.len()),
        files,
        deleted,
        unindexed,
        changed,
        carried,
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

/// A retained committed row's carry identity: the row to re-stamp plus the (sha256, language,
/// kind) triple a discovered file must match for the adoption to be sound (#502).
struct RetainedFileRow {
    file_id: i64,
    sha256: String,
    language: String,
    kind: String,
}

/// Retained committed rows — this repo + live generation, `worktree_id = ''`, any commit OTHER
/// than the active one — keyed by path: the carry source when a HEAD move re-keys the base scope
/// (#502). Reads the scope dimensions from `temp.connection_context` like the `files` view does.
/// ALL rows per path are kept (in id order): a path can retain rows for several old commits with
/// DIFFERENT content — e.g. a branch round-trip leaves a stale feature row with a HIGHER id
/// beside the old-main row a later pull actually matches — so the triple match must see every
/// candidate, not a pre-collapsed "latest" one. Empty outside a git context (no committed scope
/// exists, and no active commit to carry into).
fn retained_committed_file_map(
    conn: &rusqlite::Connection,
) -> anyhow::Result<BTreeMap<String, Vec<RetainedFileRow>>> {
    let active_commit: String = conn.query_row(
        "SELECT value FROM temp.connection_context WHERE key = 'commit_sha'",
        [],
        |row| row.get(0),
    )?;
    let mut files: BTreeMap<String, Vec<RetainedFileRow>> = BTreeMap::new();
    if active_commit.is_empty() {
        return Ok(files);
    }
    let mut stmt = conn.prepare(
        "SELECT path, id, sha256, language, kind FROM main.files
         WHERE repo_id = (SELECT value FROM temp.connection_context WHERE key = 'repo_id')
           AND generation = (SELECT value FROM temp.connection_context WHERE key = \
         'files_generation')
           AND worktree_id = '' AND kind != 'deleted'
           AND commit_sha != '' AND commit_sha != ?1
         ORDER BY id",
    )?;
    let rows = stmt.query_map([&active_commit], |row| {
        Ok((row.get::<_, String>(0)?, RetainedFileRow {
            file_id: row.get(1)?,
            sha256: row.get(2)?,
            language: row.get(3)?,
            kind: row.get(4)?,
        }))
    })?;
    for row in rows {
        let (path, retained) = row?;
        files.entry(path).or_default().push(retained);
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

    use rag_rat_base::language::Language;

    use super::*;

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
        let config = rag_rat_base::config::Config::load(root.join("rag-rat.toml")).unwrap();

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
