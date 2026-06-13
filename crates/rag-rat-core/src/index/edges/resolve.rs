use super::*;

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
    let symbols = all_symbols(conn)?;
    let index = SymbolIndex::build(&symbols);
    // Crate-aware import scope (#61 Project B): the active checkout's Imports edges → per-file
    // {leaf name → crate root}, so resolution suppresses a local bind when the name is `use`d from
    // an external dependency. Scoped via the `files` TEMP VIEW like the resolution query below.
    let mut import_scope = imports::ImportScope::new(imports::load_local_roots(conn));
    {
        let mut stmt = conn.prepare(
            "SELECT d.source_file_id, d.evidence FROM edges_data d JOIN files ON files.id = \
             d.source_file_id JOIN edge_strings ek ON ek.id = d.edge_kind_id WHERE ek.value = \
             'imports'",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let file_id: i64 = row.get(0)?;
            let evidence: Option<String> = row.get(1)?;
            if let Some(evidence) = evidence {
                import_scope.add_use(file_id, &evidence);
            }
        }
    }
    // Read/write `edges_data` directly (#79): this loop is per-edge hot on every incremental
    // pass, so the strings it needs come from explicit dictionary joins and the verdict UPDATEs
    // write pre-interned ids instead of paying the view triggers' per-row probes. The `files`
    // join is the active-checkout scope (#89) and is unaffected by the interning.
    let mut interner = EdgeStringInterner::default();
    let mut stmt = conn.prepare(
        "SELECT d.id, d.source_file_id, tn.value, tqn.value, ek.value, conf.value, d.evidence, \
         rh.value FROM edges_data d JOIN files ON files.id = d.source_file_id LEFT JOIN \
         edge_strings tn ON tn.id = d.to_name_id LEFT JOIN edge_strings tqn ON tqn.id = \
         d.target_qualified_name_id LEFT JOIN edge_strings ek ON ek.id = d.edge_kind_id LEFT JOIN \
         edge_strings conf ON conf.id = d.confidence_id LEFT JOIN edge_strings rh ON rh.id = \
         d.receiver_hint_id ORDER BY d.id",
    )?;
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
    ) in rows
    {
        let resolution = resolve_symbol(
            ResolveSymbolRequest {
                name: &to_name,
                target_qualified_name: target_qualified_name.as_deref(),
                edge_kind: &edge_kind,
                evidence: evidence.as_deref(),
                receiver_hint: receiver_hint.as_deref(),
                source_file_id,
                source_language: index.file_language.get(&source_file_id).copied(),
                imported_external: import_scope
                    .is_external_import(source_file_id, short_name(&to_name))
                    || import_scope.is_external_qualified_root(
                        source_file_id,
                        target_qualified_name.as_deref(),
                    ),
            },
            &index,
        );
        let Some((to_symbol_id, confidence, reason)) = resolution else {
            let confidence = if current_confidence == EdgeConfidence::Ambiguous.as_str() {
                EdgeConfidence::Ambiguous
            } else {
                EdgeConfidence::NameOnly
            };
            // prepare_cached: one UPDATE per edge; cache the statement so the SQL compiles once per
            // connection instead of on every call.
            let confidence_id = interner.get(conn, confidence.as_str())?;
            let resolution_id = interner.get(conn, "unresolved")?;
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
    // Crate-aware import scope (#61 Project B): map each file's `use`d leaf names to their crate
    // root from the accumulated Imports edges, so resolution can suppress a bind to a local symbol
    // when the name actually comes from an external dependency crate.
    let mut import_scope = imports::ImportScope::new(imports::load_local_roots(conn));
    for (file_id, candidate) in &edges {
        if candidate.edge_kind == EdgeKind::Imports
            && let Some(evidence) = arena.get_opt(candidate.evidence)
        {
            import_scope.add_use(*file_id, evidence);
        }
    }

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
        let resolution = resolve_symbol(
            ResolveSymbolRequest {
                name: to_name,
                target_qualified_name,
                edge_kind: candidate.edge_kind.as_str(),
                evidence,
                receiver_hint,
                source_file_id: *file_id,
                source_language: index.file_language.get(file_id).copied(),
                imported_external: import_scope.is_external_import(*file_id, short_name(to_name))
                    || import_scope.is_external_qualified_root(*file_id, target_qualified_name),
            },
            &index,
        );
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
                    let confidence = if candidate.confidence == EdgeConfidence::Ambiguous {
                        EdgeConfidence::Ambiguous
                    } else {
                        EdgeConfidence::NameOnly
                    };
                    (None, confidence, None, None, "unresolved")
                },
            };
        // NULL when the sentinel marks an absent callee range; see
        // `CompactEdge::callee_byte_columns`.
        let (callee_start_byte, callee_end_byte) = candidate.callee_byte_columns();
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
                edge_kind_id, confidence_id,
                to_symbol_id, target_start_line, target_end_line, resolution_id
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19)
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
    Ok(())
}

pub(crate) fn resolve_symbol<'a>(
    request: ResolveSymbolRequest<'_>,
    index: &SymbolIndex<'a>,
) -> Option<(&'a IndexedSymbol, EdgeConfidence, &'static str)> {
    let kind_matches = |symbol: &IndexedSymbol| {
        request.edge_kind != EdgeKind::UsesMacro.as_str() || symbol.kind == "macro"
    };
    // Crate-aware import suppression (#61 Project B): the name is `use`d from an external
    // dependency crate, so it denotes that dependency's item — never a local same-named symbol.
    // Leave it unresolved (the SCIP oracle bins it `resolved-external`). This kills the
    // external-dep collisions (`Url` → local instead of the `url` crate) WITHOUT touching
    // correct cross-crate binds into local workspace crates (those roots are in the local-crate
    // set, so not external).
    //
    // EXCEPTION: an explicitly LOCAL-qualified reference (`crate::Url`, `self::Url`, `super::Url`)
    // names a local item by construction — the qualifier overrides the bare-leaf import. Don't
    // suppress it just because the file also imports a same-named external item; fall through so
    // the qualified path can bind.
    if request.imported_external && !targets_local_qualified_path(request.target_qualified_name) {
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
    let preferred = preferred_matches(request.edge_kind, &matches);
    // In languages with separate type/value namespaces (Rust, C, C++), a `references_type`
    // reference must resolve to a type DEFINITION (struct/enum/trait/type/…). If none of the
    // same-named candidates is one, do NOT fall back to a non-type symbol (an `impl` block, a
    // module, a function/const/macro) — leave it unresolved, so the graph never points a type
    // reference at a non-type. Those fallbacks were pure contradictions (#61): when a type's real
    // definition is external / in another crate, the only in-corpus same-named symbol is often an
    // `impl Foo` or a module, and binding to it is always wrong. NOT applied to TS/Kotlin, where a
    // type-position reference legitimately targets a value (a React component `const`/`function`).
    if preferred.is_empty()
        && request.edge_kind == EdgeKind::ReferencesType.as_str()
        && matches!(request.source_language, Some("rust" | "c" | "cpp"))
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
pub(crate) fn same_logical_symbol(symbols: &[&IndexedSymbol]) -> bool {
    let Some(first) = symbols.first() else {
        return false;
    };
    symbols.iter().all(|symbol| {
        symbol.qualified_name == first.qualified_name
            && symbol.name == first.name
            && symbol.kind == first.kind
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
    if matches!(qualifier, "crate" | "self" | "super") {
        return true;
    }
    if receiver_hint
        .is_some_and(|receiver| looks_like_type_name(receiver) && !is_common_member_name(target))
        && matches!(source_language, Some("rust" | "kotlin"))
    {
        return true;
    }
    if receiver_hint.is_some_and(|receiver| !matches!(receiver, "self" | "Self"))
        && evidence.is_some_and(|value| value.contains('.'))
    {
        return source_language == Some(Language::Kotlin.as_str())
            && !is_common_member_name(target);
    }
    if is_external_rust_root(qualifier) {
        return false;
    }
    if looks_like_type_name(qualifier) && is_common_member_name(target) {
        return false;
    }
    true
}
/// Whether an edge's `target_qualified_name` is an explicitly LOCAL-rooted path (`crate::…`,
/// `self::…`, `super::…`) — code disambiguating a local item with a qualifier. Such a reference is
/// exempt from crate-aware import suppression (#61 Project B) even when the file also imports a
/// same-named external item.
fn targets_local_qualified_path(target_qualified_name: Option<&str>) -> bool {
    target_qualified_name
        .and_then(|path| path.split_once("::"))
        .is_some_and(|(root, _)| matches!(root, "crate" | "self" | "super"))
}
pub(crate) fn is_external_rust_root(value: &str) -> bool {
    matches!(
        value,
        "std"
            | "core"
            | "alloc"
            | "tokio"
            | "serde"
            | "serde_json"
            | "anyhow"
            | "thiserror"
            | "rusqlite"
            | "tree_sitter"
            | "tracing"
            | "log"
            | "Vec"
            | "String"
            | "Option"
            | "Result"
            | "HashMap"
            | "BTreeMap"
            | "HashSet"
            | "BTreeSet"
    )
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
    matches: &[&'a IndexedSymbol],
) -> Vec<&'a IndexedSymbol> {
    let preferred_kinds: &[&str] = match edge_kind {
        "calls_name" => &["function", "method"],
        "constructs" => &["struct", "class", "object"],
        "uses_macro" => &["macro"],
        "implements" => &["trait", "interface"],
        "references_type" => &["struct", "enum", "trait", "type", "class", "interface", "object"],
        _ => &[],
    };
    if preferred_kinds.is_empty() {
        return Vec::new();
    }
    matches
        .iter()
        .copied()
        .filter(|symbol| preferred_kinds.contains(&symbol.kind.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::*;
    use crate::index::schema;

    const NEW: &str = "newcommitsha";
    const OLD: &str = "oldcommitsha";

    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn add_file(conn: &Connection, path: &str, commit: &str) -> i64 {
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id) VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, '')",
            params![path, format!("sha-{commit}-{path}"), commit],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn add_symbol(conn: &Connection, file_id: i64, name: &str, qualified: &str) -> i64 {
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, \
             end_byte, start_line, end_line) VALUES (?1, 'rust', ?2, ?3, 'function', 0, 10, 1, 1)",
            params![file_id, name, qualified],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn add_edge(
        conn: &Connection,
        source_file_id: i64,
        to_name: &str,
        target_qualified_name: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO edges(source_file_id, to_name, target_qualified_name, edge_kind, \
             confidence, resolution) VALUES (?1, ?2, ?3, 'calls_name', 'NameOnly', 'unresolved')",
            params![source_file_id, to_name, target_qualified_name],
        )
        .unwrap();
        // `edges` is a view; `last_insert_rowid` does not survive its INSTEAD OF trigger (#79).
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
    }

    fn edge_state(conn: &Connection, edge_id: i64) -> (Option<i64>, String, String) {
        conn.query_row(
            "SELECT to_symbol_id, confidence, resolution FROM edges WHERE id = ?1",
            params![edge_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    }

    /// The #89 regression: with a DEAD scope's rows still in the DB (post-HEAD-move before gc, or
    /// a sibling worktree's live scope), resolution must behave exactly as in a single-scope DB —
    /// unique qualified-suffix matches stay `qualified_suffix` (not demoted to `logical_variant`
    /// picking an arbitrary scope's copy), and the target id belongs to the ACTIVE scope.
    #[test]
    fn resolution_is_scoped_to_the_active_checkout() {
        let conn = seeded_conn();
        // Active scope NEW + dead scope OLD, same corpus shape in both.
        let caller_new = add_file(&conn, "a.rs", NEW);
        let defs_new = add_file(&conn, "b.rs", NEW);
        let caller_old = add_file(&conn, "a.rs", OLD);
        let defs_old = add_file(&conn, "b.rs", OLD);
        let target_new = add_symbol(&conn, defs_new, "target", "crate::b::target");
        let target_old = add_symbol(&conn, defs_old, "target", "crate::b::target");
        add_symbol(&conn, caller_new, "caller", "crate::a::caller");
        add_symbol(&conn, caller_old, "caller", "crate::a::caller");

        // The suffix-shaped qualified target exercises the by_qn_tail arm (the one duplicates
        // demote): `b::target` matches `crate::b::target` by suffix.
        let edge_new = add_edge(&conn, caller_new, "b::target", "b::target");
        // The dead scope's own edge: pre-resolved to its own scope's symbol; must stay untouched.
        let edge_old = add_edge(&conn, caller_old, "b::target", "b::target");
        conn.execute(
            "UPDATE edges SET to_symbol_id = ?2, confidence = 'Syntactic', resolution = \
             'qualified_suffix' WHERE id = ?1",
            params![edge_old, target_old],
        )
        .unwrap();

        crate::index::install_scope_view(&conn, NEW, "").unwrap();
        resolve_all_edges(&conn).unwrap();

        let (to, confidence, resolution) = edge_state(&conn, edge_new);
        assert_eq!(
            to,
            Some(target_new),
            "the active edge must resolve to the ACTIVE scope's symbol, not an arbitrary copy"
        );
        assert_eq!(confidence, "Syntactic");
        assert_eq!(
            resolution, "qualified_suffix",
            "a unique in-scope suffix match must not demote to logical_variant"
        );

        let (to, _, resolution) = edge_state(&conn, edge_old);
        assert_eq!(to, Some(target_old), "the dead scope's edge is left untouched");
        assert_eq!(resolution, "qualified_suffix");
    }

    /// A dirty-worktree overlay shadows the committed row: resolution must target the OVERLAY's
    /// symbols (the active content), not the shadowed committed copy.
    #[test]
    fn resolution_prefers_overlay_over_shadowed_committed_rows() {
        let conn = seeded_conn();
        let caller = add_file(&conn, "a.rs", NEW);
        let defs_committed = add_file(&conn, "b.rs", NEW);
        // Overlay row for b.rs (dirty file): commit_sha empty, worktree id set.
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id) VALUES ('b.rs', 'rust', 'source', 'sha-overlay', 0, 0, '', \
             '/wt')",
            [],
        )
        .unwrap();
        let defs_overlay = conn.last_insert_rowid();
        add_symbol(&conn, defs_committed, "target", "crate::b::target");
        let target_overlay = add_symbol(&conn, defs_overlay, "target", "crate::b::target");
        add_symbol(&conn, caller, "caller", "crate::a::caller");
        let edge = add_edge(&conn, caller, "b::target", "b::target");

        crate::index::install_scope_view(&conn, NEW, "/wt").unwrap();
        resolve_all_edges(&conn).unwrap();

        let (to, _, resolution) = edge_state(&conn, edge);
        assert_eq!(to, Some(target_overlay), "overlay symbols win over shadowed committed rows");
        assert_eq!(resolution, "qualified_suffix");
    }

    fn add_symbol_kind(
        conn: &Connection,
        file_id: i64,
        name: &str,
        qualified: &str,
        kind: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, \
             end_byte, start_line, end_line) VALUES (?1, 'rust', ?2, ?3, ?4, 0, 10, 1, 1)",
            params![file_id, name, qualified, kind],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn add_type_ref_edge(conn: &Connection, source_file_id: i64, to_name: &str) -> i64 {
        conn.execute(
            "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution) VALUES \
             (?1, ?2, 'references_type', 'NameOnly', 'unresolved')",
            params![source_file_id, to_name],
        )
        .unwrap();
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
    }

    /// #61: a `references_type` reference resolves only to a type DEFINITION. When the sole
    /// same-named in-corpus symbol is a non-type (an `impl` block — the type's real definition is
    /// external / in another crate), the edge stays UNRESOLVED rather than binding to the non-type.
    /// A real type definition still resolves.
    #[test]
    fn references_type_does_not_resolve_to_a_non_type_symbol() {
        let conn = seeded_conn();
        let user = add_file(&conn, "a.rs", NEW);
        let defs = add_file(&conn, "b.rs", NEW);
        // A symbol in the source file so the index knows it's Rust (source_language drives the
        // type/value-namespace strictness; a real source file always has at least the caller).
        add_symbol(&conn, user, "user_fn", "crate::a::user_fn");
        // Only same-named candidate for `Widget` is an impl block (no struct/enum/trait in-corpus).
        add_symbol_kind(&conn, defs, "Widget", "crate::b::Widget", "impl");
        // A genuine type definition under a different name (the positive control).
        let gadget = add_symbol_kind(&conn, defs, "Gadget", "crate::b::Gadget", "struct");
        let ref_impl = add_type_ref_edge(&conn, user, "Widget");
        let ref_struct = add_type_ref_edge(&conn, user, "Gadget");

        crate::index::install_scope_view(&conn, NEW, "").unwrap();
        resolve_all_edges(&conn).unwrap();

        let (to, _, resolution) = edge_state(&conn, ref_impl);
        assert_eq!(to, None, "a type reference must not bind to an impl block");
        assert_eq!(resolution, "unresolved");

        let (to, _, _) = edge_state(&conn, ref_struct);
        assert_eq!(to, Some(gadget), "a type reference still resolves to a struct definition");
    }

    fn add_symbol_scope(
        conn: &Connection,
        file_id: i64,
        name: &str,
        qualified: &str,
        scope_path: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name, scope_path, kind, \
             start_byte, end_byte, start_line, end_line) VALUES (?1, 'rust', ?2, ?3, ?4, \
             'function', 0, 10, 1, 1)",
            params![file_id, name, qualified, scope_path],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// #61: `scope_path` is NOT file-unique. When two symbols in different files share a scope_path,
    /// an exact scope match is AMBIGUOUS and must NOT bind one as `Exact` — it falls through.
    #[test]
    fn scope_exact_does_not_bind_an_ambiguous_scope_path() {
        let conn = seeded_conn();
        let f1 = add_file(&conn, "a.rs", NEW);
        let f2 = add_file(&conn, "b.rs", NEW);
        let caller = add_file(&conn, "c.rs", NEW);
        // Two distinct symbols sharing the SAME scope_path (a multi-crate same-name collision).
        add_symbol_scope(&conn, f1, "build", "a.rs::build", "core::Builder::build");
        add_symbol_scope(&conn, f2, "build", "b.rs::build", "core::Builder::build");
        let edge = add_edge(&conn, caller, "build", "core::Builder::build");

        crate::index::install_scope_view(&conn, NEW, "").unwrap();
        resolve_all_edges(&conn).unwrap();

        let (to, _, resolution) = edge_state(&conn, edge);
        assert_eq!(to, None, "an ambiguous scope_path must not silently bind one at Exact");
        assert_eq!(resolution, "unresolved");
    }

    /// The positive control: a UNIQUE scope_path binds `Exact` via `scope_exact`.
    #[test]
    fn scope_exact_binds_a_unique_scope_path() {
        let conn = seeded_conn();
        let defs = add_file(&conn, "b.rs", NEW);
        let caller = add_file(&conn, "c.rs", NEW);
        let target = add_symbol_scope(&conn, defs, "build", "b.rs::build", "core::Builder::build");
        let edge = add_edge(&conn, caller, "build", "core::Builder::build");

        crate::index::install_scope_view(&conn, NEW, "").unwrap();
        resolve_all_edges(&conn).unwrap();

        let (to, _, resolution) = edge_state(&conn, edge);
        assert_eq!(to, Some(target));
        assert_eq!(resolution, "scope_exact");
    }

    fn set_local_crate_roots(conn: &Connection, roots: &str) {
        conn.execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES ('local_crate_roots', ?1)",
            params![roots],
        )
        .unwrap();
    }

    fn add_import_edge(conn: &Connection, source_file_id: i64, to_name: &str, evidence: &str) {
        conn.execute(
            "INSERT INTO edges(source_file_id, to_name, target_qualified_name, edge_kind, \
             confidence, resolution, evidence) VALUES (?1, ?2, '', 'imports', 'NameOnly', \
             'unresolved', ?3)",
            params![source_file_id, to_name, evidence],
        )
        .unwrap();
    }

    /// #61 Project B: a bare reference to a name `use`d from an EXTERNAL crate (`url::Url`) must not
    /// bind to a local same-named symbol — but an explicitly LOCAL-qualified `crate::Url` reference
    /// in the same file still must (the qualifier overrides the import; Codex review
    /// resolve.rs:334).
    #[test]
    fn external_import_suppresses_bare_but_not_locally_qualified() {
        let conn = seeded_conn();
        set_local_crate_roots(&conn, "mycrate");
        let user = add_file(&conn, "a.rs", NEW);
        let defs = add_file(&conn, "b.rs", NEW);
        let local = add_symbol(&conn, defs, "Url", "crate::b::Url");
        add_import_edge(&conn, user, "Url", "use url::Url;");
        let bare = add_edge(&conn, user, "Url", "");
        let qualified = add_edge(&conn, user, "Url", "crate::Url");

        crate::index::install_scope_view(&conn, NEW, "").unwrap();
        resolve_all_edges(&conn).unwrap();

        let (to, _, resolution) = edge_state(&conn, bare);
        assert_eq!(to, None, "a bare `Url` from the external `url` crate must not bind locally");
        assert_eq!(resolution, "unresolved");

        let (to, _, _) = edge_state(&conn, qualified);
        assert_eq!(
            to,
            Some(local),
            "explicit `crate::Url` names the local item despite the import"
        );
    }

    /// #61 Project B (Codex review imports.rs:87 / resolve.rs:41): the imports edge stream emits the
    /// path PREFIX of a braced `use` (`std::path`) as well as the real bindings, so the scope must
    /// be built from parsed bindings — a local `path` must stay resolvable next to `use
    /// std::path::{…}`.
    #[test]
    fn use_path_prefix_does_not_suppress_a_local_name() {
        let conn = seeded_conn();
        set_local_crate_roots(&conn, "mycrate");
        let user = add_file(&conn, "a.rs", NEW);
        let local = add_symbol(&conn, user, "path", "crate::a::path");
        add_import_edge(&conn, user, "Path", "use std::path::{Path, PathBuf};");
        let call = add_edge(&conn, user, "path", "");

        crate::index::install_scope_view(&conn, NEW, "").unwrap();
        resolve_all_edges(&conn).unwrap();

        let (to, _, _) = edge_state(&conn, call);
        assert_eq!(
            to,
            Some(local),
            "`path` is the use PREFIX, not a binding — local `path` resolves"
        );
    }

    /// #61 Project B (Codex review resolve.rs:89): a path-qualified call whose RECEIVER is an
    /// external import (`Url::parse`, with `use url::Url`) must not bind to an in-repo `Url::parse`
    /// via the scope-path lookup — the leaf `parse` isn't itself imported, so the receiver root has
    /// to be checked. A call through a LOCAL receiver (`Widget::parse`) still resolves.
    #[test]
    fn qualified_call_through_an_external_receiver_is_suppressed() {
        let conn = seeded_conn();
        set_local_crate_roots(&conn, "mycrate");
        let user = add_file(&conn, "a.rs", NEW);
        let defs = add_file(&conn, "b.rs", NEW);
        add_import_edge(&conn, user, "Url", "use url::Url;");
        // A lowercase external import — a value-receiver method call must NOT be suppressed.
        add_import_edge(&conn, user, "config", "use external_dep::config;");
        add_symbol_scope(&conn, defs, "parse", "b.rs::parse_url", "Url::parse");
        let widget = add_symbol_scope(&conn, defs, "parse", "b.rs::parse_widget", "Widget::parse");
        // `config.build()` extracts as tqn `config::build` (helpers rewrites `.`→`::`); the head is
        // a local value receiver, not an external type path.
        let build = add_symbol_scope(&conn, defs, "build", "b.rs::cfg_build", "config::build");
        let external = add_edge(&conn, user, "parse", "Url::parse");
        let local = add_edge(&conn, user, "parse", "Widget::parse");
        let value_recv = add_edge(&conn, user, "build", "config::build");

        crate::index::install_scope_view(&conn, NEW, "").unwrap();
        resolve_all_edges(&conn).unwrap();

        let (to, _, resolution) = edge_state(&conn, external);
        assert_eq!(to, None, "`Url::parse` (external receiver) must not bind a local `Url::parse`");
        assert_eq!(resolution, "unresolved");

        let (to, _, _) = edge_state(&conn, local);
        assert_eq!(to, Some(widget), "`Widget::parse` (local receiver) resolves normally");

        let (to, _, _) = edge_state(&conn, value_recv);
        assert_eq!(
            to,
            Some(build),
            "`config.build()` (lowercase value receiver) must NOT be suppressed by the import"
        );
    }
}
