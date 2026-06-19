//! Graph/edge/logical-symbol index lifecycle: resolve edges, (re)build logical symbols, graph
//! coverage, and graph-index freshness.

use super::*;

/// Grouping key that collapses cfg variants / overloads of one symbol into a single logical symbol.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct LogicalSymbolKey {
    pub(super) language: String,
    pub(super) path: String,
    pub(super) name: String,
    pub(super) qualified_name: String,
    pub(super) kind: String,
    // Signature is part of the identity so that two distinct same-named symbols in one file (e.g.
    // `new` on two different impls — same `qualified_name`, different signatures) do NOT collapse
    // into one logical symbol. Genuine cfg variants share a signature, so they still group.
    pub(super) signature: Option<String>,
}

impl LogicalSymbolKey {
    pub(super) fn from(row: &LogicalSymbolMemberRow) -> Self {
        Self {
            language: row.language.clone(),
            path: row.path.clone(),
            name: row.name.clone(),
            qualified_name: row.qualified_name.clone(),
            kind: row.kind.clone(),
            signature: row.signature.clone(),
        }
    }

    /// Deterministic logical-symbol id derived from the key, so it is **stable across reindex**
    /// (the table is fully rebuilt each pass; an autoincrement rowid would churn the id every
    /// time, breaking any cached id or logical-symbol-bound memory). A 63-bit truncation of the
    /// key's SHA-256 — collisions are astronomically unlikely across a repo's symbols, and a
    /// collision would surface as a loud primary-key error on rebuild rather than silent merging.
    pub(super) fn stable_id(&self) -> i64 {
        let canonical = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.language,
            self.path,
            self.name,
            self.qualified_name,
            self.kind,
            self.signature.as_deref().unwrap_or(""),
        );
        let digest = Sha256::digest(canonical.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        (u64::from_be_bytes(bytes) >> 1) as i64
    }
}

#[derive(Debug, Clone)]
pub(super) struct LogicalSymbolMemberRow {
    pub(super) symbol_id: i64,
    pub(super) path: String,
    pub(super) language: String,
    pub(super) name: String,
    pub(super) qualified_name: String,
    pub(super) kind: String,
    pub(super) signature: Option<String>,
    pub(super) start_line: i64,
    pub(super) end_line: i64,
}

impl IndexDatabase {
    pub(super) fn resolve_edges(&self) -> anyhow::Result<()> {
        edges::resolve_all_edges(self.storage.connection())
    }

    /// Resolve edges for a LINKED-WORKTREE OVERLAY pass (#219 P1): re-resolve / re-synthesize ONLY
    /// the worktree's own overlay source files, never the SHARED committed (base) rows that are
    /// merely visible in the overlay scope view. Resolution targets still span the full overlay
    /// view, so an overlay edge into a base symbol resolves correctly. The plain `resolve_edges`
    /// (base/incremental/full-rebuild) owns its scope and rewrites everything in view.
    pub(super) fn resolve_overlay_edges(&self, worktree_id: &str) -> anyhow::Result<()> {
        edges::resolve_overlay_edges(self.storage.connection(), worktree_id)
    }

    pub(super) fn rebuild_logical_symbols(&self) -> anyhow::Result<()> {
        // The insert below re-derives the COMPLETE logical-symbol table from all current symbols,
        // so clear it entirely first. A member-join "rebuild set" misses logical_symbols whose
        // members were cascade-deleted with their symbols (clear_full_rebuild_tables deletes
        // files → symbols → logical_symbol_members via FK, but logical_symbols has no such FK).
        // Those orphans would then collide with the deterministic stable id on re-insert.
        self.storage.connection().execute_batch(
            "
            DELETE FROM main.logical_symbol_members;
            DELETE FROM main.logical_symbols;
            ",
        )?;

        // STREAM the grouping instead of materializing every symbol. The previous version built a
        // `BTreeMap<LogicalSymbolKey, Vec<row>>` over ALL symbols — at kernel scale ~3.5M rows ×
        // six owned `String`s each (plus a cloned key per group). That structure, allocated in the
        // trailing rebuild phase AFTER the edge accumulator is freed, was the dominant full-rebuild
        // peak-RSS spike (~6 GB transient at the very end of a whole-kernel index). Ordering the
        // SELECT by the key's `Ord` (language, path, name, qualified_name, kind, signature — SQLite
        // ASC sorts NULL first, matching Rust `None < Some`) then the within-group member order
        // (start_byte, end_byte, which the old per-group Vec preserved) makes each group's rows
        // arrive contiguously, so we flush a group the moment its key changes and hold only the
        // current group's members (kilobytes). Byte-identical: same grouping, same
        // `logical_symbols` insert order (ids are content-derived via `stable_id`, not rowids), and
        // the same member order, verified against the golden index.
        // Read RAW `main.files` (all scopes), NOT the per-connection `files` scope VIEW.
        // logical_symbols is a GLOBAL table; building it must not depend on whichever scope happens
        // to be active. When this runs in a worktree-overlay context (a scope view IS installed),
        // an unqualified `files` resolves to the scoped temp view, so the wholesale DELETE
        // + repopulate would WIPE every other scope's grouping (base + sibling worktrees)
        // and restore only the active scope's — persistently breaking `sym_<hex>`-handle
        // graph nav for base symbols (the #219 review finding). Reading `main.files`
        // groups every symbol in every live scope; the content-derived `stable_id`
        // collapses cross-scope duplicates into one logical symbol with per-scope members,
        // and downstream reads stay scope-filtered via the `files` view.
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(
            "
            SELECT symbols.id, main.files.path, symbols.language, symbols.name,
                   qn.value, symbols.kind, symbols.signature, symbols.start_line,
                   symbols.end_line
            FROM main.symbols AS symbols
            JOIN main.files ON main.files.id = symbols.file_id
            LEFT JOIN main.name_strings qn ON qn.id = symbols.qualified_name_id
            ORDER BY symbols.language, main.files.path, symbols.name, qn.value,
                     symbols.kind, symbols.signature, symbols.start_byte, symbols.end_byte
            ",
        )?;
        let mut rows = stmt.query([])?;
        let mut current: Option<(LogicalSymbolKey, Vec<LogicalSymbolMemberRow>)> = None;
        while let Some(row) = rows.next()? {
            let member = LogicalSymbolMemberRow {
                symbol_id: row.get(0)?,
                path: row.get(1)?,
                language: row.get(2)?,
                name: row.get(3)?,
                qualified_name: row.get(4)?,
                kind: row.get(5)?,
                signature: row.get(6)?,
                start_line: row.get(7)?,
                end_line: row.get(8)?,
            };
            // Compare the member's key fields against the current group WITHOUT allocating a key
            // per row (only per group, on a boundary).
            let same_group = current.as_ref().is_some_and(|(key, _)| {
                key.language == member.language
                    && key.path == member.path
                    && key.name == member.name
                    && key.qualified_name == member.qualified_name
                    && key.kind == member.kind
                    && key.signature == member.signature
            });
            if same_group {
                current.as_mut().expect("same_group implies Some").1.push(member);
            } else {
                if let Some((key, members)) = current.take() {
                    Self::insert_logical_group(conn, &key, &members)?;
                }
                let key = LogicalSymbolKey::from(&member);
                current = Some((key, vec![member]));
            }
        }
        if let Some((key, members)) = current.take() {
            Self::insert_logical_group(conn, &key, &members)?;
        }
        Ok(())
    }

    pub(super) fn graph_coverage(
        &self,
        paths: BTreeSet<String>,
    ) -> anyhow::Result<crate::query::graph::GraphCoverage> {
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
                parser_coverage_for_paths.push(crate::query::graph::GraphPathCoverage {
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
            parser_coverage_for_paths.push(crate::query::graph::GraphPathCoverage {
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
        Ok(crate::query::graph::GraphCoverage {
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

    pub(super) fn ensure_graph_index_current(&self) -> anyhow::Result<()> {
        if self.meta("graph_index_version")?.as_deref() == Some(GRAPH_INDEX_VERSION) {
            return Ok(());
        }
        let Some(root) = self.storage.source_root().map(Path::to_path_buf) else {
            return Ok(());
        };
        self.storage.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| -> anyhow::Result<()> {
            self.storage.connection().execute("DELETE FROM edges_data", [])?;
            // Repopulate the per-package import scope BEFORE re-resolving (#61). A bare
            // version-bump re-resolve would re-derive `import_scope_*` on the new edges
            // but read an empty `packages` table (V022 only ADDED the column; it did not
            // backfill `packages`), so every file would fall open to the global union and
            // the new per-package behavior would never engage on a migrated index.
            // `refresh_packages` writes the active scope's `packages` rows + the global
            // `local_crate_roots` union; `resolve_edges` below then computes each file's
            // package at load time (`load_package_roots_into_scope`) from those rows.
            self.refresh_packages(&root)?;
            let files = self.graph_reindex_files()?;
            for file in files {
                if file.kind == TargetKind::Generated || file.language == Language::Markdown {
                    continue;
                }
                let full_path = root.join(&file.path);
                let Ok(text) = fs::read_to_string(full_path) else {
                    continue;
                };
                if text.len() > edges::MAX_GRAPH_PARSE_BYTES {
                    continue;
                }
                edges::index_file_edges(
                    self.storage.connection(),
                    file.id,
                    Path::new(&file.path),
                    file.language,
                    &text,
                )?;
            }
            self.resolve_edges()?;
            self.mark_graph_index_current()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.storage.execute_batch("ROLLBACK");
        }
        result?;
        self.storage.execute_batch("COMMIT")?;
        Ok(())
    }

    pub(super) fn mark_graph_index_current(&self) -> anyhow::Result<()> {
        self.set_meta("graph_index_version", GRAPH_INDEX_VERSION)
    }

    fn graph_reindex_files(&self) -> anyhow::Result<Vec<GraphReindexFile>> {
        let mut stmt = self
            .storage
            .connection()
            .prepare("SELECT id, path, language, kind FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            let language: String = row.get(2)?;
            let kind: String = row.get(3)?;
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, language, kind))
        })?;
        let mut files = Vec::new();
        for row in rows {
            let (id, path, language, kind) = row?;
            files.push(GraphReindexFile {
                id,
                path,
                language: language.parse()?,
                kind: kind.parse()?,
            });
        }
        Ok(files)
    }
}
