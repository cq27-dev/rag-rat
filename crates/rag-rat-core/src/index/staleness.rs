//! Staleness detection + lazy-heal reads: is an indexed file stale vs disk, which search hits are
//! stale, regex/line reads over current source.

use super::*;

impl IndexDatabase {
    pub(super) fn source_path_is_stale(&self, path: &str, indexed_sha256: &str) -> bool {
        let Some(root) = self.storage.source_root() else {
            return false;
        };
        let Ok(bytes) = fs::read(root.join(path)) else {
            return true;
        };
        hex_sha256(&bytes) != indexed_sha256
    }

    pub(super) fn regex_hits(
        &self,
        pattern: &str,
        regex: &Regex,
        include_tests: bool,
    ) -> anyhow::Result<Vec<crate::query::graph::TextOnlyHit>> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("cannot compare graph to text: source_root is missing from index_meta");
        };
        let mut stmt = self.storage.connection().prepare("SELECT path FROM files ORDER BY path")?;
        let paths =
            stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        let mut hits = Vec::new();
        for path in paths {
            if !include_tests && is_test_like_path(&path) {
                continue;
            }
            let full_path = root.join(&path);
            let Ok(text) = fs::read_to_string(&full_path) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    hits.push(crate::query::graph::TextOnlyHit {
                        path: path.clone(),
                        line: i64::try_from(index + 1).unwrap_or(i64::MAX),
                        text: line.trim().to_string(),
                        reason: "text pattern matched".to_string(),
                        likely_gap: pattern.to_string(),
                    });
                }
            }
        }
        Ok(hits)
    }

    pub(super) fn current_line_text(
        &self,
        path: &str,
        line: i64,
    ) -> anyhow::Result<Option<String>> {
        let Some(root) = self.storage.source_root() else {
            return Ok(None);
        };
        let Ok(text) = fs::read_to_string(root.join(path)) else {
            return Ok(None);
        };
        let Some(index) = usize::try_from(line.saturating_sub(1)).ok() else {
            return Ok(None);
        };
        Ok(text.lines().nth(index).map(|line| line.trim().to_string()))
    }

    pub(super) fn search_with_heal(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        allow_heal: bool,
        explain: bool,
        options: SearchOptions,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let hits = crate::search::lexical::search_with_options(
            self.storage.connection(),
            query,
            limit,
            include_generated,
            explain,
            options,
        )?;
        if !allow_heal {
            return Ok(hits);
        }
        let stale = self.stale_hit_paths(&hits)?;
        if stale.is_empty() {
            return Ok(hits);
        }
        if stale.len() > MAX_AUTO_HEAL_FILES_PER_CALL {
            anyhow::bail!(IndexError::NeedsReindex {
                stale_files: stale.len(),
                cap: MAX_AUTO_HEAL_FILES_PER_CALL,
            });
        }
        for path in stale {
            self.heal_file(Path::new(&path))?;
        }
        self.sync_fts()?;
        self.search_with_heal(query, limit, include_generated, false, explain, options)
    }

    fn stale_hit_paths(&self, hits: &[SearchHit]) -> anyhow::Result<Vec<String>> {
        let Some(root) = self.storage.source_root() else {
            return Ok(Vec::new());
        };
        let mut stale = Vec::new();
        let mut seen = BTreeSet::new();
        for hit in hits {
            if !seen.insert(hit.path.clone()) {
                continue;
            }
            let source_path = root.join(&hit.path);
            let Ok(text) = fs::read_to_string(source_path) else {
                stale.push(hit.path.clone());
                continue;
            };
            let chunk = crate::query::read_chunk(self.storage.connection(), hit.chunk_id)?;
            let Some(chunk) = chunk else {
                stale.push(hit.path.clone());
                continue;
            };
            let anchor = self.chunk_anchor(hit.chunk_id)?;
            let status = anchors::validate(
                &chunk.text,
                usize::try_from(chunk.start_line).unwrap_or(1),
                usize::try_from(chunk.end_line).unwrap_or(1),
                &anchor,
                &text,
            );
            if !matches!(status, AnchorStatus::Exact) {
                stale.push(hit.path.clone());
            }
        }
        Ok(stale)
    }

    pub(super) fn chunk_anchor(&self, chunk_id: i64) -> anyhow::Result<ChunkAnchor> {
        Ok(self.storage.connection().query_row(
            "
            SELECT anchor_version, normalized_hash, start_boundary_hash, end_boundary_hash,
                   start_context_hash, end_context_hash, context_radius
            FROM chunks WHERE id = ?1
            ",
            [chunk_id],
            |row| {
                Ok(ChunkAnchor {
                    version: row.get(0)?,
                    normalized_hash: row.get(1)?,
                    start_boundary_hash: row.get(2)?,
                    end_boundary_hash: row.get(3)?,
                    start_context_hash: row.get(4)?,
                    end_context_hash: row.get(5)?,
                    context_radius: row.get(6)?,
                })
            },
        )?)
    }
}
