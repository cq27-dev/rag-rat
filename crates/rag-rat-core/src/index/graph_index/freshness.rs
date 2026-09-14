//! Graph-index freshness: edge resolution entry points, per-row graph/scope provenance, the
//! on-open heal, and graph coverage.

use super::logical_key::KeyVersionStamp;
use super::references::resolve_synced_symbol_anchors;
use crate::index::*;

/// The `#<part>` tail a continuation chunk carries, or `""`. Only a tail of DIGITS counts: a
/// qualified name can hold a `#` of its own (a markdown context chunk is `…::#context-…`), and
/// treating that as a part marker would cut the name in half.
fn chunk_part_suffix(symbol_path: &str) -> &str {
    match symbol_path.rfind('#') {
        Some(at)
            if symbol_path.len() > at + 1
                && symbol_path[at + 1..].bytes().all(|b| b.is_ascii_digit()) =>
            &symbol_path[at..],
        _ => "",
    }
}

impl IndexDatabase {
    pub(in crate::index) fn resolve_edges(&self) -> anyhow::Result<()> {
        edges::resolve_all_edges(self.storage.connection())
    }

    /// Re-resolve ONLY the source files staged in `temp.edge_rewrite_files` (#827) — the
    /// incremental content-changed pass's narrowed twin of [`Self::resolve_edges`]. The caller
    /// must have armed capture (`begin_scoped_edge_rewrite`) so the staging holds this pass's
    /// changed files plus the source files of the in-edges its removals NULLed, and must only
    /// reach here when the pass's mutations are purely per-file symbol/edge changes (the
    /// incremental resolve gate). Resolution TARGETS still span the full active scope, so an
    /// edge in a changed file into an unchanged symbol resolves.
    pub(in crate::index) fn resolve_changed_edges(&self) -> anyhow::Result<()> {
        edges::resolve_changed_edges(self.storage.connection())
    }

    /// Resolve edges for a LINKED-WORKTREE OVERLAY pass (#219 P1): re-resolve / re-synthesize ONLY
    /// the worktree's own overlay source files, never the SHARED committed (base) rows that are
    /// merely visible in the overlay scope view. Resolution targets still span the full overlay
    /// view, so an overlay edge into a base symbol resolves correctly. The plain `resolve_edges`
    /// (base/incremental/full-rebuild) owns its scope and rewrites everything in view.
    pub(in crate::index) fn resolve_overlay_edges(&self, worktree_id: &str) -> anyhow::Result<()> {
        edges::resolve_overlay_edges(self.storage.connection(), worktree_id)
    }

    pub(in crate::index) fn graph_coverage(
        &self,
        paths: BTreeSet<String>,
    ) -> anyhow::Result<rag_rat_query::graph::GraphCoverage> {
        let indexed_files =
            self.storage
                .connection()
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?;
        let parser_failure_paths = self.parser_failure_paths()?;
        let parser_failures = u64::try_from(parser_failure_paths.len()).unwrap_or(0);
        let known_index_gaps = parser_failure_paths
            .iter()
            .map(|failure| {
                format!(
                    "{} parser failed for {}: {}",
                    failure.language, failure.path, failure.message
                )
            })
            .collect::<Vec<_>>();
        let mut stale_files = 0_u64;
        let mut parser_coverage_for_paths = Vec::new();
        for path in paths {
            let Some(row) = self.graph_path_row(&path)? else {
                parser_coverage_for_paths.push(rag_rat_query::graph::GraphPathCoverage {
                    path,
                    language: "unknown".to_string(),
                    parser_status: "missing_from_index".to_string(),
                    graph_status: "missing_from_index".to_string(),
                    last_indexed_revision: None,
                });
                continue;
            };
            let stale = self.source_path_is_stale(&path, &row.sha256);
            if stale {
                stale_files += 1;
            }
            let parser_failed = parser_failure_paths.iter().any(|failure| failure.path == path);
            parser_coverage_for_paths.push(rag_rat_query::graph::GraphPathCoverage {
                path,
                language: row.language,
                parser_status: if parser_failed { "failed" } else { "ok" }.to_string(),
                graph_status: if stale {
                    "stale_source"
                } else if parser_failed {
                    "parser_failed"
                } else {
                    "ok"
                }
                .to_string(),
                last_indexed_revision: (!row.indexed_revision.is_empty())
                    .then_some(row.indexed_revision),
            });
        }
        Ok(rag_rat_query::graph::GraphCoverage {
            indexed_files: u64::try_from(indexed_files).unwrap_or(0),
            parser_failures,
            stale_files,
            known_index_gaps,
            parser_coverage_for_paths,
        })
    }

    fn graph_path_row(&self, path: &str) -> anyhow::Result<Option<GraphPathRow>> {
        self.storage
            .connection()
            .query_row(
                "SELECT language, sha256, indexed_revision FROM files WHERE path = ?1",
                [path],
                |row| {
                    Ok(GraphPathRow {
                        language: row.get(0)?,
                        sha256: row.get(1)?,
                        indexed_revision: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub(in crate::index) fn ensure_graph_index_current(&self) -> anyhow::Result<()> {
        self.ensure_graph_index_current_inner()?;
        // Re-resolve synced SYMBOL anchors against the now-current index. This must run OUTSIDE the
        // inner heal's early returns: a peer that synced anchors but whose code index is unchanged
        // hits those returns on every open, and that is exactly the case this pass exists for. It
        // is a differs-only UPDATE (a no-op in steady state) and read-only opens skip
        // `ensure` entirely, so there is no write-on-read hazard. Same-session immediacy
        // after a sync is handled at the table-sync settle points; this is the safety net
        // that resolves at the next index open.
        resolve_synced_symbol_anchors(
            self.storage.connection(),
            &self.active_repo_id,
            self.active_generation,
        )?;
        Ok(())
    }

    fn ensure_graph_index_current_inner(&self) -> anyhow::Result<()> {
        let graph_current =
            self.repo_meta("graph_index_version")?.as_deref() == Some(GRAPH_INDEX_VERSION);
        let active_derivation_owed = self.active_derivation_rows_owed()?;
        let scope_rows_newer = self.scope_rows_newer()?;
        if scope_rows_newer && !self.active_graph_rows_owed()? {
            return Ok(());
        }
        if !active_derivation_owed {
            // A sibling checkout may still owe rows this scope cannot read. The active graph is
            // safe to serve; once the final sibling heals, that opener advances the repo summary.
            if !graph_current && !self.graph_rows_owed()? {
                self.mark_graph_index_current()?;
            }
            return Ok(());
        }
        let Some(root) = self.storage.source_root().map(Path::to_path_buf) else {
            return Ok(());
        };
        self.storage.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| -> anyhow::Result<()> {
            self.begin_scoped_edge_rewrite()?;
            // Repopulate the per-package import scope BEFORE re-resolving (#61). A bare
            // version-bump re-resolve would re-derive `import_scope_*` on the new edges
            // but read an empty `packages` table (V022 only ADDED the column; it did not
            // backfill `packages`), so every file would fall open to the global union and
            // the new per-package behavior would never engage on a migrated index.
            // `refresh_packages` writes the active scope's `packages` rows + the global
            // `local_crate_roots` union; `resolve_edges` below then computes each file's
            // package at load time (`load_package_roots_into_scope`) from those rows.
            self.refresh_packages(&root)?;
            let (files, unreadable) = self.graph_reindex_files()?;
            // The heal walks the repo+generation's WHOLE file set (A3 + A6 scope it to this
            // repo's live generation), which spans every commit/worktree scope — but it has
            // exactly ONE checkout to read from. So each row is re-derived only from bytes
            // PROVEN to be its own: `files.sha256` is the digest of the very text that produced
            // the row, so a match means this checkout holds that file's indexed content, whoever
            // else shares the path. Everything else — a sibling worktree whose copy differs, a
            // path absent from this checkout, a file edited since it was indexed — keeps the
            // graph and scopes it already has. Re-deriving those from the active root instead
            // would stamp this checkout's graph onto another scope's rows; failing the open over
            // them would brick every later open, since this runs ON the open path.
            //
            // A skipped row keeps its old per-file versions, so the checkout that owns those bytes
            // can resume both derivations later without forcing this scope to parse them again.
            // Rows the row reader could not name are uncovered exactly like an unreadable file.
            let mut unverified = unreadable;
            let mut unrefreshed = 0usize;
            let mut edge_rewrite_staged = false;
            for file in files {
                let full_path = root.join(&file.path);
                let Ok(text) = fs::read_to_string(full_path) else {
                    unverified += 1;
                    continue;
                };
                if rag_rat_base::hash::hex_sha256(text.as_bytes()) != file.sha256 {
                    unverified += 1;
                    continue;
                }
                // A file needs its scopes re-derived: Rust because a scope-affecting key bump
                // changed what its impl scopes ARE. Any
                // other language because the scope entered the key at all: `scope_path` landed
                // nullable with no backfill, so a row indexed before it reads as `''` here while a
                // fresh index hashes a real enclosing scope. Left alone, those rows would take the
                // new stamp holding a key no fresh index produces, and nested same-named symbols
                // would stay collapsed with no later pass owing them a re-derivation.
                let scope_needs_refresh = file.scope_owed
                    && file.kind != TargetKind::Generated
                    && file.language != Language::Markdown
                // Above the parse limit there are no persisted symbols to refresh at all
                // (`prepare_index_content_from_text` skips the same bound), so the scope shape
                // is vacuously current for this file.
                    && text.len() <= edges::MAX_GRAPH_PARSE_BYTES
                    && (file.language == Language::Rust
                        || self.file_has_unscoped_symbols(file.id)?);
                if file.scope_owed && !scope_rows_newer {
                    if !scope_needs_refresh {
                        self.mark_file_scope_current(file.id)?;
                    } else if self.refresh_symbol_scopes(
                        file.id,
                        Path::new(&file.path),
                        &text,
                        file.language,
                    )? {
                        self.stage_edge_rewrite_inedge_sources(
                            &file.path,
                            &self.active_repo_id,
                            self.active_generation,
                        )?;
                        self.stage_edge_rewrite_file(file.id)?;
                        edge_rewrite_staged = true;
                        self.mark_file_scope_current(file.id)?;
                    } else {
                        unrefreshed += 1;
                    }
                }
                if !file.graph_owed {
                    continue;
                }
                if file.kind == TargetKind::Generated
                    || file.language == Language::Markdown
                    || text.len() > edges::MAX_GRAPH_PARSE_BYTES
                {
                    self.mark_file_graph_current(file.id)?;
                    continue;
                }
                // Wipe exactly the row being re-derived, immediately before re-deriving it.
                // A repo-wide DELETE up front is what turned an unreadable or diverged row into
                // silent edge LOSS: nothing repopulates a row this loop skips. Per-row, the wipe
                // removes precisely the set the insert below replaces, and a skipped row keeps
                // the edges it already has — which `resolve_edges` will NOT revisit on a scoped
                // open (it writes only rows the connection's `files` view admits), so leaving
                // them in place is the difference between stale and absent.
                self.storage
                    .connection()
                    .prepare_cached("DELETE FROM edges_data WHERE source_file_id = ?1")?
                    .execute([file.id])?;
                edges::index_file_edges(
                    self.storage.connection(),
                    file.id,
                    Path::new(&file.path),
                    file.language,
                    &text,
                )?;
                self.stage_edge_rewrite_file(file.id)?;
                edge_rewrite_staged = true;
                self.mark_file_graph_current(file.id)?;
            }
            if unverified > 0 || unrefreshed > 0 {
                tracing::warn!(
                    unverified,
                    unrefreshed,
                    "graph heal skipped file rows this checkout could not vouch for; their \
                     per-file provenance remains owed for a later checkout (#1014)"
                );
            }
            // `resolve_edges` and `rebuild_logical_symbols` are SIBLINGS, not a chain: both
            // consume `symbols.scope_path`, and neither consumes the other's output
            // (`same_logical_symbol` compares in-memory `IndexedSymbol` fields, not
            // `logical_symbols` rows — edge resolution never reads that table). What is
            // load-bearing is that the refresh loop above completed for every row this checkout
            // can advance first; their relative order here is free.
            if edge_rewrite_staged {
                self.resolve_changed_edges()?;
            }
            if !scope_rows_newer
                && self.repo_meta(LOGICAL_KEY_VERSION_KEY)?.as_deref() != Some(LOGICAL_KEY_VERSION)
            {
                let stamp = if self.scope_rows_owed()? {
                    KeyVersionStamp::Defer
                } else {
                    KeyVersionStamp::FullRederive
                };
                self.rebuild_logical_symbols(stamp)?;
            }
            self.mark_graph_index_current()?;
            Ok(())
        })();
        self.finish_scoped_edge_rewrite();
        if result.is_err() {
            let _ = self.storage.execute_batch("ROLLBACK");
        }
        result?;
        self.storage.execute_batch("COMMIT")?;
        Ok(())
    }

    pub(in crate::index) fn mark_graph_index_current(&self) -> anyhow::Result<()> {
        if self.graph_rows_owed()? {
            self.storage.connection().execute(
                "DELETE FROM repo_meta WHERE repo_id = ?1 AND key = 'graph_index_version'",
                [&self.active_repo_id],
            )?;
            Ok(())
        } else {
            self.set_repo_meta("graph_index_version", GRAPH_INDEX_VERSION)
        }
    }

    pub(in crate::index) fn active_derivation_rows_owed(&self) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM main.files
                 WHERE repo_id = ?1 AND generation = ?2 AND kind != 'deleted'
                   AND graph_version < CAST(?3 AS INTEGER)
                   AND id IN (SELECT id FROM files)
             ) OR EXISTS(
                 SELECT 1 FROM main.files
                 WHERE repo_id = ?1 AND generation = ?2 AND kind != 'deleted'
                   AND scope_version < CAST(?4 AS INTEGER)
                   AND id IN (SELECT id FROM files)
             )",
            params![
                self.active_repo_id,
                self.active_generation,
                GRAPH_INDEX_VERSION,
                LOGICAL_KEY_VERSION
            ],
            |row| row.get::<_, i64>(0),
        )? == 1)
    }

    fn active_graph_rows_owed(&self) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM main.files
                 WHERE repo_id = ?1 AND generation = ?2 AND kind != 'deleted'
                   AND graph_version < CAST(?3 AS INTEGER)
                   AND id IN (SELECT id FROM files)
             )",
            params![self.active_repo_id, self.active_generation, GRAPH_INDEX_VERSION],
            |row| row.get::<_, i64>(0),
        )? == 1)
    }

    fn graph_rows_owed(&self) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM main.files
                 WHERE repo_id = ?1 AND generation = ?2 AND kind != 'deleted'
                   AND graph_version < CAST(?3 AS INTEGER)
             )",
            params![self.active_repo_id, self.active_generation, GRAPH_INDEX_VERSION],
            |row| row.get::<_, i64>(0),
        )? == 1)
    }

    fn scope_rows_owed(&self) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM main.files
                 WHERE repo_id = ?1 AND generation = ?2 AND kind != 'deleted'
                   AND scope_version < CAST(?3 AS INTEGER)
             )",
            params![self.active_repo_id, self.active_generation, LOGICAL_KEY_VERSION],
            |row| row.get::<_, i64>(0),
        )? == 1)
    }

    fn scope_rows_newer(&self) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM main.files
                 WHERE repo_id = ?1 AND generation = ?2 AND kind != 'deleted'
                   AND scope_version > CAST(?3 AS INTEGER)
             )",
            params![self.active_repo_id, self.active_generation, LOGICAL_KEY_VERSION],
            |row| row.get::<_, i64>(0),
        )? == 1)
    }

    fn mark_file_graph_current(&self, file_id: i64) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "UPDATE main.files SET graph_version = MAX(graph_version, CAST(?1 AS INTEGER))
             WHERE id = ?2",
            params![GRAPH_INDEX_VERSION, file_id],
        )?;
        Ok(())
    }

    fn mark_file_scope_current(&self, file_id: i64) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "UPDATE main.files SET scope_version = MAX(scope_version, CAST(?1 AS INTEGER))
             WHERE id = ?2",
            params![LOGICAL_KEY_VERSION, file_id],
        )?;
        Ok(())
    }

    /// Refresh symbol fields whose extraction semantics changed without rewriting chunks or
    /// embeddings. Version 2 gives a Rust trait impl a trait-qualified scope
    /// (`Type as std.fmt.Display`), and logical regrouping is only correct after persisted
    /// symbols carry that new scope.
    ///
    /// Returns whether EVERY persisted symbol of the file was refreshed. Rows are matched on
    /// their exact byte span, so a file edited since it was indexed matches only the symbols
    /// ahead of the edit — an ordinary state between an edit and the next watcher pass, and one
    /// the next index of that file resolves on its own. The caller reports the shortfall rather
    /// than failing the open on it.
    /// Whether this file holds a symbol from before `scope_path` existed. The column landed
    /// nullable and unbackfilled, so NULL means "never derived" — a parsed row always carries at
    /// least its own name.
    fn file_has_unscoped_symbols(&self, file_id: i64) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM main.symbols WHERE file_id = ?1 AND scope_path IS NULL)",
            [file_id],
            |row| row.get::<_, i64>(0),
        )? == 1)
    }

    fn refresh_symbol_scopes(
        &self,
        file_id: i64,
        path: &Path,
        text: &str,
        language: Language,
    ) -> anyhow::Result<bool> {
        let Some(parsed) = parser::parse_file(path, language, text) else {
            return Ok(false);
        };
        let expected: i64 = self.storage.connection().query_row(
            "SELECT COUNT(*) FROM main.symbols WHERE file_id = ?1",
            [file_id],
            |row| row.get(0),
        )?;
        // Match on the SPAN, not the name. Version 2 also changed what an `impl` symbol is NAMED —
        // the old extractor took the first nominal child, which for `impl Trait for Type` is the
        // TRAIT, and it is now the self type. Keeping `name` in the predicate would leave exactly
        // those rows unmatched, so an upgraded index would carry impl identities a fresh index
        // never produces, and the shortfall would be reported forever. A span plus kind is unique
        // per file — spans come from the AST — so the name can be an OUTPUT of the refresh.
        // `qualified_name` is `{path}::{name}`, so a renamed symbol needs it re-interned too —
        // both fields are hashed into `LogicalSymbolKey`, and refreshing one without the other
        // mints an identity no fresh index ever produces.
        let mut update = self.storage.connection().prepare_cached(
            "UPDATE main.symbols
             SET scope_path = ?1, name = ?2, qualified_name_id = ?3
             WHERE file_id = ?4 AND start_byte = ?5 AND end_byte = ?6 AND kind = ?7",
        )?;
        let mut refreshed = 0usize;
        for symbol in parsed.symbols {
            let qualified_name_id =
                edges::intern_edge_string(self.storage.connection(), &symbol.qualified_name)?;
            refreshed += update.execute(params![
                symbol.scope_path,
                symbol.name,
                qualified_name_id,
                file_id,
                i64::try_from(symbol.start_byte)?,
                i64::try_from(symbol.end_byte)?,
                symbol.kind,
            ])?;
        }
        // A chunk records the symbol it covers by PATH as well as by id, and readers still match on
        // the path — so a rename that moves `A` to `W` and stops there leaves every chunk-keyed
        // lookup for that impl searching a name nothing answers to, and its chunk-bound memories
        // vanish on upgrade. Realign the linked chunks to the names their symbols now carry.
        // The vector for a chunk is built from its symbol_path among other fields, and a stored
        // embedding is served while its `input_hash` is non-empty — so moving the path without
        // touching that hash leaves semantic search answering from a vector labelled with a name
        // the chunk no longer has. Clearing it first puts those chunks back in the reconcile queue.
        // Same predicate as the update below, and run BEFORE it, while the rows still differ.
        // A chunk whose symbol is longer than one chunk is stored as `<qualified_name>#<part>`,
        // so the realign replaces the BASE and keeps the part. Overwriting the whole value
        // collapsed every part of a long symbol onto the same path, which is both a loss of the
        // part labels and a divergence from what a fresh index produces for the same file.
        //
        // Computed here rather than in SQL so the embedding invalidation and the path rewrite
        // cannot disagree about which rows are affected — they read one `desired` value.
        let mut linked = self.storage.connection().prepare(
            "SELECT c.id, c.symbol_path, ns.value
               FROM main.chunks c
               JOIN main.symbols s ON s.id = c.symbol_id
               JOIN main.name_strings ns ON ns.id = s.qualified_name_id
              WHERE c.file_id = ?1 AND c.symbol_id IS NOT NULL AND c.symbol_path IS NOT NULL",
        )?;
        let realigned: Vec<(i64, String)> = linked
            .query_map([file_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(id, current, qualified_name)| {
                let desired = format!("{qualified_name}{}", chunk_part_suffix(&current));
                (desired != current).then_some((id, desired))
            })
            .collect();
        // The vector for a chunk is built from its symbol_path among other fields, and a stored
        // embedding is served while its `input_hash` is non-empty — so moving the path without
        // touching that hash leaves semantic search answering from a vector labelled with a name
        // the chunk no longer has. Clearing it puts those chunks back in the reconcile queue.
        // Prepared once: this heal renames every impl symbol in the repo, so `realigned` is the
        // corpus's impl count and `execute` would re-plan both statements for each of them.
        let mut clear_embedding = self
            .storage
            .connection()
            .prepare("UPDATE main.chunk_embeddings SET input_hash = '' WHERE chunk_id = ?1")?;
        let mut move_path = self
            .storage
            .connection()
            .prepare("UPDATE main.chunks SET symbol_path = ?2 WHERE id = ?1")?;
        for (chunk_id, desired) in &realigned {
            clear_embedding.execute([chunk_id])?;
            move_path.execute(params![chunk_id, desired])?;
        }
        Ok(i64::try_from(refreshed)? == expected)
    }

    /// The rows this heal will re-derive, plus the count it could NOT name — the caller folds that
    /// into its coverage so an unrecognized row defers the key stamp instead of being stamped over.
    fn graph_reindex_files(&self) -> anyhow::Result<(Vec<GraphReindexFile>, usize)> {
        // Read raw `main.files` for the A3/A6 predicates the view cannot supply on a bare open,
        // but INTERSECT with the connection's `files` view — the heal must re-extract exactly the
        // rows its own `resolve_edges` will re-resolve, and that pass writes only rows the view
        // admits. Re-extracting a row outside the view replaces its resolved edges with fresh
        // unresolved candidates that nothing then resolves, so a sibling commit or worktree scope
        // would permanently lose its targets — worse than leaving it on the previous extraction.
        // On a bare open the view is repo+generation-wide, so cross-scope coverage is unchanged.
        //
        // `kind != 'deleted'` guards the bare-open view, which does not filter tombstones:
        // `mark_file_deleted` leaves `language='unknown', kind='deleted'`, and neither parses, so
        // letting one through would turn every open into a hard error.
        let mut stmt = self.storage.connection().prepare(
            "SELECT id, path, language, kind, sha256,
                    graph_version < CAST(?3 AS INTEGER),
                    scope_version < CAST(?4 AS INTEGER)
                 FROM main.files
                 WHERE repo_id = ?1 AND generation = ?2 AND kind != 'deleted'
                   AND id IN (SELECT id FROM files)
                   AND (graph_version < CAST(?3 AS INTEGER)
                        OR scope_version < CAST(?4 AS INTEGER))
                 ORDER BY path",
        )?;
        let rows = stmt.query_map(
            params![
                self.active_repo_id,
                self.active_generation,
                GRAPH_INDEX_VERSION,
                LOGICAL_KEY_VERSION
            ],
            |row| {
                let language: String = row.get(2)?;
                let kind: String = row.get(3)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    language,
                    kind,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )?;
        let mut files = Vec::new();
        // A row this build cannot name is still a row the heal did not cover, so the caller has to
        // hear about it: with the count lost, a repo whose every OTHER row refreshed would take the
        // new key stamp, and once the stamp matches nothing ever owes this row a re-derivation.
        let mut unreadable = 0usize;
        for row in rows {
            let (id, path, language, kind, sha256, graph_owed, scope_owed) = row?;
            // A marker row this build cannot name is a row to LEAVE ALONE, not a reason to fail
            // the open. `ensure_graph_index_current` is on the open path, so an unparseable
            // language/kind here would wedge the database for every later open too.
            let (Ok(language), Ok(kind)) =
                (language.parse::<Language>(), kind.parse::<TargetKind>())
            else {
                tracing::warn!(path = %path, "skipping unrecognized file row during graph heal");
                unreadable += 1;
                continue;
            };
            files.push(GraphReindexFile {
                id,
                path,
                language,
                kind,
                sha256,
                graph_owed,
                scope_owed,
            });
        }
        Ok((files, unreadable))
    }
}

#[cfg(test)]
mod chunk_path_tests {
    use super::chunk_part_suffix;

    /// A symbol longer than one chunk is stored as `<qualified_name>#<part>`, and the realign
    /// replaces the base while keeping the part. Rewriting the whole value collapsed every part of
    /// a long symbol onto one path, so an upgraded index stopped matching a fresh one.
    #[test]
    fn a_continuation_part_survives_a_rename() {
        assert_eq!(chunk_part_suffix("src/lib.rs::Foo<T>#1"), "#1");
        assert_eq!(chunk_part_suffix("src/lib.rs::Foo<T>#12"), "#12");
        assert_eq!(chunk_part_suffix("src/lib.rs::Foo<T>"), "");
        // A `#` that is part of the NAME is not a part marker — a markdown context chunk carries
        // one, and cutting there would halve the name.
        assert_eq!(chunk_part_suffix("docs/x.md::#context-intro"), "");
        assert_eq!(chunk_part_suffix("docs/x.md::#context-intro#3"), "#3");
        // A bare trailing `#` marks nothing.
        assert_eq!(chunk_part_suffix("src/lib.rs::Foo#"), "");
    }
}
