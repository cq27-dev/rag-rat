use std::collections::{HashMap, HashSet};

use super::*;

mod dispatch;
pub(crate) use dispatch::synthesize_dispatch_edges;

/// Build an [`ImportScopeRange`] from the three dedicated edge columns, or `None` when the start
/// byte is NULL (a non-import edge). The DB driver's twin of `CompactEdge::import_scope_range`
/// (the full-rebuild path) — they must stay in lockstep (#61 both-driver parity).
fn import_scope_from_row(
    scope_start: Option<i64>,
    scope_end: Option<i64>,
    mod_id: Option<i64>,
) -> Option<ImportScopeRange> {
    let scope_start = scope_start?;
    Some(ImportScopeRange {
        scope_start: usize::try_from(scope_start).unwrap_or(0),
        scope_end: usize::try_from(scope_end.unwrap_or(0)).unwrap_or(0),
        mod_id: mod_id.unwrap_or(MOD_FILE_ROOT),
    })
}

/// Apply a language package's import-alias rewrite. The shared resolver owns scope lookup and the
/// collision guard; the policy owns which reference shapes are rewritten.
struct ImportAliasResolveRequest<'a> {
    file_id: i64,
    source_language: Option<&'a str>,
    to_name: &'a str,
    target_qualified_name: Option<&'a str>,
    receiver_hint: Option<&'a str>,
    ref_byte: usize,
}

fn import_alias_rebind(
    import_scope: &imports::ImportScope,
    index: &SymbolIndex<'_>,
    request: ImportAliasResolveRequest<'_>,
) -> crate::index::languages::ImportAliasRebind {
    let Some(policy) = crate::index::languages::resolver_policy_for_name(request.source_language)
    else {
        return Default::default();
    };
    let mut lookup = |name: &str| {
        let target = import_scope.import_alias_target(request.file_id, name, request.ref_byte)?;
        (!index.file_defines(request.file_id, short_name(target))).then(|| target.to_string())
    };
    policy.rebind_import_alias(crate::index::languages::ImportAliasRequest {
        to_name: request.to_name,
        target_qualified_name: request.target_qualified_name,
        receiver_hint: request.receiver_hint,
        lookup: &mut lookup,
    })
}

/// Load the per-package local-crate sets and COMPUTE each active file's owning package into `scope`
/// (#61 per-package locality). `packages.local_roots_json` is a JSON string array of crate roots.
///
/// The file→package mapping is computed AT LOAD time — not read from a persisted `files.package_id`
/// — by longest-`manifest_dir`-prefix over the active scope's `packages` rows. A persisted pointer
/// was the #106 multi-worktree leak: a clean file is a SHARED commit-scope row read by every
/// worktree at that commit, while a package row is worktree-scoped, so one worktree's refresh
/// stamped its package ids onto the shared rows a sibling then followed (and the DELETE + reinsert
/// churned those ids each pass). Computing here against the active scope's OWN `packages` rows
/// means worktree B never sees worktree A's package map.
///
/// The `packages` read is scoped to the active `(commit_sha, worktree_id)` via the per-connection
/// `temp.connection_context` (the same context `install_scope_view` reads to build the `files`
/// view); the file list comes from the `files` TEMP VIEW (overlay wins, dead/sibling scopes
/// excluded), matching the symbol/edge resolution scope. A missing/empty `packages` table
/// (non-Cargo corpus, pre-V022 index) leaves the per-package maps empty so every file falls open to
/// the global set. Shared by BOTH drivers so the per-package model is identical (#61 both-driver
/// parity).
fn load_package_roots_into_scope(
    conn: &Connection,
    scope: &mut imports::ImportScope,
) -> anyhow::Result<()> {
    // The active checkout's (repo_id, commit_sha, worktree_id), so the `packages` read is scoped
    // exactly like the `files` view (which reads the same context table). The `repo_id` predicate
    // (A3) is what keeps a sibling repo's package roots out of THIS repo's import scope in a
    // consolidated DB where two repos share the empty non-git scope. A raw test connection without
    // the context falls back to the sole repo (placeholder on an un-adopted DB, matching the
    // placeholder-defaulted `packages` rows) and to empty commit/worktree — the same scope
    // `add_package`/`refresh_packages` write under for non-git fixtures, so the test path stays
    // consistent.
    let active_repo_id = crate::index::schema::active_repo_id(conn)?;
    let active_commit_sha = scope_context_value(conn, "commit_sha");
    let active_worktree_id = scope_context_value(conn, "worktree_id");

    // The active scope's packages, longest `manifest_dir` first so the first matching prefix is the
    // most specific package. A synthetic per-load index keys `scope.package_roots` — the persisted
    // `packages.id` is deliberately NOT consulted (it churns on every `refresh_packages` DELETE +
    // reinsert and is meaningless across scopes).
    let packages: Vec<(String, HashSet<String>)> = {
        let mut stmt = match conn.prepare(
            "SELECT manifest_dir, local_roots_json FROM packages WHERE repo_id = ?1 AND \
             commit_sha = ?2 AND worktree_id = ?3",
        ) {
            Ok(stmt) => stmt,
            // No `packages` table (pre-V022 / non-Cargo): nothing to load, fall open.
            Err(_) => return Ok(()),
        };
        let rows = stmt
            .query_map(params![active_repo_id, active_commit_sha, active_worktree_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
        let mut packages: Vec<(String, HashSet<String>)> = rows
            .map(|row| {
                let (manifest_dir, roots_json) = row?;
                let roots: HashSet<String> = serde_json::from_str(&roots_json).unwrap_or_default();
                Ok::<_, anyhow::Error>((manifest_dir, roots))
            })
            .collect::<Result<_, _>>()?;
        packages.sort_by_key(|(dir, _)| std::cmp::Reverse(dir.len()));
        packages
    };
    // No package rows for this scope (non-Cargo corpus, or a scope that never ran
    // refresh_packages): leave the per-package maps empty so every file falls open to the
    // global union.
    if packages.is_empty() {
        return Ok(());
    }
    for (synthetic_id, (_, roots)) in packages.iter().enumerate() {
        scope.set_package_roots(synthetic_id as i64, roots.clone());
    }

    // Assign each active file its package by longest manifest-dir prefix — the SAME prefix rule the
    // persisted assignment used: the empty-dir root manifest is the catch-all, else the path must
    // equal the dir or continue with `/` after it. Built once on ingest, so `is_external_import`'s
    // per-file lookup stays O(1). The `files` view scopes to the active checkout (#89). Any package
    // row in this scope means the corpus is a Cargo project, so even a file matching no package
    // (left unmapped → global fallback) still marks the scope as having manifests — a bin-only
    // crate suppresses external imports rather than failing open (#4).
    {
        let mut stmt = conn.prepare("SELECT id, path FROM files")?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        for row in rows {
            let (file_id, path) = row?;
            scope.mark_has_manifests();
            let package = packages.iter().enumerate().find_map(|(synthetic_id, (dir, _))| {
                let in_package = dir.is_empty()
                    || path == *dir
                    || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'));
                in_package.then_some(synthetic_id as i64)
            });
            if let Some(synthetic_id) = package {
                scope.set_file_package(file_id, synthetic_id);
            }
        }
    }
    Ok(())
}

/// Read a value from the per-connection `temp.connection_context` (the scope table
/// `install_scope_view` populates). Empty string when absent — a raw test connection without the
/// view, where `refresh_packages`/`add_package` also write the empty scope.
fn scope_context_value(conn: &Connection, key: &str) -> String {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT value FROM temp.connection_context WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or_default()
}

/// Which edges a resolve pass may WRITE (re-resolve / re-synthesize).
///
/// READS always span the full active-checkout `files` scope view (so a target in a shadowed/base
/// file still resolves); this only narrows the SET of source files whose `edges_data` rows the pass
/// mutates. A LINKED-WORKTREE OVERLAY pass must NOT rewrite a SHARED committed (base-scoped) caller
/// file's edges: the base row's `files.id` is the same row the base scope reads, so re-resolving it
/// against the overlay's (shadowed) symbol set would corrupt base `find_callers`/impact until a
/// base pass resolved it back — graph results flipping by whichever worktree refreshed last (#219
/// P1).
#[derive(Clone, Copy)]
pub(crate) enum EdgeWriteScope<'a> {
    /// Every in-view source file (the base/incremental/full-rebuild path: the active scope OWNS its
    /// committed rows, so rewriting them is correct).
    ActiveScope,
    /// Only the linked worktree's OVERLAY rows (`files.worktree_id = id`). Shared committed rows in
    /// the view are read for resolution targets but never written.
    OverlayOnly(&'a str),
}

impl EdgeWriteScope<'_> {
    /// `AND`-able predicate (with a leading space) restricting `files` to the writable source rows,
    /// or empty for `ActiveScope`. Inlined (not bound) because it is appended to several different
    /// SELECTs; the embedded value is a git-derived worktree id, single-quote-escaped defensively.
    fn files_write_predicate(&self) -> String {
        match self {
            EdgeWriteScope::ActiveScope => String::new(),
            EdgeWriteScope::OverlayOnly(worktree_id) => {
                format!(" AND files.worktree_id = '{}'", worktree_id.replace('\'', "''"))
            },
        }
    }
}

/// Re-resolve the ACTIVE CHECKOUT's edges against the ACTIVE CHECKOUT's symbols.
///
/// SCOPE (load-bearing, #89): both SELECTs join `files` — the per-connection scoped TEMP VIEW
/// (`install_scope_view`: overlay rows win, shadowed committed rows and other commits/worktrees
/// are excluded). The DB legitimately holds MULTIPLE scopes at once (a dead commit's rows linger
/// until gc after every HEAD move; a sibling worktree's scope is live permanently; dirty files
/// add overlay rows), and resolving against raw `symbols` made every symbol a duplicate: unique
/// qualified-suffix/name matches demoted to `logical_variant` picking `matches[0]` — an ARBITRARY
/// scope's symbol id, which the active-checkout reads (and the oracle's def-drift gate) then
/// rightly refuse — and cross-scope mixtures went ambiguous → mass-NULLed targets. On a
/// connection without the view (a raw test connection), `files` falls back to `main.files` and
/// resolution is unscoped — production connections always have the view (`set_context` installs
/// it at open/rebuild/incremental).
///
/// Dead scopes' edges are likewise NOT re-resolved (their source file is outside the view): each
/// scope's edges stay self-consistent until gc prunes them or their own worktree's pass rewrites
/// them.
pub(crate) fn resolve_all_edges(conn: &Connection) -> anyhow::Result<()> {
    resolve_edges_with_scope(conn, EdgeWriteScope::ActiveScope)
}

/// Re-resolve ONLY a linked worktree's OVERLAY edges (#219 P1). Resolution targets still span the
/// full active scope (so an overlay edge into a base symbol resolves), but the WRITE set is the
/// worktree's own overlay rows — a SHARED committed caller's `edges_data` is never rewritten by an
/// overlay pass, so the base scope's graph is left intact. Accepted recall trade-off: a BASE
/// caller's edge into an OVERLAY-modified symbol is not re-pointed in the overlay scope (the base
/// row is read-only here); the overlay still serves its own files' edges, and the base resolves its
/// own on its next pass.
pub(crate) fn resolve_overlay_edges(conn: &Connection, worktree_id: &str) -> anyhow::Result<()> {
    resolve_edges_with_scope(conn, EdgeWriteScope::OverlayOnly(worktree_id))
}

fn resolve_edges_with_scope(conn: &Connection, write: EdgeWriteScope<'_>) -> anyhow::Result<()> {
    let symbols = all_symbols(conn)?;
    let index = SymbolIndex::build(&symbols);
    // Per-package + module-aware import scope (#61): the active checkout's Imports edges → per-file
    // module-scoped bindings, plus the per-package local-crate sets, so resolution suppresses a
    // local bind only when the name is `use`d from an external dependency in that reference's
    // module + package. Scoped via the `files` TEMP VIEW like the resolution query below.
    let mut import_scope = imports::ImportScope::new(imports::load_local_roots(conn));
    load_package_roots_into_scope(conn, &mut import_scope)?;
    {
        // The dedicated import-scope columns (NULL on non-import edges); the `use` text rides in
        // `evidence`. Building the per-file module interval set here is once-per-pass, not
        // per-edge.
        let mut stmt = conn.prepare(
            "SELECT d.source_file_id, files.language, tn.value, d.evidence, \
             d.import_scope_start_byte, d.import_scope_end_byte, d.import_mod_id FROM edges_data \
             d JOIN files ON files.id = d.source_file_id JOIN name_strings ek ON ek.id = \
             d.edge_kind_id LEFT JOIN name_strings tn ON tn.id = d.to_name_id WHERE ek.value = \
             'imports'",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let file_id: i64 = row.get(0)?;
            let language: String = row.get(1)?;
            let to_name: Option<String> = row.get(2)?;
            let evidence: Option<String> = row.get(3)?;
            let scope = import_scope_from_row(row.get(4)?, row.get(5)?, row.get(6)?);
            let carries_alias = crate::index::languages::resolver_policy_for_name(Some(&language))
                .is_some_and(|policy| policy.import_edges_carry_aliases());
            if carries_alias {
                // Alias carriers encode `evidence = alias`, `to_name = target`, and a lexical
                // scope. Non-aliased imports have no scope and are ignored by this path.
                if let (Some(alias), Some(target)) = (evidence, to_name) {
                    import_scope.add_import_alias(file_id, alias, target, scope);
                }
            } else if let Some(evidence) = evidence {
                import_scope.add_use(file_id, &evidence, scope);
            }
        }
    }
    import_scope.finalize();
    // Read/write `edges_data` directly (#79): this loop is per-edge hot on every incremental
    // pass, so the strings it needs come from explicit dictionary joins and the verdict UPDATEs
    // write pre-interned ids instead of paying the view triggers' per-row probes. The `files`
    // join is the active-checkout scope (#89) and is unaffected by the interning.
    let mut interner = EdgeStringInterner::default();
    // The WRITE filter (#219 P1): in an overlay pass this restricts the re-resolved (UPDATEd) rows
    // to the worktree's OVERLAY source files, so a shared committed caller's edges are never
    // rewritten; empty for the base/incremental/full-rebuild path.
    let mut stmt = conn.prepare(&format!(
        "SELECT d.id, d.source_file_id, tn.value, tqn.value, ek.value, conf.value, d.evidence, \
         rh.value, d.source_start_byte, files.language FROM edges_data d JOIN files ON files.id = \
         d.source_file_id LEFT JOIN name_strings tn ON tn.id = d.to_name_id LEFT JOIN \
         name_strings tqn ON tqn.id = d.target_qualified_name_id LEFT JOIN name_strings ek ON \
         ek.id = d.edge_kind_id LEFT JOIN name_strings conf ON conf.id = d.confidence_id LEFT \
         JOIN name_strings rh ON rh.id = d.receiver_hint_id WHERE 1 = 1{} ORDER BY d.id",
        write.files_write_predicate(),
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, String>(9)?,
        ))
    })?;
    let rows = rows.collect::<Result<Vec<_>, _>>()?;
    for (
        edge_id,
        source_file_id,
        to_name,
        target_qualified_name,
        edge_kind,
        current_confidence,
        evidence,
        receiver_hint,
        source_start_byte,
        source_language,
    ) in rows
    {
        // #200: a `dispatch_construct` fact's `to_name` is a synthetic `Enum::Variant` key, NOT a
        // real call target — resolving it would bind a bogus `to_symbol_id` to any same-named
        // symbol, and synthesis reads only the fact's `from_symbol_id`. Leave it
        // unresolved. (`dispatch_handle` DOES resolve — synthesis reads its handler
        // `to_symbol_id`.)
        if edge_kind == EdgeKind::DispatchConstruct.as_str() {
            let confidence_id = interner.get(conn, EdgeConfidence::NameOnly.as_str())?;
            let resolution_id = interner.get(conn, "unresolved")?;
            conn.prepare_cached(
                "UPDATE edges_data
                 SET to_symbol_id = NULL, target_start_line = NULL, target_end_line = NULL,
                     confidence_id = ?2, resolution_id = ?3
                 WHERE id = ?1",
            )?
            .execute(params![edge_id, confidence_id, resolution_id])?;
            continue;
        }
        // The reference's byte position drives the module-aware covering test (#61).
        let ref_byte = usize::try_from(source_start_byte).unwrap_or(0);
        // A language-owned import-alias policy may rewrite a reference to its imported target.
        // Imports edges retain their own target name.
        let rebind = if edge_kind == EdgeKind::Imports.as_str() {
            Default::default()
        } else {
            import_alias_rebind(&import_scope, &index, ImportAliasResolveRequest {
                file_id: source_file_id,
                source_language: Some(source_language.as_str()),
                to_name: &to_name,
                target_qualified_name: target_qualified_name.as_deref(),
                receiver_hint: receiver_hint.as_deref(),
                ref_byte,
            })
        };
        let resolve_name = rebind.name.as_deref().unwrap_or(to_name.as_str());
        let resolve_qualified =
            rebind.target_qualified_name.as_deref().or(target_qualified_name.as_deref());
        let resolve_receiver = rebind.receiver_hint.as_deref().or(receiver_hint.as_deref());
        let resolution = resolve_symbol(
            ResolveSymbolRequest {
                name: resolve_name,
                target_qualified_name: resolve_qualified,
                edge_kind: &edge_kind,
                evidence: evidence.as_deref(),
                receiver_hint: resolve_receiver,
                source_file_id,
                source_language: Some(source_language.as_str()),
                imported_external: import_scope.is_external_import(
                    source_file_id,
                    short_name(&to_name),
                    ref_byte,
                ) || import_scope.is_external_qualified_root(
                    source_file_id,
                    target_qualified_name.as_deref(),
                    ref_byte,
                ),
            },
            &index,
        );
        let Some((to_symbol_id, confidence, reason)) = resolution else {
            let suppressed =
                crate::index::languages::resolver_policy_for_name(Some(&source_language))
                    .is_some_and(|policy| {
                        policy.suppress_unresolved_reference(&edge_kind, evidence.as_deref())
                    });
            let confidence = if current_confidence == EdgeConfidence::Ambiguous.as_str() {
                EdgeConfidence::Ambiguous
            } else {
                EdgeConfidence::NameOnly
            };
            // prepare_cached: one UPDATE per edge; cache the statement so the SQL compiles once per
            // connection instead of on every call.
            let confidence_id = interner.get(conn, confidence.as_str())?;
            let resolution_id =
                interner.get(conn, if suppressed { "suppressed" } else { "unresolved" })?;
            conn.prepare_cached(
                "UPDATE edges_data
                 SET to_symbol_id = NULL,
                     target_start_line = NULL,
                     target_end_line = NULL,
                     confidence_id = ?2,
                     resolution_id = ?3
                 WHERE id = ?1",
            )?
            .execute(params![edge_id, confidence_id, resolution_id])?;
            continue;
        };
        let confidence_id = interner.get(conn, confidence.as_str())?;
        let resolution_id = interner.get(conn, reason)?;
        conn.prepare_cached(
            "UPDATE edges_data
             SET to_symbol_id = ?2,
                 confidence_id = ?3,
                 target_start_line = ?4,
                 target_end_line = ?5,
                 resolution_id = ?6
             WHERE id = ?1",
        )?
        .execute(params![
            edge_id,
            to_symbol_id.id,
            confidence_id,
            to_symbol_id.start_line,
            to_symbol_id.end_line,
            resolution_id,
        ])?;
    }
    // #200: now that the dispatch FACT rows are resolved (handlers bound to symbols), synthesize
    // the construct→handler `dispatches` edges. Idempotent over the write scope (#219 P1: an
    // overlay pass synthesizes/clears dispatches ONLY for its own overlay source files).
    synthesize_dispatch_edges(conn, write)?;
    Ok(())
}
/// Full-rebuild fast path: resolve every accumulated edge candidate against an in-memory symbol
/// index and insert it ONCE, already resolved — no unresolved insert, no `all_symbols` SELECT, no
/// per-edge UPDATE pass. The accumulated symbols/edges arrive interned (see [`FullRebuildGraph`]);
/// symbols are hydrated back to owned [`IndexedSymbol`]s here (the arena is frozen by now) and each
/// edge's strings are read as borrowed views into the arena — no per-edge `String` is rebuilt.
/// Symbols carry their real DB ids (assigned in insertion order); we sort by `(qualified_name, id)`
/// to reproduce `all_symbols`' `ORDER BY qualified_name` (tiebreak rowid) exactly, so
/// `resolve_symbol`'s `matches[0]` picks the same symbol as the DB-based path.
pub(crate) fn resolve_and_insert_edges(
    conn: &Connection,
    graph: FullRebuildGraph,
) -> anyhow::Result<()> {
    let (arena, compact_symbols, edges) = graph.into_parts();
    let mut symbols: Vec<IndexedSymbol> =
        compact_symbols.iter().map(|symbol| symbol.hydrate(&arena)).collect();
    drop(compact_symbols);
    symbols.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name).then(a.id.cmp(&b.id)));
    let index = SymbolIndex::build(&symbols);
    crate::index::mem_trace("edges: symbols hydrated + index built, before insert");

    // Drop the edges table's secondary indexes before the bulk insert, then rebuild them in one
    // sorted pass after. Maintaining 5 indexes incrementally across hundreds of thousands of row
    // inserts is the dominant cost here; building each once at the end is far cheaper. Drift-proof:
    // the index DDL is read from the catalog, so a future migration's index is dropped/rebuilt too.
    // Safe because a full rebuild owns the edges table inside the rebuild transaction (WAL).
    let edge_indexes = conn
        .prepare(
            "SELECT name, sql FROM sqlite_master WHERE type = 'index' AND tbl_name = 'edges_data' \
             AND sql IS NOT NULL",
        )?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    for (name, _) in &edge_indexes {
        conn.execute_batch(&format!("DROP INDEX IF EXISTS \"{name}\""))?;
    }

    // Same per-file dedup + skip rules as `insert_candidates`. Edges are accumulated in file order
    // (contiguous by `file_id`) and the dedup is per-file, so we CLEAR `seen` at each file boundary
    // instead of carrying every edge's key for the whole rebuild. That keeps `seen` to one file's
    // edges (a few hundred) rather than ~11M owned-`String` keys held until the loop ends — the
    // dominant resolve-phase structure at kernel scale. Byte-identical: a `file_id` never recurs in
    // a later block, so per-file reset makes exactly the dedup decisions a global per-`file_id` set
    // would; `file_id` therefore drops out of the key (constant within each reset window).
    // Per-package + module-aware import scope (#61): from the accumulated Imports edges, build each
    // file's module-scoped bindings + the per-file module interval set, plus the per-package local-
    // crate sets — so resolution suppresses a bind to a local symbol only when the name comes from
    // an external dependency in that reference's module + package. EXACT PARITY with the DB driver
    // `resolve_all_edges`: identical `add_use(file, use_text, scope)` + `finalize()` +
    // `is_external_*` calls, same fail-open; the only difference is the source (in-memory
    // accumulator vs DB rows).
    let mut import_scope = imports::ImportScope::new(imports::load_local_roots(conn));
    load_package_roots_into_scope(conn, &mut import_scope)?;
    // File languages come from the DB because files without symbols still need their language
    // policy. The incremental driver also joins `files.language`, keeping both drivers in lockstep.
    let file_language: HashMap<i64, String> = {
        let mut stmt = conn.prepare("SELECT id, language FROM files")?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (file_id, candidate) in &edges {
        if candidate.edge_kind == EdgeKind::Imports {
            let evidence = arena.get_opt(candidate.evidence).unwrap_or("");
            let source_language = file_language.get(file_id).map(String::as_str);
            let carries_alias = crate::index::languages::resolver_policy_for_name(source_language)
                .is_some_and(|policy| policy.import_edges_carry_aliases());
            if carries_alias {
                let target = arena.get(candidate.to_name).trim();
                if !evidence.is_empty() && !target.is_empty() {
                    import_scope.add_import_alias(
                        *file_id,
                        evidence.to_string(),
                        target.to_string(),
                        candidate.import_scope_range(),
                    );
                }
            } else {
                import_scope.add_use(*file_id, evidence, candidate.import_scope_range());
            }
        }
    }
    import_scope.finalize();

    let mut seen = BTreeSet::new();
    let mut seen_file_id: Option<i64> = None;
    let mut interner = EdgeStringInterner::default();
    for (file_id, candidate) in &edges {
        if seen_file_id != Some(*file_id) {
            seen.clear();
            seen_file_id = Some(*file_id);
        }
        // Read the interned fields back as borrowed views into the (now frozen) arena — no per-edge
        // `String` is rebuilt. `from_name` stays untrimmed (the dedup key and the stored column use
        // it verbatim); `to_name` is trimmed exactly as the owned path did.
        let from_name = arena.get_opt(candidate.from_name);
        let to_name = arena.get(candidate.to_name).trim();
        if to_name.is_empty() || from_name == Some(to_name) {
            continue;
        }
        let key = (
            candidate.from_symbol_id,
            from_name.map(str::to_string),
            to_name.to_string(),
            candidate.edge_kind,
            i64::from(candidate.source_span.start_byte),
            i64::from(candidate.source_span.end_byte),
        );
        if !seen.insert(key) {
            continue;
        }

        let target_qualified_name = arena.get_opt(candidate.target_qualified_name);
        let evidence = arena.get_opt(candidate.evidence);
        let receiver_hint = arena.get_opt(candidate.receiver_hint);
        // The reference's byte position drives the module-aware covering test (#61) — same input
        // the DB driver reads from `source_start_byte`.
        let ref_byte = candidate.source_span.start_byte as usize;
        // Mirror the DB driver: a language-owned alias policy may rewrite a reference to its
        // imported target. Imports edges keep their own target name.
        let rebind = if candidate.edge_kind == EdgeKind::Imports {
            Default::default()
        } else {
            import_alias_rebind(&import_scope, &index, ImportAliasResolveRequest {
                file_id: *file_id,
                source_language: file_language.get(file_id).map(String::as_str),
                to_name,
                target_qualified_name,
                receiver_hint,
                ref_byte,
            })
        };
        let resolve_name = rebind.name.as_deref().unwrap_or(to_name);
        let resolve_qualified = rebind.target_qualified_name.as_deref().or(target_qualified_name);
        let resolve_receiver = rebind.receiver_hint.as_deref().or(receiver_hint);
        // #200: a `dispatch_construct` fact's `to_name` is a synthetic `Enum::Variant` key, not a
        // real target — never resolve it (synthesis reads only its `from_symbol_id`). Mirrors the
        // incremental driver's skip; `dispatch_handle` DOES resolve (synthesis needs its handler
        // id).
        let resolution = if candidate.edge_kind == EdgeKind::DispatchConstruct {
            None
        } else {
            resolve_symbol(
                ResolveSymbolRequest {
                    name: resolve_name,
                    target_qualified_name: resolve_qualified,
                    edge_kind: candidate.edge_kind.as_str(),
                    evidence,
                    receiver_hint: resolve_receiver,
                    source_file_id: *file_id,
                    source_language: file_language.get(file_id).map(String::as_str),
                    imported_external: import_scope.is_external_import(
                        *file_id,
                        short_name(to_name),
                        ref_byte,
                    ) || import_scope.is_external_qualified_root(
                        *file_id,
                        target_qualified_name,
                        ref_byte,
                    ),
                },
                &index,
            )
        };
        let (to_symbol_id, confidence, target_start_line, target_end_line, reason) =
            match resolution {
                Some((symbol, confidence, reason)) => (
                    Some(symbol.id),
                    confidence,
                    Some(symbol.start_line),
                    Some(symbol.end_line),
                    reason,
                ),
                None => {
                    let suppressed = crate::index::languages::resolver_policy_for_name(
                        file_language.get(file_id).map(String::as_str),
                    )
                    .is_some_and(|policy| {
                        policy.suppress_unresolved_reference(candidate.edge_kind.as_str(), evidence)
                    });
                    let confidence = if candidate.confidence == EdgeConfidence::Ambiguous {
                        EdgeConfidence::Ambiguous
                    } else {
                        EdgeConfidence::NameOnly
                    };
                    (
                        None,
                        confidence,
                        None,
                        None,
                        if suppressed { "suppressed" } else { "unresolved" },
                    )
                },
            };
        // NULL when the sentinel marks an absent callee range; see
        // `CompactEdge::callee_byte_columns`.
        let (callee_start_byte, callee_end_byte) = candidate.callee_byte_columns();
        // NULL on non-import edges; the dedicated import-scope columns (#61).
        let (import_scope_start_byte, import_scope_end_byte, import_mod_id) =
            candidate.import_scope_columns();
        // Interned ids straight into edges_data (#79); the memo keeps repeated names to one map
        // probe, so the bulk path writes pure integers.
        let from_name_id = interner.get_opt(conn, from_name)?;
        let to_name_id = interner.get(conn, to_name)?;
        let target_qualified_name_id = interner.get_opt(conn, target_qualified_name)?;
        let receiver_hint_id = interner.get_opt(conn, receiver_hint)?;
        let edge_kind_id = interner.get(conn, candidate.edge_kind.as_str())?;
        let confidence_id = interner.get(conn, confidence.as_str())?;
        let resolution_id = interner.get(conn, reason)?;
        conn.prepare_cached(
            "
            INSERT INTO edges_data(
                source_file_id, from_symbol_id, from_name_id, to_name_id,
                target_qualified_name_id, evidence, receiver_hint_id,
                source_start_line, source_end_line, source_start_byte, source_end_byte,
                callee_start_byte, callee_end_byte,
                import_scope_start_byte, import_scope_end_byte, import_mod_id,
                edge_kind_id, confidence_id,
                to_symbol_id, target_start_line, target_end_line, resolution_id
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19, ?20, ?21, ?22)
            ",
        )?
        .execute(params![
            file_id,
            candidate.from_symbol_id,
            from_name_id,
            to_name_id,
            target_qualified_name_id,
            evidence,
            receiver_hint_id,
            i64::from(candidate.source_span.start_line),
            i64::from(candidate.source_span.end_line),
            i64::from(candidate.source_span.start_byte),
            i64::from(candidate.source_span.end_byte),
            callee_start_byte,
            callee_end_byte,
            import_scope_start_byte,
            import_scope_end_byte,
            import_mod_id,
            edge_kind_id,
            confidence_id,
            to_symbol_id,
            target_start_line,
            target_end_line,
            resolution_id,
        ])?;
    }
    crate::index::mem_trace("edges: inserted, before index rebuild");

    // Rebuild the indexes we dropped, each in one bulk sorted pass over the now-populated table.
    for (_, sql) in &edge_indexes {
        conn.execute_batch(sql)?;
    }
    crate::index::mem_trace("edges: after index rebuild");
    // #200: synthesize construct→handler `dispatches` edges from the resolved fact rows. Runs after
    // the index rebuild — the few extra inserts maintain the (now rebuilt) indexes incrementally.
    // A full rebuild owns its whole scope, so it synthesizes over the active scope.
    synthesize_dispatch_edges(conn, EdgeWriteScope::ActiveScope)?;
    Ok(())
}

pub(crate) fn resolve_symbol<'a>(
    request: ResolveSymbolRequest<'_>,
    index: &SymbolIndex<'a>,
) -> Option<(&'a IndexedSymbol, EdgeConfidence, &'static str)> {
    let kind_matches = |symbol: &IndexedSymbol| {
        (request.edge_kind != EdgeKind::UsesMacro.as_str() || symbol.kind == "macro")
            && crate::index::languages::target_matches_policy(
                request.source_language,
                request.edge_kind,
                &symbol.language,
                &symbol.kind,
            )
    };
    // Crate-aware import suppression (#61 Project B): the name is `use`d from an external
    // dependency crate, so it denotes that dependency's item — never a local same-named symbol.
    // Leave it unresolved (the SCIP oracle bins it `resolved-external`). This kills the
    // external-dep collisions (`Url` → local instead of the `url` crate) WITHOUT touching
    // correct cross-crate binds into local workspace crates (those roots are in the local-crate
    // set, so not external).
    //
    let policy = crate::index::languages::resolver_policy_for_name(request.source_language);
    // A language-owned local qualifier overrides an external bare-leaf import.
    if request.imported_external
        && !request.target_qualified_name.and_then(|path| path.split_once("::")).is_some_and(
            |(root, _)| policy.is_some_and(|policy| policy.is_local_qualified_root(root)),
        )
    {
        return None;
    }
    if policy
        .is_some_and(|policy| policy.reference_is_unresolvable(request.edge_kind, request.name))
    {
        return None;
    }
    if let Some(qualified) = request.target_qualified_name.filter(|value| !value.is_empty()) {
        // Semantic SCOPE-PATH match first (#61). An edge's `target_qualified_name` is a source-code
        // path (`Workspace::new`), which aligns with a symbol's `scope_path`
        // (`core::Workspace::new`) — NOT with the file-path `qualified_name` below, which a
        // source path never equals. Exact hit → `Exact`; a unique enclosing-scope suffix
        // match → `Syntactic`. This is what lets the strong qualified path fire for
        // methods/nested items instead of collapsing to bare-name matching. On ambiguity,
        // fall through rather than guess.
        //
        // `scope_path` is NOT file-unique (a workspace with two crates each declaring
        // `mod core { impl Workspace { fn new } }` has two symbols with scope_path
        // `core::Workspace::new`), so an exact hit may be ambiguous — DON'T stamp the first one
        // `Exact`. Bind only a unique hit (or one logical symbol's variants); otherwise fall
        // through to the file-path/bare-name logic, which is itself ambiguity-aware.
        let scope_exact = index
            .by_scope_path
            .get(qualified)
            .into_iter()
            .flatten()
            .copied()
            .filter(|symbol| kind_matches(symbol))
            .collect::<Vec<_>>();
        match scope_exact.as_slice() {
            [symbol] => return Some((*symbol, EdgeConfidence::Exact, "scope_exact")),
            [_, ..] if same_logical_symbol(&scope_exact) =>
                return Some((scope_exact[0], EdgeConfidence::Syntactic, "logical_variant")),
            _ => {},
        }
        let scope_suffix = format!("::{qualified}");
        let scope_matches = index
            .by_name
            .get(qn_tail(qualified))
            .into_iter()
            .flatten()
            .copied()
            .filter(|symbol| kind_matches(symbol) && symbol.scope_path.ends_with(&scope_suffix))
            .collect::<Vec<_>>();
        match scope_matches.as_slice() {
            [symbol] => return Some((*symbol, EdgeConfidence::Syntactic, "scope_suffix")),
            [_, ..] if same_logical_symbol(&scope_matches) =>
                return Some((scope_matches[0], EdgeConfidence::Syntactic, "logical_variant")),
            _ => {},
        }
        // Exact qualified-name match (bucket entries already share `qualified_name == qualified`).
        if let Some(symbol) = index
            .by_qualified
            .get(qualified)
            .into_iter()
            .flatten()
            .copied()
            .find(|symbol| kind_matches(symbol))
        {
            return Some((symbol, EdgeConfidence::Exact, "exact"));
        }
        let suffix = format!("::{qualified}");
        let matches = index
            .by_qn_tail
            .get(qn_tail(qualified))
            .into_iter()
            .flatten()
            .copied()
            .filter(|symbol| kind_matches(symbol) && symbol.qualified_name.ends_with(&suffix))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [symbol] => return Some((*symbol, EdgeConfidence::Syntactic, "qualified_suffix")),
            [_, ..] if same_logical_symbol(&matches) => {
                return Some((matches[0], EdgeConfidence::Syntactic, "logical_variant"));
            },
            [_, ..] => return None,
            [] => {},
        }
        if !allow_unqualified_fallback(
            request.edge_kind,
            qualified,
            request.name,
            request.evidence,
            request.receiver_hint,
            request.source_language,
        ) {
            return None;
        }
    }
    let short = short_name(request.name);
    let matches = index
        .by_name
        .get(short)
        .into_iter()
        .flatten()
        .copied()
        .filter(|symbol| kind_matches(symbol))
        .collect::<Vec<_>>();
    let preferred = preferred_matches(request.edge_kind, request.source_language, &matches);
    // Language policy decides whether a type-position reference may bind a value declaration.
    if preferred.is_empty()
        && request.edge_kind == EdgeKind::ReferencesType.as_str()
        && policy.is_some_and(|policy| policy.type_reference_requires_type_definition())
    {
        return None;
    }
    if preferred.is_empty()
        && crate::index::languages::requires_same_language_target(
            request.source_language,
            request.edge_kind,
        )
    {
        return None;
    }
    let matches = if preferred.is_empty() { matches.as_slice() } else { preferred.as_slice() };
    match matches {
        [symbol] => Some((*symbol, EdgeConfidence::Syntactic, "target_name_fallback")),
        [_, ..] => {
            if same_logical_symbol(matches) {
                return Some((matches[0], EdgeConfidence::Syntactic, "logical_variant"));
            }
            let same_file = matches
                .iter()
                .copied()
                .filter(|symbol| symbol.file_id == request.source_file_id)
                .collect::<Vec<_>>();
            match same_file.as_slice() {
                [symbol] => Some((*symbol, EdgeConfidence::Syntactic, "same_file_name")),
                [_, ..] if same_logical_symbol(&same_file) =>
                    Some((same_file[0], EdgeConfidence::Syntactic, "logical_variant")),
                _ => None,
            }
        },
        [] => None,
    }
}
/// Whether a set of same-name candidate symbols are all the SAME logical symbol — so the resolver
/// may pick `matches[0]` and label it `Syntactic` (`logical_variant`) instead of bailing on
/// ambiguity. This must hold ONLY for genuine variants of one item (e.g. a forward declaration +
/// its definition, or `#[cfg]`-gated copies that share a scope), never for distinct items that
/// merely share a name.
///
/// `qualified_name` is `{file_path}::{name}` — NOT unique within a file: one file can declare many
/// distinct same-named items (e.g. serde's `impl Visitor for A { type Value }` /
/// `impl Visitor for B { type Value }`, 31 `Value` rows in one file across 10 impls). Grouping
/// those by `qualified_name` alone made the resolver assert one arbitrary pick at `Syntactic`,
/// which the SCIP oracle then counted as a `Contradict` ~93% of the time — tanking Rust precision
/// on trait/generic-heavy crates. Requiring an equal `scope_path` (which carries the enclosing
/// impl/trait/module chain, e.g. `Visitor<'de>::Value`) splits those distinct items apart while
/// keeping true variants — which share a scope — grouped.
pub(crate) fn same_logical_symbol(symbols: &[&IndexedSymbol]) -> bool {
    let Some(first) = symbols.first() else {
        return false;
    };
    // A language policy can preserve ambiguity when stored identity does not distinguish distinct
    // declarations. Languages without that restriction retain variant collapsing.
    if first
        .language
        .parse::<Language>()
        .ok()
        .and_then(crate::index::languages::resolver_policy)
        .is_some_and(|policy| !policy.collapse_same_named_declarations())
    {
        return false;
    }
    symbols.iter().all(|symbol| {
        symbol.qualified_name == first.qualified_name
            && symbol.name == first.name
            && symbol.kind == first.kind
            && symbol.scope_path == first.scope_path
    })
}
pub(crate) fn allow_unqualified_fallback(
    edge_kind: &str,
    qualified: &str,
    name: &str,
    evidence: Option<&str>,
    receiver_hint: Option<&str>,
    source_language: Option<&str>,
) -> bool {
    if edge_kind == EdgeKind::UsesMacro.as_str() {
        return false;
    }
    let target = short_name(name);
    let qualifier = qualified
        .rsplit_once("::")
        .map(|(qualifier, _)| qualifier)
        .unwrap_or(qualified)
        .split("::")
        .next()
        .unwrap_or_default();
    let policy = crate::index::languages::resolver_policy_for_name(source_language);
    if policy.is_some_and(|policy| policy.is_local_qualified_root(qualifier)) {
        return true;
    }
    if receiver_hint
        .is_some_and(|receiver| looks_like_type_name(receiver) && !is_common_member_name(target))
        && policy.is_some_and(|policy| policy.allow_type_receiver_fallback())
    {
        return true;
    }
    if receiver_hint.is_some_and(|receiver| !matches!(receiver, "self" | "Self"))
        && evidence.is_some_and(|value| value.contains('.'))
    {
        return policy.is_some_and(|policy| policy.allow_value_receiver_fallback())
            && !is_common_member_name(target);
    }
    if policy.is_some_and(|policy| policy.rejects_qualified_root(qualifier)) {
        return false;
    }
    if looks_like_type_name(qualifier) && is_common_member_name(target) {
        return false;
    }
    true
}
pub(crate) fn is_common_member_name(value: &str) -> bool {
    matches!(
        value,
        "new"
            | "default"
            | "clone"
            | "to_string"
            | "into"
            | "from"
            | "as_ref"
            | "as_mut"
            | "iter"
            | "map"
            | "collect"
            | "build"
            | "unwrap"
            | "expect"
            | "ok"
            | "err"
    )
}
pub(crate) fn preferred_matches<'a>(
    edge_kind: &str,
    source_language: Option<&str>,
    matches: &[&'a IndexedSymbol],
) -> Vec<&'a IndexedSymbol> {
    let generic_preferred_kinds: &[&str] = match edge_kind {
        // `dispatch_handle` (#200) is a call to the handler the match arm delegates to — resolve it
        // with the SAME callable preference as a direct call, so a same-named type/const never wins
        // over the handler function/method.
        "calls_name" | "dispatch_handle" => &["function", "method"],
        "constructs" => &["struct", "class", "object"],
        "uses_macro" => &["macro"],
        "implements" => &["trait", "interface"],
        "references_type" => &["struct", "enum", "trait", "type", "class", "interface", "object"],
        _ => &[],
    };
    let source_language = source_language.and_then(|name| name.parse::<Language>().ok());
    let language_preference = source_language
        .and_then(crate::index::languages::resolver_policy)
        .and_then(|policy| policy.preferred_kinds(edge_kind));
    let preferred_kinds = language_preference
        .as_ref()
        .map_or(generic_preferred_kinds, |preference| preference.symbol_kinds);
    if preferred_kinds.is_empty() {
        return Vec::new();
    }
    let preferred = matches
        .iter()
        .copied()
        .filter(|symbol| preferred_kinds.contains(&symbol.kind.as_str()))
        .collect::<Vec<_>>();
    if let (Some(source_language), Some(language_preference)) =
        (source_language, language_preference)
    {
        let same_language = preferred
            .iter()
            .copied()
            .filter(|symbol| symbol.language == source_language.as_str())
            .collect::<Vec<_>>();
        if !same_language.is_empty() {
            return same_language;
        }
        if language_preference.same_language_only {
            return Vec::new();
        }
    }
    preferred
}

#[cfg(test)]
mod tests;
