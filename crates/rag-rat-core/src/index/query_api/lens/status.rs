//! Stable status/version shapes for editor clients.

use std::collections::BTreeMap;

use rag_rat_base::config::Config;
use serde::Serialize;

use crate::index::IndexDatabase;

#[derive(Debug, Serialize)]
pub struct LensStatus {
    pub repo_id: String,
    /// The CHECKOUT this index is serving — empty for the main worktree, the linked worktree's
    /// path otherwise. A repository's linked worktrees share one `repo_id` by design, so a client
    /// binding itself to a hosted server needs this to tell them apart: line-anchored memories and
    /// clone regions describe the checkout they were indexed from, not the repository at large.
    pub worktree_id: String,
    pub repo_root: String,
    pub indexed_root: String,
    pub case_insensitive_paths: bool,
    pub live_files_generation: i64,
    pub clone_graph_generation: Option<i64>,
    pub indexed_head: Option<String>,
    pub git_dirty: Option<String>,
    pub indexed_at_ms: Option<String>,
    pub schema_version: u32,
    pub live_file_count: u64,
    pub counts: BTreeMap<&'static str, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LensVersion {
    pub generation: i64,
    pub max_indexed_at_ms: i64,
    pub git_dirty: Option<String>,
    pub content_revision: String,
    pub lanes: LensLaneVersions,
    /// Aggregate compatibility token for clients predating per-lane revisions.
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LensLaneVersions {
    pub symbols: String,
    pub clones: String,
    pub memories: String,
    pub coupling: String,
    pub papertrail: String,
}

impl IndexDatabase {
    /// Repository-scoped status in the stable editor-shim shape.
    pub fn lens_status(
        &self,
        config: &Config,
        indexed_root: String,
        case_insensitive_paths: bool,
    ) -> anyhow::Result<LensStatus> {
        let conn = self.storage.connection();
        let clone_graph_generation =
            self.repo_meta("clone_graph_live_generation")?.and_then(|value| value.parse().ok());
        let live_file_count = count(conn, "SELECT COUNT(*) FROM files")?;
        let mut counts = BTreeMap::new();
        counts.insert(
            "symbols",
            count(conn, "SELECT COUNT(*) FROM symbols s JOIN files f ON f.id = s.file_id")?,
        );
        counts.insert(
            "edges_data",
            count(
                conn,
                "SELECT COUNT(*) FROM edges_data e JOIN files f ON f.id = e.source_file_id",
            )?,
        );
        let memory_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM repo_memories WHERE repo_id = ?1",
            [&self.active_repo_id],
            |row| row.get(0),
        )?;
        counts.insert("repo_memories", u64::try_from(memory_count).unwrap_or(0));
        Ok(LensStatus {
            repo_id: self.active_repo_id.clone(),
            worktree_id: self.active_worktree_id.clone(),
            repo_root: config.root.display().to_string(),
            indexed_root,
            case_insensitive_paths,
            live_files_generation: self.active_generation,
            clone_graph_generation,
            indexed_head: self.repo_meta("git_commit")?,
            git_dirty: self.repo_meta("git_dirty")?,
            indexed_at_ms: self.repo_meta("indexed_at_ms")?,
            schema_version: rag_rat_db::schema::status(conn)?.current_version,
            live_file_count,
            counts,
        })
    }

    /// Cheap freshness token in the stable editor-shim shape.
    pub fn lens_version(&self) -> anyhow::Result<LensVersion> {
        use rag_rat_db::meta;

        let enrichment_revision =
            self.repo_meta(meta::LENS_ENRICHMENT_REVISION_META)?.unwrap_or_else(|| "0".to_string());
        let clone_graph_generation =
            self.repo_meta("clone_graph_live_generation")?.unwrap_or_else(|| "none".to_string());
        let revision = |key| -> anyhow::Result<String> {
            Ok(self.repo_meta(key)?.unwrap_or_else(|| "0".to_string()))
        };
        Ok(LensVersion {
            generation: self.active_generation,
            max_indexed_at_ms: self
                .repo_meta("indexed_at_ms")?
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            git_dirty: self.repo_meta("git_dirty")?,
            content_revision: self.content_revision()?,
            lanes: LensLaneVersions {
                symbols: revision(meta::LENS_SYMBOLS_REVISION_META)?,
                clones: format!(
                    "{}:{clone_graph_generation}",
                    revision(meta::LENS_CLONES_REVISION_META)?
                ),
                memories: revision(meta::LENS_MEMORIES_REVISION_META)?,
                coupling: revision(meta::LENS_COUPLING_REVISION_META)?,
                papertrail: revision(meta::LENS_PAPERTRAIL_REVISION_META)?,
            },
            revision: format!("{enrichment_revision}:{clone_graph_generation}"),
        })
    }
}

fn count(conn: &rusqlite::Connection, sql: &str) -> anyhow::Result<u64> {
    let count: i64 = conn.query_row(sql, [], |row| row.get(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}
