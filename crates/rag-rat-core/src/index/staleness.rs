//! Staleness detection + lazy-heal reads: is an indexed file stale vs disk, which search hits are
//! stale, regex/line reads over current source.

use super::*;

/// Whether `search_with_heal` may lazily re-index files whose chunks have drifted from disk.
/// `Allow` heals once and retries; `Skip` is the recursion's base case (and the explicit
/// "don't touch the index" path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Heal {
    Allow,
    Skip,
}

impl IndexDatabase {
    pub(crate) fn source_path_is_stale(&self, path: &str, indexed_sha256: &str) -> bool {
        let Some(root) = self.storage.source_root() else {
            return false;
        };
        let Ok(bytes) = fs::read(root.join(path)) else {
            return true;
        };
        hex_sha256(&bytes) != indexed_sha256
    }

    pub(super) fn find_regex_hits(
        &self,
        pattern: &str,
        regex: &Regex,
        include_tests: bool,
    ) -> anyhow::Result<Vec<crate::query::graph::TextOnlyHit>> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("cannot compare graph to text: source_root is missing from repo_meta");
        };
        let mut stmt = self.storage.connection().prepare("SELECT path FROM files ORDER BY path")?;
        let paths =
            stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        let mut hits = Vec::new();
        for path in paths {
            if !include_tests && crate::index::parser::is_test_path(&path) {
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

    pub(super) fn read_current_line_text(
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
        request: &crate::search::lexical::LexicalQuery<'_>,
        heal: Heal,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let hits = crate::search::lexical::search_with_options(self.storage.connection(), request)?;
        if heal == Heal::Skip {
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
        // Retry once against the freshly healed index; Heal::Skip stops the recursion.
        self.search_with_heal(request, Heal::Skip)
    }

    fn stale_hit_paths(&self, hits: &[SearchHit]) -> anyhow::Result<Vec<String>> {
        // Under a LINKED-WORKTREE OVERLAY scope, `source_root` is the MAIN checkout — NOT the
        // branch these hits came from. Revalidating overlay chunks against main's copy of the file
        // marks every branch-changed file stale; `search_with_heal` then either raises
        // `NeedsReindex` past the cap or calls the overlay-guarded `heal_file` no-op. The overlay
        // rows are authoritative (maintained by `index_worktree_overlay`), so report nothing stale
        // — same rationale as the read_chunk overlay skip (#219 review).
        if self.active_scope_is_linked_overlay() {
            return Ok(Vec::new());
        }
        let Some(root) = self.storage.source_root() else {
            return Ok(Vec::new());
        };
        // One dict decoder for the whole hit set: this loops `read_chunk` per hit, and the per-call
        // dict SELECT + dictionary prep is ~20x the decompress itself, so reusing it across the
        // batch keeps the healing-search check cheap (#77 Phase 2 read-path perf).
        let dicts = crate::query::chunk_text_dicts(self.storage.connection())?;
        let mut decoder = crate::index::text_compression::ChunkTextDecoder::new(&dicts);
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
            let chunk = crate::query::read_chunk_with(
                self.storage.connection(),
                hit.chunk_id,
                &mut decoder,
            )?;
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
